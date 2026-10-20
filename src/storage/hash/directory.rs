//! The slots a hash file's root page holds after its header: an array of
//! fixed-size slots (an extendible file's directory, a linear file's
//! bucket list), kept in the root while they fit, so that a small file
//! finds a bucket from its root alone, and otherwise in directory pages,
//! whose ids the root lists in their place.
//!
//! ```text
//! few slots    root: [header][slot 0][slot 1]...
//! many slots   root: [header][page id][page id]...  each page: [slot][slot]...
//! ```
//!
//! Directory pages change only with the root page write-latched, so a
//! reader holding the root's read latch can read them without their own
//! mattering.

use crate::error::Result;
use crate::storage::buffer::pool::BufferPool;
use crate::storage::io::disk::{PAGE_USABLE, Page, PageId, read_u32, write_u32};

pub struct Slots {
    /// Bytes of the root before the slots.
    pub header: usize,
    /// Bytes of a slot.
    pub size: usize,
}

impl Slots {
    /// The most slots the root holds itself.
    fn inline(&self) -> usize {
        (PAGE_USABLE - self.header) / self.size
    }

    fn per_page(&self) -> usize {
        PAGE_USABLE / self.size
    }

    /// The most slots there can be: as many directory pages as the root can list, full.
    pub fn max(&self) -> usize {
        (PAGE_USABLE - self.header) / 4 * self.per_page()
    }

    /// How many directory pages `count` slots take.
    fn pages(&self, count: usize) -> usize {
        if count <= self.inline() { 0 } else { count.div_ceil(self.per_page()) }
    }

    fn page(&self, root: &Page, k: usize) -> PageId {
        PageId(read_u32(root, self.header + 4 * k))
    }

    /// Calls `f` with slot `i` of `count`.
    pub fn read<T>(&self, pool: &BufferPool, root: &Page, count: usize, i: usize, f: impl FnOnce(&[u8]) -> T) -> Result<T> {
        debug_assert!(i < count);
        if self.pages(count) == 0 {
            let at = self.header + i * self.size;
            return Ok(f(&root[at..at + self.size]));
        }
        let at = i % self.per_page() * self.size;
        let page = pool.fetch_read(self.page(root, i / self.per_page()))?;
        Ok(f(&page[at..at + self.size]))
    }

    /// Sets slot `i` of `count` to `bytes`.
    pub fn write(&self, pool: &BufferPool, root: &mut Page, count: usize, i: usize, bytes: &[u8]) -> Result<()> {
        debug_assert!(i < count && bytes.len() == self.size);
        if self.pages(count) == 0 {
            let at = self.header + i * self.size;
            root[at..at + self.size].copy_from_slice(bytes);
            return Ok(());
        }
        let at = i % self.per_page() * self.size;
        let mut page = pool.fetch_write(self.page(root, i / self.per_page()))?;
        page[at..at + self.size].copy_from_slice(bytes);
        Ok(())
    }

    /// The bytes of all `count` slots.
    pub fn load(&self, pool: &BufferPool, root: &Page, count: usize) -> Result<Vec<u8>> {
        let len = count * self.size;
        let pages = self.pages(count);
        if pages == 0 {
            return Ok(root[self.header..self.header + len].to_vec());
        }
        let mut out = Vec::with_capacity(len);
        let chunk = self.per_page() * self.size;
        for k in 0..pages {
            let page = pool.fetch_read(self.page(root, k))?;
            out.extend_from_slice(&page[..chunk.min(len - k * chunk)]);
        }
        Ok(out)
    }

    /// Replaces `old` slots with those in `bytes`, as many as they hold,
    /// moving them between the root and directory pages, and adding or
    /// freeing directory pages, as their number needs.
    pub fn store(&self, pool: &BufferPool, root: &mut Page, old: usize, bytes: &[u8]) -> Result<()> {
        let count = bytes.len() / self.size;
        self.repage(pool, root, old, count)?;
        let pages = self.pages(count);
        if pages == 0 {
            root[self.header..self.header + bytes.len()].copy_from_slice(bytes);
            return Ok(());
        }
        let chunk = self.per_page() * self.size;
        for (k, part) in bytes.chunks(chunk).enumerate() {
            pool.fetch_write(self.page(root, k))?[..part.len()].copy_from_slice(part);
        }
        Ok(())
    }

    /// Makes the array `new` slots long instead of `old`, keeping the first
    /// of them; the slots added hold anything until written.
    pub fn resize(&self, pool: &BufferPool, root: &mut Page, old: usize, new: usize) -> Result<()> {
        let (before, after) = (self.pages(old), self.pages(new));
        if (before == 0) != (after == 0) {
            // Between the root and directory pages: moved whole.
            let mut bytes = self.load(pool, root, old)?;
            bytes.resize(new * self.size, 0);
            return self.store(pool, root, old, &bytes);
        }
        self.repage(pool, root, old, new)
    }

    /// Frees the directory pages `old` slots take beyond those `new` take,
    /// or adds the ones they lack, listing them in the root.
    fn repage(&self, pool: &BufferPool, root: &mut Page, old: usize, new: usize) -> Result<()> {
        let (before, after) = (self.pages(old), self.pages(new));
        for k in after..before {
            pool.delete_page(self.page(root, k))?;
        }
        for k in before..after {
            let mut page = pool.new_page()?;
            page.fill(0);
            write_u32(root, self.header + 4 * k, page.id().0);
        }
        Ok(())
    }

    /// Frees the directory pages of `count` slots.
    pub fn destroy(&self, pool: &BufferPool, root: &Page, count: usize) -> Result<()> {
        (0..self.pages(count)).try_for_each(|k| pool.delete_page(self.page(root, k)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::hash::tests::pool;

    #[test]
    fn slots_move_between_the_root_and_directory_pages() {
        let pool = pool(16);
        let slots = Slots { header: 10, size: 5 };
        let mut root = [0; crate::storage::PAGE_SIZE];
        let slot = |i: usize| [(i % 251) as u8, (i / 251) as u8, 7, 8, 9];
        let mut count = 0;
        for new in [1, 300, 2000, 5000, 3000, 815, 2] {
            slots.resize(&pool, &mut root, count, new).unwrap();
            for i in count.min(new)..new {
                slots.write(&pool, &mut root, new, i, &slot(i)).unwrap();
            }
            count = new;
            for i in 0..count {
                assert_eq!(slots.read(&pool, &root, count, i, |b| b.to_vec()).unwrap(), slot(i), "{i} of {count}");
            }
            let all = slots.load(&pool, &root, count).unwrap();
            assert_eq!(all, (0..count).flat_map(slot).collect::<Vec<_>>());
        }
        let pages = pool.disk().page_count();
        slots.destroy(&pool, &root, count).unwrap();
        assert_eq!(pool.disk().page_count(), pages);
        assert!(slots.max() > 500_000);
    }
}
