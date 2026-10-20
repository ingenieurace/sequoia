//! SSTable (sorted string table): an immutable run of entries sorted by key,
//! written once by the LSM tree and only read afterwards.
//!
//! An SSTable is a linked list of slotted pages filled in key order, so slot
//! `i` of a page holds its `i`-th smallest entry. Each entry is:
//!
//! ```text
//! 0..2       key length (u16)
//! key
//! 1 byte     0 = deletion (tombstone), 1 = put
//! value      the rest, for a put
//! ```
//!
//! The summary (first key of each page, the largest key, and a Bloom filter)
//! is kept in memory. It is not stored: opening an SSTable reads it once to
//! rebuild the summary. A lookup checks the key range and the filter, picks
//! the one page that could hold the key, and binary searches its slots.

use std::ops::Range;
use std::sync::Arc;

use super::bloom::{self, Bloom};
use crate::storage::buffer::pool::{BufferPool, PageWriteGuard};
use crate::storage::io::disk::{PAGE_SIZE, Page, PageId};
use crate::storage::page::SlottedPage;
use crate::error::{Result, err};

/// A key and its value, or `None` if the key was deleted.
pub type Entry = (Vec<u8>, Option<Vec<u8>>);

pub struct SsTable {
    pool: Arc<BufferPool>,
    /// Every page with the first key stored in it, in key order.
    pages: Vec<(PageId, Vec<u8>)>,
    max_key: Vec<u8>,
    bloom: Bloom,
}

impl SsTable {
    /// Writes sorted entries to new pages and flushes them. Returns `None`,
    /// writing nothing, if there are no entries. Keys must be strictly increasing.
    pub fn write(pool: Arc<BufferPool>, entries: impl IntoIterator<Item = Result<Entry>>) -> Result<Option<Self>> {
        let mut pages: Vec<(PageId, Vec<u8>)> = Vec::new();
        let mut hashes = Vec::new();
        let mut max_key: Option<Vec<u8>> = None;
        let mut current: Option<PageWriteGuard> = None;

        for entry in entries {
            let (key, value) = entry?;
            debug_assert!(max_key.as_ref().is_none_or(|max| *max < key), "keys must be sorted");
            let bytes = encode_entry(&key, value.as_deref());
            let fits = match &mut current {
                Some(guard) => SlottedPage::new(&mut **guard).insert(&bytes).is_some(),
                None => false,
            };
            if !fits {
                let mut new = pool.new_page()?;
                SlottedPage::init(&mut *new).insert(&bytes).expect("an entry fits in an empty page");
                let id = new.id();
                if let Some(mut previous) = current.replace(new) {
                    SlottedPage::new(&mut *previous).set_next_page(Some(id));
                }
                pages.push((id, key.clone()));
            }
            hashes.push(bloom::hash(&key));
            max_key = Some(key);
        }
        drop(current);

        let Some(max_key) = max_key else { return Ok(None) };
        for &(id, _) in &pages {
            pool.flush_page(id)?;
        }
        Ok(Some(SsTable { pool, pages, max_key, bloom: Bloom::new(&hashes) }))
    }

    /// Opens an SSTable written earlier, reading all of it to rebuild the summary.
    pub fn open(pool: Arc<BufferPool>, first_page: PageId) -> Result<Self> {
        let mut pages = Vec::new();
        let mut hashes = Vec::new();
        let mut max_key = None;
        let mut next = Some(first_page);
        while let Some(id) = next {
            let guard = pool.fetch_read(id)?;
            let page = SlottedPage::new(&*guard);
            next = page.next_page();
            for (slot, bytes) in page.iter() {
                let (key, _) = decode_entry(bytes)?;
                if slot == 0 {
                    pages.push((id, key.to_vec()));
                }
                hashes.push(bloom::hash(key));
                max_key = Some(key.to_vec());
            }
        }
        let Some(max_key) = max_key else {
            return err(format!("corrupted SSTable at page {}: no entries", first_page.0));
        };
        Ok(SsTable { pool, pages, max_key, bloom: Bloom::new(&hashes) })
    }

    pub fn first_page(&self) -> PageId {
        self.pages[0].0
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn max_key(&self) -> &[u8] {
        &self.max_key
    }

    /// Looks up a key. `Some(None)` means this SSTable records its deletion;
    /// `None` means it says nothing about the key.
    pub fn get(&self, key: &[u8]) -> Result<Option<Option<Vec<u8>>>> {
        let min_key = self.pages[0].1.as_slice();
        if key < min_key || key > self.max_key.as_slice() || !self.bloom.may_contain(key) {
            return Ok(None);
        }
        // The last page whose first key is <= key.
        let index = self.pages.partition_point(|(_, first)| first.as_slice() <= key) - 1;
        let guard = self.pool.fetch_read(self.pages[index].0)?;
        let page = SlottedPage::new(&*guard);

        let (mut low, mut high) = (0, page.slot_count());
        while low < high {
            let mid = low + (high - low) / 2;
            let Some(bytes) = page.get(mid) else { return err("corrupted SSTable: empty slot") };
            let (found, value) = decode_entry(bytes)?;
            match found.cmp(key) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(Some(value.map(<[u8]>::to_vec))),
            }
        }
        Ok(None)
    }

    /// Iterates over all entries in key order, tombstones included.
    pub fn iter(&self) -> Iter<'_> {
        Iter { pool: &self.pool, next_page: Some(self.first_page()), pending: Vec::new().into_iter() }
    }

    /// A cursor at the first entry with a key `>= start`, tombstones
    /// included, which reads from the one page that could hold `start`, or
    /// reads nothing if every key is below it.
    pub fn cursor(&self, start: &[u8]) -> Result<Cursor<'_>> {
        let first = (start <= self.max_key.as_slice()).then(|| {
            // The last page whose first key is <= start, or the first page.
            self.pages[self.pages.partition_point(|(_, first)| first.as_slice() <= start).saturating_sub(1)].0
        });
        let mut cursor =
            Cursor { pool: &self.pool, page: Box::new([0; PAGE_SIZE]), next_page: first, slot: 0, slots: 0, current: None };
        cursor.load()?;
        while cursor.entry().is_some_and(|(key, _)| key < start) {
            cursor.advance()?;
        }
        Ok(cursor)
    }

    /// Returns all of the SSTable's pages to the free list.
    pub fn destroy(self) -> Result<()> {
        self.pages.iter().try_for_each(|&(id, _)| self.pool.delete_page(id))
    }
}

pub struct Iter<'a> {
    pool: &'a BufferPool,
    next_page: Option<PageId>,
    /// Entries of the current page not yet returned.
    pending: std::vec::IntoIter<Entry>,
}

impl Iterator for Iter<'_> {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(entry) = self.pending.next() {
                return Some(Ok(entry));
            }
            let page_id = self.next_page.take()?;
            let entries = self.pool.fetch_read(page_id).and_then(|guard| {
                let page = SlottedPage::new(&*guard);
                self.next_page = page.next_page();
                page.iter()
                    .map(|(_, bytes)| {
                        let (key, value) = decode_entry(bytes)?;
                        Ok((key.to_vec(), value.map(<[u8]>::to_vec)))
                    })
                    .collect::<Result<Vec<_>>>()
            });
            match entries {
                Ok(entries) => self.pending = entries.into_iter(),
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

/// Reads an SSTable's entries in key order from a copy of one page at a
/// time, handing out the current entry where it lies in the copy, without
/// copying it out.
pub struct Cursor<'a> {
    pool: &'a BufferPool,
    page: Box<Page>,
    next_page: Option<PageId>,
    /// The current entry's slot in `page`; past the last once exhausted.
    slot: u16,
    slots: u16,
    /// Where the current entry's key and value lie in `page`, found when
    /// the cursor reaches it; `None` once exhausted.
    current: Option<(Range<usize>, Option<Range<usize>>)>,
}

impl Cursor<'_> {
    /// The current entry: its key and value (`None` for a tombstone), or `None` once exhausted.
    pub fn entry(&self) -> Option<(&[u8], Option<&[u8]>)> {
        let (key, value) = self.current.as_ref()?;
        Some((&self.page[key.clone()], value.clone().map(|value| &self.page[value])))
    }

    /// Moves to the next entry, reading the next page if this one is done.
    pub fn advance(&mut self) -> Result<()> {
        self.slot += 1;
        if self.slot < self.slots {
            return self.reach();
        }
        self.load()
    }

    /// Copies the next page, if any, and starts at its first entry.
    fn load(&mut self) -> Result<()> {
        let Some(id) = self.next_page.take() else {
            (self.slot, self.current) = (self.slots, None);
            return Ok(());
        };
        self.page.copy_from_slice(&*self.pool.fetch_read(id)?);
        let page = SlottedPage::new(&*self.page);
        self.next_page = page.next_page();
        (self.slot, self.slots) = (0, page.slot_count());
        self.reach()
    }

    /// Finds where the entry in the current slot lies, failing if it is
    /// missing or corrupted.
    fn reach(&mut self) -> Result<()> {
        let page: &Page = &self.page;
        let Some(bytes) = SlottedPage::new(page).into_tuple(self.slot) else {
            return err("corrupted SSTable: empty slot");
        };
        let (key, value) = decode_entry(bytes)?;
        let at = |part: &[u8]| {
            let start = part.as_ptr() as usize - page.as_ptr() as usize;
            start..start + part.len()
        };
        self.current = Some((at(key), value.map(at)));
        Ok(())
    }
}

/// Encodes an entry as stored in SSTable pages and in the LSM tree's log.
pub fn encode_entry(key: &[u8], value: Option<&[u8]>) -> Vec<u8> {
    let key_len = u16::try_from(key.len()).expect("keys are shorter than a page");
    let mut bytes = Vec::with_capacity(3 + key.len() + value.map_or(0, <[u8]>::len));
    bytes.extend_from_slice(&key_len.to_le_bytes());
    bytes.extend_from_slice(key);
    match value {
        None => bytes.push(0),
        Some(value) => {
            bytes.push(1);
            bytes.extend_from_slice(value);
        }
    }
    bytes
}

/// Size of an encoded entry.
pub fn entry_len(key: &[u8], value: &[u8]) -> usize {
    3 + key.len() + value.len()
}

pub fn decode_entry(bytes: &[u8]) -> Result<(&[u8], Option<&[u8]>)> {
    let Some(len) = bytes.get(..2) else { return err("corrupted entry: too short") };
    let key_end = 2 + u16::from_le_bytes([len[0], len[1]]) as usize;
    match (bytes.get(2..key_end), bytes.get(key_end)) {
        (Some(key), Some(0)) if bytes.len() == key_end + 1 => Ok((key, None)),
        (Some(key), Some(1)) => Ok((key, Some(&bytes[key_end + 1..]))),
        _ => err("corrupted entry"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DiskManager;

    fn pool() -> Arc<BufferPool> {
        Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 16))
    }

    fn key(i: u32) -> Vec<u8> {
        i.to_be_bytes().to_vec()
    }

    /// Even keys 0..2000 step 2 with 100-byte values; every tenth is a tombstone.
    fn entries() -> Vec<Entry> {
        (0..1000u32)
            .map(|i| (key(i * 2), (i % 10 != 0).then(|| vec![i as u8; 100])))
            .collect()
    }

    #[test]
    fn entry_round_trip() {
        let put = encode_entry(b"k", Some(b"value"));
        assert_eq!(decode_entry(&put).unwrap(), (&b"k"[..], Some(&b"value"[..])));
        assert_eq!(put.len(), entry_len(b"k", b"value"));
        let tombstone = encode_entry(b"k", None);
        assert_eq!(decode_entry(&tombstone).unwrap(), (&b"k"[..], None));
        assert!(decode_entry(&tombstone[..2]).is_err());
        assert!(decode_entry(&[1, 0, b'k', 9]).is_err());
    }

    #[test]
    fn get_finds_every_key_across_pages() {
        let table = SsTable::write(pool(), entries().into_iter().map(Ok)).unwrap().unwrap();
        assert!(table.page_count() > 10);
        assert_eq!(table.max_key(), key(1998));

        for (k, value) in entries() {
            assert_eq!(table.get(&k).unwrap(), Some(value));
        }
        // Odd keys, and keys outside the range, are not there.
        for i in [1, 777, 1999, 5000] {
            assert_eq!(table.get(&key(i)).unwrap(), None);
        }
    }

    #[test]
    fn iter_and_reopen_return_the_same_entries() {
        let pool = pool();
        let table = SsTable::write(pool.clone(), entries().into_iter().map(Ok)).unwrap().unwrap();
        let all: Vec<Entry> = table.iter().collect::<Result<_>>().unwrap();
        assert_eq!(all, entries());

        let reopened = SsTable::open(pool, table.first_page()).unwrap();
        assert_eq!(reopened.pages, table.pages);
        assert_eq!(reopened.max_key(), table.max_key());
        assert_eq!(reopened.get(&key(10)).unwrap(), Some(Some(vec![5; 100])));
    }

    #[test]
    fn writing_nothing_allocates_nothing() {
        let pool = pool();
        let pages = pool.disk().page_count();
        assert!(SsTable::write(pool.clone(), std::iter::empty()).unwrap().is_none());
        assert_eq!(pool.disk().page_count(), pages);
    }

    #[test]
    fn destroy_frees_every_page() {
        let pool = pool();
        let table = SsTable::write(pool.clone(), entries().into_iter().map(Ok)).unwrap().unwrap();
        let pages = pool.disk().page_count();
        table.destroy().unwrap();
        SsTable::write(pool.clone(), entries().into_iter().map(Ok)).unwrap().unwrap();
        assert_eq!(pool.disk().page_count(), pages, "freed pages are reused");
    }
}
