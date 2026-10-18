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
//! Not crash-safe without a WAL. Writes are ordered so that a crash leaks
//! at most one page rather than corrupting the free list.

use std::fs::{OpenOptions, TryLockError};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Mutex, RwLock};

use crate::error::{Error, Result, err};

pub const PAGE_SIZE: usize = 4096;
/// Bytes per page available to upper layers; the tail is reserved for a checksum.
pub const PAGE_USABLE: usize = PAGE_SIZE - 4;

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
    storage: Box<dyn Storage>,
    header: Mutex<Header>,
}

impl DiskManager {
    /// Opens a database file, creating it if it does not exist.
    /// Fails if another process has the file open.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        file.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => Error("database file is in use by another process".into()),
            TryLockError::Error(e) => e.into(),
        })?;

        let len = file.metadata()?.len();
        if len > 0 && len < PAGE_SIZE as u64 {
            return err("not a Sequoia database file");
        }
        let is_new = len == 0;
        let disk = Self::init(Box::new(FileStorage(file)), is_new)?;
        if !is_new && len < disk.page_count() as u64 * PAGE_SIZE as u64 {
            return err("database file is truncated");
        }
        Ok(disk)
    }

    /// Creates a database that lives only in memory.
    pub fn in_memory() -> Self {
        Self::init(Box::new(MemStorage::default()), true).expect("in-memory storage cannot fail")
    }

    fn init(storage: Box<dyn Storage>, is_new: bool) -> Result<Self> {
        let header = if is_new {
            let header = Header { page_count: 1, free_head: None, root: None };
            storage.write_block(HEADER_PAGE, &header.encode())?;
            header
        } else {
            let mut page = ZERO_PAGE;
            storage.read_block(HEADER_PAGE, &mut page)?;
            Header::decode(&page)?
        };
        Ok(Self { storage, header: Mutex::new(header) })
    }

    pub fn read_page(&self, id: PageId, buf: &mut Page) -> Result<()> {
        validate(&self.header.lock().unwrap(), id)?;
        self.storage.read_block(id.0, buf)
    }

    pub fn write_page(&self, id: PageId, buf: &Page) -> Result<()> {
        validate(&self.header.lock().unwrap(), id)?;
        self.storage.write_block(id.0, buf)
    }

    /// Returns a zeroed page, reusing a deallocated one if possible.
    pub fn allocate_page(&self) -> Result<PageId> {
        let mut header = self.header.lock().unwrap();
        let mut next = *header;

        let id = match header.free_head {
            Some(id) => {
                let mut page = ZERO_PAGE;
                self.storage.read_block(id.0, &mut page)?;
                next.free_head = decode_id(read_u32(&page, 0));
                // Unlink before zeroing: a crash in between leaks the page
                // instead of leaving the free list pointing at garbage.
                self.write_header(&next)?;
                self.storage.write_block(id.0, &ZERO_PAGE)?;
                id
            }
            None => {
                if next.page_count == NONE {
                    return err("database file is full");
                }
                let id = PageId(next.page_count);
                next.page_count += 1;
                // Extend the file before publishing the new page count.
                self.storage.write_block(id.0, &ZERO_PAGE)?;
                self.write_header(&next)?;
                id
            }
        };

        *header = next;
        Ok(id)
    }

    /// Returns a page to the free list.
    /// Freeing the same page twice is not detected and would corrupt the free list.
    pub fn deallocate_page(&self, id: PageId) -> Result<()> {
        let mut header = self.header.lock().unwrap();
        validate(&header, id)?;

        // Write the link before publishing the page as the new head.
        let mut page = ZERO_PAGE;
        write_u32(&mut page, 0, encode_id(header.free_head));
        self.storage.write_block(id.0, &page)?;

        let next = Header { free_head: Some(id), ..*header };
        self.write_header(&next)?;
        *header = next;
        Ok(())
    }

    /// Entry point page for upper layers (e.g. the catalog). Stored but not interpreted.
    pub fn root_page(&self) -> Option<PageId> {
        self.header.lock().unwrap().root
    }

    pub fn set_root_page(&self, root: Option<PageId>) -> Result<()> {
        let mut header = self.header.lock().unwrap();
        if let Some(id) = root {
            validate(&header, id)?;
        }
        let next = Header { root, ..*header };
        self.write_header(&next)?;
        *header = next;
        Ok(())
    }

    /// Number of pages in the file, including the header page.
    pub fn page_count(&self) -> u32 {
        self.header.lock().unwrap().page_count
    }

    /// Flushes written pages to durable storage.
    pub fn sync(&self) -> Result<()> {
        self.storage.sync()
    }

    fn write_header(&self, header: &Header) -> Result<()> {
        self.storage.write_block(HEADER_PAGE, &header.encode())
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

pub(super) fn read_u32(page: &Page, at: usize) -> u32 {
    u32::from_le_bytes(page[at..at + 4].try_into().unwrap())
}

pub(super) fn write_u32(page: &mut Page, at: usize, value: u32) {
    page[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub(super) fn encode_id(id: Option<PageId>) -> u32 {
    id.map_or(NONE, |id| id.0)
}

pub(super) fn decode_id(raw: u32) -> Option<PageId> {
    (raw != NONE).then_some(PageId(raw))
}

// ---- storage backends ----

/// A raw array of page-sized blocks. Knows nothing about headers or free lists.
trait Storage: Send + Sync {
    fn read_block(&self, index: u32, buf: &mut Page) -> Result<()>;
    fn write_block(&self, index: u32, buf: &Page) -> Result<()>;
    fn sync(&self) -> Result<()>;
}

/// File-backed storage using positional I/O, so no shared cursor is needed.
struct FileStorage(std::fs::File);

impl Storage for FileStorage {
    fn read_block(&self, index: u32, buf: &mut Page) -> Result<()> {
        Ok(self.0.read_exact_at(buf, index as u64 * PAGE_SIZE as u64)?)
    }

    fn write_block(&self, index: u32, buf: &Page) -> Result<()> {
        Ok(self.0.write_all_at(buf, index as u64 * PAGE_SIZE as u64)?)
    }

    fn sync(&self) -> Result<()> {
        Ok(self.0.sync_data()?)
    }
}

#[derive(Default)]
struct MemStorage(RwLock<Vec<Box<Page>>>);

impl Storage for MemStorage {
    fn read_block(&self, index: u32, buf: &mut Page) -> Result<()> {
        match self.0.read().unwrap().get(index as usize) {
            Some(block) => buf.copy_from_slice(&block[..]),
            None => return err(format!("block {index} does not exist")),
        }
        Ok(())
    }

    fn write_block(&self, index: u32, buf: &Page) -> Result<()> {
        let mut blocks = self.0.write().unwrap();
        let index = index as usize;
        match index.cmp(&blocks.len()) {
            std::cmp::Ordering::Less => blocks[index].copy_from_slice(buf),
            std::cmp::Ordering::Equal => blocks.push(Box::new(*buf)),
            std::cmp::Ordering::Greater => return err(format!("block {index} is past the end")),
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A temp file path that is removed when dropped.
    struct TempPath(PathBuf);

    impl TempPath {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("sequoia-{name}-{}.db", std::process::id()));
            let _ = std::fs::remove_file(&path);
            TempPath(path)
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
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
            disk.sync().unwrap();
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
        }
        // The free list survives a reopen.
        let disk = DiskManager::open(&path.0).unwrap();
        assert_eq!(disk.allocate_page().unwrap(), PageId(3));
        assert_eq!(disk.allocate_page().unwrap(), PageId(1));
        assert_eq!(read(&disk, PageId(1)), ZERO_PAGE, "reused pages are zeroed");
        assert_eq!(disk.allocate_page().unwrap(), PageId(4), "free list exhausted, file grows");
        assert_eq!(disk.page_count(), 5);
    }

    #[test]
    fn rejects_invalid_page_ids() {
        let disk = DiskManager::in_memory();
        let mut page = ZERO_PAGE;
        assert!(disk.read_page(PageId(0), &mut page).is_err(), "header page is private");
        assert!(disk.read_page(PageId(1), &mut page).is_err(), "not allocated yet");
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

        let mut header = Header { page_count: 1, free_head: None, root: None }.encode();
        write_u32(&mut header, 12, 8192);
        std::fs::write(&path.0, header).unwrap();
        assert!(DiskManager::open(&path.0).is_err(), "page size mismatch");

        let header = Header { page_count: 3, free_head: None, root: None }.encode();
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
