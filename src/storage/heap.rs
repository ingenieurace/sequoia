//! Table heap: an unordered collection of tuples stored as a linked list of slotted pages.
//!
//! ```text
//! first_page --next--> page --next--> last_page --next--> (none)
//! ```
//!
//! Inserts go to the last page; a new page is appended when it is full.
//! Space freed by deletes in earlier pages is not reused, and pages are
//! never unlinked, even when empty.
//!
//! The heap stores raw bytes and does not persist its own `first_page` /
//! `last_page`; the catalog is responsible for saving them after changes.
//!
//! All page access goes through the buffer pool. The heap pins at most two
//! pages at a time.

use std::sync::Arc;

use super::buffer::BufferPool;
use super::disk::PageId;
use super::page::{MAX_TUPLE_SIZE, SlottedPage};
use crate::error::{Result, err};

/// Physical location of a tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordId {
    pub page_id: PageId,
    pub slot: u16,
}

pub struct TableHeap {
    pool: Arc<BufferPool>,
    first_page: PageId,
    last_page: PageId,
}

impl TableHeap {
    /// Creates an empty heap with one page.
    pub fn create(pool: Arc<BufferPool>) -> Result<Self> {
        let mut page = pool.new_page()?;
        SlottedPage::init(&mut *page);
        let id = page.id();
        drop(page);
        Ok(Self { pool, first_page: id, last_page: id })
    }

    /// Opens an existing heap. `last_page` is a hint: the chain is followed from
    /// there to the real end, in case pages were appended after it was recorded.
    pub fn open(pool: Arc<BufferPool>, first_page: PageId, last_page: PageId) -> Result<Self> {
        let mut heap = Self { pool, first_page, last_page };
        while let Some(next) = heap.next_page(heap.last_page)? {
            heap.last_page = next;
        }
        Ok(heap)
    }

    pub fn first_page(&self) -> PageId {
        self.first_page
    }

    pub fn last_page(&self) -> PageId {
        self.last_page
    }

    /// Appends a tuple and returns its location. May change `last_page`.
    pub fn insert(&mut self, tuple: &[u8]) -> Result<RecordId> {
        if tuple.len() > MAX_TUPLE_SIZE {
            return err(format!("row is {} bytes, the maximum is {MAX_TUPLE_SIZE}", tuple.len()));
        }

        let mut last = self.pool.fetch_write(self.last_page)?;
        if let Some(slot) = SlottedPage::new(&mut *last).insert(tuple) {
            return Ok(RecordId { page_id: self.last_page, slot });
        }

        // The last page is full: start a new one.
        let mut new = self.pool.new_page()?;
        let slot = SlottedPage::init(&mut *new).insert(tuple).expect("a tuple fits in an empty page");
        let new_id = new.id();
        drop(new);
        // Write the new page before linking it, so a crash leaks a page instead of breaking the chain.
        self.pool.flush_page(new_id)?;
        SlottedPage::new(&mut *last).set_next_page(Some(new_id));
        drop(last);

        self.last_page = new_id;
        Ok(RecordId { page_id: new_id, slot })
    }

    /// Returns the tuple at `rid`, or `None` if it was deleted.
    /// `rid` must belong to this heap; that is not checked.
    pub fn get(&self, rid: RecordId) -> Result<Option<Vec<u8>>> {
        let page = self.pool.fetch_read(rid.page_id)?;
        Ok(SlottedPage::new(&*page).get(rid.slot).map(<[u8]>::to_vec))
    }

    /// Deletes the tuple at `rid`. Returns false if there was none.
    pub fn delete(&self, rid: RecordId) -> Result<bool> {
        let mut page = self.pool.fetch_write(rid.page_id)?;
        Ok(SlottedPage::new(&mut *page).delete(rid.slot))
    }

    /// Replaces a tuple in place with one of the same length. Touches a single page.
    pub fn overwrite(&self, rid: RecordId, tuple: &[u8]) -> Result<()> {
        let mut page = self.pool.fetch_write(rid.page_id)?;
        if !SlottedPage::new(&mut *page).overwrite(rid.slot, tuple) {
            return err("cannot overwrite tuple: missing or different length");
        }
        Ok(())
    }

    /// Iterates over all live tuples, page by page.
    ///
    /// Each page is read when the scan reaches it, so changes made during the
    /// scan may be seen. Callers that modify the heap should collect first.
    pub fn scan(&self) -> Scan<'_> {
        Scan { pool: &self.pool, next_page: Some(self.first_page), pending: Vec::new().into_iter() }
    }

    /// Returns all of the heap's pages to the free list.
    pub fn destroy(self) -> Result<()> {
        let mut next = Some(self.first_page);
        while let Some(id) = next {
            next = self.next_page(id)?;
            self.pool.delete_page(id)?;
        }
        Ok(())
    }

    fn next_page(&self, id: PageId) -> Result<Option<PageId>> {
        Ok(SlottedPage::new(&*self.pool.fetch_read(id)?).next_page())
    }
}

pub struct Scan<'a> {
    pool: &'a BufferPool,
    next_page: Option<PageId>,
    /// Tuples of the current page not yet returned.
    pending: std::vec::IntoIter<(RecordId, Vec<u8>)>,
}

impl Iterator for Scan<'_> {
    type Item = Result<(RecordId, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.pending.next() {
                return Some(Ok(item));
            }
            let page_id = self.next_page.take()?;
            let guard = match self.pool.fetch_read(page_id) {
                Ok(guard) => guard,
                Err(e) => return Some(Err(e)),
            };
            let page = SlottedPage::new(&*guard);
            self.next_page = page.next_page();
            let tuples: Vec<_> =
                page.iter().map(|(slot, tuple)| (RecordId { page_id, slot }, tuple.to_vec())).collect();
            self.pending = tuples.into_iter();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DiskManager;

    fn pool(disk: DiskManager, frames: usize) -> Arc<BufferPool> {
        Arc::new(BufferPool::new(Arc::new(disk), frames))
    }

    fn heap() -> TableHeap {
        TableHeap::create(pool(DiskManager::in_memory(), 16)).unwrap()
    }

    fn row(i: u32) -> Vec<u8> {
        let mut bytes = i.to_le_bytes().to_vec();
        bytes.resize(200, b'.');
        bytes
    }

    fn scan_all(heap: &TableHeap) -> Vec<(RecordId, Vec<u8>)> {
        heap.scan().collect::<Result<_>>().unwrap()
    }

    #[test]
    fn insert_spans_pages_and_scan_keeps_order() {
        let mut heap = heap();
        let rids: Vec<RecordId> = (0..100).map(|i| heap.insert(&row(i)).unwrap()).collect();

        assert_ne!(heap.first_page(), heap.last_page(), "100 x 200 bytes needs several pages");
        assert_eq!(rids[0].page_id, heap.first_page());
        assert_eq!(rids[99].page_id, heap.last_page());

        let scanned = scan_all(&heap);
        assert_eq!(scanned.len(), 100);
        for (i, (rid, tuple)) in scanned.into_iter().enumerate() {
            assert_eq!((rid, tuple), (rids[i], row(i as u32)));
        }
    }

    #[test]
    fn works_with_a_two_frame_pool() {
        let mut heap = TableHeap::create(pool(DiskManager::in_memory(), 2)).unwrap();
        let rids: Vec<RecordId> = (0..100).map(|i| heap.insert(&row(i)).unwrap()).collect();
        for (i, &rid) in rids.iter().enumerate() {
            assert_eq!(heap.get(rid).unwrap(), Some(row(i as u32)));
        }
        assert_eq!(scan_all(&heap).len(), 100);
    }

    #[test]
    fn get_and_delete() {
        let mut heap = heap();
        let a = heap.insert(b"a").unwrap();
        let b = heap.insert(b"b").unwrap();

        assert_eq!(heap.get(a).unwrap(), Some(b"a".to_vec()));
        assert!(heap.delete(a).unwrap());
        assert!(!heap.delete(a).unwrap(), "already deleted");
        assert_eq!(heap.get(a).unwrap(), None);

        let remaining: Vec<RecordId> = scan_all(&heap).into_iter().map(|(rid, _)| rid).collect();
        assert_eq!(remaining, vec![b]);
    }

    #[test]
    fn overwrite_requires_same_length() {
        let mut heap = heap();
        let rid = heap.insert(b"abc").unwrap();
        heap.overwrite(rid, b"xyz").unwrap();
        assert_eq!(heap.get(rid).unwrap(), Some(b"xyz".to_vec()));
        assert!(heap.overwrite(rid, b"too long").is_err());
        heap.delete(rid).unwrap();
        assert!(heap.overwrite(rid, b"abc").is_err(), "deleted");
    }

    #[test]
    fn rejects_rows_larger_than_a_page() {
        let mut heap = heap();
        assert!(heap.insert(&vec![0; MAX_TUPLE_SIZE + 1]).is_err());
        heap.insert(&vec![0; MAX_TUPLE_SIZE]).unwrap();
        heap.insert(&vec![0; MAX_TUPLE_SIZE]).unwrap();
        assert_eq!(scan_all(&heap).len(), 2);
    }

    #[test]
    fn reopens_from_catalog_page_ids() {
        let path = std::env::temp_dir().join(format!("sequoia-heap-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (first, last) = {
            let pool = pool(DiskManager::open(&path).unwrap(), 2);
            let mut heap = TableHeap::create(pool.clone()).unwrap();
            for i in 0..50 {
                heap.insert(&row(i)).unwrap();
            }
            pool.flush_all().unwrap();
            (heap.first_page(), heap.last_page())
        };

        let pool = pool(DiskManager::open(&path).unwrap(), 2);
        // A stale last_page hint still finds the real end of the chain.
        assert_eq!(TableHeap::open(pool.clone(), first, first).unwrap().last_page(), last);

        let heap = TableHeap::open(pool, first, last).unwrap();
        let tuples: Vec<Vec<u8>> = scan_all(&heap).into_iter().map(|(_, t)| t).collect();
        assert_eq!(tuples, (0..50).map(row).collect::<Vec<_>>());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn destroy_frees_all_pages() {
        let pool = pool(DiskManager::in_memory(), 16);
        let mut heap = TableHeap::create(pool.clone()).unwrap();
        for i in 0..100 {
            heap.insert(&row(i)).unwrap();
        }
        let pages = pool.disk().page_count();
        heap.destroy().unwrap();

        // Every freed page is handed out again before the file grows.
        let mut other = TableHeap::create(pool.clone()).unwrap();
        for i in 0..100 {
            other.insert(&row(i)).unwrap();
        }
        assert_eq!(pool.disk().page_count(), pages);
    }
}
