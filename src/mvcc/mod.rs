//! Multiversion concurrency control: transactions that read a snapshot of
//! the database and write new versions of rows, so that readers never wait
//! for writers, and writers wait only for each other's rows.
//!
//! ```text
//! row id 7:  [xmin 12 | xmax 15 | name 'ann']   written by 12, deleted by 15
//! row id 9:  [xmin 15 | xmax  0 | name 'amy']   the version 15 wrote instead
//!
//! transaction 14, began before 15 committed:  sees row 7 ('ann')
//! transaction 16, began after:                sees row 9 ('amy')
//! ```
//!
//! **Versions.** Every stored row starts with a header of two transaction
//! ids (see `storage/version.rs`): `xmin`, which wrote it, and `xmax`, which
//! deleted it. An update deletes the old version and inserts a new one, as
//! PostgreSQL does (append-only storage): the old one stays where it was,
//! with its index entries, for the transactions that still read it. Which
//! versions a statement reads is decided by its [`Snapshot`].
//!
//! **Transactions.** A transaction gets an id when it first writes, and
//! one more after each savepoint it writes after (subtransactions), so that
//! `ROLLBACK TO` can undo what followed the savepoint by aborting those ids.
//! [`Transactions`] keeps, in memory, the ids running and the ids that
//! aborted; any other id below the next one committed. The commit log
//! (`clog.rs`) keeps the commits in the database file.
//!
//! **Commit and rollback.** The pages a statement writes go to the
//! write-ahead log when it ends, committed there as a group, so the next
//! statement of any session starts from them. A transaction's commit sets
//! its bits in the commit log and syncs the log: only then do its versions
//! count. A rollback writes nothing: it marks the ids aborted, and readers
//! skip their versions from then on. A crash aborts every transaction that
//! was running, since none of them has its bit. So the log no longer undoes
//! transactions, only failed statements (see `database.rs`).
//!
//! **Vacuum.** Versions no transaction can see any more, deleted before the
//! oldest snapshot or written by an aborted transaction, are removed by
//! `VACUUM`, with their index entries (see [`Transactions::horizon`]).
//!
//! **What still takes turns.** Statements that write run one at a time, and
//! readers wait for the one running (not for its transaction). A statement
//! that changes the schema (CREATE, ALTER, DROP, ANALYZE) needs the database to
//! itself: it waits until no other transaction is open, and keeps the
//! others out until its own transaction ends, as every transaction did
//! before there were versions (see `shared.rs`).

pub mod clog;
pub mod snapshot;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::error::{Result, err};
use crate::storage::version::{FIRST_XID, FROZEN, Fate, Header, Horizon, NO_XID, Visibility, Xid};
use crate::storage::{BufferPool, PageId};
use clog::CommitLog;
pub use snapshot::{Base, Claim, Holding, Snapshot};

/// Where a transaction stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Committed,
    Aborted,
}

/// Every transaction of a database: which ids are running and which
/// aborted, the snapshots that vacuum must keep versions for, and who
/// waits for whom.
pub struct Transactions {
    state: Mutex<State>,
    /// Signalled when a transaction ends.
    ended: Condvar,
}

struct State {
    /// The commit log, once the catalog found or made it.
    log: Option<CommitLog>,
    /// The next id to hand out.
    next: Xid,
    /// The ids running, each with its transaction's first id.
    running: BTreeMap<Xid, Xid>,
    /// The ids that ended without committing. Replaced, not changed, so
    /// that snapshots can hold on to it.
    aborted: Arc<BTreeSet<Xid>>,
    /// The registered snapshots, by the oldest id each does not see, counted.
    snapshots: BTreeMap<Xid, usize>,
    /// What each waiting transaction (by its first id) waits for.
    waits: HashMap<Xid, Xid>,
}

impl Transactions {
    /// The transactions of a database whose commit log is not read yet:
    /// [`Transactions::attach`] reads it.
    pub fn new() -> Arc<Self> {
        Arc::new(Transactions {
            state: Mutex::new(State {
                log: None,
                next: FIRST_XID,
                running: BTreeMap::new(),
                aborted: Arc::default(),
                snapshots: BTreeMap::new(),
                waits: HashMap::new(),
            }),
            ended: Condvar::new(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Nothing that changes the state panics halfway.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads the commit log at `root` the first time, or makes one if there
    /// is none; returns where it is. Opened again, as a rollback opens the
    /// catalog, the transactions in memory stay as they are.
    pub(crate) fn attach(&self, pool: &BufferPool, root: Option<PageId>) -> Result<PageId> {
        let mut state = self.state();
        if let Some(log) = &state.log {
            return Ok(log.root);
        }
        let log = match root {
            Some(root) => CommitLog { root },
            None => CommitLog::create(pool)?,
        };
        let (next, aborted) = log.load(pool)?;
        let root = log.root;
        state.next = next;
        state.aborted = Arc::new(aborted);
        state.log = Some(log);
        Ok(root)
    }

    /// Hands out a new id, running from now on: the first of a transaction,
    /// or one more of the transaction whose first id is `first`.
    pub fn begin(&self, first: Option<Xid>) -> Xid {
        let mut state = self.state();
        let xid = state.next;
        state.next += 1;
        state.running.insert(xid, first.unwrap_or(xid));
        xid
    }

    pub fn status(&self, xid: Xid) -> Status {
        let state = self.state();
        if xid == FROZEN {
            Status::Committed
        } else if state.running.contains_key(&xid) || xid >= state.next {
            Status::Running
        } else if state.aborted.contains(&xid) {
            Status::Aborted
        } else {
            Status::Committed
        }
    }

    /// A snapshot of the transactions now: what committed so far. A
    /// registered one keeps vacuum from removing the versions it sees until
    /// it is dropped; a statement's own needs none, since vacuum cannot run
    /// while it does.
    pub fn snapshot(self: &Arc<Self>, register: bool) -> Arc<Base> {
        let mut state = self.state();
        let running: Vec<Xid> = state.running.keys().copied().collect();
        let horizon = register.then(|| {
            let horizon = Base::oldest_unseen(state.next, &running);
            *state.snapshots.entry(horizon).or_default() += 1;
            horizon
        });
        Arc::new(Base::new(
            self.clone(),
            state.next,
            running,
            state.aborted.clone(),
            horizon,
        ))
    }

    /// A snapshot of what committed so far, for reads outside any statement
    /// (the planner's row counts, ANALYZE).
    pub fn latest(self: &Arc<Self>) -> Snapshot {
        Snapshot::new(self.snapshot(false), Vec::new(), None)
    }

    /// The newest version of every row, whether the transaction that wrote
    /// it committed yet or not: what the planner's row counts follow, as
    /// writes change them when they are made.
    pub fn newest(&self) -> Newest {
        Newest(self.state().aborted.clone())
    }

    pub(super) fn unregister(&self, horizon: Xid) {
        let mut state = self.state();
        if let Some(count) = state.snapshots.get_mut(&horizon) {
            *count -= 1;
            if *count == 0 {
                state.snapshots.remove(&horizon);
            }
        }
    }

    /// Writes the commit bits of a transaction's ids, through `pool`, for
    /// the commit to make durable. [`Transactions::end`] then ends them.
    pub(crate) fn write_commit(&self, pool: &BufferPool, xids: &[Xid]) -> Result<()> {
        let state = self.state();
        let Some(log) = &state.log else {
            return err("the commit log is not open");
        };
        log.commit(pool, xids)
    }

    /// Writes the next id to the commit log, before the rows written with
    /// the ids handed out reach the disk.
    pub(crate) fn save(&self, pool: &BufferPool) -> Result<()> {
        let state = self.state();
        match &state.log {
            Some(log) => log.save_next(pool, state.next),
            None => Ok(()),
        }
    }

    /// Ends ids: committed, or else aborted. Wakes whoever waits for them.
    pub fn end(&self, xids: &[Xid], committed: bool) {
        if xids.is_empty() {
            return;
        }
        let mut state = self.state();
        for xid in xids {
            if let Some(first) = state.running.remove(xid) {
                state.waits.remove(&first);
            }
        }
        if !committed {
            let mut aborted = (*state.aborted).clone();
            aborted.extend(xids);
            state.aborted = Arc::new(aborted);
        }
        drop(state);
        self.ended.notify_all();
    }

    /// Waits until `xid` ends, at most until `deadline`, for the transaction
    /// whose first id is `waiter` (none for a statement outside a
    /// transaction, which holds nothing another could wait for). Fails at
    /// once if the transaction it waits for waits, through others maybe,
    /// for the waiter: a deadlock, which waiting would never end.
    pub fn wait(&self, waiter: Option<Xid>, xid: Xid, deadline: Instant) -> Result<()> {
        let mut state = self.state();
        if let Some(waiter) = waiter {
            let mut holder = state.running.get(&xid).copied();
            while let Some(first) = holder {
                if first == waiter {
                    return err("deadlock detected: two transactions wait for each other's rows");
                }
                holder = state
                    .waits
                    .get(&first)
                    .and_then(|x| state.running.get(x).copied());
            }
            state.waits.insert(waiter, xid);
        }
        let result = loop {
            if !state.running.contains_key(&xid) {
                break Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break err(
                    "could not serialize access: a row is being changed by another transaction, which did not end in time",
                );
            }
            state = self
                .ended
                .wait_timeout(state, left.min(Duration::from_secs(1)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        if let Some(waiter) = waiter {
            state.waits.remove(&waiter);
        }
        result
    }

    /// Whether no transaction but one with these ids is running, and no
    /// snapshot but `own` (if there is one) is registered: then no one else
    /// can see a version as it was.
    pub fn quiet(&self, xids: &[Xid], own: Option<&Base>) -> bool {
        let state = self.state();
        let others = state.running.keys().any(|xid| !xids.contains(xid));
        let snapshots: usize = state.snapshots.values().sum();
        !others && snapshots <= own.is_some_and(|base| base.is_registered()) as usize
    }

    /// What vacuum may do with each version now (see [`Cutoff`]).
    pub fn horizon(&self) -> Cutoff {
        let state = self.state();
        let below = [
            state.snapshots.keys().next().copied(),
            state.running.keys().next().copied(),
            Some(state.next),
        ]
        .into_iter()
        .flatten()
        .min()
        .expect("next is there");
        Cutoff {
            below,
            aborted: state.aborted.clone(),
        }
    }
}

/// Sees the versions not written, nor deleted, by a transaction that aborted
/// (see [`Transactions::newest`]).
#[derive(Clone)]
pub struct Newest(Arc<BTreeSet<Xid>>);

impl Visibility for Newest {
    fn visible(&self, header: Header) -> Result<bool> {
        let alive = |xid: Xid| !self.0.contains(&xid);
        Ok(alive(header.xmin) && (header.xmax == NO_XID || !alive(header.xmax)))
    }
}

/// What vacuum may do with versions: every transaction running or to come
/// sees the ids below `below` that did not abort, and sees the versions
/// they deleted as deleted.
pub struct Cutoff {
    below: Xid,
    aborted: Arc<BTreeSet<Xid>>,
}

impl Cutoff {
    fn settled(&self, xid: Xid) -> bool {
        xid == FROZEN || (xid < self.below && !self.aborted.contains(&xid))
    }
}

impl Horizon for Cutoff {
    fn fate(&self, header: Header) -> Fate {
        if self.aborted.contains(&header.xmin) {
            return Fate::Dead;
        }
        let deleted = header.xmax != NO_XID && !self.aborted.contains(&header.xmax);
        match (deleted, self.settled(header.xmin)) {
            (true, _) if self.settled(header.xmax) => Fate::Dead,
            (true, _) | (false, false) => Fate::Kept,
            (false, true) => Fate::Settled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DiskManager;

    fn transactions() -> (Arc<Transactions>, BufferPool) {
        let pool = BufferPool::new(Arc::new(DiskManager::in_memory()), 16);
        let txns = Transactions::new();
        txns.attach(&pool, None).unwrap();
        (txns, pool)
    }

    fn version(xmin: Xid, xmax: Xid) -> Header {
        Header { xmin, xmax }
    }

    #[test]
    fn a_snapshot_sees_what_committed_before_it_and_its_own_writes() {
        let (txns, _pool) = transactions();
        let (a, b) = (txns.begin(None), txns.begin(None));
        txns.end(&[a], true);
        let base = txns.snapshot(true);
        let c = txns.begin(None);
        let mine = Snapshot::new(base.clone(), vec![c], Some(c));
        txns.end(&[b], true); // after the snapshot: unseen
        assert!(mine.sees_version(version(a, NO_XID)));
        assert!(!mine.sees_version(version(b, NO_XID)));
        assert!(mine.sees_version(version(c, NO_XID)));
        assert!(!mine.sees_version(version(a, c)), "deleted by itself");
        assert!(mine.sees_version(version(a, b)), "deleted after it began");
        assert!(mine.sees_version(version(FROZEN, NO_XID)));

        let d = txns.begin(None);
        txns.end(&[d], false);
        let later = txns.latest();
        assert!(later.visible(version(b, NO_XID)).unwrap());
        assert!(!later.visible(version(d, NO_XID)).unwrap(), "aborted");
        assert!(later.visible(version(a, d)).unwrap(), "its deleter aborted");
        assert!(!later.visible(version(c, NO_XID)).unwrap(), "still running");
    }

    #[test]
    fn the_first_to_change_a_row_wins() {
        let (txns, _pool) = transactions();
        let a = txns.begin(None);
        txns.end(&[a], true);
        let (b, c) = (txns.begin(None), txns.begin(None));
        let snapshot = |xid| Snapshot::new(txns.snapshot(false), vec![xid], Some(xid));
        let (for_b, for_c) = (snapshot(b), snapshot(c));
        assert_eq!(for_b.claim(version(a, NO_XID)).unwrap(), Claim::Mark);
        assert_eq!(for_b.claim(version(b, NO_XID)).unwrap(), Claim::Own);
        // b marked it: c must wait for b, and fails for good once b commits.
        assert!(for_c.claim(version(a, b)).is_err());
        assert_eq!(for_c.take_blocker(), Some(b));
        txns.end(&[b], true);
        assert!(for_c.claim(version(a, b)).is_err());
        assert_eq!(for_c.take_blocker(), None);
        // A deleter that aborted is no obstacle.
        let d = txns.begin(None);
        txns.end(&[d], false);
        assert_eq!(for_c.claim(version(a, d)).unwrap(), Claim::Mark);
    }

    #[test]
    fn keys_are_held_by_versions_not_known_to_be_gone() {
        let (txns, _pool) = transactions();
        let (a, b) = (txns.begin(None), txns.begin(None));
        let mine = Snapshot::new(txns.snapshot(false), vec![b], Some(b));
        assert!(mine.holds(version(a, NO_XID)).is_err(), "a may commit");
        assert_eq!(mine.take_blocker(), Some(a));
        txns.end(&[a], true);
        assert!(
            mine.holds(version(a, NO_XID)).unwrap(),
            "committed after it began"
        );
        assert!(!mine.holds(version(a, b)).unwrap(), "it deleted it itself");
        let c = txns.begin(None);
        txns.end(&[c], false);
        assert!(!mine.holds(version(c, NO_XID)).unwrap());
        assert!(mine.holds(version(a, c)).unwrap());
    }

    #[test]
    fn vacuum_keeps_what_a_registered_snapshot_may_see() {
        let (txns, _pool) = transactions();
        let a = txns.begin(None);
        txns.end(&[a], true);
        let old = txns.snapshot(true);
        let b = txns.begin(None);
        txns.end(&[b], true);
        let horizon = txns.horizon();
        assert_eq!(horizon.fate(version(a, b)), Fate::Kept, "old still sees it");
        assert_eq!(horizon.fate(version(a, NO_XID)), Fate::Settled);
        assert_eq!(horizon.fate(version(b, NO_XID)), Fate::Kept);
        drop(old);
        let horizon = txns.horizon();
        assert_eq!(horizon.fate(version(a, b)), Fate::Dead);
        assert_eq!(horizon.fate(version(b, NO_XID)), Fate::Settled);
        let c = txns.begin(None);
        txns.end(&[c], false);
        assert_eq!(txns.horizon().fate(version(c, NO_XID)), Fate::Dead);
        assert_eq!(txns.horizon().fate(version(a, c)), Fate::Settled);
    }

    #[test]
    fn a_wait_ends_with_the_transaction_or_finds_a_deadlock() {
        let (txns, _pool) = transactions();
        let (a, b) = (txns.begin(None), txns.begin(None));
        let soon = Instant::now() + Duration::from_millis(20);
        assert!(txns.wait(Some(b), a, soon).is_err(), "a never ends");
        let waiter = {
            let txns = txns.clone();
            std::thread::spawn(move || {
                txns.wait(Some(b), a, Instant::now() + Duration::from_secs(30))
            })
        };
        // Wait until b waits for a, then make a wait for b.
        while !txns.state().waits.contains_key(&b) {
            std::thread::yield_now();
        }
        let far = Instant::now() + Duration::from_secs(30);
        let deadlock = txns.wait(Some(a), b, far).unwrap_err();
        assert!(deadlock.0.contains("deadlock"), "{deadlock}");
        txns.end(&[a], true);
        waiter.join().unwrap().unwrap();
    }

    #[test]
    fn the_log_survives_and_ids_running_in_a_crash_aborted() {
        let pool = BufferPool::new(Arc::new(DiskManager::in_memory()), 16);
        let txns = Transactions::new();
        let root = txns.attach(&pool, None).unwrap();
        let (a, b) = (txns.begin(None), txns.begin(None));
        txns.write_commit(&pool, &[a]).unwrap();
        txns.end(&[a], true);
        txns.save(&pool).unwrap();
        // b was running: opened again, it aborted.
        let again = Transactions::new();
        again.attach(&pool, Some(root)).unwrap();
        assert_eq!(again.status(a), Status::Committed);
        assert_eq!(again.status(b), Status::Aborted);
        assert_eq!(again.begin(None), b + 1);
    }
}
