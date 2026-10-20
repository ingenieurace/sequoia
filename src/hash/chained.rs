//! Chained hashing: an array of buckets, each a chain of every entry whose
//! hash picks it.
//!
//! ```text
//! bucket 0   filter 0010..   [(h, a, 1)]
//! bucket 1   filter 0000..   []
//! bucket 2   filter 1010..   [(h, b, 2), (h, c, 3), (h, d, 4)]
//! ```
//!
//! A chain takes any number of entries, so the table is never full; but a
//! lookup reads its key's chain from the start, and long chains make that a
//! scan. Two things keep that short:
//!
//! - Each bucket has a filter: 64 bits, of which each of its keys sets two
//!   chosen by its hash, a Bloom filter in one word. A lookup for a key whose
//!   bits are not all set ends there without reading the chain.
//! - Once there are more than `MAX_LOAD` entries per bucket on average, the
//!   bucket array doubles, and every entry moves to its new bucket. Each
//!   entry keeps its hash, so that moves it without hashing its key again.
//!   That move takes all the entries at once; extendible and linear hashing
//!   split one bucket at a time instead.

use std::hash::{BuildHasher, Hash, RandomState};
use std::mem;

use super::HashTable;

const MAX_LOAD: usize = 4;
const MIN_BUCKETS: usize = 8;

struct Bucket<K, V> {
    /// [`filter_bits`] of every entry's hash, or'ed together.
    filter: u64,
    /// Each entry with its key's hash.
    entries: Vec<(u64, K, V)>,
}

impl<K, V> Bucket<K, V> {
    fn new() -> Self {
        Bucket { filter: 0, entries: Vec::new() }
    }
}

/// The two filter bits of a hash, from its top 12 bits: the bucket comes
/// from its low bits, so the two are independent.
fn filter_bits(hash: u64) -> u64 {
    1 << (hash >> 58) | 1 << ((hash >> 52) & 63)
}

pub struct Chained<K, V, S = RandomState> {
    /// A power of two of them, so a hash picks one by its low bits.
    buckets: Vec<Bucket<K, V>>,
    len: usize,
    hasher: S,
}

impl<K, V> Chained<K, V> {
    pub fn new() -> Self {
        Self::with_hasher(RandomState::new())
    }
}

impl<K, V, S> Chained<K, V, S> {
    pub fn with_hasher(hasher: S) -> Self {
        Chained { buckets: (0..MIN_BUCKETS).map(|_| Bucket::new()).collect(), len: 0, hasher }
    }

    pub fn buckets(&self) -> usize {
        self.buckets.len()
    }

    pub fn longest_chain(&self) -> usize {
        self.buckets.iter().map(|bucket| bucket.entries.len()).max().unwrap_or(0)
    }
}

impl<K, V, S: Default> Default for Chained<K, V, S> {
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> Chained<K, V, S> {
    fn bucket(&self, hash: u64) -> usize {
        hash as usize & (self.buckets.len() - 1)
    }

    /// The bucket and position in its chain of the entry for `key`, if any.
    fn find(&self, hash: u64, key: &K) -> Option<(usize, usize)> {
        let b = self.bucket(hash);
        let bucket = &self.buckets[b];
        let bits = filter_bits(hash);
        if bucket.filter & bits != bits {
            return None;
        }
        bucket.entries.iter().position(|(h, k, _)| *h == hash && k == key).map(|i| (b, i))
    }

    fn push(&mut self, hash: u64, key: K, value: V) {
        let b = self.bucket(hash);
        let bucket = &mut self.buckets[b];
        bucket.filter |= filter_bits(hash);
        bucket.entries.push((hash, key, value));
    }

    fn double(&mut self) {
        let doubled = (0..self.buckets.len() * 2).map(|_| Bucket::new()).collect();
        let old = mem::replace(&mut self.buckets, doubled);
        for (hash, key, value) in old.into_iter().flat_map(|bucket| bucket.entries) {
            self.push(hash, key, value);
        }
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> HashTable<K, V> for Chained<K, V, S> {
    fn insert(&mut self, key: K, value: V) -> Option<V> {
        let hash = self.hasher.hash_one(&key);
        if let Some((b, i)) = self.find(hash, &key) {
            return Some(mem::replace(&mut self.buckets[b].entries[i].2, value));
        }
        if self.len + 1 > self.buckets.len() * MAX_LOAD {
            self.double();
        }
        self.push(hash, key, value);
        self.len += 1;
        None
    }

    fn get(&self, key: &K) -> Option<&V> {
        let (b, i) = self.find(self.hasher.hash_one(key), key)?;
        Some(&self.buckets[b].entries[i].2)
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let (b, i) = self.find(self.hasher.hash_one(key), key)?;
        let bucket = &mut self.buckets[b];
        let (_, _, value) = bucket.entries.swap_remove(i);
        // A bit may be set for other keys too, so the filter is made again from those left.
        bucket.filter = bucket.entries.iter().fold(0, |filter, (h, ..)| filter | filter_bits(*h));
        self.len -= 1;
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
    fn chains_stay_short() {
        let mut table = Chained::new();
        for key in 0..100_000 {
            table.insert(key, ());
        }
        assert!(table.buckets() * MAX_LOAD >= table.len());
        assert!(table.longest_chain() < 30, "longest chain {}", table.longest_chain());
    }

    #[test]
    fn filters_hold_every_key() {
        let mut table = Chained::new();
        for key in 0..1000 {
            table.insert(key, ());
        }
        for key in (0..1000).step_by(3) {
            table.remove(&key);
        }
        for bucket in &table.buckets {
            assert_eq!(bucket.filter, bucket.entries.iter().fold(0, |f, (h, ..)| f | filter_bits(*h)));
        }
    }
}
