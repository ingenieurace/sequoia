//! Snapshots: which versions of each row a statement sees, and whether it
//! may change the ones it sees.
//!
//! A transaction takes its snapshot when its first statement begins, and
//! keeps it to the end; a statement outside a transaction takes one of its
//! own. The snapshot sees what committed before it was taken, and what its
//! own transaction wrote (but not what it rolled back to a savepoint):
//!
//! ```text
//! sees(id) = id is FROZEN
//!         or id is one of its own transaction's ids, not rolled back
//!         or id < xmax, id was not running then, and id did not abort
//! a version is visible if it sees xmin and does not see xmax
//! ```
//!
//! A transaction that commits after the snapshot was taken stays unseen,
//! however long the snapshot lives: that is snapshot isolation.
//!
//! **Writes.** A statement changes or deletes only versions its snapshot
//! sees, and then only if no other transaction has deleted them since, or
//! is deleting them now: the first to change a row wins. A version whose
//! xmax is a transaction still running makes the statement fail and its
//! session wait for that transaction (see `shared.rs`), then try again; if
//! it committed, a transaction that cannot see its write fails for good
//! ("could not serialize access"), as one whose xmax committed already does.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use super::{Status, Transactions};
use crate::error::{Error, Result};
use crate::storage::version::{FROZEN, Header, NO_XID, Visibility, Xid};

/// What a transaction sees of the others, fixed when its first statement began.
pub struct Base {
    txns: Arc<Transactions>,
    /// Ids from this one on were handed out after: not seen.
    xmax: Xid,
    /// Ids running then, sorted: not seen, even once they commit.
    running: Vec<Xid>,
    /// Ids that had ended without committing by then.
    aborted: Arc<BTreeSet<Xid>>,
    /// The oldest id it does not see, under which vacuum counts it (see
    /// [`Transactions::horizon`]), while it is registered.
    horizon: Option<Xid>,
}

impl Base {
    pub(super) fn new(
        txns: Arc<Transactions>,
        xmax: Xid,
        running: Vec<Xid>,
        aborted: Arc<BTreeSet<Xid>>,
        horizon: Option<Xid>,
    ) -> Self {
        Base {
            txns,
            xmax,
            running,
            aborted,
            horizon,
        }
    }

    /// The oldest id it does not see.
    pub(super) fn oldest_unseen(xmax: Xid, running: &[Xid]) -> Xid {
        running.first().copied().unwrap_or(xmax).min(xmax)
    }

    fn sees(&self, xid: Xid) -> bool {
        xid == FROZEN
            || (xid < self.xmax
                && self.running.binary_search(&xid).is_err()
                && !self.aborted.contains(&xid))
    }

    pub(super) fn is_registered(&self) -> bool {
        self.horizon.is_some()
    }
}

impl Drop for Base {
    fn drop(&mut self) {
        if let Some(horizon) = self.horizon {
            self.txns.unregister(horizon);
        }
    }
}

/// A statement's view: its transaction's [`Base`], what the transaction
/// wrote itself, and the id it writes with. Cheap to clone; the threads of
/// a parallel scan share one.
#[derive(Clone)]
pub struct Snapshot {
    base: Arc<Base>,
    /// The ids of its own transaction whose writes it sees.
    own: Arc<[Xid]>,
    /// The id its writes are made with; none for a statement that only reads.
    xid: Option<Xid>,
    /// Whether it may change every version it writes in place (see
    /// [`Snapshot::alone`]).
    alone: bool,
    /// A transaction still running that a write found in its way.
    blocker: Arc<Mutex<Option<Xid>>>,
}

/// How a write may change a version its snapshot sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// The version was written by this very id, and no one else ever saw
    /// it: it may be changed or removed in place.
    Own,
    /// Its xmax is to be set, and a new version written for an update.
    Mark,
}

impl Snapshot {
    pub fn new(base: Arc<Base>, own: Vec<Xid>, xid: Option<Xid>) -> Self {
        Snapshot {
            base,
            own: own.into(),
            xid,
            alone: false,
            blocker: Arc::default(),
        }
    }

    /// Lets its statement change in place every version it changes, as if
    /// it had written them itself ([`Claim::Own`]), and not only its own:
    /// for a statement that no one else can see a version of, being the
    /// only one running (see [`Transactions::quiet`]), and that the log
    /// undoes if it fails or its transaction rolls back, so that it needs
    /// no old version to go back to either (see `database.rs`). Its writes
    /// then leave nothing for vacuum, and keep the rows where they were.
    /// Such a statement writes with the id [`FROZEN`]: by the time anyone
    /// else reads, it has committed, and every transaction is to see it.
    pub fn alone(mut self) -> Self {
        self.alone = true;
        self
    }

    /// A snapshot that writes versions every transaction sees at once, as if
    /// they had always been there: for an upgrade, which copies the rows of
    /// a database from before versions, and for tests.
    pub fn frozen(txns: &Arc<Transactions>) -> Self {
        Snapshot::new(txns.snapshot(false), Vec::new(), Some(FROZEN))
    }

    /// The transactions this snapshot reads under.
    pub fn transactions(&self) -> &Arc<Transactions> {
        &self.base.txns
    }

    /// The id its writes are made with.
    pub fn xid(&self) -> Result<Xid> {
        self.xid
            .ok_or_else(|| Error("a statement that only reads cannot write".into()))
    }

    /// A transaction still running that a write of this statement found
    /// in its way, for the session to wait for; taken once.
    pub fn take_blocker(&self) -> Option<Xid> {
        self.blocker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn is_own(&self, xid: Xid) -> bool {
        self.own.contains(&xid)
    }

    fn sees(&self, xid: Xid) -> bool {
        self.is_own(xid) || self.base.sees(xid)
    }

    /// Whether it sees a version: [`Visibility::visible`] without the `Result`.
    pub fn sees_version(&self, header: Header) -> bool {
        self.sees(header.xmin) && (header.xmax == NO_XID || !self.sees(header.xmax))
    }

    /// How a write may change a version this snapshot sees, or why it may not.
    pub fn claim(&self, header: Header) -> Result<Claim> {
        let xid = self.xid()?;
        if header.xmax != NO_XID {
            if self.is_own(header.xmax) {
                return Err(Error("a row was changed twice by one statement".into()));
            }
            match self.status(header.xmax) {
                Status::Aborted => {}
                Status::Running => return Err(self.blocked(header.xmax)),
                Status::Committed => return Err(changed_since()),
            }
        }
        Ok(match header.xmin == xid || self.alone {
            true => Claim::Own,
            false => Claim::Mark,
        })
    }

    /// Whether a version holds its row's keys, for unique indexes and
    /// primary keys to check: whether it is there now, whether or not this
    /// snapshot sees it. A version written or deleted by a transaction
    /// still running might be or not, so the write must wait for it.
    pub fn holds(&self, header: Header) -> Result<bool> {
        if !self.is_own(header.xmin) {
            match self.status(header.xmin) {
                Status::Aborted => return Ok(false),
                Status::Running => return Err(self.blocked(header.xmin)),
                Status::Committed => {}
            }
        }
        if header.xmax == NO_XID {
            return Ok(true);
        }
        if self.is_own(header.xmax) {
            return Ok(false);
        }
        match self.status(header.xmax) {
            Status::Aborted => Ok(true),
            Status::Running => Err(self.blocked(header.xmax)),
            Status::Committed => Ok(false),
        }
    }

    /// The status of an id now, its own transaction's ids aside.
    fn status(&self, xid: Xid) -> Status {
        if self.is_own(xid) {
            return Status::Running;
        }
        self.base.txns.status(xid)
    }

    fn blocked(&self, xid: Xid) -> Error {
        *self.blocker.lock().unwrap_or_else(PoisonError::into_inner) = Some(xid);
        Error("could not serialize access: a row is being changed by another transaction".into())
    }
}

fn changed_since() -> Error {
    Error(
        "could not serialize access: a row was changed by a transaction that committed after this one began"
            .into(),
    )
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("xmax", &self.base.xmax)
            .field("running", &self.base.running)
            .field("own", &self.own)
            .field("xid", &self.xid)
            .finish()
    }
}

impl Visibility for Snapshot {
    fn visible(&self, header: Header) -> Result<bool> {
        Ok(self.sees_version(header))
    }
}

/// The versions whose keys are taken (see [`Snapshot::holds`]).
#[derive(Clone, Copy)]
pub struct Holding<'a>(pub &'a Snapshot);

impl Visibility for Holding<'_> {
    fn visible(&self, header: Header) -> Result<bool> {
        self.0.holds(header)
    }
}
