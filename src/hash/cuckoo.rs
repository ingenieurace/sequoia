//! Cuckoo hashing: two tables, each with its own hash function, so each key
//! has one slot in either table and is always in one of those two.
//!
//! ```text
//! insert x:  x's slot in table 0 holds a  -->  x takes it; a goes to its slot in table 1,
//!            which holds b                -->  a takes it; b goes to its slot in table 0,
//!            which is empty               -->  b takes it. Done.
//! ```
//!
//! A lookup or a remove reads two slots, never more. An insert may kick keys
//! along a long path, or around a cycle that never ends; after `MAX_KICKS`
//! kicks it gives up, and every entry is inserted again with two new hash
//! functions, into tables twice the size if that fails too.
//!
//! Two tables of one slot per bucket fill up past about 50%, so the tables
//! grow, doubling, once their entries take more than `MAX_LOAD_PERCENT` of
//! the slots. Nothing needs a tombstone: no lookup walks past a slot.

use std::hash::{BuildHasher, Hash, RandomState};
use std::mem;

use super::HashTable;

const MAX_LOAD_PERCENT: usize = 45;
const MAX_KICKS: usize = 64;
/// Per table.
const MIN_SLOTS: usize = 8;

pub struct Cuckoo<K, V, S = RandomState> {
    /// Each a power of two of slots, the same number in both.
    tables: [Vec<Option<(K, V)>>; 2],
    /// Each table's hash function is the hasher's, of its seed and the key.
    seeds: [u64; 2],
    len: usize,
    /// How many times new hash functions were picked.
    rehashes: usize,
    hasher: S,
}

impl<K, V> Cuckoo<K, V> {
    pub fn new() -> Self {
        Self::with_hasher(RandomState::new())
    }
}

impl<K, V, S> Cuckoo<K, V, S> {
    pub fn with_hasher(hasher: S) -> Self {
        Cuckoo { tables: [Vec::new(), Vec::new()], seeds: [1, 2], len: 0, rehashes: 0, hasher }
    }

    /// Slots in both tables together.
    pub fn slots(&self) -> usize {
        self.tables[0].len() * 2
    }

    pub fn rehashes(&self) -> usize {
        self.rehashes
    }
}

impl<K, V, S: Default> Default for Cuckoo<K, V, S> {
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> Cuckoo<K, V, S> {
    fn slot(&self, table: usize, key: &K) -> usize {
        self.hasher.hash_one((self.seeds[table], key)) as usize & (self.tables[table].len() - 1)
    }

    /// The table and slot holding `key`, if any.
    fn find(&self, key: &K) -> Option<(usize, usize)> {
        if self.len == 0 {
            return None;
        }
        (0..2).map(|t| (t, self.slot(t, key))).find(|&(t, i)| matches!(&self.tables[t][i], Some((k, _)) if k == key))
    }

    /// Puts an entry whose key is not in the tables into an empty slot of
    /// its two, or else kicks keys out until one lands in an empty slot.
    /// Gives back the entry left without a slot after `MAX_KICKS` kicks.
    fn place(&mut self, entry: (K, V)) -> Result<(), (K, V)> {
        for t in 0..2 {
            let i = self.slot(t, &entry.0);
            if self.tables[t][i].is_none() {
                self.tables[t][i] = Some(entry);
                return Ok(());
            }
        }
        let mut entry = entry;
        for kick in 0..MAX_KICKS {
            // The entry kicked out of one table goes to its slot in the other.
            let t = kick % 2;
            let i = self.slot(t, &entry.0);
            match self.tables[t][i].replace(entry) {
                None => return Ok(()),
                Some(out) => entry = out,
            }
        }
        Err(entry)
    }

    /// Inserts every entry, and `extra`, again into tables of `size` slots
    /// each with new hash functions, doubling the size until they all fit.
    fn rehash(&mut self, mut size: usize, extra: Option<(K, V)>) {
        let mut entries = self.take_entries();
        entries.extend(extra);
        loop {
            self.rehashes += 1;
            self.seeds = self.seeds.map(|seed| seed.wrapping_add(0x9e37_79b9_7f4a_7c15));
            self.tables = [(0..size).map(|_| None).collect(), (0..size).map(|_| None).collect()];
            let mut homeless = None;
            while let Some(entry) = entries.pop() {
                if let Err(entry) = self.place(entry) {
                    homeless = Some(entry);
                    break;
                }
            }
            let Some(homeless) = homeless else { return };
            entries.push(homeless);
            entries.extend(self.take_entries());
            size *= 2;
        }
    }

    fn take_entries(&mut self) -> Vec<(K, V)> {
        mem::take(&mut self.tables).into_iter().flatten().flatten().collect()
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> HashTable<K, V> for Cuckoo<K, V, S> {
    fn insert(&mut self, key: K, value: V) -> Option<V> {
        if let Some((t, i)) = self.find(&key) {
            let Some((_, old)) = &mut self.tables[t][i] else { unreachable!("find returns full slots") };
            return Some(mem::replace(old, value));
        }
        if (self.len + 1) * 100 > self.slots() * MAX_LOAD_PERCENT {
            self.rehash((self.tables[0].len() * 2).max(MIN_SLOTS), None);
        }
        self.len += 1;
        if let Err(homeless) = self.place((key, value)) {
            // A cycle, or too long a path: new hash functions, at the same size first.
            self.rehash(self.tables[0].len(), Some(homeless));
        }
        None
    }

    fn get(&self, key: &K) -> Option<&V> {
        let (t, i) = self.find(key)?;
        self.tables[t][i].as_ref().map(|(_, value)| value)
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let (t, i) = self.find(key)?;
        self.len -= 1;
        self.tables[t][i].take().map(|(_, value)| value)
    }

    fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_under_max_load() {
        let mut table = Cuckoo::new();
        for key in 0..10_000 {
            table.insert(key, ());
            assert!(table.len() * 100 <= table.slots() * MAX_LOAD_PERCENT);
        }
        assert!((0..10_000).all(|key| table.get(&key).is_some()));
    }

    #[test]
    fn a_key_is_in_one_of_its_two_slots() {
        let mut table = Cuckoo::new();
        for key in 0..10_000u64 {
            table.insert(key, key);
        }
        for t in 0..2 {
            for (i, slot) in table.tables[t].iter().enumerate() {
                if let Some((key, _)) = slot {
                    assert_eq!(table.slot(t, key), i);
                }
            }
        }
    }
}
