//! A database shared by the threads of a process: the thread model, in
//! which every connection is a thread with a session of its own, and all of
//! them read the same pages through the same buffer pool.
//!
//! ```text
//! SharedDatabase (clone it into each thread)
//!   gate: who may use the core now      core: pages and catalog, in a RwLock
//!     readers  3                          (one Core, as a Database has)
//!     writer   no
//!   Session (one per thread): settings, transaction, busy timeout
//! ```
//!
//! **Locking.** Sessions take turns as SQLite's do: any number of them may
//! read at once, or one may write. A session that reads (SELECT, EXPLAIN)
//! waits until no session writes; one that writes waits until no session
//! reads or writes, and keeps the database to itself until its statement is
//! committed, or with `BEGIN`, until its transaction ends with `COMMIT` or
//! `ROLLBACK`. So no session sees what another has not committed, and every
//! transaction runs as if alone. The log holds one transaction at a time,
//! committed or rolled back whole, which is why writes cannot run side by
//! side. A session waiting to write goes before sessions that come to read
//! after it, so readers cannot hold it off forever.
//!
//! A session waits at most its busy timeout ([`BUSY_TIMEOUT`] by default),
//! then fails with "database is locked", as a session whose transaction is
//! left open holds off all the others. A session dropped in a transaction
//! rolls it back.
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
                core: RwLock::new(core),
            }),
        }
    }

    /// A new session, with the default settings and no transaction.
    pub fn session(&self) -> Session {
        Session {
            shared: self.shared.clone(),
            state: SessionState::default(),
            writing: false,
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
    /// reader in before the writer's session has left, rolling back.
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
    /// Whether this session holds the gate to write, as it does from a
    /// write until it is committed, or the transaction ends.
    writing: bool,
    busy_timeout: Duration,
}

impl Session {
    /// Parses and runs every statement in `sql`, stopping at the first error.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        parse(sql)?.into_iter().map(|stmt| self.run(stmt)).collect()
    }

    /// Runs a single parsed statement, once it is this session's turn.
    pub fn run(&mut self, statement: Statement) -> Result<QueryResult> {
        if !self.writing && executor::reads_only(&statement) {
            let _reading = self.shared.gate.read(self.busy_timeout)?;
            return self.state.read(&self.shared.read_core(), statement);
        }
        if !self.writing {
            self.shared.gate.write(self.busy_timeout)?;
            self.writing = true;
        }
        let result = self.state.run(&mut self.shared.write_core(), statement);
        if !self.state.in_transaction() {
            self.leave();
        }
        result
    }

    /// Whether a transaction is in progress.
    pub fn in_transaction(&self) -> bool {
        self.state.in_transaction()
    }

    /// The names of the tables, once this session may read.
    pub fn table_names(&self) -> Result<Vec<String>> {
        let _reading = match self.writing {
            true => None,
            false => Some(self.shared.gate.read(self.busy_timeout)?),
        };
        let core = self.shared.read_core();
        Ok(core
            .catalog()
            .table_names()
            .into_iter()
            .map(String::from)
            .collect())
    }

    /// Sets how long this session waits for its turn before it fails.
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

    /// Lets the other sessions in again.
    fn leave(&mut self) {
        if self.writing {
            self.writing = false;
            self.shared.gate.leave_write();
        }
    }
}

impl Drop for Session {
    /// Rolls back a transaction in progress, and what a statement that
    /// panicked had not committed, then lets the other sessions in.
    fn drop(&mut self) {
        if self.writing {
            let mut core = self.shared.write_core();
            if self.state.in_transaction() {
                let _ = self
                    .state
                    .run(&mut core, Statement::Rollback { savepoint: None });
            }
        }
        self.leave();
    }
}

/// Who may use the core: any number of readers, or one writer.
#[derive(Default)]
struct Gate {
    users: Mutex<Users>,
    changed: Condvar,
}

#[derive(Default)]
struct Users {
    readers: usize,
    writer: bool,
    /// Sessions waiting to write, which readers let go first.
    waiting: usize,
}

/// A reader's place at the gate, given up when it is dropped.
struct Reading<'a>(&'a Gate);

impl Gate {
    fn users(&self) -> MutexGuard<'_, Users> {
        // Its counts stay right: no code that changes them panics.
        self.users.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits, at most `timeout`, until no session writes or waits to.
    fn read(&self, timeout: Duration) -> Result<Reading<'_>> {
        let mut users = self.wait(timeout, |u| u.writer || u.waiting > 0)?;
        users.readers += 1;
        Ok(Reading(self))
    }

    /// Waits, at most `timeout`, until no session reads or writes, and then
    /// writes until [`Gate::leave_write`].
    fn write(&self, timeout: Duration) -> Result<()> {
        self.users().waiting += 1;
        let waited = self.wait(timeout, |u| u.writer || u.readers > 0);
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
