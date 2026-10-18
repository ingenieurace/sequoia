//! Buffer pool: caches disk pages in a fixed number of in-memory frames.
//!
//! Upper layers never read or write pages through the DiskManager directly.
//! They ask the pool for a page and get a guard that derefs to its bytes.
//!
//! ```text
//! page table (PageId -> frame)    frames
//!   7 -> 0                        [0] page 7  pins 1  dirty
//!   3 -> 2                        [1] (free)
//!                                 [2] page 3  pins 0  clean   <- clock hand
//! ```
//!
//! - A guard pins its frame, and pinned frames are never evicted. Dropping
//!   the guard unpins it. A write guard that was written through marks the
//!   frame dirty.
//! - When no frame is free, a victim is chosen with CLOCK, an approximation
//!   of LRU: every access sets a frame's reference bit, and the hand clears
//!   set bits as it sweeps, evicting the first unpinned frame whose bit is
//!   already clear.
//! - Write-back: a dirty page reaches disk only when it is evicted or flushed.
//!
//! Locking: one mutex (`state`) over all metadata, plus one RwLock per frame
//! over its bytes. While holding `state`, the pool only locks frames that are
//! unpinned, and no guard holds an unpinned frame (guards release the lock
//! before unpinning), so that never waits. A thread must not fetch a page it
//! already holds a write guard on, or flush a page it holds any guard on.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::disk::{DiskManager, PAGE_SIZE, Page, PageId};
use crate::error::{Error, Result, err};

pub struct BufferPool {
    disk: Arc<DiskManager>,
    frames: Box<[RwLock<Page>]>,
    state: Mutex<State>,
}

struct State {
    page_table: HashMap<PageId, usize>,
    /// Metadata of each frame, indexed like `BufferPool::frames`.
    meta: Vec<FrameMeta>,
    /// Frames that hold no page.
    free: Vec<usize>,
    hand: usize,
}

#[derive(Default)]
struct FrameMeta {
    page: Option<PageId>,
    pins: u32,
    dirty: bool,
    referenced: bool,
}

impl BufferPool {
    /// Creates a pool of `capacity` frames. Callers that pin several pages at
    /// once (e.g. the table heap pins two) need at least that many.
    pub fn new(disk: Arc<DiskManager>, capacity: usize) -> Self {
        assert!(capacity > 0, "a buffer pool needs at least one frame");
        let state = State {
            page_table: HashMap::new(),
            meta: (0..capacity).map(|_| FrameMeta::default()).collect(),
            free: (0..capacity).rev().collect(),
            hand: 0,
        };
        let frames = (0..capacity).map(|_| RwLock::new([0; PAGE_SIZE])).collect();
        Self { disk, frames, state: Mutex::new(state) }
    }

    /// The underlying disk, for file-level operations (root page, sync).
    pub fn disk(&self) -> &DiskManager {
        &self.disk
    }

    pub fn capacity(&self) -> usize {
        self.frames.len()
    }

    /// Returns a page for reading, loading it from disk if it is not cached.
    pub fn fetch_read(&self, id: PageId) -> Result<PageReadGuard<'_>> {
        let frame = self.pin(id)?;
        Ok(PageReadGuard { pool: self, id, frame, data: Some(self.frames[frame].read().unwrap()) })
    }

    /// Returns a page for writing, loading it from disk if it is not cached.
    pub fn fetch_write(&self, id: PageId) -> Result<PageWriteGuard<'_>> {
        let frame = self.pin(id)?;
        Ok(self.write_guard(id, frame))
    }

    /// Allocates a zeroed page on disk and returns it for writing.
    pub fn new_page(&self) -> Result<PageWriteGuard<'_>> {
        let mut state = self.state.lock().unwrap();
        let frame = self.take_frame(&mut state)?;
        let id = match self.disk.allocate_page() {
            Ok(id) => id,
            Err(e) => {
                state.free.push(frame);
                return Err(e);
            }
        };
        *self.frames[frame].write().unwrap() = [0; PAGE_SIZE];
        state.install(frame, id);
        drop(state);
        Ok(self.write_guard(id, frame))
    }

    /// Removes a page from the pool, discarding unwritten changes, and frees
    /// it on disk. Fails if the page is pinned.
    pub fn delete_page(&self, id: PageId) -> Result<()> {
        // Held across the deallocation so no one can load the page meanwhile.
        let mut state = self.state.lock().unwrap();
        if let Some(&frame) = state.page_table.get(&id) {
            if state.meta[frame].pins > 0 {
                return err(format!("cannot delete pinned page {}", id.0));
            }
            state.evict(frame);
            state.free.push(frame);
        }
        self.disk.deallocate_page(id)
    }

    /// Writes a page back to disk if it is cached and dirty. Does not sync.
    pub fn flush_page(&self, id: PageId) -> Result<()> {
        let frame = {
            let mut state = self.state.lock().unwrap();
            let Some(&frame) = state.page_table.get(&id) else { return Ok(()) };
            if !state.meta[frame].dirty {
                return Ok(());
            }
            // Keeps the frame from being evicted while the state lock is released.
            state.meta[frame].pins += 1;
            frame
        };

        // Waits out any writer; no one can change the page while this is held.
        let data = self.frames[frame].read().unwrap();
        let result = self.disk.write_page(id, &data);
        let mut state = self.state.lock().unwrap();
        if result.is_ok() {
            state.meta[frame].dirty = false;
        }
        drop(data);
        state.meta[frame].pins -= 1;
        result
    }

    /// Writes every dirty page back to disk. Does not sync.
    pub fn flush_all(&self) -> Result<()> {
        let mut pages: Vec<PageId> = self.state.lock().unwrap().page_table.keys().copied().collect();
        pages.sort();
        pages.into_iter().try_for_each(|id| self.flush_page(id))
    }

    /// Pins the frame holding `id`, loading the page into one if needed.
    fn pin(&self, id: PageId) -> Result<usize> {
        let mut state = self.state.lock().unwrap();
        if let Some(&frame) = state.page_table.get(&id) {
            let meta = &mut state.meta[frame];
            meta.pins += 1;
            meta.referenced = true;
            return Ok(frame);
        }

        let frame = self.take_frame(&mut state)?;
        if let Err(e) = self.disk.read_page(id, &mut self.frames[frame].write().unwrap()) {
            state.free.push(frame);
            return Err(e);
        }
        state.install(frame, id);
        Ok(frame)
    }

    /// Returns a frame that holds no page: a free one, or an evicted victim
    /// whose page is written back first if dirty.
    fn take_frame(&self, state: &mut State) -> Result<usize> {
        if let Some(frame) = state.free.pop() {
            return Ok(frame);
        }
        let frame = state.find_victim().ok_or_else(|| {
            Error(format!("buffer pool is full: all {} frames are pinned", self.capacity()))
        })?;
        let meta = &state.meta[frame];
        if meta.dirty {
            let page = meta.page.expect("a dirty frame holds a page");
            // On failure the victim stays cached and dirty.
            self.disk.write_page(page, &self.frames[frame].read().unwrap())?;
        }
        state.evict(frame);
        Ok(frame)
    }

    fn write_guard(&self, id: PageId, frame: usize) -> PageWriteGuard<'_> {
        PageWriteGuard { pool: self, id, frame, modified: false, data: Some(self.frames[frame].write().unwrap()) }
    }

    fn unpin(&self, frame: usize, dirty: bool) {
        let mut state = self.state.lock().unwrap();
        let meta = &mut state.meta[frame];
        meta.pins -= 1;
        meta.dirty |= dirty;
    }

    #[cfg(test)]
    fn is_cached(&self, id: PageId) -> bool {
        self.state.lock().unwrap().page_table.contains_key(&id)
    }
}

impl Drop for BufferPool {
    /// Best effort: errors are lost. Call `flush_all` to see them.
    fn drop(&mut self) {
        let _ = self.flush_all();
    }
}

impl State {
    /// Puts `id` in a frame taken from `take_frame`, pinned once.
    fn install(&mut self, frame: usize, id: PageId) {
        self.page_table.insert(id, frame);
        self.meta[frame] = FrameMeta { page: Some(id), pins: 1, dirty: false, referenced: true };
    }

    /// Forgets the frame's page without writing it.
    fn evict(&mut self, frame: usize) {
        if let Some(page) = self.meta[frame].page.take() {
            self.page_table.remove(&page);
        }
        self.meta[frame] = FrameMeta::default();
    }

    /// CLOCK: sweeps from the hand, giving referenced frames a second chance.
    /// Two full turns suffice: the first may only clear reference bits.
    fn find_victim(&mut self) -> Option<usize> {
        let n = self.meta.len();
        for _ in 0..2 * n {
            let frame = self.hand;
            self.hand = (self.hand + 1) % n;
            let meta = &mut self.meta[frame];
            if meta.pins > 0 || meta.page.is_none() {
                continue;
            }
            if !meta.referenced {
                return Some(frame);
            }
            meta.referenced = false;
        }
        None
    }
}

// ---- guards ----

/// Shared access to a cached page. Unpins it when dropped.
pub struct PageReadGuard<'a> {
    pool: &'a BufferPool,
    id: PageId,
    frame: usize,
    /// `Some` until drop, which releases the lock before unpinning.
    data: Option<RwLockReadGuard<'a, Page>>,
}

impl PageReadGuard<'_> {
    pub fn id(&self) -> PageId {
        self.id
    }
}

impl Deref for PageReadGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Page {
        self.data.as_ref().unwrap()
    }
}

impl Drop for PageReadGuard<'_> {
    fn drop(&mut self) {
        self.data = None;
        self.pool.unpin(self.frame, false);
    }
}

/// Exclusive access to a cached page. Unpins it when dropped, marking it
/// dirty if it was borrowed mutably.
pub struct PageWriteGuard<'a> {
    pool: &'a BufferPool,
    id: PageId,
    frame: usize,
    modified: bool,
    /// `Some` until drop, which releases the lock before unpinning.
    data: Option<RwLockWriteGuard<'a, Page>>,
}

impl PageWriteGuard<'_> {
    pub fn id(&self) -> PageId {
        self.id
    }
}

impl Deref for PageWriteGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Page {
        self.data.as_ref().unwrap()
    }
}

impl DerefMut for PageWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Page {
        self.modified = true;
        self.data.as_mut().unwrap()
    }
}

impl Drop for PageWriteGuard<'_> {
    fn drop(&mut self) {
        self.data = None;
        self.pool.unpin(self.frame, self.modified);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(capacity: usize) -> BufferPool {
        BufferPool::new(Arc::new(DiskManager::in_memory()), capacity)
    }

    fn filled(byte: u8) -> Page {
        [byte; PAGE_SIZE]
    }

    /// Allocates a page filled with `byte` and returns its id, unpinned.
    fn new_filled(pool: &BufferPool, byte: u8) -> PageId {
        let mut page = pool.new_page().unwrap();
        *page = filled(byte);
        page.id()
    }

    fn on_disk(pool: &BufferPool, id: PageId) -> Page {
        let mut page = [0; PAGE_SIZE];
        pool.disk().read_page(id, &mut page).unwrap();
        page
    }

    #[test]
    fn writes_stay_in_memory_until_flushed() {
        let pool = pool(4);
        let id = new_filled(&pool, 7);

        assert_eq!(*pool.fetch_read(id).unwrap(), filled(7));
        assert_eq!(on_disk(&pool, id), filled(0), "write-back, not write-through");

        pool.flush_page(id).unwrap();
        assert_eq!(on_disk(&pool, id), filled(7));
    }

    #[test]
    fn read_guards_do_not_dirty_pages() {
        let pool = pool(1);
        let a = new_filled(&pool, 1);
        pool.flush_page(a).unwrap();
        drop(pool.fetch_read(a).unwrap());
        drop(pool.fetch_write(a).unwrap());

        // Change the disk behind the pool's back; a clean page is dropped on
        // eviction, so the change survives.
        pool.disk().write_page(a, &filled(9)).unwrap();
        new_filled(&pool, 2);
        assert_eq!(*pool.fetch_read(a).unwrap(), filled(9));
    }

    #[test]
    fn evicted_dirty_pages_are_written_back() {
        let pool = pool(2);
        let ids: Vec<PageId> = (1..=5).map(|byte| new_filled(&pool, byte)).collect();
        for (byte, &id) in (1..=5).zip(&ids) {
            assert_eq!(*pool.fetch_read(id).unwrap(), filled(byte));
        }
    }

    #[test]
    fn clock_gives_recently_used_pages_a_second_chance() {
        let pool = pool(3);
        let [a, b, c] = [1, 2, 3].map(|byte| new_filled(&pool, byte));

        // Every frame is referenced: one full turn clears all bits, then the
        // hand evicts the first frame (a) and stops after it.
        let d = new_filled(&pool, 4);
        assert!(!pool.is_cached(a));

        // c is used again; b is not, so b is next.
        drop(pool.fetch_read(c).unwrap());
        new_filled(&pool, 5);
        assert!(!pool.is_cached(b));
        assert!(pool.is_cached(c) && pool.is_cached(d));
    }

    #[test]
    fn pinned_pages_are_not_evicted() {
        let pool = pool(2);
        let [a, b, c] = [1, 2, 3].map(|byte| new_filled(&pool, byte));

        let guard_a = pool.fetch_read(a).unwrap();
        let guard_b = pool.fetch_write(b).unwrap();
        assert!(pool.fetch_read(c).is_err(), "every frame is pinned");
        assert!(pool.new_page().is_err());

        drop(guard_b);
        assert_eq!(*pool.fetch_read(c).unwrap(), filled(3));
        assert!(pool.is_cached(a), "still pinned");
        drop(guard_a);
    }

    #[test]
    fn pages_can_be_pinned_by_many_readers() {
        let pool = pool(1);
        let [a, b] = [1, 2].map(|byte| new_filled(&pool, byte));
        let (r1, r2) = (pool.fetch_read(a).unwrap(), pool.fetch_read(a).unwrap());
        assert_eq!(*r1, *r2);
        drop(r1);
        assert!(pool.fetch_read(b).is_err(), "r2 still pins the only frame");
        drop(r2);
        assert_eq!(*pool.fetch_read(b).unwrap(), filled(2));
        assert!(!pool.is_cached(a));
    }

    #[test]
    fn delete_page_discards_and_frees() {
        let pool = pool(2);
        let a = new_filled(&pool, 1);

        let guard = pool.fetch_read(a).unwrap();
        assert!(pool.delete_page(a).is_err(), "pinned");
        drop(guard);

        pool.delete_page(a).unwrap();
        assert!(!pool.is_cached(a));
        // The disk reuses the page; its old contents were never written.
        let page = pool.new_page().unwrap();
        assert_eq!((page.id(), *page), (a, filled(0)));
    }

    #[test]
    fn concurrent_writers_with_a_small_pool() {
        const THREADS: usize = 4;
        const ROUNDS: u8 = 200;
        let pool = pool(THREADS);
        let ids: Vec<PageId> = (0..2 * THREADS).map(|_| new_filled(&pool, 0)).collect();

        // Each thread owns two pages and holds at most one pin at a time.
        std::thread::scope(|s| {
            for pages in ids.chunks(2) {
                let pool = &pool;
                s.spawn(move || {
                    for round in 0..ROUNDS {
                        let mut page = pool.fetch_write(pages[round as usize % 2]).unwrap();
                        page[0] += 1;
                    }
                });
            }
        });

        for &id in &ids {
            assert_eq!(pool.fetch_read(id).unwrap()[0], ROUNDS / 2);
        }
        pool.flush_all().unwrap();
        for &id in &ids {
            assert_eq!(on_disk(&pool, id)[0], ROUNDS / 2);
        }
    }
}
