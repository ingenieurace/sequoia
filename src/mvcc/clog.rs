//! The commit log: which transactions committed, one bit each, in pages of
//! the database file, so that it survives a crash as the rows do.
//!
//! ```text
//! header page   0..8    the next transaction id to hand out
//!               8..12   bit page count n
//!               12..    n bit page ids
//! bit page      bit i of page p (byte i / 8, bit i % 8): whether transaction
//!               FIRST_XID + p * BITS + i committed
//! ```
//!
//! Only commits are written. A transaction that rolled back, or was running
//! when the database crashed, has no bit: an id below the next one without
//! its bit ended without committing. The next id is written before any row
//! a transaction writes is (see [`CommitLog::save_next`]), so no row can
//! carry an id the log has not handed out.
//!
//! The pages are written through the buffer pool and the write-ahead log,
//! like every other page: a commit's bit is durable once the commit is.

use std::collections::BTreeSet;

use crate::error::{Result, err};
use crate::storage::io::disk::{PAGE_USABLE, read_u32, write_u32};
use crate::storage::version::{FIRST_XID, Xid};
use crate::storage::{BufferPool, PageId};

/// Transactions a bit page holds.
const BITS: u64 = PAGE_USABLE as u64 * 8;
/// Where the header page's list of bit pages starts.
const PAGES: usize = 12;
/// Bit pages a header page lists at most: about 33 million transactions.
const MAX_PAGES: usize = (PAGE_USABLE - PAGES) / 4;

pub(crate) struct CommitLog {
    pub(crate) root: PageId,
}

impl CommitLog {
    /// Writes an empty log, whose first id is [`FIRST_XID`].
    pub(crate) fn create(pool: &BufferPool) -> Result<Self> {
        let mut page = pool.new_page()?;
        page[0..8].copy_from_slice(&FIRST_XID.to_le_bytes());
        write_u32(&mut page, 8, 0);
        let root = page.id();
        drop(page);
        pool.flush_page(root)?;
        Ok(CommitLog { root })
    }

    /// The next id to hand out, and every id before it that did not commit.
    pub(crate) fn load(&self, pool: &BufferPool) -> Result<(Xid, BTreeSet<Xid>)> {
        let (next, pages) = self.header(pool)?;
        let mut aborted = BTreeSet::new();
        let mut xid = FIRST_XID;
        for id in pages {
            let page = pool.fetch_read(id)?;
            for i in 0..BITS {
                if xid >= next {
                    break;
                }
                if page[(i / 8) as usize] & (1 << (i % 8)) == 0 {
                    aborted.insert(xid);
                }
                xid += 1;
            }
        }
        aborted.extend(xid..next);
        Ok((next, aborted))
    }

    /// Sets the bits of transactions that commit.
    pub(crate) fn commit(&self, pool: &BufferPool, xids: &[Xid]) -> Result<()> {
        let (_, mut pages) = self.header(pool)?;
        for &xid in xids {
            let index = xid - FIRST_XID;
            let number = (index / BITS) as usize;
            while pages.len() <= number {
                if pages.len() == MAX_PAGES {
                    return err("the commit log is full: too many transactions");
                }
                let id = pool.new_page()?.id();
                let mut header = pool.fetch_write(self.root)?;
                write_u32(&mut header, PAGES + 4 * pages.len(), id.0);
                write_u32(&mut header, 8, pages.len() as u32 + 1);
                pages.push(id);
            }
            let bit = index % BITS;
            pool.fetch_write(pages[number])?[(bit / 8) as usize] |= 1 << (bit % 8);
        }
        Ok(())
    }

    /// Records that ids up to `next` may have been handed out, if the log
    /// says fewer.
    pub(crate) fn save_next(&self, pool: &BufferPool, next: Xid) -> Result<()> {
        if self.header(pool)?.0 < next {
            pool.fetch_write(self.root)?[0..8].copy_from_slice(&next.to_le_bytes());
        }
        Ok(())
    }

    fn header(&self, pool: &BufferPool) -> Result<(Xid, Vec<PageId>)> {
        let page = pool.fetch_read(self.root)?;
        let next = u64::from_le_bytes(page[0..8].try_into().expect("8 bytes"));
        let count = read_u32(&page, 8) as usize;
        if count > MAX_PAGES || next < FIRST_XID {
            return err("corrupted commit log");
        }
        let pages = (0..count)
            .map(|i| PageId(read_u32(&page, PAGES + 4 * i)))
            .collect();
        Ok((next, pages))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::storage::DiskManager;

    #[test]
    fn ids_without_a_commit_bit_did_not_commit() {
        let pool = BufferPool::new(Arc::new(DiskManager::in_memory()), 16);
        let log = CommitLog::create(&pool).unwrap();
        assert_eq!(log.load(&pool).unwrap(), (FIRST_XID, BTreeSet::new()));
        // Ids on the first two bit pages and the start of the third.
        let far = FIRST_XID + 2 * BITS + 5;
        log.save_next(&pool, far + 3).unwrap();
        log.commit(&pool, &[FIRST_XID, FIRST_XID + 2, far, far + 1])
            .unwrap();
        log.save_next(&pool, FIRST_XID).unwrap(); // never goes back
        let (next, aborted) = log.load(&pool).unwrap();
        assert_eq!(next, far + 3);
        assert!(!aborted.contains(&FIRST_XID) && aborted.contains(&(FIRST_XID + 1)));
        assert!(!aborted.contains(&far) && !aborted.contains(&(far + 1)));
        assert!(aborted.contains(&(far + 2)) && aborted.contains(&(far - 1)));
        assert_eq!(aborted.len() as u64, far + 3 - FIRST_XID - 4);
    }
}
