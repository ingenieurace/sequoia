//! Linear probe hashing: one array of slots. A key goes in the slot its hash
//! picks or, if that is taken, the next free one after it, wrapping around.
//!
//! ```text
//! insert c, hash -> 2:   [ . | a | b | d | . | . ]      2 taken, 3 taken
//!                                     ^-------^ c goes in 4, the first free slot
//! ```
//!
//! A lookup starts where the key's hash points and walks on until it finds
//! the key or an empty slot. Keys that collide pile up into runs of taken
//! slots, which new keys hashed anywhere into them make longer: the fuller
//! the table, the longer the walks. So the table grows, to twice as many
//! slots with every entry inserted again, once more than `max_load` percent
//! of them are in use.
//!
//! Removing a key cannot just empty its slot: a lookup for a key further
//! along the same run would stop there and miss it. The slot becomes a
//! tombstone instead, which lookups walk past and inserts reuse. Tombstones
//! count as slots in use, so a table that fills up with them is rebuilt
//! without them, at the same size if its entries alone do not need more.

use std::hash::{BuildHasher, Hash, RandomState};
use std::mem;

use super::HashTable;

/// The default `max_load`: past 70% taken, the runs get long quickly.
const MAX_LOAD_PERCENT: usize = 70;
const MIN_SLOTS: usize = 8;

enum Slot<K, V> {
    Empty,
    Tombstone,
    Full(K, V),
}

pub struct LinearProbe<K, V, S = RandomState> {
    /// A power of two of them, so a hash picks one by its low bits.
    slots: Vec<Slot<K, V>>,
    len: usize,
    tombstones: usize,
    /// Percent of slots that may be in use, entries and tombstones, before the table grows.
    max_load: usize,
    hasher: S,
}

impl<K, V> LinearProbe<K, V> {
    pub fn new() -> Self {
        Self::with_hasher(RandomState::new())
    }

    /// A table that grows once more than `percent` of its slots are in use.
    pub fn with_max_load(percent: usize) -> Self {
        assert!((1..100).contains(&percent), "a max load must leave a slot empty");
        LinearProbe { max_load: percent, ..Self::new() }
    }
}

impl<K, V, S> LinearProbe<K, V, S> {
    pub fn with_hasher(hasher: S) -> Self {
        LinearProbe { slots: Vec::new(), len: 0, tombstones: 0, max_load: MAX_LOAD_PERCENT, hasher }
    }

    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    pub fn tombstones(&self) -> usize {
        self.tombstones
    }
}

impl<K, V, S: Default> Default for LinearProbe<K, V, S> {
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> LinearProbe<K, V, S> {
    fn home(&self, key: &K) -> usize {
        self.hasher.hash_one(key) as usize & (self.slots.len() - 1)
    }

    /// The slot holding `key`, if any. The walk ends, as `max_load` < 100
    /// leaves some slot empty.
    fn find(&self, key: &K) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut i = self.home(key);
        loop {
            match &self.slots[i] {
                Slot::Empty => return None,
                Slot::Full(k, _) if k == key => return Some(i),
                _ => i = (i + 1) & mask,
            }
        }
    }

    /// Puts an entry whose key is not in the table in the first slot that
    /// is not full from its home on, reusing a tombstone if it finds one first.
    fn place(&mut self, key: K, value: V) {
        let mask = self.slots.len() - 1;
        let mut i = self.home(&key);
        while let Slot::Full(..) = self.slots[i] {
            i = (i + 1) & mask;
        }
        if let Slot::Tombstone = self.slots[i] {
            self.tombstones -= 1;
        }
        self.slots[i] = Slot::Full(key, value);
        self.len += 1;
    }

    /// Inserts every entry again into a new array, which has room for twice
    /// as many entries as there are before reaching `max_load`, and no tombstones.
    fn rebuild(&mut self) {
        let mut size = self.slots.len().max(MIN_SLOTS);
        while (self.len + 1) * 200 > size * self.max_load {
            size *= 2;
        }
        let old = mem::replace(&mut self.slots, (0..size).map(|_| Slot::Empty).collect());
        self.len = 0;
        self.tombstones = 0;
        for slot in old {
            if let Slot::Full(key, value) = slot {
                self.place(key, value);
            }
        }
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> HashTable<K, V> for LinearProbe<K, V, S> {
    fn insert(&mut self, key: K, value: V) -> Option<V> {
        if let Some(i) = self.find(&key) {
            let Slot::Full(_, old) = &mut self.slots[i] else { unreachable!("find returns full slots") };
            return Some(mem::replace(old, value));
        }
        if (self.len + self.tombstones + 1) * 100 > self.slots.len() * self.max_load {
            self.rebuild();
        }
        self.place(key, value);
        None
    }

    fn get(&self, key: &K) -> Option<&V> {
        match &self.slots[self.find(key)?] {
            Slot::Full(_, value) => Some(value),
            _ => unreachable!("find returns full slots"),
        }
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let i = self.find(key)?;
        // If the next slot is empty, no walk goes past this one: it can be
        // left empty rather than a tombstone.
        let next_empty = matches!(self.slots[(i + 1) & (self.slots.len() - 1)], Slot::Empty);
        let freed = if next_empty { Slot::Empty } else { Slot::Tombstone };
        let Slot::Full(_, value) = mem::replace(&mut self.slots[i], freed) else {
            unreachable!("find returns full slots")
        };
        self.len -= 1;
        if !next_empty {
            self.tombstones += 1;
        }
        Some(value)
    }

    fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_before_max_load() {
        let mut table = LinearProbe::with_max_load(50);
        for key in 0..1000 {
            table.insert(key, ());
            assert!(table.len() * 100 <= table.slots() * 50);
        }
    }

    #[test]
    fn removes_leave_tombstones_that_inserts_reuse() {
        let mut table = LinearProbe::new();
        for key in 0..100 {
            table.insert(key, key);
        }
        let slots = table.slots();
        for key in 0..50 {
            table.remove(&key);
        }
        assert!(table.tombstones() > 0);
        assert!((50..100).all(|key| table.get(&key) == Some(&key)), "lookups walk past tombstones");

        // As many inserts as removes: tombstones are reused, or swept away by a
        // rebuild at the same size, as the entries alone need no more room.
        for key in 100..150 {
            table.insert(key, key);
        }
        assert_eq!(table.slots(), slots);
        assert!((50..150).all(|key| table.get(&key) == Some(&key)));
    }
}
