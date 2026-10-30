//! A database shared by the threads of a process: the thread model, in
//! which every connection is a thread with a session of its own, and all of
//! them read the same pages through the same buffer pool.
//!
//! ```text
//! SharedDatabase (clone it into each thread)
//!   gate: who may use the core now      core: pages, catalog, transactions,
//!     readers  3   writer  no             in a RwLock (one Core, as a
//!     open transactions  2                Database has)
//!     alone    no
//!   Session (one per thread): settings, transaction, busy timeout
//! ```
//!
//! **Transactions run side by side.** Each reads its own snapshot and
//! writes versions of rows (see `mvcc/mod.rs`), so any number of them may
//! be open at once, reading and writing: none waits for another to end
//! unless it needs a row the other is changing.
//!
//! **Statements take turns.** Any number of statements that read (SELECT,
//! EXPLAIN) may run at once, or one that writes, as SQLite's do: a statement
//! that writes waits until no other statement runs, and keeps the core to
//! itself until it ends, not until its transaction does. A statement waiting
//! to write goes before statements that come to read after it, so readers
//! cannot hold it off forever.
//!
//! **Rows.** A statement that needs to change a row another transaction is
//! changing, or to insert a key another transaction has written, fails,
//! undone; its session then waits for that transaction to end, and runs the
//! statement again. If the other committed, a transaction that cannot see
//! its write fails for good ("could not serialize access"), and its client
//! may roll back and try again; a statement outside a transaction just sees
//! the new row. Two transactions waiting for each other are a deadlock: the
//! second to wait fails at once.
//!
//! **The schema.** A statement that changes the catalog (CREATE, ALTER, DROP,
//! ANALYZE) waits until no other session has a transaction open, and then
//! has the database to itself until its own transaction ends: other
//! sessions' statements wait, as transactions did before there were
//! versions. Sessions waiting for that let the open transactions finish
//! first, but start no new one.
//!
//! A session waits at most its busy timeout ([`BUSY_TIMEOUT`] by default),
//! for its turn and for the rows it needs together, then fails with
//! "database is locked" or the conflict it found. A session dropped in a
//! transaction rolls it back.
//!
//! The gate decides who goes; the core's RwLock only hands each its access,
//! and never waits. A thread that panics while writing poisons the lock: the
//! next to take it rolls back what was not committed.

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockWriteGuard};
use std::time::{Duration, Instant};

use crate::database::{Core, SessionState};
use crate::error::{Result, err};
use crate::executor;
use crate::executor::model::Model;
use crate::mvcc::Transactions;
use crate::plan::query::ForceJoin;
use crate::result::QueryResult;
use crate::sql::ast::Statement;
use crate::sql::parse;
use crate::storage::{Policy, Stats};

/// How long a session waits for its turn by default.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A database that many threads use at once, each through a [`Session`] of
/// its own. Cloning it shares the same database.
#[derive(Clone)]
pub struct SharedDatabase {
    shared: Arc<Shared>,
}

struct Shared {
    gate: Gate,
    core: RwLock<Core>,
    /// The core's transactions, to wait for one without the core.
    txns: Arc<Transactions>,
}

impl SharedDatabase {
    /// Creates a database that lives only in memory.
    pub fn in_memory() -> Self {
        Self::of(Core::in_memory())
    }

    /// Opens (or creates) a database stored in the given file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::of(Core::open(path)?))
    }

    fn of(core: Core) -> Self {
        SharedDatabase {
            shared: Arc::new(Shared {
                gate: Gate::default(),
                txns: core.transactions().clone(),
                core: RwLock::new(core),
            }),
        }
    }

    /// A new session, with the default settings and no transaction.
    pub fn session(&self) -> Session {
        Session {
            shared: self.shared.clone(),
            state: SessionState::default(),
            open: false,
            alone: false,
            busy_timeout: BUSY_TIMEOUT,
        }
    }

    /// Switches the buffer pool to another page replacement policy, for
    /// every session (see [`crate::Database::set_replacement_policy`]).
    pub fn set_replacement_policy(&self, policy: Policy) {
        self.shared.read_core().pool().set_policy(policy);
    }

    pub fn replacement_policy(&self) -> Policy {
        let core = self.shared.read_core();
        core.pool()
            .policy()
            .expect("the database's pool is made with a policy")
    }

    /// The buffer pool's hits and misses since its policy was last set, and
    /// how many frames it has.
    pub fn buffer_stats(&self) -> (Stats, usize) {
        let core = self.shared.read_core();
        (core.pool().stats(), core.pool().capacity())
    }
}

impl Shared {
    /// The core to read. Only a writer poisons the lock, and the next to
    /// write repairs it, so a reader may take it as it is: the gate lets no
    /// reader in before the writer's statement has ended, rolling back.
    fn read_core(&self) -> std::sync::RwLockReadGuard<'_, Core> {
        self.core.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The core to change. If a thread panicked while changing it, what it
    /// did not commit is rolled back first.
    fn write_core(&self) -> RwLockWriteGuard<'_, Core> {
        match self.core.write() {
            Ok(core) => core,
            Err(poisoned) => {
                let mut core = poisoned.into_inner();
                self.core.clear_poison();
                // If this fails, the core says so on its next use.
                let _ = core.restore(None);
                core
            }
        }
    }
}

/// One thread's use of a [`SharedDatabase`]: its own settings and
/// transaction, as a [`Database`](crate::Database) has.
pub struct Session {
    shared: Arc<Shared>,
    state: SessionState,
    /// Whether this session has a transaction open, counted at the gate.
    open: bool,
    /// Whether this session has the database to itself, from a statement
    /// that changed the schema until its transaction ends.
    alone: bool,
    busy_timeout: Duration,
}

impl Session {
    /// Parses and runs every statement in `sql`, stopping at the first error.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        parse(sql)?.into_iter().map(|stmt| self.run(stmt)).collect()
    }

    /// Runs a single parsed statement, once it is this session's turn. A
    /// statement that needed a row another transaction was changing runs
    /// again once that transaction ends.
    pub fn run(&mut self, statement: Statement) -> Result<QueryResult> {
        let deadline = Instant::now() + self.busy_timeout;
        loop {
            let result = self.attempt(statement.clone(), deadline);
            match (&result, self.state.take_blocker()) {
                (Err(_), Some(blocker)) => {
                    let waiter = self.state.first_xid();
                    self.shared.txns.wait(waiter, blocker, deadline)?;
                }
                _ => return result,
            }
        }
    }

    /// Runs a statement once, after waiting at the gate until `deadline`.
    fn attempt(&mut self, statement: Statement, deadline: Instant) -> Result<QueryResult> {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if self.alone {
            // No other session is in: nothing to wait for.
            let result = self.state.run(&mut self.shared.write_core(), statement);
            self.after();
            return result;
        }
        let in_transaction = self.state.in_transaction();
        if let Statement::Begin = statement {
            if in_transaction {
                return self.state.begin();
            }
            self.shared.gate.begin(timeout)?;
            self.open = true;
            return self.state.begin();
        }
        if executor::reads_only(&statement) {
            let _reading = self.shared.gate.read(timeout, in_transaction)?;
            return self.state.read(&self.shared.read_core(), statement);
        }
        if executor::changes_schema(&statement) {
            self.shared.gate.claim(timeout, in_transaction)?;
            self.alone = true;
            let result = self.state.run(&mut self.shared.write_core(), statement);
            self.after();
            return result;
        }
        self.shared.gate.write(timeout, in_transaction)?;
        let result = self.state.run(&mut self.shared.write_core(), statement);
        self.shared.gate.leave_write();
        self.after();
        result
    }

    /// Whether a transaction is in progress.
    pub fn in_transaction(&self) -> bool {
        self.state.in_transaction()
    }

    /// The names of the tables, once this session may read.
    pub fn table_names(&self) -> Result<Vec<String>> {
        let _reading = match self.alone {
            true => None,
            false => Some(
                self.shared
                    .gate
                    .read(self.busy_timeout, self.state.in_transaction())?,
            ),
        };
        let core = self.shared.read_core();
        Ok(core
            .catalog()
            .table_names()
            .into_iter()
            .map(String::from)
            .collect())
    }

    /// Sets how long this session waits for its turn, and for the rows it
    /// needs, before it fails.
    pub fn set_busy_timeout(&mut self, timeout: Duration) {
        self.busy_timeout = timeout;
    }

    /// See [`crate::Database::set_sort_memory`].
    pub fn set_sort_memory(&mut self, bytes: usize) {
        self.state.settings.sort_memory = bytes;
    }

    /// See [`crate::Database::set_join_memory`].
    pub fn set_join_memory(&mut self, bytes: usize) {
        self.state.settings.join_memory = bytes;
    }

    /// See [`crate::Database::set_hash_memory`].
    pub fn set_hash_memory(&mut self, bytes: usize) {
        self.state.settings.hash_memory = bytes;
    }

    /// See [`crate::Database::set_execution_model`].
    pub fn set_execution_model(&mut self, model: Option<Model>) {
        self.state.settings.model = model;
    }

    pub fn execution_model(&self) -> Option<Model> {
        self.state.settings.model
    }

    /// See [`crate::Database::set_force_join`].
    pub fn set_force_join(&mut self, method: Option<ForceJoin>) {
        self.state.settings.force_join = method;
    }

    /// See [`crate::Database::set_join_workers`].
    pub fn set_join_workers(&mut self, workers: usize) {
        self.state.settings.join_workers = workers.max(1);
    }

    pub fn join_workers(&self) -> usize {
        self.state.settings.join_workers
    }

    /// See [`crate::Database::set_scan_workers`].
    pub fn set_scan_workers(&mut self, workers: usize) {
        self.state.settings.scan_workers = workers.max(1);
    }

    pub fn scan_workers(&self) -> usize {
        self.state.settings.scan_workers
    }

    /// Tells the gate what a statement ended: the database had to itself
    /// is let go, and an open transaction is closed, once the transaction ends.
    fn after(&mut self) {
        if self.state.in_transaction() {
            return;
        }
        if self.alone {
            self.alone = false;
            self.shared.gate.release();
        }
        if self.open {
            self.open = false;
            self.shared.gate.end();
        }
    }
}

impl Drop for Session {
    /// Rolls back a transaction in progress, then lets the other sessions in.
    fn drop(&mut self) {
        if self.state.in_transaction() {
            match self.alone {
                // Its pages are in the log: they are cut off.
                true => {
                    let rollback = Statement::Rollback { savepoint: None };
                    let _ = self.state.run(&mut self.shared.write_core(), rollback);
                }
                // Its versions are skipped once its ids abort.
                false => self.state.abandon(&self.shared.txns),
            }
        }
        self.after();
    }
}

/// Who may use the core: any number of reading statements, or one writing
/// one; and the sessions with a transaction open, or one alone.
#[derive(Default)]
struct Gate {
    users: Mutex<Users>,
    changed: Condvar,
}

#[derive(Default)]
struct Users {
    readers: usize,
    writer: bool,
    /// Statements waiting to write, which readers let go first.
    waiting: usize,
    /// Sessions with a transaction open.
    open: usize,
    /// Whether a session has the database to itself.
    alone: bool,
    /// Sessions waiting to have it to themselves, which new transactions
    /// and statements outside one let go first.
    claiming: usize,
}

/// A reader's place at the gate, given up when it is dropped.
struct Reading<'a>(&'a Gate);

impl Gate {
    fn users(&self) -> MutexGuard<'_, Users> {
        // Its counts stay right: no code that changes them panics.
        self.users.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits, at most `timeout`, until no statement writes or waits to, and
    /// no session is alone or (for a session with no transaction open)
    /// waiting to be.
    fn read(&self, timeout: Duration, in_transaction: bool) -> Result<Reading<'_>> {
        let mut users = self.wait(timeout, |u| {
            u.writer || u.waiting > 0 || u.alone || (u.claiming > 0 && !in_transaction)
        })?;
        users.readers += 1;
        Ok(Reading(self))
    }

    /// Waits, at most `timeout`, until no statement reads or writes, and then
    /// writes until [`Gate::leave_write`]. A session with no transaction
    /// open first lets those waiting to be alone go.
    fn write(&self, timeout: Duration, in_transaction: bool) -> Result<()> {
        let deadline = Instant::now() + timeout;
        drop(self.wait(timeout, |u| u.alone || (u.claiming > 0 && !in_transaction))?);
        self.users().waiting += 1;
        let left = deadline.saturating_duration_since(Instant::now());
        let waited = self.wait(left, |u| u.writer || u.readers > 0 || u.alone);
        let mut users = match waited {
            Ok(users) => users,
            Err(e) => {
                self.users().waiting -= 1;
                self.changed.notify_all(); // readers may go now
                return Err(e);
            }
        };
        users.waiting -= 1;
        users.writer = true;
        Ok(())
    }

    fn leave_write(&self) {
        self.users().writer = false;
        self.changed.notify_all();
    }

    /// Opens a transaction, once no session is alone or waiting to be.
    fn begin(&self, timeout: Duration) -> Result<()> {
        self.wait(timeout, |u| u.alone || u.claiming > 0)?.open += 1;
        Ok(())
    }

    fn end(&self) {
        self.users().open -= 1;
        self.changed.notify_all();
    }

    /// Waits, at most `timeout`, until no statement runs and no session but
    /// this one has a transaction open, and then has the database to itself
    /// until [`Gate::release`].
    fn claim(&self, timeout: Duration, in_transaction: bool) -> Result<()> {
        self.users().claiming += 1;
        let mine = in_transaction as usize;
        let waited = self.wait(timeout, |u| {
            u.alone || u.writer || u.readers > 0 || u.open > mine
        });
        let mut users = match waited {
            Ok(users) => users,
            Err(e) => {
                self.users().claiming -= 1;
                self.changed.notify_all();
                return Err(e);
            }
        };
        users.claiming -= 1;
        users.alone = true;
        Ok(())
    }

    fn release(&self) {
        self.users().alone = false;
        self.changed.notify_all();
    }

    /// The counts, once `blocked` is false of them, or an error after `timeout`.
    fn wait(
        &self,
        timeout: Duration,
        blocked: impl Fn(&Users) -> bool,
    ) -> Result<MutexGuard<'_, Users>> {
        let deadline = Instant::now() + timeout;
        let mut users = self.users();
        while blocked(&users) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return err(format!(
                    "database is locked: another session is using it (waited {timeout:?})"
                ));
            }
            users = self
                .changed
                .wait_timeout(users, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Ok(users)
    }
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        self.0.users().readers -= 1;
        self.0.changed.notify_all();
    }
}
