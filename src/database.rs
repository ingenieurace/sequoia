//! Public entry point: owns the catalog and the disk it lives on.
//!
//! Every statement that changes data is atomic. When it succeeds, the catalog
//! is saved, dirty pages are written back from the buffer pool to the disk's
//! write-ahead log, and the disk commits them, so a statement that returns
//! successfully is durable. When it fails, its writes are rolled back: the
//! buffer pool drops its cached pages, the disk drops the uncommitted log
//! frames, and the catalog is opened again from the committed pages. A crash
//! in the middle of a statement likewise loses all of it.
//!
//! **Transactions.** Between `BEGIN` and `COMMIT`, statements are saved to
//! the log as they succeed but not committed, so later ones see them and
//! `COMMIT` makes all of them durable at once. `ROLLBACK`, closing the
//! database, or a crash undoes all of them. A statement that fails inside a
//! transaction is undone alone, back to a savepoint of the disk taken before
//! it (with the pool clean, as the statement before saved everything), and
//! the transaction goes on. `SAVEPOINT name` takes such a savepoint by name,
//! for `ROLLBACK TO name` to go back to (keeping it, and dropping the ones
//! taken after it) and `RELEASE name` to forget (with those after it).

use std::path::Path;
use std::sync::Arc;

use crate::catalog::Catalog;
use crate::error::{Error, Result, err};
use crate::executor::model::Model;
use crate::executor::{self, Query, Settings};
use crate::sql::ast::Statement;
use crate::storage::io::disk::Savepoint;
use crate::sql::parse;
use crate::result::QueryResult;
use crate::storage::{BufferPool, DiskManager};

/// Frames in the buffer pool: 256 x 4 KB = 1 MB of cached pages.
const BUFFER_POOL_FRAMES: usize = 256;

pub struct Database {
    pool: Arc<BufferPool>,
    catalog: Catalog,
    settings: Settings,
    /// The savepoints of the transaction in progress, oldest first, if there is one.
    transaction: Option<Vec<(String, Savepoint)>>,
    /// Set when a rollback failed, leaving the state in memory unknown.
    broken: bool,
}

/// The position of the newest savepoint with this name.
fn find(savepoints: &[(String, Savepoint)], name: &str) -> Result<usize> {
    match savepoints.iter().rposition(|(n, _)| n == name) {
        Some(i) => Ok(i),
        None => err(format!("no such savepoint: {name}")),
    }
}

fn message(text: impl Into<String>) -> QueryResult {
    QueryResult::Message(text.into())
}

impl Database {
    /// Creates a database that lives only in memory.
    pub fn in_memory() -> Self {
        Self::with_disk(DiskManager::in_memory()).expect("in-memory storage cannot fail")
    }

    /// Opens (or creates) a database stored in the given file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_disk(DiskManager::open(path)?)
    }

    fn with_disk(disk: DiskManager) -> Result<Self> {
        let pool = Arc::new(BufferPool::new(Arc::new(disk), BUFFER_POOL_FRAMES));
        let mut catalog = Catalog::open(pool.clone())?;
        catalog.save()?; // a new database's empty catalog
        pool.disk().commit()?;
        Ok(Database { pool, catalog, settings: Settings::default(), transaction: None, broken: false })
    }

    /// Parses and runs every statement in `sql`, stopping at the first error.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        parse(sql)?.into_iter().map(|stmt| self.run(stmt)).collect()
    }

    /// Runs a single parsed statement.
    pub fn run(&mut self, statement: Statement) -> Result<QueryResult> {
        if self.broken {
            return err("a failed rollback left the database unusable: open it again");
        }
        match statement {
            Statement::Begin => {
                if self.transaction.is_some() {
                    return err("a transaction is already in progress");
                }
                self.transaction = Some(Vec::new());
                Ok(message("transaction started"))
            }
            Statement::Commit => {
                if self.transaction.take().is_none() {
                    return err("no transaction is in progress");
                }
                match self.pool.disk().commit() {
                    Ok(()) => Ok(message("transaction committed")),
                    Err(e) => self.undo(e, None),
                }
            }
            Statement::Rollback { savepoint: None } => {
                if self.transaction.take().is_none() {
                    return err("no transaction is in progress");
                }
                self.restore(None)?;
                Ok(message("transaction rolled back"))
            }
            Statement::Rollback { savepoint: Some(name) } => {
                let savepoints = self.savepoints("ROLLBACK TO")?;
                let i = find(savepoints, &name)?;
                savepoints.truncate(i + 1);
                let savepoint = savepoints[i].1.clone();
                self.restore(Some(&savepoint))?;
                Ok(message(format!("rolled back to savepoint {name}")))
            }
            Statement::Savepoint { name } => {
                let savepoint = self.pool.disk().savepoint();
                self.savepoints("SAVEPOINT")?.push((name.clone(), savepoint));
                Ok(message(format!("savepoint {name} created")))
            }
            Statement::Release { name } => {
                let savepoints = self.savepoints("RELEASE")?;
                let i = find(savepoints, &name)?;
                savepoints.truncate(i);
                Ok(message(format!("savepoint {name} released")))
            }
            Statement::Checkpoint if self.transaction.is_some() => err("CHECKPOINT cannot run inside a transaction"),
            statement => self.statement(statement),
        }
    }

    /// Runs a statement other than the transaction ones, atomically: commits
    /// it, or inside a transaction saves it, or else undoes it.
    fn statement(&mut self, statement: Statement) -> Result<QueryResult> {
        let modifies = !matches!(statement, Statement::Select(_) | Statement::Explain { .. });
        let savepoint = (modifies && self.transaction.is_some()).then(|| self.pool.disk().savepoint());
        let result = executor::execute(&mut self.catalog, statement, &self.settings);
        if !modifies {
            return result;
        }
        let saved = result.and_then(|result| {
            self.catalog.save()?;
            if self.transaction.is_none() {
                self.pool.disk().commit()?;
            }
            Ok(result)
        });
        match saved {
            Ok(result) => Ok(result),
            Err(e) => self.undo(e, savepoint.as_ref()),
        }
    }

    /// Whether a transaction is in progress.
    pub fn in_transaction(&self) -> bool {
        self.transaction.is_some()
    }

    /// The savepoints of the transaction in progress, for `statement` to use.
    fn savepoints(&mut self, statement: &str) -> Result<&mut Vec<(String, Savepoint)>> {
        match &mut self.transaction {
            Some(savepoints) => Ok(savepoints),
            None => err(format!("{statement} can only be used in a transaction")),
        }
    }

    /// Undoes what failed with `e`: everything since `to`, or since the last commit.
    fn undo(&mut self, e: Error, to: Option<&Savepoint>) -> Result<QueryResult> {
        match self.restore(to) {
            Ok(()) => Err(e),
            Err(rollback) => err(format!("{e}; then the rollback failed: {rollback}")),
        }
    }

    /// Runs one SELECT and returns its rows as they are produced, without
    /// collecting them: a scan with a filter and a limit reads only as far as
    /// it must, and a sort or aggregate holds only what it needs. The
    /// database cannot change while the rows are being read.
    pub fn query(&self, sql: &str) -> Result<Query<'_>> {
        if self.broken {
            return err("a failed rollback left the database unusable: open it again");
        }
        let mut statements = parse(sql)?;
        match (statements.pop(), statements.is_empty()) {
            (Some(Statement::Select(query)), true) => executor::select(&self.catalog, query, &self.settings),
            _ => err("query runs a single SELECT; use execute for other statements"),
        }
    }

    /// Undoes everything since `to`, or since the last commit, in memory and
    /// on disk. If that fails, the database is unusable.
    fn restore(&mut self, to: Option<&Savepoint>) -> Result<()> {
        let result = (|| {
            self.pool.discard_all()?;
            match to {
                Some(savepoint) => self.pool.disk().rollback_to(savepoint)?,
                None => self.pool.disk().rollback()?,
            }
            self.catalog = Catalog::open(self.pool.clone())?;
            Ok(())
        })();
        self.broken |= result.is_err();
        result
    }

    /// Sets how many bytes a sort may buffer before it writes sorted runs to
    /// a temporary file ([`executor::sort::SORT_MEMORY`] by default).
    pub fn set_sort_memory(&mut self, bytes: usize) {
        self.settings.sort_memory = bytes;
    }

    /// Sets how many bytes of left rows a block nested loop join may hold at
    /// once ([`executor::join::JOIN_MEMORY`] by default); it reads its right
    /// side once for each block of them.
    pub fn set_join_memory(&mut self, bytes: usize) {
        self.settings.join_memory = bytes;
    }

    /// Sets how many bytes of right rows a hash join, or of groups a hash
    /// aggregation, may hold ([`executor::join::HASH_MEMORY`] by default)
    /// before it writes partitions of them to temporary files.
    pub fn set_hash_memory(&mut self, bytes: usize) {
        self.settings.hash_memory = bytes;
    }

    /// Sets the execution model every query runs in, or with `None`, lets
    /// each query run in the one that suits it, as by default (see
    /// [`executor::model`]).
    pub fn set_execution_model(&mut self, model: Option<Model>) {
        self.settings.model = model;
    }

    pub fn execution_model(&self) -> Option<Model> {
        self.settings.model
    }

    pub fn table_names(&self) -> Vec<&str> {
        self.catalog.table_names()
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
            db.execute(&format!("CREATE TABLE {table} {schema} USING {storage}")).unwrap();
            if storage != "COLUMN" {
                db.execute(&format!("CREATE INDEX {table}_tag ON {table} (tag)")).unwrap();
            }
            let rows = values((0..300).map(|i| format!("({i}, 'tag{}', {i}, '{}')", i % 7, "b".repeat(100))));
            db.execute(&format!("INSERT INTO {table} VALUES {rows}")).unwrap();
        }
        db
    }

    /// Statements that each write many pages: heap pages and B+ tree nodes
    /// split, rows move, an LSM tree flushes, a column store migrates.
    fn statements() -> Vec<String> {
        let rows = |n: i64, body: usize| {
            values((1000..1000 + n).map(|i| format!("({i}, 'new{}', {i}, '{}')", i % 5, "n".repeat(body))))
        };
        vec![
            format!("INSERT INTO h VALUES {}", rows(200, 150)),
            format!("UPDATE h SET body = '{}', tag = 'moved' WHERE id < 100", "z".repeat(300)),
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
                    let found = rows(db, &format!("SELECT * FROM {table} WHERE tag = {}", tag.to_sql())).unwrap();
                    let expected: Vec<Vec<Value>> = all.iter().filter(|row| row[1] == tag).cloned().collect();
                    assert!(found == expected, "index {table}_tag for {}", tag.to_sql());
                }
                let found = rows(db, &format!("SELECT * FROM {table} WHERE n >= 100 AND n < 200")).unwrap();
                let in_range = |row: &&Vec<Value>| matches!(row[2], Value::Int(n) if (100..200).contains(&n));
                let expected: Vec<Vec<Value>> = all.iter().filter(in_range).cloned().collect();
                assert!(found == expected, "range of n in {table}");
            }
            let plan = match db.execute(&format!("EXPLAIN SELECT * FROM {table} WHERE n = 5")).unwrap().pop() {
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
                    let label = format!("statement {i}, crash after {crash} of {ops} ops, {kept} unsynced kept");
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
                        assert!(snapshot(&mut db) == expected, "statement {i} failing after {fail} ops left nothing");
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
        let big = |from: i64| values((from..from + 3000).map(|i| format!("({i}, 'big', {i}, '{}')", "b".repeat(600))));
        db.execute(&format!("INSERT INTO h VALUES {}", big(10_000))).unwrap();
        // A statement failing after evictions is undone alone: the last row is a duplicate.
        let failing = format!("INSERT INTO h VALUES {}, (10000, 'dup', 0, '')", big(20_000));
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
                        assert!(snapshot(&mut db) == expected, "statement {i} failing after {fail} ops left nothing");
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
}
