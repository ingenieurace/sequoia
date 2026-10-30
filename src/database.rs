//! Public entry point: owns the catalog and the disk it lives on.
//!
//! Every statement that changes data is atomic. When it succeeds, the catalog
//! is saved, dirty pages are written back from the buffer pool to the disk's
//! write-ahead log, and the disk commits them. When it fails, its writes are
//! rolled back: the buffer pool drops its cached pages, the disk drops the
//! log frames written since the last commit, and the catalog is opened again
//! from the committed pages. A crash in the middle of a statement likewise
//! loses all of it. Outside a transaction, a statement is a transaction of
//! its own, durable once it returns.
//!
//! **Transactions** (see `mvcc/mod.rs`). A transaction reads the snapshot
//! its first statement took, and writes versions of rows under its ids. Its
//! statements' pages are committed to the log as each statement ends,
//! without a sync, so that the statements of other sessions can run after
//! them; its versions count once `COMMIT` sets its bits in the commit log
//! and syncs the log. `ROLLBACK`, closing the database, or a crash aborts
//! its ids instead, and every reader skips their versions from then on. A
//! statement that fails inside a transaction is undone alone, back to the
//! last commit (the statement before), and the transaction goes on.
//! `SAVEPOINT name` marks a point to go back to: the writes after it are
//! made under a new id, which `ROLLBACK TO name` aborts (keeping the
//! savepoint, and dropping the ones taken after it); `RELEASE name` forgets
//! it (with those after it).
//!
//! **The schema.** A statement that changes the catalog (CREATE, DROP,
//! ANALYZE) leaves the transaction with the database to itself until it
//! ends (`shared.rs` keeps the other sessions out), and from then on its
//! statements are not committed one by one: they stay in the log, as every
//! transaction's did before there were versions, for `ROLLBACK` to cut off.
//!
//! **Sessions.** What a database holds, its pages, its catalog and its
//! transactions, is its [`Core`]; what each user of it holds, its settings
//! and its transaction, is a [`SessionState`]. A [`Database`] is one of
//! each, for one thread. A [`SharedDatabase`](crate::shared::SharedDatabase)
//! shares a core between the sessions of many threads (see `shared.rs`).

use std::cell::OnceCell;
use std::path::Path;
use std::sync::Arc;

use crate::catalog::Catalog;
use crate::error::{Error, Result, err};
use crate::executor::model::Model;
use crate::executor::{self, Query, Settings};
use crate::mvcc::{Base, Snapshot, Transactions};
use crate::plan::query::ForceJoin;
use crate::result::QueryResult;
use crate::sql::ast::Statement;
use crate::sql::parse;
use crate::storage::io::disk::Savepoint;
use crate::storage::version::{FROZEN, Xid};
use crate::storage::{BufferPool, DiskManager, Policy, Stats};

/// Frames in the buffer pool: 256 x 4 KB = 1 MB of cached pages.
const BUFFER_POOL_FRAMES: usize = 256;
/// Shards the frames are split into, each with a lock of its own, so that
/// threads reading pages at once mostly wait for no one (see
/// `storage/buffer/pool.rs`): 32 frames each, more than the pages the
/// threads of a query pin at once.
const BUFFER_POOL_SHARDS: usize = 8;

pub struct Database {
    core: Core,
    session: SessionState,
}

/// What every session of a database shares: its pages, its catalog and its
/// transactions.
pub(crate) struct Core {
    pool: Arc<BufferPool>,
    txns: Arc<Transactions>,
    catalog: Catalog,
    /// Set when a rollback failed, leaving the state in memory unknown.
    broken: bool,
}

/// What a session holds of its own: its settings and its transaction.
#[derive(Default)]
pub(crate) struct SessionState {
    pub(crate) settings: Settings,
    transaction: Option<Transaction>,
    /// A transaction still running whose row the last statement needed, for
    /// the session to wait for before it tries again.
    blocker: Option<Xid>,
}

#[derive(Default)]
struct Transaction {
    /// What it reads: taken by its first statement.
    base: OnceCell<Arc<Base>>,
    /// Its ids, oldest first: one, and then one more for each savepoint it
    /// wrote after. They commit or abort together, but for those rolled
    /// back to a savepoint, which abort then.
    xids: Vec<Xid>,
    /// Whether its next write needs a new id: after a savepoint, so that
    /// rolling back to it aborts only what follows.
    fresh: bool,
    savepoints: Vec<SavepointMark>,
    /// Whether it changed the schema: it has the database to itself, and its
    /// statements stay in the log uncommitted.
    alone: bool,
}

struct SavepointMark {
    name: String,
    /// How many ids the transaction had then.
    xids: usize,
    /// Where the log was then, if the transaction had the database to itself.
    disk: Option<Savepoint>,
}

/// The position of the newest savepoint with this name.
fn find(savepoints: &[SavepointMark], name: &str) -> Result<usize> {
    match savepoints.iter().rposition(|s| s.name == name) {
        Some(i) => Ok(i),
        None => err(format!("no such savepoint: {name}")),
    }
}

fn message(text: impl Into<String>) -> QueryResult {
    QueryResult::Message(text.into())
}

impl Core {
    pub(crate) fn in_memory() -> Self {
        Self::with_disk(DiskManager::in_memory()).expect("in-memory storage cannot fail")
    }

    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_disk(DiskManager::open(path)?)
    }

    fn with_disk(disk: DiskManager) -> Result<Self> {
        let pool = Arc::new(BufferPool::sharded(
            Arc::new(disk),
            BUFFER_POOL_FRAMES,
            BUFFER_POOL_SHARDS,
        ));
        let txns = Transactions::new();
        let mut catalog = Catalog::open(pool.clone(), txns.clone())?;
        catalog.save()?; // a new database's empty catalog, or an upgraded one
        pool.disk().commit()?;
        Ok(Core {
            pool,
            txns,
            catalog,
            broken: false,
        })
    }

    pub(crate) fn pool(&self) -> &Arc<BufferPool> {
        &self.pool
    }

    pub(crate) fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub(crate) fn transactions(&self) -> &Arc<Transactions> {
        &self.txns
    }

    /// An error if a failed rollback left the core unusable.
    fn usable(&self) -> Result<()> {
        match self.broken {
            true => err("a failed rollback left the database unusable: open it again"),
            false => Ok(()),
        }
    }

    /// Undoes what failed with `e`: everything since `to`, or since the last commit.
    fn undo(&mut self, e: Error, to: Option<&Savepoint>) -> Result<QueryResult> {
        match self.restore(to) {
            Ok(()) => Err(e),
            Err(rollback) => err(format!("{e}; then the rollback failed: {rollback}")),
        }
    }

    /// Undoes everything since `to`, or since the last commit, in memory and
    /// on disk. If that fails, the database is unusable.
    pub(crate) fn restore(&mut self, to: Option<&Savepoint>) -> Result<()> {
        let result = (|| {
            self.pool.discard_all()?;
            match to {
                Some(savepoint) => self.pool.disk().rollback_to(savepoint)?,
                None => self.pool.disk().rollback()?,
            }
            self.catalog = Catalog::open(self.pool.clone(), self.txns.clone())?;
            Ok(())
        })();
        self.broken |= result.is_err();
        result
    }

    /// Makes a transaction's writes count: sets the commit bits of its ids,
    /// and commits everything written since the last commit, synced. If this
    /// fails, the caller restores the core and aborts the ids.
    fn commit(&mut self, xids: &[Xid]) -> Result<()> {
        self.txns.write_commit(&self.pool, xids)?;
        self.catalog.save()?;
        self.pool.disk().commit()
    }

    /// After a commit, lets the tables tidy up (see `Table::tidy`), and
    /// commits what that changed apart. If it fails, it is undone and
    /// forgotten: the commit before stands.
    fn tidy(&mut self) {
        let tidied = (|| {
            if self.catalog.tidy()? {
                self.catalog.save()?;
                self.pool.disk().commit_unsynced()?;
            }
            Ok::<_, Error>(())
        })();
        if tidied.is_err() {
            let _ = self.restore(None);
        }
    }
}

impl SessionState {
    pub(crate) fn in_transaction(&self) -> bool {
        self.transaction.is_some()
    }

    /// The first id of the transaction in progress, if it wrote.
    pub(crate) fn first_xid(&self) -> Option<Xid> {
        self.transaction.as_ref()?.xids.first().copied()
    }

    /// The transaction whose row the last statement needed, taken once.
    pub(crate) fn take_blocker(&mut self) -> Option<Xid> {
        self.blocker.take()
    }

    /// Starts a transaction: `BEGIN`, which needs nothing of the core.
    pub(crate) fn begin(&mut self) -> Result<QueryResult> {
        if self.transaction.is_some() {
            return err("a transaction is already in progress");
        }
        self.transaction = Some(Transaction::default());
        Ok(message("transaction started"))
    }

    /// Ends the transaction in progress without the core: a rollback, for a
    /// session that goes away. Its ids abort; one that had the database to
    /// itself must be rolled back with the core instead.
    pub(crate) fn abandon(&mut self, txns: &Transactions) {
        if let Some(transaction) = self.transaction.take() {
            debug_assert!(!transaction.alone, "its pages are in the log");
            txns.end(&transaction.xids, false);
        }
    }

    /// Runs a single parsed statement on `core`.
    pub(crate) fn run(&mut self, core: &mut Core, statement: Statement) -> Result<QueryResult> {
        core.usable()?;
        match statement {
            Statement::Begin => self.begin(),
            Statement::Commit => {
                let Some(transaction) = self.transaction.take() else {
                    return err("no transaction is in progress");
                };
                if transaction.xids.is_empty() && !transaction.alone {
                    return Ok(message("transaction committed")); // it only read
                }
                match core.commit(&transaction.xids) {
                    Ok(()) => {
                        core.txns.end(&transaction.xids, true);
                        core.tidy();
                        Ok(message("transaction committed"))
                    }
                    Err(e) => {
                        core.txns.end(&transaction.xids, false);
                        core.undo(e, None)
                    }
                }
            }
            Statement::Rollback { savepoint: None } => {
                let Some(transaction) = self.transaction.take() else {
                    return err("no transaction is in progress");
                };
                core.txns.end(&transaction.xids, false);
                match transaction.alone {
                    true => core.restore(None)?,
                    // The row counts it changed count rows that are not there.
                    false => core.catalog.forget_row_counts(),
                }
                Ok(message("transaction rolled back"))
            }
            Statement::Rollback {
                savepoint: Some(name),
            } => {
                let transaction = self.transaction("ROLLBACK TO")?;
                let i = find(&transaction.savepoints, &name)?;
                transaction.savepoints.truncate(i + 1);
                let mark = &transaction.savepoints[i];
                let undone = transaction.xids.split_off(mark.xids);
                transaction.fresh = true;
                core.txns.end(&undone, false);
                match transaction.alone {
                    true => core.restore(mark.disk.as_ref())?,
                    false => core.catalog.forget_row_counts(),
                }
                Ok(message(format!("rolled back to savepoint {name}")))
            }
            Statement::Savepoint { name } => {
                let transaction = self.transaction("SAVEPOINT")?;
                let mark = SavepointMark {
                    name: name.clone(),
                    xids: transaction.xids.len(),
                    disk: transaction.alone.then(|| core.pool.disk().savepoint()),
                };
                transaction.savepoints.push(mark);
                transaction.fresh = true;
                Ok(message(format!("savepoint {name} created")))
            }
            Statement::Release { name } => {
                let transaction = self.transaction("RELEASE")?;
                let i = find(&transaction.savepoints, &name)?;
                transaction.savepoints.truncate(i);
                Ok(message(format!("savepoint {name} released")))
            }
            Statement::Checkpoint if self.transaction.is_some() => {
                err("CHECKPOINT cannot run inside a transaction")
            }
            Statement::Vacuum { .. } if self.transaction.is_some() => {
                err("VACUUM cannot run inside a transaction")
            }
            statement if executor::reads_only(&statement) => self.read(core, statement),
            statement => self.write(core, statement),
        }
    }

    /// What the statements of the transaction in progress read, taken now
    /// if this is its first; for a statement outside one, a snapshot of its
    /// own.
    fn base(&self, core: &Core) -> Arc<Base> {
        match &self.transaction {
            Some(transaction) => transaction
                .base
                .get_or_init(|| core.txns.snapshot(true))
                .clone(),
            None => core.txns.snapshot(false),
        }
    }

    /// The snapshot of a statement that only reads.
    pub(crate) fn read_snapshot(&self, core: &Core) -> Snapshot {
        let own = match &self.transaction {
            Some(transaction) => transaction.xids.clone(),
            None => Vec::new(),
        };
        Snapshot::new(self.base(core), own, None)
    }

    /// Runs a statement that only reads (see [`executor::reads_only`]) on a
    /// core that other sessions may be reading too.
    pub(crate) fn read(&self, core: &Core, statement: Statement) -> Result<QueryResult> {
        core.usable()?;
        let settings = self.settings.under(self.read_snapshot(core));
        executor::execute_read(&core.catalog, statement, &settings)
    }

    /// Runs a statement that changes data, atomically: commits it as a
    /// transaction of its own, or inside one, commits its pages to the log
    /// for the transaction's commit to count; or else undoes it.
    fn write(&mut self, core: &mut Core, statement: Statement) -> Result<QueryResult> {
        if self.transaction.is_none() {
            return self.autocommit(core, statement);
        }
        let base = self.base(core);
        let transaction = self.transaction.as_mut().expect("checked above");
        transaction.alone |= executor::changes_schema(&statement);
        let alone = transaction.alone;
        // The log undoes what a transaction with the database to itself
        // writes: if no one else can see a version either, it writes as a
        // statement outside a transaction does (see `autocommit`).
        let snapshot = if alone && core.txns.quiet(&transaction.xids, Some(&base)) {
            Snapshot::new(base, transaction.xids.clone(), Some(FROZEN)).alone()
        } else {
            if transaction.fresh || transaction.xids.is_empty() {
                let xid = core.txns.begin(transaction.xids.first().copied());
                transaction.xids.push(xid);
                transaction.fresh = false;
            }
            let xid = *transaction.xids.last().expect("one at least");
            Snapshot::new(base, transaction.xids.clone(), Some(xid))
        };
        let savepoint = alone.then(|| core.pool.disk().savepoint());
        let settings = self.settings.under(snapshot.clone());
        let result = executor::execute(&mut core.catalog, statement, &settings);
        let saved = result.and_then(|result| {
            core.catalog.save()?;
            if !alone {
                core.pool.disk().commit_unsynced()?;
            }
            Ok(result)
        });
        match saved {
            Ok(result) => Ok(result),
            Err(e) => {
                self.blocker = snapshot.take_blocker();
                core.undo(e, savepoint.as_ref())
            }
        }
    }

    /// Runs a statement that changes data outside a transaction, as a
    /// transaction of its own.
    fn autocommit(&mut self, core: &mut Core, statement: Statement) -> Result<QueryResult> {
        let base = core.txns.snapshot(false);
        if core.txns.quiet(&[], None) {
            // No one else can see a version, and the log undoes the
            // statement if it fails: it changes rows in place, and writes
            // versions every transaction sees once it commits, under no id
            // of its own (see `Snapshot::alone`). Its commit needs no bit.
            let snapshot = Snapshot::new(base, Vec::new(), Some(FROZEN)).alone();
            let settings = self.settings.under(snapshot);
            let result = executor::execute(&mut core.catalog, statement, &settings)
                .and_then(|result| core.commit(&[]).map(|()| result));
            return match result {
                Ok(result) => {
                    core.tidy();
                    Ok(result)
                }
                Err(e) => core.undo(e, None),
            };
        }
        let xid = core.txns.begin(None);
        let snapshot = Snapshot::new(base, vec![xid], Some(xid));
        let settings = self.settings.under(snapshot.clone());
        let result = executor::execute(&mut core.catalog, statement, &settings)
            .and_then(|result| core.commit(&[xid]).map(|()| result));
        match result {
            Ok(result) => {
                core.txns.end(&[xid], true);
                core.tidy();
                Ok(result)
            }
            Err(e) => {
                core.txns.end(&[xid], false);
                self.blocker = snapshot.take_blocker();
                core.undo(e, None)
            }
        }
    }

    /// The transaction in progress, for `statement` to use.
    fn transaction(&mut self, statement: &str) -> Result<&mut Transaction> {
        match &mut self.transaction {
            Some(transaction) => Ok(transaction),
            None => err(format!("{statement} can only be used in a transaction")),
        }
    }
}

impl Database {
    /// Creates a database that lives only in memory.
    pub fn in_memory() -> Self {
        Self::of(Core::in_memory())
    }

    /// Opens (or creates) a database stored in the given file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::of(Core::open(path)?))
    }

    #[cfg(test)]
    fn with_disk(disk: DiskManager) -> Result<Self> {
        Ok(Self::of(Core::with_disk(disk)?))
    }

    fn of(core: Core) -> Self {
        Database {
            core,
            session: SessionState::default(),
        }
    }

    /// Parses and runs every statement in `sql`, stopping at the first error.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        parse(sql)?.into_iter().map(|stmt| self.run(stmt)).collect()
    }

    /// Runs a single parsed statement.
    pub fn run(&mut self, statement: Statement) -> Result<QueryResult> {
        self.session.run(&mut self.core, statement)
    }

    /// Whether a transaction is in progress.
    pub fn in_transaction(&self) -> bool {
        self.session.in_transaction()
    }

    /// Runs one SELECT and returns its rows as they are produced, without
    /// collecting them: a scan with a filter and a limit reads only as far as
    /// it must, and a sort or aggregate holds only what it needs. The
    /// database cannot change while the rows are being read.
    pub fn query(&self, sql: &str) -> Result<Query<'_>> {
        self.core.usable()?;
        let mut statements = parse(sql)?;
        match (statements.pop(), statements.is_empty()) {
            (Some(Statement::Select(query)), true) => {
                let snapshot = self.session.read_snapshot(&self.core);
                let settings = self.session.settings.under(snapshot);
                executor::select(&self.core.catalog, *query, &settings)
            }
            _ => err("query runs a single SELECT; use execute for other statements"),
        }
    }

    /// Sets how many bytes a sort may buffer before it writes sorted runs to
    /// a temporary file ([`executor::sort::SORT_MEMORY`] by default).
    pub fn set_sort_memory(&mut self, bytes: usize) {
        self.session.settings.sort_memory = bytes;
    }

    /// Sets how many bytes of left rows a block nested loop join may hold at
    /// once ([`executor::joins::JOIN_MEMORY`] by default); it reads its right
    /// side once for each block of them.
    pub fn set_join_memory(&mut self, bytes: usize) {
        self.session.settings.join_memory = bytes;
    }

    /// Sets how many bytes of right rows a hash join, or of groups a hash
    /// aggregation, may hold ([`executor::joins::HASH_MEMORY`] by default)
    /// before it writes partitions of them to temporary files.
    pub fn set_hash_memory(&mut self, bytes: usize) {
        self.session.settings.hash_memory = bytes;
    }

    /// Sets the execution model every query runs in, or with `None`, lets
    /// each query run in the one that suits it, as by default (see
    /// [`executor::model`]).
    pub fn set_execution_model(&mut self, model: Option<Model>) {
        self.session.settings.model = model;
    }

    pub fn execution_model(&self) -> Option<Model> {
        self.session.settings.model
    }

    /// Makes every join use `method`, or with `None`, lets the planner pick
    /// each, as by default: for comparing the methods on the same query.
    /// A query with a join that cannot use it fails to plan (see [`ForceJoin`]).
    pub fn set_force_join(&mut self, method: Option<ForceJoin>) {
        self.session.settings.force_join = method;
    }

    /// Sets how many threads a hash join runs on (one by default). With
    /// more, every hash join that need not keep its left side's order is a
    /// parallel grace hash join, whose rows come out in no order, and its
    /// workers share the hash join's memory (see `executor/joins/parallel.rs`).
    pub fn set_join_workers(&mut self, workers: usize) {
        self.session.settings.join_workers = workers.max(1);
    }

    pub fn join_workers(&self) -> usize {
        self.session.settings.join_workers
    }

    /// Sets how many threads a scan of a heap table runs on (one by
    /// default). With more, every full scan of a heap whose filters and
    /// projection have no subquery is gathered from that many threads, each
    /// taking the next page in turn, and its rows come out in no order (see
    /// `executor/gather.rs`).
    pub fn set_scan_workers(&mut self, workers: usize) {
        self.session.settings.scan_workers = workers.max(1);
    }

    pub fn scan_workers(&self) -> usize {
        self.session.settings.scan_workers
    }

    /// Switches the buffer pool to another page replacement policy, keeping
    /// the pages it holds (see [`BufferPool::set_policy`]). The default is
    /// [`crate::storage::REPLACEMENT_POLICY`].
    pub fn set_replacement_policy(&mut self, policy: Policy) {
        self.core.pool.set_policy(policy);
    }

    pub fn replacement_policy(&self) -> Policy {
        self.core
            .pool
            .policy()
            .expect("the database's pool is made with a policy")
    }

    /// The buffer pool's hits and misses since its policy was last set, and
    /// how many frames it has.
    pub fn buffer_stats(&self) -> (Stats, usize) {
        (self.core.pool.stats(), self.core.pool.capacity())
    }

    pub fn table_names(&self) -> Vec<&str> {
        self.core.catalog.table_names()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::storage::io::medium::sim::Sim;
    use crate::types::Value;

    /// A database on simulated files: 0 is the database file, 1 its log.
    fn open(sim: &Arc<Sim>) -> Result<Database> {
        Database::with_disk(DiskManager::init(sim.file(0), sim.file(1))?)
    }

    fn values(rows: impl Iterator<Item = String>) -> String {
        rows.collect::<Vec<_>>().join(", ")
    }

    /// A heap, an LSM and a column table with some rows; the first two with an index on `tag`.
    fn setup(sim: &Arc<Sim>) -> Database {
        let mut db = open(sim).unwrap();
        for (table, storage) in [("h", "HEAP"), ("l", "LSM"), ("c", "COLUMN")] {
            let schema = "(id INT PRIMARY KEY, tag TEXT, n INT, body TEXT)";
            db.execute(&format!("CREATE TABLE {table} {schema} USING {storage}"))
                .unwrap();
            if storage != "COLUMN" {
                db.execute(&format!("CREATE INDEX {table}_tag ON {table} (tag)"))
                    .unwrap();
            }
            let rows = values(
                (0..300).map(|i| format!("({i}, 'tag{}', {i}, '{}')", i % 7, "b".repeat(100))),
            );
            db.execute(&format!("INSERT INTO {table} VALUES {rows}"))
                .unwrap();
        }
        db
    }

    /// Statements that each write many pages: heap pages and B+ tree nodes
    /// split, rows move, an LSM tree flushes, a column store migrates.
    fn statements() -> Vec<String> {
        let rows = |n: i64, body: usize| {
            values(
                (1000..1000 + n)
                    .map(|i| format!("({i}, 'new{}', {i}, '{}')", i % 5, "n".repeat(body))),
            )
        };
        vec![
            format!("INSERT INTO h VALUES {}", rows(200, 150)),
            format!(
                "UPDATE h SET body = '{}', tag = 'moved' WHERE id < 100",
                "z".repeat(300)
            ),
            "DELETE FROM h WHERE tag = 'tag3' OR id > 250".into(),
            "CREATE INDEX h_n ON h (n)".into(),
            format!("INSERT INTO l VALUES {}", rows(400, 200)),
            "UPDATE l SET id = id + 5000, tag = 'up' WHERE n < 150".into(),
            format!("INSERT INTO c VALUES {}", rows(1100, 10)),
            "DROP TABLE h".into(),
        ]
    }

    fn rows(db: &mut Database, sql: &str) -> Result<Vec<Vec<Value>>> {
        match db.execute(sql)?.pop() {
            Some(QueryResult::Rows { mut rows, .. }) => {
                rows.sort_by(|a, b| a[0].sort_cmp(&b[0]));
                Ok(rows)
            }
            other => panic!("expected rows, got {other:?}"),
        }
    }

    /// Every table's rows by id and how it finds `n = 5` (which shows its
    /// indexes), after checking that its indexes agree with its rows.
    fn snapshot(db: &mut Database) -> Vec<(&'static str, String, Vec<Vec<Value>>)> {
        let mut snapshot = Vec::new();
        for table in ["h", "l", "c"] {
            let Ok(all) = rows(db, &format!("SELECT * FROM {table}")) else {
                continue; // dropped
            };
            if table != "c" {
                let mut tags: Vec<Value> = all.iter().map(|row| row[1].clone()).collect();
                tags.sort_by(|a, b| a.sort_cmp(b));
                tags.dedup();
                for tag in tags {
                    let found = rows(
                        db,
                        &format!("SELECT * FROM {table} WHERE tag = {}", tag.to_sql()),
                    )
                    .unwrap();
                    let expected: Vec<Vec<Value>> =
                        all.iter().filter(|row| row[1] == tag).cloned().collect();
                    assert!(found == expected, "index {table}_tag for {}", tag.to_sql());
                }
                let found = rows(
                    db,
                    &format!("SELECT * FROM {table} WHERE n >= 100 AND n < 200"),
                )
                .unwrap();
                let in_range =
                    |row: &&Vec<Value>| matches!(row[2], Value::Int(n) if (100..200).contains(&n));
                let expected: Vec<Vec<Value>> = all.iter().filter(in_range).cloned().collect();
                assert!(found == expected, "range of n in {table}");
            }
            let plan = match db
                .execute(&format!("EXPLAIN SELECT * FROM {table} WHERE n = 5"))
                .unwrap()
                .pop()
            {
                Some(QueryResult::Message(plan)) => plan,
                other => panic!("expected a plan, got {other:?}"),
            };
            snapshot.push((table, plan, all));
        }
        snapshot
    }

    #[test]
    fn committed_statements_survive_a_crash() {
        let sim = Sim::new(2);
        let mut db = setup(&sim);
        for statement in statements() {
            db.execute(&statement).unwrap();
        }
        let expected = snapshot(&mut db);
        // Only what was synced reaches the disk.
        let mut db = open(&sim.crashed(0)).unwrap();
        assert!(snapshot(&mut db) == expected);
    }

    #[test]
    fn a_crash_loses_all_of_a_statement_or_none() {
        for (i, statement) in statements().into_iter().enumerate() {
            // Run it once to learn how many operations it does and what it leaves.
            let sim = Sim::new(2);
            let mut db = setup(&sim);
            let before = snapshot(&mut db);
            let start = sim.ops();
            db.execute(&statement).unwrap();
            let ops = sim.ops() - start;
            let after = snapshot(&mut db);
            assert!(before != after, "statement {i} changes something");

            for crash in (0..ops).step_by(ops.div_ceil(12)).chain([ops - 1]) {
                let sim = Sim::new(2);
                let mut db = setup(&sim);
                sim.crash_after(crash);
                let result = db.execute(&statement);
                for kept in [0, 1, usize::MAX] {
                    let mut db = open(&sim.crashed(kept)).unwrap();
                    let found = snapshot(&mut db);
                    let label = format!(
                        "statement {i}, crash after {crash} of {ops} ops, {kept} unsynced kept"
                    );
                    match &result {
                        Ok(_) => assert!(found == after, "{label}"),
                        Err(_) => assert!(found == before || found == after, "{label}"),
                    }
                }
            }
        }
    }

    #[test]
    fn a_failed_statement_is_rolled_back() {
        let sim = Sim::new(2);
        let mut db = setup(&sim);
        let mut expected = snapshot(&mut db);
        for (i, statement) in statements().iter().enumerate() {
            let mut done = false;
            for fail in [0, 3, 40, 200] {
                sim.fail_once_after(fail);
                match db.execute(statement) {
                    Ok(_) => {
                        done = true; // it did fewer operations than that
                        break;
                    }
                    Err(e) => {
                        assert_eq!(e.0, "simulated I/O error", "statement {i}");
                        assert!(
                            snapshot(&mut db) == expected,
                            "statement {i} failing after {fail} ops left nothing"
                        );
                    }
                }
            }
            sim.heal();
            if !done {
                db.execute(statement).unwrap();
            }
            expected = snapshot(&mut db);
        }
        // Everything committed survives a crash.
        let mut db = open(&sim.crashed(0)).unwrap();
        assert!(snapshot(&mut db) == expected);
    }
    /// Runs every statement of `statements()` in one transaction.
    fn transaction(db: &mut Database, end: &str) -> Result<()> {
        db.execute("BEGIN")?;
        for statement in statements() {
            db.execute(&statement)?;
        }
        db.execute(end).map(drop)
    }

    #[test]
    fn a_crash_loses_all_of_a_transaction_or_none() {
        let sim = Sim::new(2);
        let mut db = setup(&sim);
        let before = snapshot(&mut db);
        let start = sim.ops();
        transaction(&mut db, "COMMIT").unwrap();
        let ops = sim.ops() - start;
        let after = snapshot(&mut db);

        for crash in (0..ops).step_by(ops.div_ceil(24)).chain([ops - 1]) {
            let sim = Sim::new(2);
            let mut db = setup(&sim);
            sim.crash_after(crash);
            let result = transaction(&mut db, "COMMIT");
            for kept in [0, 1, usize::MAX] {
                let mut db = open(&sim.crashed(kept)).unwrap();
                let found = snapshot(&mut db);
                let label = format!("crash after {crash} of {ops} ops, {kept} unsynced kept");
                match &result {
                    Ok(()) => assert!(found == after, "{label}"),
                    Err(_) => assert!(found == before || found == after, "{label}"),
                }
            }
        }
    }

    #[test]
    fn a_rolled_back_transaction_leaves_nothing() {
        let sim = Sim::new(2);
        let mut db = setup(&sim);
        let before = snapshot(&mut db);
        db.execute("BEGIN").unwrap();
        // About 2 MB, twice the buffer pool, so dirty pages are evicted into the log.
        let big = |from: i64| {
            values((from..from + 3000).map(|i| format!("({i}, 'big', {i}, '{}')", "b".repeat(600))))
        };
        db.execute(&format!("INSERT INTO h VALUES {}", big(10_000)))
            .unwrap();
        // A statement failing after evictions is undone alone: the last row is a duplicate.
        let failing = format!(
            "INSERT INTO h VALUES {}, (10000, 'dup', 0, '')",
            big(20_000)
        );
        assert!(db.execute(&failing).unwrap_err().0.contains("duplicate"));
        let count = rows(&mut db, "SELECT COUNT(*) FROM h WHERE tag = 'big'").unwrap();
        assert_eq!(count, vec![vec![Value::Int(3000)]]);
        for statement in statements() {
            db.execute(&statement).unwrap();
        }
        db.execute("ROLLBACK").unwrap();
        assert!(snapshot(&mut db) == before);
        let mut db = open(&sim.crashed(usize::MAX)).unwrap();
        assert!(snapshot(&mut db) == before);
    }

    #[test]
    fn a_failed_statement_in_a_transaction_is_undone_alone() {
        let sim = Sim::new(2);
        let mut db = setup(&sim);
        let before = snapshot(&mut db);
        db.execute("BEGIN").unwrap();
        let mut expected = before.clone();
        for (i, statement) in statements().iter().enumerate() {
            let mut done = false;
            for fail in [0, 3, 40, 200] {
                sim.fail_once_after(fail);
                match db.execute(statement) {
                    Ok(_) => {
                        done = true;
                        break;
                    }
                    Err(e) => {
                        assert_eq!(e.0, "simulated I/O error", "statement {i}");
                        assert!(db.in_transaction(), "statement {i}");
                        assert!(
                            snapshot(&mut db) == expected,
                            "statement {i} failing after {fail} ops left nothing"
                        );
                    }
                }
            }
            sim.heal();
            if !done {
                db.execute(statement).unwrap();
            }
            expected = snapshot(&mut db);
        }
        // Nothing is durable before COMMIT, and everything after.
        assert!(snapshot(&mut open(&sim.crashed(usize::MAX)).unwrap()) == before);
        db.execute("COMMIT").unwrap();
        assert!(snapshot(&mut open(&sim.crashed(0)).unwrap()) == expected);
    }

    /// Runs statements in one session of a core shared with others.
    fn run(session: &mut SessionState, core: &mut Core, sql: &str) {
        for statement in parse(sql).unwrap() {
            session.run(core, statement).unwrap();
        }
    }

    #[test]
    fn a_crash_aborts_a_transaction_whose_pages_another_commit_made_durable() {
        let sim = Sim::new(2);
        let mut core =
            Core::with_disk(DiskManager::init(sim.file(0), sim.file(1)).unwrap()).unwrap();
        let (mut a, mut b) = (SessionState::default(), SessionState::default());
        for using in ["HEAP", "LSM", "COLUMN"] {
            let table = using.to_lowercase();
            run(
                &mut a,
                &mut core,
                &format!(
                    "CREATE TABLE {table} (id INT PRIMARY KEY, n INT) USING {using};
                     INSERT INTO {table} VALUES (1, 0), (2, 0)"
                ),
            );
        }
        // a's statements reach the log, committed there but not synced...
        run(&mut a, &mut core, "BEGIN");
        for table in ["heap", "lsm", "column"] {
            run(
                &mut a,
                &mut core,
                &format!(
                    "UPDATE {table} SET n = 1 WHERE id = 1; INSERT INTO {table} VALUES (3, 0);
                     DELETE FROM {table} WHERE id = 2"
                ),
            );
        }
        // ...and b's commit syncs them, with its own.
        run(&mut b, &mut core, "INSERT INTO heap VALUES (10, 5)");
        let mut db = open(&sim.crashed(0)).unwrap();
        let expected = vec![
            vec![Value::Int(1), Value::Int(0)],
            vec![Value::Int(2), Value::Int(0)],
        ];
        let mut with_b = expected.clone();
        with_b.push(vec![Value::Int(10), Value::Int(5)]);
        assert_eq!(rows(&mut db, "SELECT id, n FROM heap").unwrap(), with_b);
        for table in ["lsm", "column"] {
            assert_eq!(
                rows(&mut db, &format!("SELECT id, n FROM {table}")).unwrap(),
                expected
            );
        }
        // a's rows are there, written by a transaction that aborted: vacuum removes them.
        match db.execute("VACUUM").unwrap().pop() {
            Some(QueryResult::Message(text)) => assert!(!text.starts_with("0 "), "{text}"),
            other => panic!("expected a message, got {other:?}"),
        }
        assert_eq!(rows(&mut db, "SELECT id, n FROM heap").unwrap(), with_b);

        // Once a commits, its writes survive a crash.
        run(&mut a, &mut core, "COMMIT");
        let mut db = open(&sim.crashed(0)).unwrap();
        for table in ["heap", "lsm", "column"] {
            let found = rows(&mut db, &format!("SELECT id, n FROM {table}")).unwrap();
            let ids: Vec<Value> = found.iter().map(|row| row[0].clone()).collect();
            let mut want = vec![Value::Int(1), Value::Int(3)];
            if table == "heap" {
                want.push(Value::Int(10));
            }
            assert_eq!(ids, want, "{table}");
            assert_eq!(found[0][1], Value::Int(1), "{table}");
        }
    }
}
