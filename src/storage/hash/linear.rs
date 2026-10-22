//! Linear hashing: buckets are split one at a time, in order, whichever
//! bucket overflowed, so the file grows by one bucket at a time and needs
//! no directory of slots that share buckets.
//!
//! With `n = 2^level + next` buckets, a hash's bucket is named by its low
//! `level` bits, or, if that bucket is below `next` (already split this
//! round), by its low `level + 1` bits:
//!
//! ```text
//! level 2, next 1     bucket 0 (hashes ending 000)  split this round
//!                     bucket 1 (ending 01)          next to split
//!                     bucket 2 (ending 10)
//!                     bucket 3 (ending 11)
//!                     bucket 4 (ending 100)         split from bucket 0
//! ```
//!
//! When the entries outgrow [`SPLIT_LOAD`] of the buckets' first pages,
//! bucket `next` splits by bit `level`, its entries with it set moving to a
//! new bucket `next + 2^level`, and `next` moves on; once every bucket of
//! the round is split, the level goes up and `next` starts again from 0.
//! Until its turn, a full bucket goes on in overflow pages. When deletes
//! leave the entries below [`MERGE_LOAD`], the last split is undone: the
//! last bucket goes back into the one it split from.
//!
//! Root page:
//!
//! ```text
//! 0       'L'
//! 1       level
//! 2..6    next
//! 6..14   bytes of all entries, as they take them in pages
//! 14..    n slots of (u32 bucket), in it or in directory pages (see `directory.rs`)
//! ```
//!
//! Every insert and delete changes the bytes in the root, so it write-latches
//! the root for the whole change; lookups read-latch it until they have
//! read-latched their bucket.

use std::sync::Arc;

use super::directory::Slots;
use super::{LINEAR, bucket, mask};
use crate::error::Result;
use crate::storage::buffer::pool::BufferPool;
use crate::storage::io::disk::{Page, PageId, read_u32, write_u32};

const SLOTS: Slots = Slots {
    header: 14,
    size: 4,
};

/// The part of the buckets' first pages that entries may take before a bucket splits.
pub const SPLIT_LOAD: f64 = 0.75;
/// The part below which the last split is undone.
pub const MERGE_LOAD: f64 = 0.35;

pub struct Linear {
    pool: Arc<BufferPool>,
    root: PageId,
}

/// What the root page says.
struct State {
    level: u32,
    next: u32,
    bytes: u64,
}

impl State {
    fn read(root: &Page) -> State {
        let bytes = u64::from_le_bytes(root[6..14].try_into().unwrap());
        State {
            level: root[1] as u32,
            next: read_u32(root, 2),
            bytes,
        }
    }

    fn write(&self, root: &mut Page) {
        root[1] = self.level as u8;
        write_u32(root, 2, self.next);
        root[6..14].copy_from_slice(&self.bytes.to_le_bytes());
    }

    fn buckets(&self) -> usize {
        (1 << self.level) + self.next as usize
    }

    fn bucket_of(&self, hash: u32) -> u32 {
        let i = hash & mask(self.level);
        if i < self.next {
            hash & mask(self.level + 1)
        } else {
            i
        }
    }

    /// The load the entries put on the buckets' first pages.
    fn load(&self, buckets: usize) -> f64 {
        self.bytes as f64 / (buckets * bucket::ROOM) as f64
    }
}

impl Linear {
    /// Creates an empty file of one bucket.
    pub fn create(pool: Arc<BufferPool>) -> Result<Self> {
        let bucket = {
            let mut page = pool.new_page()?;
            bucket::init(&mut page);
            page.id()
        };
        let root = {
            let mut page = pool.new_page()?;
            page[0] = LINEAR;
            State {
                level: 0,
                next: 0,
                bytes: 0,
            }
            .write(&mut page);
            SLOTS.write(&pool, &mut page, 1, 0, &bucket.0.to_le_bytes())?;
            page.id()
        };
        Ok(Linear { pool, root })
    }

    pub fn open(pool: Arc<BufferPool>, root: PageId) -> Self {
        Linear { pool, root }
    }

    pub fn root_page(&self) -> PageId {
        self.root
    }

    pub fn find(&self, hash: u32, key: &[u8]) -> Result<Vec<Vec<u8>>> {
        let root = self.pool.fetch_read(self.root)?;
        let state = State::read(&root);
        let head = self.pool.fetch_read(self.bucket(
            &root,
            state.buckets(),
            state.bucket_of(hash) as usize,
        )?)?;
        drop(root);
        let mut out = Vec::new();
        bucket::find(&self.pool, &head, hash, key, &mut out)?;
        Ok(out)
    }

    pub fn insert(&self, hash: u32, entry: &[u8]) -> Result<()> {
        let mut root = self.pool.fetch_write(self.root)?;
        let mut state = State::read(&root);
        let id = self.bucket(&root, state.buckets(), state.bucket_of(hash) as usize)?;
        bucket::insert(&self.pool, &mut *self.pool.fetch_write(id)?, hash, entry)?;
        state.bytes += bucket::size(entry) as u64;
        if state.load(state.buckets()) > SPLIT_LOAD && state.buckets() < SLOTS.max() {
            self.split(&mut root, &mut state)?;
        }
        state.write(&mut root);
        Ok(())
    }

    pub fn delete(&self, hash: u32, entry: &[u8]) -> Result<bool> {
        let mut root = self.pool.fetch_write(self.root)?;
        let mut state = State::read(&root);
        let id = self.bucket(&root, state.buckets(), state.bucket_of(hash) as usize)?;
        if !bucket::delete(&self.pool, &mut *self.pool.fetch_write(id)?, hash, entry)? {
            return Ok(false);
        }
        state.bytes -= bucket::size(entry) as u64;
        let n = state.buckets();
        if n > 1 && state.load(n - 1) < MERGE_LOAD {
            self.merge(&mut root, &mut state)?;
        }
        state.write(&mut root);
        Ok(true)
    }

    /// Splits bucket `next` by bit `level`, into a new last bucket.
    fn split(&self, root: &mut Page, state: &mut State) -> Result<()> {
        let n = state.buckets();
        let bit = 1 << state.level;
        let mut head = self
            .pool
            .fetch_write(self.bucket(root, n, state.next as usize)?)?;
        let (stay, go): (Vec<_>, Vec<_>) = bucket::all(&self.pool, &head)?
            .into_iter()
            .partition(|(h, _)| h & bit == 0);
        let mut new = self.pool.new_page()?;
        bucket::init(&mut new);
        bucket::rewrite(&self.pool, &mut new, &go)?;
        bucket::rewrite(&self.pool, &mut head, &stay)?;
        let new = new.id();
        drop(head);

        SLOTS.resize(&self.pool, root, n, n + 1)?;
        SLOTS.write(&self.pool, root, n + 1, n, &new.0.to_le_bytes())?;
        state.next += 1;
        if state.next == bit {
            state.level += 1;
            state.next = 0;
        }
        Ok(())
    }

    /// Undoes the last split: moves the last bucket's entries back into
    /// the bucket it split from.
    fn merge(&self, root: &mut Page, state: &mut State) -> Result<()> {
        let n = state.buckets();
        let (level, next) = match state.next {
            0 => (state.level - 1, (1 << (state.level - 1)) - 1),
            next => (state.level, next - 1),
        };
        let (last, into) = (
            self.bucket(root, n, n - 1)?,
            self.bucket(root, n, next as usize)?,
        );
        let mut head = self.pool.fetch_write(into)?;
        let mut entries = bucket::all(&self.pool, &head)?;
        entries.extend(bucket::all(&self.pool, &*self.pool.fetch_read(last)?)?);
        bucket::rewrite(&self.pool, &mut head, &entries)?;
        drop(head);
        bucket::destroy(&self.pool, last)?;
        SLOTS.resize(&self.pool, root, n, n - 1)?;
        (state.level, state.next) = (level, next);
        Ok(())
    }

    /// The first page of bucket `i` of `n`.
    fn bucket(&self, root: &Page, n: usize, i: usize) -> Result<PageId> {
        SLOTS.read(&self.pool, root, n, i, |s| {
            PageId(u32::from_le_bytes(s.try_into().unwrap()))
        })
    }

    pub fn destroy(self) -> Result<()> {
        let root = self.pool.fetch_read(self.root)?;
        let n = State::read(&root).buckets();
        let slots = SLOTS.load(&self.pool, &root, n)?;
        for slot in slots.chunks(4) {
            bucket::destroy(
                &self.pool,
                PageId(u32::from_le_bytes(slot.try_into().unwrap())),
            )?;
        }
        SLOTS.destroy(&self.pool, &root, n)?;
        drop(root);
        self.pool.delete_page(self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::hash::tests::{entry, pool};

    #[test]
    fn splits_in_order_and_merges_back() {
        let pool = pool(32);
        let file = Linear::create(pool).unwrap();
        let state = || State::read(&file.pool.fetch_read(file.root).unwrap());
        let entries: Vec<_> = (0..5000).map(|i| entry(i, i)).collect();
        for (h, _, e) in &entries {
            file.insert(*h, e).unwrap();
            let state = state();
            assert!(state.load(state.buckets()) <= SPLIT_LOAD);
        }
        let grown = state();
        let bytes: u64 = entries.iter().map(|(_, _, e)| bucket::size(e) as u64).sum();
        assert_eq!(grown.bytes, bytes);
        assert!(grown.buckets() > 40, "{} buckets", grown.buckets());
        for (h, _, e) in &entries {
            assert!(file.delete(*h, e).unwrap());
        }
        let shrunk = state();
        assert_eq!((shrunk.level, shrunk.next, shrunk.bytes), (0, 0, 0));
    }
}
