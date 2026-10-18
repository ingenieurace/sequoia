//! Write-ahead log: makes a group of page writes durable all at once.
//!
//! The database file is never written directly. Each page write is appended
//! to the log as a frame holding the whole new page; a commit appends a
//! commit frame and syncs the log. Until a page is checkpointed, reads find
//! its newest version in the log through an in-memory map of where each
//! page's latest frame is.
//!
//! ```text
//! page frame     0..4   page id
//!                4..12  checksum
//!                12..   the page (PAGE_SIZE bytes)
//! commit frame   0..4   u32::MAX
//!                4..12  checksum
//! ```
//!
//! Each checksum covers the frame's page id and page and the previous frame's
//! checksum, so a frame only checks out after every frame before it. Opening
//! the log reads frames until one is missing, cut short or fails its checksum,
//! keeps those up to the last commit frame and cuts off the rest: a crash
//! loses exactly the writes that were not committed.
//!
//! A checkpoint copies the newest committed version of each page into the
//! database file, syncs it, and only then empties the log, so a crash during a
//! checkpoint leaves the log to be copied again.

use std::collections::HashMap;

use super::disk::{PAGE_SIZE, Page, PageId};
use super::medium::Medium;
use crate::error::Result;

const FRAME_HEADER: u64 = 12;
const PAGE_FRAME: u64 = FRAME_HEADER + PAGE_SIZE as u64;
/// The page id of a commit frame.
const COMMIT: u32 = u32::MAX;
/// The checksum before the first frame: the FNV-1a offset basis.
const SEED: u64 = 0xcbf2_9ce4_8422_2325;

pub struct Wal {
    file: Box<dyn Medium>,
    /// Where the page of each page's newest committed frame starts.
    committed: HashMap<PageId, u64>,
    /// Likewise for frames written since the last commit.
    pending: HashMap<PageId, u64>,
    /// End of the last frame, and its checksum.
    end: u64,
    checksum: u64,
    /// End of the last commit frame, and its checksum: where a rollback cuts the log.
    committed_end: u64,
    committed_checksum: u64,
}

impl Wal {
    /// Opens a log, keeping every committed frame and cutting off the rest.
    pub fn open(file: Box<dyn Medium>) -> Result<Self> {
        let len = file.size()?;
        let mut wal = Wal {
            file,
            committed: HashMap::new(),
            pending: HashMap::new(),
            end: 0,
            checksum: SEED,
            committed_end: 0,
            committed_checksum: SEED,
        };
        let mut header = [0; FRAME_HEADER as usize];
        let mut page = [0; PAGE_SIZE];
        while wal.end + FRAME_HEADER <= len {
            wal.file.read_at(wal.end, &mut header)?;
            let id = u32::from_le_bytes(header[0..4].try_into().unwrap());
            let stored = u64::from_le_bytes(header[4..12].try_into().unwrap());
            let body: &[u8] = if id == COMMIT {
                &[]
            } else if wal.end + PAGE_FRAME <= len {
                wal.file.read_at(wal.end + FRAME_HEADER, &mut page)?;
                &page
            } else {
                break; // cut short
            };
            let sum = checksum(wal.checksum, id, body);
            if sum != stored {
                break;
            }
            if id != COMMIT {
                wal.pending.insert(PageId(id), wal.end + FRAME_HEADER);
            }
            wal.end += FRAME_HEADER + body.len() as u64;
            wal.checksum = sum;
            if id == COMMIT {
                wal.mark_committed();
            }
        }
        if len > wal.committed_end {
            wal.rollback()?;
        }
        Ok(wal)
    }

    /// Bytes in the log.
    pub fn size(&self) -> u64 {
        self.end
    }

    /// Whether any page was written since the last commit.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Reads the newest version of a page in the log. Returns false if the
    /// log has none, leaving `buf` unchanged.
    pub fn read(&self, id: PageId, buf: &mut Page) -> Result<bool> {
        match self.pending.get(&id).or_else(|| self.committed.get(&id)) {
            Some(&offset) => {
                self.file.read_at(offset, buf)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Appends a new version of a page. Readers see it at once; a crash loses
    /// it unless it is committed.
    pub fn write(&mut self, id: PageId, page: &Page) -> Result<()> {
        let sum = checksum(self.checksum, id.0, page);
        let mut frame = Vec::with_capacity(PAGE_FRAME as usize);
        frame.extend_from_slice(&id.0.to_le_bytes());
        frame.extend_from_slice(&sum.to_le_bytes());
        frame.extend_from_slice(page);
        self.file.write_at(self.end, &frame)?;
        self.pending.insert(id, self.end + FRAME_HEADER);
        self.end += PAGE_FRAME;
        self.checksum = sum;
        Ok(())
    }

    /// Makes every page written since the last commit durable. Does nothing
    /// if there are none. If this fails, call [`Wal::rollback`].
    pub fn commit(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let sum = checksum(self.checksum, COMMIT, &[]);
        let mut frame = COMMIT.to_le_bytes().to_vec();
        frame.extend_from_slice(&sum.to_le_bytes());
        self.file.write_at(self.end, &frame)?;
        self.file.sync()?;
        self.end += FRAME_HEADER;
        self.checksum = sum;
        self.mark_committed();
        Ok(())
    }

    /// Forgets every page written since the last commit and cuts them off the log.
    pub fn rollback(&mut self) -> Result<()> {
        self.pending.clear();
        self.end = self.committed_end;
        self.checksum = self.committed_checksum;
        // Synced, so that a commit frame that was written but not known to be
        // durable cannot turn up after a crash.
        self.file.set_len(self.end)?;
        self.file.sync()
    }

    /// Copies the newest committed version of every page into `db` and
    /// empties the log. Must not be called with pages pending.
    pub fn checkpoint(&mut self, db: &dyn Medium) -> Result<()> {
        assert!(self.pending.is_empty(), "checkpoint with uncommitted pages");
        if self.end == 0 {
            return Ok(());
        }
        let mut pages: Vec<(PageId, u64)> = self.committed.iter().map(|(&id, &offset)| (id, offset)).collect();
        pages.sort();
        let mut page = [0; PAGE_SIZE];
        for (id, offset) in pages {
            self.file.read_at(offset, &mut page)?;
            db.write_at(id.0 as u64 * PAGE_SIZE as u64, &page)?;
        }
        db.sync()?;
        self.committed.clear();
        self.committed_end = 0;
        self.committed_checksum = SEED;
        self.rollback()
    }

    fn mark_committed(&mut self) {
        self.committed.extend(self.pending.drain());
        self.committed_end = self.end;
        self.committed_checksum = self.checksum;
    }
}

/// FNV-1a over the previous checksum, the page id and the page.
fn checksum(previous: u64, id: u32, page: &[u8]) -> u64 {
    let mut hash = SEED;
    for &byte in previous.to_le_bytes().iter().chain(&id.to_le_bytes()).chain(page) {
        hash = (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::storage::medium::sim::Sim;

    fn filled(byte: u8) -> Page {
        [byte; PAGE_SIZE]
    }

    fn read(wal: &Wal, id: u32) -> Option<u8> {
        let mut page = [0; PAGE_SIZE];
        wal.read(PageId(id), &mut page).unwrap().then_some(page[0])
    }

    /// Files 0 (the log) and 1 (the database).
    fn sim() -> Arc<Sim> {
        Sim::new(2)
    }

    #[test]
    fn reads_see_the_newest_frame_of_a_page() {
        let sim = sim();
        let mut wal = Wal::open(sim.file(0)).unwrap();
        assert_eq!(read(&wal, 1), None);
        wal.write(PageId(1), &filled(1)).unwrap();
        wal.write(PageId(2), &filled(2)).unwrap();
        wal.commit().unwrap();
        wal.write(PageId(1), &filled(3)).unwrap();
        assert_eq!((read(&wal, 1), read(&wal, 2)), (Some(3), Some(2)));

        wal.rollback().unwrap();
        assert_eq!((read(&wal, 1), read(&wal, 2)), (Some(1), Some(2)));
        assert_eq!(wal.size(), 2 * PAGE_FRAME + FRAME_HEADER);
    }

    #[test]
    fn reopening_keeps_committed_frames_only() {
        let sim = sim();
        let mut wal = Wal::open(sim.file(0)).unwrap();
        wal.write(PageId(1), &filled(1)).unwrap();
        wal.commit().unwrap();
        wal.write(PageId(1), &filled(2)).unwrap();
        wal.write(PageId(2), &filled(2)).unwrap();
        // Everything reached the disk except a commit frame.
        let wal = Wal::open(sim.crashed(usize::MAX).file(0)).unwrap();
        assert_eq!((read(&wal, 1), read(&wal, 2)), (Some(1), None));
        assert_eq!(wal.size(), PAGE_FRAME + FRAME_HEADER, "the uncommitted frames were cut off");
    }

    #[test]
    fn a_torn_or_damaged_frame_ends_the_log() {
        let sim = sim();
        let mut wal = Wal::open(sim.file(0)).unwrap();
        for i in 1..=3 {
            wal.write(PageId(i), &filled(i as u8)).unwrap();
            wal.commit().unwrap();
        }
        let len = wal.size();
        drop(wal);

        let file = sim.file(0);
        file.set_len(len - 1).unwrap();
        let wal = Wal::open(sim.file(0)).unwrap();
        assert_eq!((read(&wal, 2), read(&wal, 3)), (Some(2), None), "the last commit frame is cut short");
        drop(wal);

        // A changed byte in the second page fails its checksum and every later one.
        file.write_at(PAGE_FRAME + FRAME_HEADER + FRAME_HEADER + 7, &[9]).unwrap();
        let wal = Wal::open(sim.file(0)).unwrap();
        assert_eq!((read(&wal, 1), read(&wal, 2)), (Some(1), None));
    }

    #[test]
    fn checkpoints_copy_pages_and_survive_crashing_halfway() {
        let sim = sim();
        let mut wal = Wal::open(sim.file(0)).unwrap();
        for (id, byte) in [(1, 1), (2, 2), (1, 3)] {
            wal.write(PageId(id), &filled(byte)).unwrap();
        }
        wal.commit().unwrap();
        let before = sim.ops();
        let db = sim.file(1);
        wal.checkpoint(&*db).unwrap();
        assert_eq!(wal.size(), 0);
        assert_eq!(read(&wal, 1), None, "now read from the database file");
        let mut page = [0; PAGE_SIZE];
        db.read_at(PAGE_SIZE as u64, &mut page).unwrap();
        assert_eq!(page[0], 3, "the newest version");
        let ops = sim.ops() - before;

        // A crash at any point of a checkpoint leaves the pages in the log or the database file.
        for crash in 0..ops {
            let sim = Sim::new(2);
            let mut wal = Wal::open(sim.file(0)).unwrap();
            for (id, byte) in [(1, 1), (2, 2), (1, 3)] {
                wal.write(PageId(id), &filled(byte)).unwrap();
            }
            wal.commit().unwrap();
            sim.crash_after(crash);
            assert!(wal.checkpoint(&*sim.file(1)).is_err());
            for kept in [0, usize::MAX] {
                let after = sim.crashed(kept);
                let mut wal = Wal::open(after.file(0)).unwrap();
                wal.checkpoint(&*after.file(1)).unwrap();
                let db = after.file(1);
                for (id, byte) in [(1, 3), (2, 2)] {
                    db.read_at(id * PAGE_SIZE as u64, &mut page).unwrap();
                    assert_eq!(page[0], byte, "crash after {crash} ops, {kept} kept");
                }
            }
        }
    }
}
