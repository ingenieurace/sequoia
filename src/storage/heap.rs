//! Table heap: an unordered collection of tuples stored as a linked list of slotted pages.
//!
//! ```text
//! first_page --next--> page --next--> last_page --next--> (none)
//! ```
//!
//! A free-space map in memory records how big a tuple each page can still
//! take. Inserts go to the first page with room, so space freed by deletes is
//! reused; a new page is appended only when no page has room. Pages are never
//! unlinked, even when empty: they stay in the map and fill up again.
//!
//! The map is not stored on disk. It is built by reading every page the first
//! time the heap is changed after opening, and kept up to date from then on.
//!
//! The heap stores raw bytes and does not persist its own `first_page` /
//! `last_page`; the catalog is responsible for saving them after changes.
//!
//! All page access goes through the buffer pool. The heap pins at most two
//! pages at a time.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::storage::buffer::pool::BufferPool;
use crate::storage::io::disk::PageId;
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
    /// Each page's [`SlottedPage::room`]. `None` until first needed.
    free_space: Option<BTreeMap<PageId, usize>>,
}

impl TableHeap {
    /// Creates an empty heap with one page.
    pub fn create(pool: Arc<BufferPool>) -> Result<Self> {
        let mut page = pool.new_page()?;
        let room = SlottedPage::init(&mut *page).room();
        let id = page.id();
        drop(page);
        Ok(Self { pool, first_page: id, last_page: id, free_space: Some(BTreeMap::from([(id, room)])) })
    }

    /// Opens an existing heap. `last_page` is a hint: the chain is followed from
    /// there to the real end, in case pages were appended after it was recorded.
    pub fn open(pool: Arc<BufferPool>, first_page: PageId, last_page: PageId) -> Result<Self> {
        let mut heap = Self { pool, first_page, last_page, free_space: None };
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

    /// Stores a tuple in the first page with room and returns its location.
    /// Appends a page, changing `last_page`, if none has room.
    pub fn insert(&mut self, tuple: &[u8]) -> Result<RecordId> {
        if tuple.len() > MAX_TUPLE_SIZE {
            return err(format!("row is {} bytes, the maximum is {MAX_TUPLE_SIZE}", tuple.len()));
        }

        let target = self.free_space()?.iter().find(|&(_, &room)| room >= tuple.len()).map(|(&id, _)| id);
        if let Some(page_id) = target {
            let mut guard = self.pool.fetch_write(page_id)?;
            let mut page = SlottedPage::new(&mut *guard);
            let slot = page.insert(tuple).expect("the free-space map is exact");
            let room = page.room();
            drop(guard);
            self.set_room(page_id, room);
            return Ok(RecordId { page_id, slot });
        }

        // No page has room: start a new one.
        let mut last = self.pool.fetch_write(self.last_page)?;
        let mut new = self.pool.new_page()?;
        let mut page = SlottedPage::init(&mut *new);
        let slot = page.insert(tuple).expect("a tuple fits in an empty page");
        let room = page.room();
        let new_id = new.id();
        drop(new);
        SlottedPage::new(&mut *last).set_next_page(Some(new_id));
        drop(last);

        self.last_page = new_id;
        self.set_room(new_id, room);
        Ok(RecordId { page_id: new_id, slot })
    }

    /// Returns the tuple at `rid`, or `None` if it was deleted.
    /// `rid` must belong to this heap; that is not checked.
    pub fn get(&self, rid: RecordId) -> Result<Option<Vec<u8>>> {
        let page = self.pool.fetch_read(rid.page_id)?;
        Ok(SlottedPage::new(&*page).get(rid.slot).map(<[u8]>::to_vec))
    }

    /// Deletes the tuple at `rid`. Returns false if there was none.
    pub fn delete(&mut self, rid: RecordId) -> Result<bool> {
        let mut guard = self.pool.fetch_write(rid.page_id)?;
        let mut page = SlottedPage::new(&mut *guard);
        let deleted = page.delete(rid.slot);
        let room = page.room();
        drop(guard);
        self.set_room(rid.page_id, room);
        Ok(deleted)
    }

    /// Replaces the tuple at `rid` without moving it to another page, so `rid`
    /// stays valid. Returns false if there is no tuple at `rid` or its page has
    /// no room for the new one; the caller can then delete and re-insert it.
    pub fn update(&mut self, rid: RecordId, tuple: &[u8]) -> Result<bool> {
        let mut guard = self.pool.fetch_write(rid.page_id)?;
        let mut page = SlottedPage::new(&mut *guard);
        let updated = page.update(rid.slot, tuple);
        let room = page.room();
        drop(guard);
        self.set_room(rid.page_id, room);
        Ok(updated)
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

    /// Returns the free-space map, building it from every page if needed.
    fn free_space(&mut self) -> Result<&BTreeMap<PageId, usize>> {
        if self.free_space.is_none() {
            let mut map = BTreeMap::new();
            let mut next = Some(self.first_page);
            while let Some(id) = next {
                let guard = self.pool.fetch_read(id)?;
                let page = SlottedPage::new(&*guard);
                map.insert(id, page.room());
                next = page.next_page();
            }
            self.free_space = Some(map);
        }
        Ok(self.free_space.as_ref().expect("built above"))
    }

    /// Records a page's room, if the map has been built; otherwise building it reads the page.
    fn set_room(&mut self, id: PageId, room: usize) {
        if let Some(map) = &mut self.free_space {
            map.insert(id, room);
        }
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
    fn update_stays_in_the_page_or_reports_no_room() {
        let mut heap = heap();
        let rids: Vec<RecordId> = (0..20).map(|i| heap.insert(&row(i)).unwrap()).collect();
        assert_eq!(rids[0].page_id, rids[1].page_id);

        assert!(heap.update(rids[0], b"short").unwrap());
        assert_eq!(heap.get(rids[0]).unwrap(), Some(b"short".to_vec()));
        // The first page is full of other rows, so a page-sized tuple cannot stay.
        assert!(!heap.update(rids[1], &vec![0; MAX_TUPLE_SIZE]).unwrap());
        assert_eq!(heap.get(rids[1]).unwrap(), Some(row(1)), "unchanged");

        heap.delete(rids[2]).unwrap();
        assert!(!heap.update(rids[2], b"gone").unwrap());
    }

    #[test]
    fn inserts_reuse_space_freed_in_earlier_pages() {
        let mut heap = heap();
        let rids: Vec<RecordId> = (0..100).map(|i| heap.insert(&row(i)).unwrap()).collect();
        let pages = heap.pool.disk().page_count();
        for &rid in &rids[..10] {
            heap.delete(rid).unwrap();
        }
        for i in 0..10 {
            let rid = heap.insert(&row(1000 + i)).unwrap();
            assert_eq!(rid.page_id, heap.first_page(), "first page with room");
        }
        assert_eq!(heap.pool.disk().page_count(), pages, "no page appended");
        assert_eq!(scan_all(&heap).len(), 100);
    }

    #[test]
    fn free_space_is_found_after_reopening() {
        let pool = pool(DiskManager::in_memory(), 16);
        let mut heap = TableHeap::create(pool.clone()).unwrap();
        let rids: Vec<RecordId> = (0..100).map(|i| heap.insert(&row(i)).unwrap()).collect();
        let (first, last) = (heap.first_page(), heap.last_page());
        for &rid in &rids[..5] {
            heap.delete(rid).unwrap();
        }

        let mut heap = TableHeap::open(pool.clone(), first, last).unwrap();
        let pages = pool.disk().page_count();
        for i in 0..5 {
            assert_eq!(heap.insert(&row(i)).unwrap().page_id, first);
        }
        assert_eq!(pool.disk().page_count(), pages);
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
            pool.disk().commit().unwrap();
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
