//! Slotted page: the layout of a heap page.
//!
//! ```text
//! 0        8                       free_end          PAGE_USABLE
//! +--------+-------------------+------+--------------+----------+
//! | header | slot array  -->   | free |  <-- tuples  | reserved |
//! +--------+-------------------+------+--------------+----------+
//!
//! header
//!   0..4  next page of the table heap (u32::MAX = none)
//!   4..6  slot count, including empty slots
//!   6..8  free_end: tuples occupy [free_end, PAGE_USABLE)
//!
//! slot
//!   0..2  offset
//!   2..4  length
//! ```
//!
//! Slot states:
//! - live:   length > 0
//! - unused: length = 0, offset = 0. Free for reuse.
//! - dead:   length = 0, offset = 1. Reserved for later: deleted but possibly
//!   still referenced (e.g. by an index), so it must not be reused yet.
//!
//! A slot number is part of a RecordId. Tuples may move within the page
//! (compaction), but a slot never changes its number.

use std::ops::{Deref, DerefMut};

use crate::storage::io::disk::{PAGE_USABLE, Page, PageId, decode_id, encode_id, read_u32, write_u32};

const HEADER_SIZE: usize = 8;
const SLOT_SIZE: usize = 4;
const UNUSED: (usize, usize) = (0, 0);

/// Largest tuple that fits in an empty page.
pub const MAX_TUPLE_SIZE: usize = PAGE_USABLE - HEADER_SIZE - SLOT_SIZE;

/// A view over a page buffer. Works with `&Page` for reading and `&mut Page` for writing.
pub struct SlottedPage<P>(P);

impl<P: Deref<Target = Page>> SlottedPage<P> {
    /// Wraps a page that has already been initialized with [`SlottedPage::init`].
    pub fn new(page: P) -> Self {
        Self(page)
    }

    pub fn next_page(&self) -> Option<PageId> {
        decode_id(read_u32(&self.0, 0))
    }

    pub fn slot_count(&self) -> u16 {
        self.u16_at(4)
    }

    /// Returns the tuple in a slot, or `None` if the slot is empty or out of range.
    pub fn get(&self, slot: u16) -> Option<&[u8]> {
        if slot >= self.slot_count() {
            return None;
        }
        let (offset, len) = self.slot(slot);
        (len > 0).then(|| &self.0[offset..offset + len])
    }

    /// Iterates over live tuples with their slot numbers.
    pub fn iter(&self) -> impl Iterator<Item = (u16, &[u8])> {
        (0..self.slot_count()).filter_map(|slot| Some((slot, self.get(slot)?)))
    }

    /// Contiguous free bytes between the slot array and the tuple data.
    pub fn free_space(&self) -> usize {
        self.free_end() - self.slots_end()
    }

    /// Size of the largest tuple [`SlottedPage::insert`] will accept, counting
    /// holes left by deleted tuples and the slot a new tuple may need.
    pub fn room(&self) -> usize {
        let slot_cost = if self.unused_slot().is_some() { 0 } else { SLOT_SIZE };
        self.total_free_space().saturating_sub(slot_cost)
    }

    /// Free bytes including holes left by deleted tuples.
    fn total_free_space(&self) -> usize {
        let live: usize = self.iter().map(|(_, tuple)| tuple.len()).sum();
        PAGE_USABLE - self.slots_end() - live
    }

    fn free_end(&self) -> usize {
        self.u16_at(6) as usize
    }

    fn slots_end(&self) -> usize {
        HEADER_SIZE + self.slot_count() as usize * SLOT_SIZE
    }

    fn unused_slot(&self) -> Option<u16> {
        (0..self.slot_count()).find(|&slot| self.slot(slot) == UNUSED)
    }

    fn slot(&self, slot: u16) -> (usize, usize) {
        let at = HEADER_SIZE + slot as usize * SLOT_SIZE;
        (self.u16_at(at) as usize, self.u16_at(at + 2) as usize)
    }

    fn u16_at(&self, at: usize) -> u16 {
        u16::from_le_bytes([self.0[at], self.0[at + 1]])
    }
}

impl<P: DerefMut<Target = Page>> SlottedPage<P> {
    /// Formats a page as an empty slotted page.
    pub fn init(page: P) -> Self {
        let mut this = Self(page);
        this.set_next_page(None);
        this.set_u16(4, 0);
        this.set_u16(6, PAGE_USABLE as u16);
        this
    }

    pub fn set_next_page(&mut self, next: Option<PageId>) {
        write_u32(&mut self.0, 0, encode_id(next));
    }

    /// Stores a tuple and returns its slot, or `None` if it does not fit.
    /// Reuses the lowest unused slot, compacting the page if needed.
    pub fn insert(&mut self, tuple: &[u8]) -> Option<u16> {
        assert!(!tuple.is_empty(), "a tuple must not be empty");
        let reuse = self.unused_slot();
        let needed = tuple.len() + if reuse.is_some() { 0 } else { SLOT_SIZE };

        if self.free_space() < needed {
            if self.total_free_space() < needed {
                return None;
            }
            self.compact();
        }

        let slot = reuse.unwrap_or_else(|| {
            let slot = self.slot_count();
            self.set_u16(4, slot + 1);
            slot
        });
        let offset = self.free_end() - tuple.len();
        self.0[offset..offset + tuple.len()].copy_from_slice(tuple);
        self.set_u16(6, offset as u16);
        self.set_slot(slot, offset, tuple.len());
        Some(slot)
    }

    /// Deletes a live tuple, leaving its slot unused. Returns false if the slot holds no tuple.
    /// The tuple's bytes become a hole that the next compaction reclaims.
    pub fn delete(&mut self, slot: u16) -> bool {
        if self.get(slot).is_none() {
            return false;
        }
        self.set_slot(slot, UNUSED.0, UNUSED.1);

        // Trailing unused slots can go; earlier ones stay so slot numbers never shift.
        let mut count = self.slot_count();
        while count > 0 && self.slot(count - 1) == UNUSED {
            count -= 1;
        }
        self.set_u16(4, count);
        true
    }

    /// Replaces a live tuple, keeping its slot number. Returns false if the slot
    /// holds no tuple or the new tuple does not fit in this page.
    ///
    /// A tuple that is not longer is written over the old bytes; a longer one
    /// moves within the page, compacting it if needed.
    pub fn update(&mut self, slot: u16, tuple: &[u8]) -> bool {
        assert!(!tuple.is_empty(), "a tuple must not be empty");
        let Some(old_len) = self.get(slot).map(<[u8]>::len) else {
            return false;
        };
        let (offset, _) = self.slot(slot);
        if tuple.len() <= old_len {
            // The unused tail of the old tuple becomes a hole.
            self.0[offset..offset + tuple.len()].copy_from_slice(tuple);
            self.set_slot(slot, offset, tuple.len());
            return true;
        }

        if self.total_free_space() + old_len < tuple.len() {
            return false;
        }
        // Free the old bytes but keep the slot, so compaction can reclaim them.
        self.set_slot(slot, UNUSED.0, UNUSED.1);
        if self.free_space() < tuple.len() {
            self.compact();
        }
        let offset = self.free_end() - tuple.len();
        self.0[offset..offset + tuple.len()].copy_from_slice(tuple);
        self.set_u16(6, offset as u16);
        self.set_slot(slot, offset, tuple.len());
        true
    }

    /// Moves live tuples to the end of the page, squeezing out holes.
    fn compact(&mut self) {
        let mut packed = [0u8; PAGE_USABLE];
        let mut end = PAGE_USABLE;
        for slot in 0..self.slot_count() {
            let (offset, len) = self.slot(slot);
            if len == 0 {
                continue;
            }
            end -= len;
            packed[end..end + len].copy_from_slice(&self.0[offset..offset + len]);
            self.set_slot(slot, end, len);
        }
        self.0[end..PAGE_USABLE].copy_from_slice(&packed[end..]);
        self.set_u16(6, end as u16);
    }

    fn set_slot(&mut self, slot: u16, offset: usize, len: usize) {
        let at = HEADER_SIZE + slot as usize * SLOT_SIZE;
        self.set_u16(at, offset as u16);
        self.set_u16(at + 2, len as u16);
    }

    fn set_u16(&mut self, at: usize, value: u16) {
        self.0[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::PAGE_SIZE;

    fn empty() -> Page {
        let mut page = [0xFF; PAGE_SIZE];
        SlottedPage::init(&mut page);
        page
    }

    #[test]
    fn insert_get_and_iterate() {
        let mut buf = empty();
        let mut page = SlottedPage::new(&mut buf);
        assert_eq!(page.next_page(), None);
        page.set_next_page(Some(PageId(9)));

        assert_eq!(page.insert(b"alice"), Some(0));
        assert_eq!(page.insert(b"bob"), Some(1));
        assert_eq!(page.get(0), Some(&b"alice"[..]));
        assert_eq!(page.get(2), None);

        // A read-only view over the same bytes.
        let view = SlottedPage::new(&buf);
        assert_eq!(view.next_page(), Some(PageId(9)));
        let tuples: Vec<(u16, &[u8])> = view.iter().collect();
        assert_eq!(tuples, vec![(0, &b"alice"[..]), (1, &b"bob"[..])]);
    }

    #[test]
    fn rejects_tuples_when_full() {
        let mut buf = empty();
        let mut page = SlottedPage::new(&mut buf);
        let mut count = 0;
        while page.insert(&[1; 100]).is_some() {
            count += 1;
        }
        assert_eq!(count, (PAGE_USABLE - HEADER_SIZE) / (100 + SLOT_SIZE));
        assert!(page.free_space() < 100 + SLOT_SIZE);
    }

    #[test]
    fn largest_tuple_fits_exactly() {
        let mut buf = empty();
        let mut page = SlottedPage::new(&mut buf);
        assert_eq!(page.insert(&[0; MAX_TUPLE_SIZE + 1]), None);
        assert_eq!(page.insert(&[7; MAX_TUPLE_SIZE]), Some(0));
        assert_eq!(page.free_space(), 0);
    }

    #[test]
    fn delete_reuses_lowest_slot_and_keeps_others() {
        let mut buf = empty();
        let mut page = SlottedPage::new(&mut buf);
        for name in [b"a", b"b", b"c", b"d"] {
            page.insert(name).unwrap();
        }
        assert!(page.delete(2));
        assert!(page.delete(0));
        assert!(!page.delete(0), "already deleted");
        assert_eq!(page.get(0), None);

        assert_eq!(page.insert(b"x"), Some(0));
        assert_eq!(page.insert(b"y"), Some(2));
        assert_eq!(page.insert(b"z"), Some(4));
        assert_eq!(page.get(1), Some(&b"b"[..]));
        assert_eq!(page.get(3), Some(&b"d"[..]));
    }

    #[test]
    fn trailing_unused_slots_are_trimmed() {
        let mut buf = empty();
        let mut page = SlottedPage::new(&mut buf);
        for name in [b"a", b"b", b"c"] {
            page.insert(name).unwrap();
        }
        page.delete(1);
        assert_eq!(page.slot_count(), 3, "a middle slot keeps its place");
        page.delete(2);
        assert_eq!(page.slot_count(), 1, "slots 1 and 2 are both trailing now");
    }

    #[test]
    fn compaction_reclaims_holes_without_changing_slots() {
        let mut buf = empty();
        let mut page = SlottedPage::new(&mut buf);
        let slots: Vec<u16> = (0..30u8).map(|i| page.insert(&[i; 100]).unwrap()).collect();
        for &slot in slots.iter().step_by(2) {
            page.delete(slot);
        }
        // Too big for the contiguous gap, small enough once holes are squeezed out.
        let big = [0xEE; 1500];
        assert!(page.free_space() < big.len());
        let slot = page.insert(&big).unwrap();

        assert_eq!(page.get(slot), Some(&big[..]));
        for &slot in slots.iter().skip(1).step_by(2) {
            assert_eq!(page.get(slot), Some(&[slot as u8; 100][..]));
        }
    }

    #[test]
    fn room_is_exactly_the_largest_insertable_tuple() {
        let mut buf = empty();
        let mut page = SlottedPage::new(&mut buf);
        assert_eq!(page.room(), MAX_TUPLE_SIZE);
        for i in 0..10u8 {
            page.insert(&[i; 300]).unwrap();
        }
        page.delete(3);
        page.delete(6);
        // Slot 3 is unused, so the next tuple needs no new slot.
        let room = page.room();
        assert_eq!(page.insert(&vec![1; room + 1]), None);
        assert_eq!(page.insert(&vec![1; room]), Some(3));
        assert_eq!(page.room(), 0);
    }

    #[test]
    fn update_keeps_the_slot() {
        let mut buf = empty();
        let mut page = SlottedPage::new(&mut buf);
        let a = page.insert(b"alice").unwrap();
        let b = page.insert(b"bob").unwrap();

        assert!(page.update(a, b"al"), "shrink");
        assert_eq!(page.get(a), Some(&b"al"[..]));
        assert!(page.update(a, b"alexandra"), "grow");
        assert_eq!(page.get(a), Some(&b"alexandra"[..]));
        assert_eq!(page.get(b), Some(&b"bob"[..]));
        assert_eq!(page.slot_count(), 2);

        page.delete(b);
        assert!(!page.update(b, b"x"), "no tuple there");
    }

    #[test]
    fn update_compacts_to_grow_and_fails_without_room() {
        let mut buf = empty();
        let mut page = SlottedPage::new(&mut buf);
        let slots: Vec<u16> = (0..30u8).map(|i| page.insert(&[i; 100]).unwrap()).collect();
        for &slot in slots.iter().skip(1).step_by(2) {
            page.delete(slot);
        }
        // Only fits once the holes are squeezed out.
        let big = [0xEE; 1500];
        assert!(page.free_space() < big.len());
        assert!(page.update(slots[0], &big));
        assert_eq!(page.get(slots[0]), Some(&big[..]));
        for &slot in slots.iter().step_by(2).skip(1) {
            assert_eq!(page.get(slot), Some(&[slot as u8; 100][..]));
        }

        let too_big = vec![0xAA; page.room() + big.len() + 1];
        assert!(!page.update(slots[0], &too_big));
        assert_eq!(page.get(slots[0]), Some(&big[..]), "unchanged");
    }

    #[test]
    fn dead_slots_are_not_reused() {
        let mut buf = empty();
        let mut page = SlottedPage::new(&mut buf);
        page.insert(b"a").unwrap();
        page.set_slot(0, 1, 0); // mark slot 0 dead
        assert_eq!(page.get(0), None);
        assert!(!page.delete(0));
        assert_eq!(page.insert(b"b"), Some(1));
        assert_eq!(page.slot_count(), 2);
    }
}
