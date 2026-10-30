//! Page-level disk I/O: reads and writes fixed-size pages and manages allocation.
//!
//! The DiskManager knows nothing about what a page contains. It owns page 0
//! (the file header) and a free list of deallocated pages; every other page
//! belongs to upper layers.
//!
//! File layout (little-endian):
//!
//! ```text
//! page 0 (header)
//!   0..8     magic "SEQUOIA\0"
//!   8..12    format version
//!   12..16   page size
//!   16..20   page count, including page 0
//!   20..24   free list head  (u32::MAX = none)
//!   24..28   root page       (u32::MAX = none)
//!
//! free page
//!   0..4     next free page  (u32::MAX = end)
//!
//! every page: the last 4 bytes are reserved for a future checksum
//! ```
//!
//! Every page write, the header's included, goes to the write-ahead log (see
//! `wal.rs`), a second file next to the database named `<file>-wal`. Nothing
//! is durable until [`DiskManager::commit`], which makes every write since the
//! last commit durable at once; [`DiskManager::rollback`] undoes them instead,
//! and [`DiskManager::rollback_to`] undoes those since a [`Savepoint`].
//! So a crash, or a rollback, never leaves a group of writes half done.
//! Committed pages are copied into the database file by a checkpoint: once
//! the log reaches [`CHECKPOINT_BYTES`], on [`DiskManager::checkpoint`], when
//! the database is closed, and when it is opened after a crash.

use std::fs::{OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::medium::{FileMedium, Medium, MemMedium};
use super::wal::{Wal, WalSavepoint};
use crate::error::{Error, Result, err};

pub const PAGE_SIZE: usize = 4096;
/// Bytes per page available to upper layers; the tail is reserved for a checksum.
pub const PAGE_USABLE: usize = PAGE_SIZE - 4;
/// A commit that leaves the log at least this long checkpoints it (about 1000 pages).
pub const CHECKPOINT_BYTES: u64 = 4 << 20;

pub type Page = [u8; PAGE_SIZE];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageId(pub u32);

const MAGIC: &[u8; 8] = b"SEQUOIA\0";
const VERSION: u32 = 1;
const HEADER_PAGE: u32 = 0;
/// On-disk encoding of "no page".
const NONE: u32 = u32::MAX;
const ZERO_PAGE: Page = [0; PAGE_SIZE];

pub struct DiskManager {
    db: Box<dyn Medium>,
    wal: Mutex<Wal>,
    /// The log file, removed when the database is closed cleanly.
    wal_path: Option<PathBuf>,
    header: Mutex<Headers>,
}

/// A point among the writes since the last commit, to roll back to.
#[derive(Clone)]
pub struct Savepoint {
    wal: WalSavepoint,
    header: Header,
}

struct Headers {
    /// As changed by the writes since the last commit.
    current: Header,
    /// As of the last commit, for a rollback to go back to.
    committed: Header,
}

impl DiskManager {
    /// Opens a database file, creating it if it does not exist, and recovers
    /// the pages committed before a crash from its log.
    /// Fails if another process has the file open.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => Error("database file is in use by another process".into()),
            TryLockError::Error(e) => e.into(),
        })?;
        let wal_path = PathBuf::from(format!("{}-wal", path.display()));
        let wal = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&wal_path)?;
        let mut disk = Self::init(Box::new(FileMedium(file)), Box::new(FileMedium(wal)))?;
        disk.wal_path = Some(wal_path);
        Ok(disk)
    }

    /// Creates a database that lives only in memory.
    pub fn in_memory() -> Self {
        Self::init(
            Box::new(MemMedium::default()),
            Box::new(MemMedium::default()),
        )
        .expect("in-memory storage cannot fail")
    }

    /// Opens a database stored on `db`, with its log on `wal`.
    pub(crate) fn init(db: Box<dyn Medium>, wal: Box<dyn Medium>) -> Result<Self> {
        let mut wal = Wal::open(wal)?;
        wal.checkpoint(&*db)?;

        let len = db.size()?;
        if len > 0 && len < PAGE_SIZE as u64 {
            return err("not a Sequoia database file");
        }
        let header = if len == 0 {
            Header {
                page_count: 1,
                free_head: None,
                root: None,
            }
        } else {
            let mut page = ZERO_PAGE;
            db.read_at(0, &mut page)?;
            let header = Header::decode(&page)?;
            if len < header.page_count as u64 * PAGE_SIZE as u64 {
                return err("database file is truncated");
            }
            header
        };
        let headers = Mutex::new(Headers {
            current: header,
            committed: header,
        });
        let disk = Self {
            db,
            wal: Mutex::new(wal),
            wal_path: None,
            header: headers,
        };
        if len == 0 {
            disk.write_header(&header)?;
            disk.commit()?;
        }
        Ok(disk)
    }

    pub fn read_page(&self, id: PageId, buf: &mut Page) -> Result<()> {
        validate(&self.header.lock().unwrap().current, id)?;
        self.read_block(id.0, buf)
    }

    pub fn write_page(&self, id: PageId, buf: &Page) -> Result<()> {
        validate(&self.header.lock().unwrap().current, id)?;
        self.write_block(id.0, buf)
    }

    /// Returns a zeroed page, reusing a deallocated one if possible.
    pub fn allocate_page(&self) -> Result<PageId> {
        let mut headers = self.header.lock().unwrap();
        let mut next = headers.current;

        let id = match next.free_head {
            Some(id) => {
                let mut page = ZERO_PAGE;
                self.read_block(id.0, &mut page)?;
                next.free_head = decode_id(read_u32(&page, 0));
                id
            }
            None => {
                if next.page_count == NONE {
                    return err("database file is full");
                }
                next.page_count += 1;
                PageId(next.page_count - 1)
            }
        };
        self.write_block(id.0, &ZERO_PAGE)?;
        self.write_header(&next)?;
        headers.current = next;
        Ok(id)
    }

    /// Returns a page to the free list.
    /// Freeing the same page twice is not detected and would corrupt the free list.
    pub fn deallocate_page(&self, id: PageId) -> Result<()> {
        let mut headers = self.header.lock().unwrap();
        validate(&headers.current, id)?;

        let mut page = ZERO_PAGE;
        write_u32(&mut page, 0, encode_id(headers.current.free_head));
        self.write_block(id.0, &page)?;

        let next = Header {
            free_head: Some(id),
            ..headers.current
        };
        self.write_header(&next)?;
        headers.current = next;
        Ok(())
    }

    /// Entry point page for upper layers (e.g. the catalog). Stored but not interpreted.
    pub fn root_page(&self) -> Option<PageId> {
        self.header.lock().unwrap().current.root
    }

    pub fn set_root_page(&self, root: Option<PageId>) -> Result<()> {
        let mut headers = self.header.lock().unwrap();
        if let Some(id) = root {
            validate(&headers.current, id)?;
        }
        let next = Header {
            root,
            ..headers.current
        };
        self.write_header(&next)?;
        headers.current = next;
        Ok(())
    }

    /// Number of pages in the database, including the header page.
    pub fn page_count(&self) -> u32 {
        self.header.lock().unwrap().current.page_count
    }

    /// Makes every page write since the last commit durable, all at once, and
    /// checkpoints the log if it has grown to [`CHECKPOINT_BYTES`]. If this
    /// fails, call [`DiskManager::rollback`].
    pub fn commit(&self) -> Result<()> {
        self.commit_with(true)
    }

    /// Commits every page write since the last commit, as
    /// [`DiskManager::commit`] does, but without waiting for them to be
    /// durable (see [`Wal::commit_unsynced`]): for a statement inside a
    /// transaction, whose writes count only once the transaction commits.
    pub fn commit_unsynced(&self) -> Result<()> {
        self.commit_with(false)
    }

    fn commit_with(&self, sync: bool) -> Result<()> {
        let mut headers = self.header.lock().unwrap();
        let mut wal = self.wal.lock().unwrap();
        match sync {
            true => wal.commit()?,
            false => wal.commit_unsynced()?,
        }
        headers.committed = headers.current;
        if wal.size() >= CHECKPOINT_BYTES {
            wal.checkpoint(&*self.db)?;
        }
        Ok(())
    }

    /// Undoes every page write since the last commit, allocations included.
    /// Pages cached above (in the buffer pool) must be dropped too.
    pub fn rollback(&self) -> Result<()> {
        let mut headers = self.header.lock().unwrap();
        self.wal.lock().unwrap().rollback()?;
        headers.current = headers.committed;
        Ok(())
    }

    /// Marks the writes so far, for [`DiskManager::rollback_to`] to go back to.
    pub fn savepoint(&self) -> Savepoint {
        let headers = self.header.lock().unwrap();
        Savepoint {
            wal: self.wal.lock().unwrap().savepoint(),
            header: headers.current,
        }
    }

    /// Undoes every page write since `savepoint`, allocations included. Fails
    /// if there was a commit since. Pages cached above must be dropped too.
    pub fn rollback_to(&self, savepoint: &Savepoint) -> Result<()> {
        let mut headers = self.header.lock().unwrap();
        self.wal.lock().unwrap().rollback_to(&savepoint.wal)?;
        headers.current = savepoint.header;
        Ok(())
    }

    /// Copies the committed pages from the log into the database file and
    /// empties the log. Fails if there are uncommitted writes.
    pub fn checkpoint(&self) -> Result<()> {
        let mut wal = self.wal.lock().unwrap();
        if wal.has_pending() {
            return err("cannot checkpoint with uncommitted writes");
        }
        wal.checkpoint(&*self.db)
    }

    fn read_block(&self, index: u32, buf: &mut Page) -> Result<()> {
        if !self.wal.lock().unwrap().read(PageId(index), buf)? {
            self.db.read_at(index as u64 * PAGE_SIZE as u64, buf)?;
        }
        Ok(())
    }

    fn write_block(&self, index: u32, buf: &Page) -> Result<()> {
        self.wal.lock().unwrap().write(PageId(index), buf)
    }

    fn write_header(&self, header: &Header) -> Result<()> {
        self.write_block(HEADER_PAGE, &header.encode())
    }
}

impl Drop for DiskManager {
    /// Closes cleanly: drops uncommitted writes, checkpoints the log and
    /// removes its file. Best effort: on failure the log is recovered on open.
    fn drop(&mut self) {
        let wal = self.wal.get_mut().unwrap();
        if wal
            .rollback()
            .and_then(|()| wal.checkpoint(&*self.db))
            .is_ok()
            && let Some(path) = &self.wal_path
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Rejects the header page and pages past the end of the file.
fn validate(header: &Header, id: PageId) -> Result<()> {
    if id.0 == HEADER_PAGE || id.0 >= header.page_count {
        return err(format!("invalid page id: {}", id.0));
    }
    Ok(())
}

// ---- header ----

#[derive(Debug, Clone, Copy)]
struct Header {
    page_count: u32,
    free_head: Option<PageId>,
    root: Option<PageId>,
}

impl Header {
    fn encode(&self) -> Page {
        let mut page = ZERO_PAGE;
        page[0..8].copy_from_slice(MAGIC);
        write_u32(&mut page, 8, VERSION);
        write_u32(&mut page, 12, PAGE_SIZE as u32);
        write_u32(&mut page, 16, self.page_count);
        write_u32(&mut page, 20, encode_id(self.free_head));
        write_u32(&mut page, 24, encode_id(self.root));
        page
    }

    fn decode(page: &Page) -> Result<Header> {
        if &page[0..8] != MAGIC {
            return err("not a Sequoia database file");
        }
        let version = read_u32(page, 8);
        if version != VERSION {
            return err(format!("unsupported format version: {version}"));
        }
        let page_size = read_u32(page, 12);
        if page_size as usize != PAGE_SIZE {
            return err(format!("unsupported page size: {page_size}"));
        }
        Ok(Header {
            page_count: read_u32(page, 16),
            free_head: decode_id(read_u32(page, 20)),
            root: decode_id(read_u32(page, 24)),
        })
    }
}

pub(crate) fn read_u32(page: &Page, at: usize) -> u32 {
    u32::from_le_bytes(page[at..at + 4].try_into().unwrap())
}

pub(crate) fn write_u32(page: &mut Page, at: usize, value: u32) {
    page[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn encode_id(id: Option<PageId>) -> u32 {
    id.map_or(NONE, |id| id.0)
}

pub(crate) fn decode_id(raw: u32) -> Option<PageId> {
    (raw != NONE).then_some(PageId(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A temp file path that is removed when dropped.
    struct TempPath(PathBuf);

    impl TempPath {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("sequoia-{name}-{}.db", std::process::id()));
            let _ = std::fs::remove_file(&path);
            TempPath(path)
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(format!("{}-wal", self.0.display()));
        }
    }

    fn filled(byte: u8) -> Page {
        [byte; PAGE_SIZE]
    }

    fn read(disk: &DiskManager, id: PageId) -> Page {
        let mut page = ZERO_PAGE;
        disk.read_page(id, &mut page).unwrap();
        page
    }

    fn check_round_trip(disk: &DiskManager) {
        let a = disk.allocate_page().unwrap();
        let b = disk.allocate_page().unwrap();
        assert_eq!((a, b), (PageId(1), PageId(2)));
        assert_eq!(read(disk, a), ZERO_PAGE);

        disk.write_page(a, &filled(0xAA)).unwrap();
        disk.write_page(b, &filled(0xBB)).unwrap();
        assert_eq!(read(disk, a), filled(0xAA));
        assert_eq!(read(disk, b), filled(0xBB));
    }

    #[test]
    fn round_trip_in_memory() {
        check_round_trip(&DiskManager::in_memory());
    }

    #[test]
    fn round_trip_file() {
        let path = TempPath::new("round-trip");
        check_round_trip(&DiskManager::open(&path.0).unwrap());
    }

    #[test]
    fn reopen_keeps_pages_and_root() {
        let path = TempPath::new("reopen");
        {
            let disk = DiskManager::open(&path.0).unwrap();
            let id = disk.allocate_page().unwrap();
            disk.write_page(id, &filled(7)).unwrap();
            disk.set_root_page(Some(id)).unwrap();
            disk.commit().unwrap();
        }
        let disk = DiskManager::open(&path.0).unwrap();
        assert_eq!(disk.page_count(), 2);
        assert_eq!(disk.root_page(), Some(PageId(1)));
        assert_eq!(read(&disk, PageId(1)), filled(7));
    }

    #[test]
    fn freed_pages_are_reused_last_in_first_out() {
        let path = TempPath::new("free-list");
        {
            let disk = DiskManager::open(&path.0).unwrap();
            let ids: Vec<PageId> = (0..3).map(|_| disk.allocate_page().unwrap()).collect();
            disk.write_page(ids[0], &filled(1)).unwrap();
            disk.deallocate_page(ids[0]).unwrap();
            disk.deallocate_page(ids[2]).unwrap();
            disk.commit().unwrap();
        }
        // The free list survives a reopen.
        let disk = DiskManager::open(&path.0).unwrap();
        assert_eq!(disk.allocate_page().unwrap(), PageId(3));
        assert_eq!(disk.allocate_page().unwrap(), PageId(1));
        assert_eq!(read(&disk, PageId(1)), ZERO_PAGE, "reused pages are zeroed");
        assert_eq!(
            disk.allocate_page().unwrap(),
            PageId(4),
            "free list exhausted, file grows"
        );
        assert_eq!(disk.page_count(), 5);
    }

    #[test]
    fn rollback_undoes_writes_and_allocations() {
        let disk = DiskManager::in_memory();
        let a = disk.allocate_page().unwrap();
        disk.write_page(a, &filled(1)).unwrap();
        disk.commit().unwrap();

        disk.write_page(a, &filled(2)).unwrap();
        let b = disk.allocate_page().unwrap();
        disk.deallocate_page(a).unwrap();
        disk.set_root_page(Some(b)).unwrap();
        disk.rollback().unwrap();
        assert_eq!((disk.page_count(), disk.root_page()), (2, None));
        assert_eq!(read(&disk, a), filled(1));
        assert_eq!(
            disk.allocate_page().unwrap(),
            b,
            "the free list is as it was"
        );
    }

    #[test]
    fn rollback_to_a_savepoint_keeps_the_writes_before_it() {
        let disk = DiskManager::in_memory();
        let a = disk.allocate_page().unwrap();
        disk.write_page(a, &filled(1)).unwrap();
        disk.commit().unwrap();

        disk.write_page(a, &filled(2)).unwrap(); // kept: before the savepoint
        let savepoint = disk.savepoint();
        disk.write_page(a, &filled(3)).unwrap();
        let b = disk.allocate_page().unwrap();
        disk.set_root_page(Some(b)).unwrap();
        disk.rollback_to(&savepoint).unwrap();
        assert_eq!((disk.page_count(), disk.root_page()), (2, None));
        assert_eq!(read(&disk, a), filled(2));
        // The log goes on from the savepoint, and can roll back to it again.
        disk.write_page(a, &filled(4)).unwrap();
        disk.rollback_to(&savepoint).unwrap();
        assert_eq!(read(&disk, a), filled(2));
        // A rollback still goes back to the last commit.
        disk.rollback().unwrap();
        assert_eq!(read(&disk, a), filled(1));

        let savepoint = disk.savepoint();
        disk.write_page(a, &filled(5)).unwrap();
        disk.commit().unwrap();
        assert!(
            disk.rollback_to(&savepoint).is_err(),
            "from before a commit"
        );
        assert_eq!(read(&disk, a), filled(5));
    }

    #[test]
    fn only_committed_writes_survive_reopening() {
        let path = TempPath::new("commit");
        let wal = format!("{}-wal", path.0.display());
        {
            let disk = DiskManager::open(&path.0).unwrap();
            let id = disk.allocate_page().unwrap();
            disk.write_page(id, &filled(1)).unwrap();
            disk.commit().unwrap();
            disk.write_page(id, &filled(2)).unwrap();
            disk.allocate_page().unwrap();
            assert_eq!(
                std::fs::metadata(&path.0).unwrap().len(),
                0,
                "nothing is checkpointed yet"
            );
        }
        assert!(
            !std::path::Path::new(&wal).exists(),
            "a clean close removes the log"
        );
        let disk = DiskManager::open(&path.0).unwrap();
        assert_eq!(disk.page_count(), 2);
        assert_eq!(read(&disk, PageId(1)), filled(1));

        disk.write_page(PageId(1), &filled(3)).unwrap();
        disk.commit().unwrap();
        disk.checkpoint().unwrap();
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
        let mut page = ZERO_PAGE;
        disk.db.read_at(PAGE_SIZE as u64, &mut page).unwrap();
        assert_eq!(page, filled(3), "checkpointed into the database file");
    }

    #[test]
    fn rejects_invalid_page_ids() {
        let disk = DiskManager::in_memory();
        let mut page = ZERO_PAGE;
        assert!(
            disk.read_page(PageId(0), &mut page).is_err(),
            "header page is private"
        );
        assert!(
            disk.read_page(PageId(1), &mut page).is_err(),
            "not allocated yet"
        );
        assert!(disk.write_page(PageId(0), &page).is_err());
        assert!(disk.deallocate_page(PageId(0)).is_err());
        assert!(disk.set_root_page(Some(PageId(5))).is_err());
    }

    #[test]
    fn rejects_foreign_or_damaged_files() {
        let path = TempPath::new("foreign");

        std::fs::write(&path.0, b"hello").unwrap();
        assert!(DiskManager::open(&path.0).is_err(), "shorter than one page");

        std::fs::write(&path.0, filled(b'x')).unwrap();
        assert!(DiskManager::open(&path.0).is_err(), "bad magic");

        let mut header = Header {
            page_count: 1,
            free_head: None,
            root: None,
        }
        .encode();
        write_u32(&mut header, 12, 8192);
        std::fs::write(&path.0, header).unwrap();
        assert!(DiskManager::open(&path.0).is_err(), "page size mismatch");

        let header = Header {
            page_count: 3,
            free_head: None,
            root: None,
        }
        .encode();
        std::fs::write(&path.0, header).unwrap();
        assert!(DiskManager::open(&path.0).is_err(), "truncated file");
    }

    #[test]
    fn file_is_locked_while_open() {
        let path = TempPath::new("lock");
        let disk = DiskManager::open(&path.0).unwrap();
        assert!(DiskManager::open(&path.0).is_err());
        drop(disk);
        assert!(DiskManager::open(&path.0).is_ok());
    }
}
