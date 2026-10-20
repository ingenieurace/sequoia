//! Hash tables, five ways, to compare with each other and with
//! `std::collections::HashMap` (see `benches/hash.rs`). Nothing else uses
//! them yet: the executor's hash joins and aggregates still use `HashMap`.
//!
//! Static schemes keep entries in one array of slots. When it fills up, the
//! only way to grow is a new array twice the size, with every entry hashed
//! into it again.
//!
//! - [`LinearProbe`]: a taken slot sends a key on to the next one.
//! - [`Cuckoo`]: each key has one slot in each of two tables, and a key that
//!   finds both taken kicks one of their keys out to its other slot.
//!
//! Dynamic schemes keep entries in buckets, and grow by splitting buckets:
//!
//! - [`Chained`]: a bucket takes any number of entries, so the table is never
//!   full.
//! - [`Extendible`]: a full bucket splits in two, and a directory, indexed by
//!   the first bits of a hash, points to each bucket.
//! - [`LinearHash`]: an overflowing bucket makes the next bucket in turn
//!   split, so buckets split in order and no directory is needed.
//!
//! All five hash keys with a [`BuildHasher`](std::hash::BuildHasher):
//! `RandomState` (SipHash, as `HashMap` does) unless given another, so a
//! comparison measures the tables, not the hash function. All are in memory:
//! a bucket of the dynamic schemes is a `Vec`, where a database would use a page.

mod chained;
mod cuckoo;
mod extendible;
mod linear_hash;
mod linear_probe;

pub use chained::Chained;
pub use cuckoo::Cuckoo;
pub use extendible::Extendible;
pub use linear_hash::LinearHash;
pub use linear_probe::LinearProbe;

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash};

/// What every table here does, and `HashMap` too, so tests and benchmarks
/// can treat them alike.
pub trait HashTable<K, V> {
    /// Maps `key` to `value`, returning the value it had, if any.
    fn insert(&mut self, key: K, value: V) -> Option<V>;
    fn get(&self, key: &K) -> Option<&V>;
    /// Removes `key`, returning its value, if it had one.
    fn remove(&mut self, key: &K) -> Option<V>;
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> HashTable<K, V> for HashMap<K, V, S> {
    fn insert(&mut self, key: K, value: V) -> Option<V> {
        HashMap::insert(self, key, value)
    }

    fn get(&self, key: &K) -> Option<&V> {
        HashMap::get(self, key)
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        HashMap::remove(self, key)
    }

    fn len(&self) -> usize {
        HashMap::len(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs random inserts, lookups and removes on `table` and on a
    /// `HashMap`, over few enough keys that most hit, and checks that they
    /// always agree.
    fn agrees_with_std(mut table: impl HashTable<u64, u64>) {
        let mut std = HashMap::new();
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        for step in 0..200_000u64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let key = x % 5_000;
            match (x >> 32) % 4 {
                0 | 1 => assert_eq!(table.insert(key, step), std.insert(key, step), "insert {key}"),
                2 => assert_eq!(table.remove(&key), std.remove(&key), "remove {key}"),
                _ => assert_eq!(table.get(&key), std.get(&key), "get {key}"),
            }
            assert_eq!(table.len(), std.len());
        }
        for key in 0..5_000 {
            assert_eq!(table.get(&key), std.get(&key), "get {key}");
        }
        // Empty it, and fill it again past where it was.
        for key in 0..5_000 {
            assert_eq!(table.remove(&key), std.remove(&key), "remove {key}");
        }
        assert!(table.is_empty());
        for key in 0..20_000 {
            assert_eq!(table.insert(key, key), None);
        }
        assert!((0..20_000).all(|key| table.get(&key) == Some(&key)));
        assert_eq!(table.get(&20_000), None);
    }

    #[test]
    fn linear_probe_agrees_with_std() {
        agrees_with_std(LinearProbe::new());
        agrees_with_std(LinearProbe::with_max_load(95));
    }

    #[test]
    fn cuckoo_agrees_with_std() {
        agrees_with_std(Cuckoo::new());
    }

    #[test]
    fn chained_agrees_with_std() {
        agrees_with_std(Chained::new());
    }

    #[test]
    fn extendible_agrees_with_std() {
        agrees_with_std(Extendible::new());
    }

    #[test]
    fn linear_hash_agrees_with_std() {
        agrees_with_std(LinearHash::new());
    }
}
