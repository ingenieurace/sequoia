//! Extendible hashing: a directory of 2^d slots, indexed by the low `d`
//! bits of a hash (the global depth), points to buckets. A bucket with
//! local depth `l <= d` holds the entries whose hashes share their low `l`
//! bits, and 2^(d - l) slots point to it.
//!
//! ```text
//! global depth 2     slot 00 -> bucket A (local depth 1: hashes ending 0)
//!                    slot 01 -> bucket B (local depth 2: hashes ending 01)
//!                    slot 10 -> bucket A
//!                    slot 11 -> bucket C (local depth 2: hashes ending 11)
//! ```
//!
//! An entry that does not fit in its bucket's first page splits the bucket
//! by the next bit, `l`: the entries with it set move to a new bucket, and
//! half of the bucket's slots point there, both with local depth `l + 1`.
//! When `l` is already `d`, the directory first doubles, each new slot a
//! copy of its twin, which copies page ids, not entries. A bucket whose
//! entries all have one hash cannot be split apart, nor can one when the
//! directory is as large as it can be: entries go on in overflow pages.
//!
//! A delete that empties a bucket merges it into its buddy (the bucket its
//! slots split from, which differs in bit `l - 1`) when that has the same
//! local depth, and the directory halves while every bucket's local depth
//! is below the global depth.
//!
//! Root page:
//!
//! ```text
//! 0     'E'
//! 1     global depth
//! 2..   2^d slots of (u32 bucket, u8 local depth), in it or in directory pages (see `directory.rs`)
//! ```
//!
//! Inserts and deletes go down as lookups do, read-latching the root and
//! write-latching the bucket, and change the bucket alone if they can. A
//! split or merge, which changes the directory, starts again with the root
//! write-latched: no one else can be on the way down then, and the bucket's
//! latch waits out whoever is still in it.

use std::sync::Arc;

use super::directory::Slots;
use super::{EXTENDIBLE, bucket, mask};
use crate::error::Result;
use crate::storage::buffer::pool::BufferPool;
use crate::storage::io::disk::{Page, PageId};

const SLOTS: Slots = Slots { header: 2, size: 5 };

#[derive(Clone)]
pub struct Extendible {
    pool: Arc<BufferPool>,
    root: PageId,
}

impl Extendible {
    /// Creates an empty file: one bucket, with every hash (global depth 0).
    pub fn create(pool: Arc<BufferPool>) -> Result<Self> {
        let bucket = {
            let mut page = pool.new_page()?;
            bucket::init(&mut page);
            page.id()
        };
        let root = {
            let mut page = pool.new_page()?;
            page[0] = EXTENDIBLE;
            page[1] = 0;
            SLOTS.write(&pool, &mut page, 1, 0, &slot(bucket, 0))?;
            page.id()
        };
        Ok(Extendible { pool, root })
    }

    pub fn open(pool: Arc<BufferPool>, root: PageId) -> Self {
        Extendible { pool, root }
    }

    pub fn root_page(&self) -> PageId {
        self.root
    }

    pub fn find(&self, hash: u32, key: &[u8]) -> Result<Vec<Vec<u8>>> {
        let root = self.pool.fetch_read(self.root)?;
        let (id, _) = self.bucket_of(&root, hash)?;
        let head = self.pool.fetch_read(id)?;
        drop(root);
        let mut out = Vec::new();
        bucket::find(&self.pool, &head, hash, key, &mut out)?;
        Ok(out)
    }

    pub fn insert(&self, hash: u32, entry: &[u8]) -> Result<()> {
        loop {
            {
                let root = self.pool.fetch_read(self.root)?;
                let (id, depth) = self.bucket_of(&root, hash)?;
                let mut head = self.pool.fetch_write(id)?;
                drop(root);
                if bucket::fits(&head, entry) || !splittable(&head, hash, depth) {
                    return bucket::insert(&self.pool, &mut head, hash, entry);
                }
            }
            // Split the bucket, with the directory to ourselves, and try again.
            let mut root = self.pool.fetch_write(self.root)?;
            let (id, depth) = self.bucket_of(&root, hash)?;
            let mut head = self.pool.fetch_write(id)?;
            if !bucket::fits(&head, entry) && splittable(&head, hash, depth) {
                self.split(&mut root, id, &mut head, hash, depth)?;
            }
        }
    }

    pub fn delete(&self, hash: u32, entry: &[u8]) -> Result<bool> {
        {
            let root = self.pool.fetch_read(self.root)?;
            let (id, depth) = self.bucket_of(&root, hash)?;
            let mut head = self.pool.fetch_write(id)?;
            drop(root);
            if !bucket::delete(&self.pool, &mut head, hash, entry)? {
                return Ok(false);
            }
            if !bucket::is_empty(&head) || depth == 0 {
                return Ok(true);
            }
        }
        self.merge(hash)?;
        Ok(true)
    }

    /// Splits the bucket `id`, whose first page is `head`, with local depth
    /// `depth`, which `hash`'s slot points to, by bit `depth` of its
    /// entries' hashes, doubling the directory first if no slot is left to
    /// tell them apart by.
    fn split(
        &self,
        root: &mut Page,
        id: PageId,
        head: &mut Page,
        hash: u32,
        depth: u32,
    ) -> Result<()> {
        let global = root[1] as u32;
        if depth == global {
            let count = 1 << global;
            let mut slots = SLOTS.load(&self.pool, root, count)?;
            slots.extend_from_within(..);
            SLOTS.store(&self.pool, root, count, &slots)?;
            root[1] += 1;
        }
        let global = root[1] as u32;

        let bit = 1 << depth;
        let (stay, go): (Vec<_>, Vec<_>) = bucket::all(&self.pool, head)?
            .into_iter()
            .partition(|(h, _)| h & bit == 0);
        let mut new = self.pool.new_page()?;
        bucket::init(&mut new);
        bucket::rewrite(&self.pool, &mut new, &go)?;
        let new_id = new.id();
        drop(new);
        bucket::rewrite(&self.pool, head, &stay)?;

        for i in self.slots_of(hash, depth, global) {
            let to = if i & bit == 0 { id } else { new_id };
            SLOTS.write(
                &self.pool,
                root,
                1 << global,
                i as usize,
                &slot(to, depth + 1),
            )?;
        }
        Ok(())
    }

    /// Merges the bucket `hash` belongs in, if it is empty, into its buddy,
    /// again for as long as the merged bucket is empty too, then halves the
    /// directory while it is twice as large as it need be.
    fn merge(&self, hash: u32) -> Result<()> {
        let mut root = self.pool.fetch_write(self.root)?;
        let global = root[1] as u32;
        loop {
            let (id, depth) = self.bucket_of(&root, hash)?;
            if depth == 0 {
                break;
            }
            let head = self.pool.fetch_read(id)?;
            if !bucket::is_empty(&head) {
                break;
            }
            let buddy_slot = hash & mask(depth) ^ (1 << (depth - 1));
            let (buddy, buddy_depth) = self.slot_at(&root, buddy_slot)?;
            if buddy_depth != depth {
                break;
            }
            for i in self.slots_of(hash, depth - 1, global) {
                SLOTS.write(
                    &self.pool,
                    &mut root,
                    1 << global,
                    i as usize,
                    &slot(buddy, depth - 1),
                )?;
            }
            drop(head);
            // Unreachable now, but a reader that found it may still be leaving.
            self.pool.delete_unreachable_page(id)?;
        }

        let mut global = global;
        let mut slots = SLOTS.load(&self.pool, &root, 1 << global)?;
        while global > 0 && slots.chunks(SLOTS.size).all(|s| (s[4] as u32) < global) {
            // Each slot of the upper half is a copy of its twin in the lower one.
            slots.truncate(slots.len() / 2);
            global -= 1;
        }
        let old = 1 << root[1];
        if global < root[1] as u32 {
            SLOTS.store(&self.pool, &mut root, old, &slots)?;
            root[1] = global as u8;
        }
        Ok(())
    }

    /// The slots that point to the bucket with local depth `depth` that `hash` belongs in.
    fn slots_of(&self, hash: u32, depth: u32, global: u32) -> impl Iterator<Item = u32> {
        let low = hash & mask(depth);
        (0..1u32 << (global - depth)).map(move |high| high << depth | low)
    }

    /// The bucket that `hash` belongs in, and its local depth.
    fn bucket_of(&self, root: &Page, hash: u32) -> Result<(PageId, u32)> {
        self.slot_at(root, hash & mask(root[1] as u32))
    }

    fn slot_at(&self, root: &Page, i: u32) -> Result<(PageId, u32)> {
        SLOTS.read(&self.pool, root, 1 << root[1], i as usize, |s| {
            (PageId(read_u32_at(s)), s[4] as u32)
        })
    }

    pub fn destroy(self) -> Result<()> {
        let root = self.pool.fetch_read(self.root)?;
        let count = 1 << root[1];
        let slots = SLOTS.load(&self.pool, &root, count)?;
        let mut buckets: Vec<u32> = slots.chunks(SLOTS.size).map(read_u32_at).collect();
        buckets.sort_unstable();
        buckets.dedup();
        for id in buckets {
            bucket::destroy(&self.pool, PageId(id))?;
        }
        SLOTS.destroy(&self.pool, &root, count)?;
        drop(root);
        self.pool.delete_page(self.root)
    }
}

/// The deepest the directory can be: as many slots as it can hold.
fn max_depth() -> u32 {
    SLOTS.max().ilog2()
}

/// Whether splitting a bucket that cannot take an entry of hash `hash` may
/// make room: the bucket's entries and it do not all have one hash, and
/// there is a bit left to split by.
fn splittable(head: &Page, hash: u32, depth: u32) -> bool {
    depth < max_depth() && bucket::uniform(head) != Some(hash)
}

fn slot(bucket: PageId, depth: u32) -> [u8; 5] {
    let mut s = [0; 5];
    s[..4].copy_from_slice(&bucket.0.to_le_bytes());
    s[4] = depth as u8;
    s
}

fn read_u32_at(slot: &[u8]) -> u32 {
    u32::from_le_bytes(slot[..4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::hash::tests::{entry, pool};

    fn global_depth(file: &Extendible) -> u8 {
        file.pool.fetch_read(file.root).unwrap()[1]
    }

    #[test]
    fn the_directory_doubles_and_halves() {
        let pool = pool(32);
        let file = Extendible::create(pool).unwrap();
        let entries: Vec<_> = (0..5000).map(|i| entry(i, i)).collect();
        for (h, _, e) in &entries {
            file.insert(*h, e).unwrap();
        }
        let grown = global_depth(&file);
        assert!(
            grown >= 6,
            "5000 entries of ~34 bytes take 42 pages or more: depth {grown}"
        );
        for (h, _, e) in &entries {
            file.delete(*h, e).unwrap();
        }
        assert_eq!(
            global_depth(&file),
            0,
            "every bucket emptied and merged back"
        );
    }
}
