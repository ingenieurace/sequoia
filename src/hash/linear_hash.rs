//! Linear hashing: buckets that split one after another, in order, with no
//! directory.
//!
//! ```text
//! level 2, split 1:   bucket 0   1   2   3   4
//!                            ^   ^
//!                            |   next to split
//!                            split this round, into 0 and 4
//! ```
//!
//! A round starts with 2^level buckets and doubles them: `split` points to
//! the next bucket to split, and each split moves some of its entries to a
//! new bucket at the end. A hash picks bucket `hash mod 2^level`, unless that
//! bucket was already split this round: then `hash mod 2^(level + 1)`, which
//! is either it or the new bucket it split into.
//!
//! A bucket holds `BUCKET_SIZE` entries, and entries past those are its
//! overflow chain. Whenever an insert makes any bucket overflow, the bucket
//! at `split` splits, which need not be the one that overflowed: that one
//! keeps its overflow chain until its turn comes. Only the split bucket's
//! entries move, and the table grows by one bucket at a time.
//!
//! Removing entries never merges buckets.

use std::hash::{BuildHasher, Hash, RandomState};
use std::mem;

use super::HashTable;

const BUCKET_SIZE: usize = 16;

pub struct LinearHash<K, V, S = RandomState> {
    /// 2^level + split of them. Each entry with its key's hash.
    buckets: Vec<Vec<(u64, K, V)>>,
    level: u32,
    /// The next bucket to split; those before it have split this round.
    split: usize,
    len: usize,
    hasher: S,
}

impl<K, V> LinearHash<K, V> {
    pub fn new() -> Self {
        Self::with_hasher(RandomState::new())
    }
}

impl<K, V, S> LinearHash<K, V, S> {
    pub fn with_hasher(hasher: S) -> Self {
        LinearHash { buckets: vec![Vec::new()], level: 0, split: 0, len: 0, hasher }
    }

    pub fn buckets(&self) -> usize {
        self.buckets.len()
    }

    /// Buckets with an overflow chain.
    pub fn overflowing(&self) -> usize {
        self.buckets.iter().filter(|bucket| bucket.len() > BUCKET_SIZE).count()
    }

    fn bucket(&self, hash: u64) -> usize {
        let b = hash as usize & ((1 << self.level) - 1);
        match b < self.split {
            true => hash as usize & ((1 << (self.level + 1)) - 1),
            false => b,
        }
    }

    /// Splits the bucket at `split` into it and a new bucket at the end, by
    /// one more bit of hash, and moves `split` on.
    fn split_next(&mut self) {
        let old = self.split;
        debug_assert_eq!(self.buckets.len(), old + (1 << self.level));
        let mask = (1 << (self.level + 1)) - 1;
        let (kept, moved) = mem::take(&mut self.buckets[old]).into_iter().partition(|(h, ..)| *h as usize & mask == old);
        self.buckets[old] = kept;
        self.buckets.push(moved);
        self.split += 1;
        if self.split == 1 << self.level {
            self.level += 1;
            self.split = 0;
        }
    }
}

impl<K, V, S: Default> Default for LinearHash<K, V, S> {
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> LinearHash<K, V, S> {
    /// The bucket and position in it of the entry for `key`, if any.
    fn find(&self, hash: u64, key: &K) -> Option<(usize, usize)> {
        let b = self.bucket(hash);
        self.buckets[b].iter().position(|(h, k, _)| *h == hash && k == key).map(|i| (b, i))
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> HashTable<K, V> for LinearHash<K, V, S> {
    fn insert(&mut self, key: K, value: V) -> Option<V> {
        let hash = self.hasher.hash_one(&key);
        if let Some((b, i)) = self.find(hash, &key) {
            return Some(mem::replace(&mut self.buckets[b][i].2, value));
        }
        let b = self.bucket(hash);
        self.buckets[b].push((hash, key, value));
        self.len += 1;
        if self.buckets[b].len() > BUCKET_SIZE {
            self.split_next();
        }
        None
    }

    fn get(&self, key: &K) -> Option<&V> {
        let (b, i) = self.find(self.hasher.hash_one(key), key)?;
        Some(&self.buckets[b][i].2)
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let (b, i) = self.find(self.hasher.hash_one(key), key)?;
        self.len -= 1;
        Some(self.buckets[b].swap_remove(i).2)
    }

    fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_is_in_the_bucket_its_hash_picks() {
        let mut table = LinearHash::new();
        for key in 0..20_000 {
            table.insert(key, ());
            assert_eq!(table.buckets(), (1 << table.level) + table.split);
        }
        for (b, bucket) in table.buckets.iter().enumerate() {
            for (hash, ..) in bucket {
                assert_eq!(table.bucket(*hash), b);
            }
        }
    }

    #[test]
    fn grows_a_bucket_at_a_time_and_keeps_overflow_rare() {
        let mut table = LinearHash::new();
        for key in 0..20_000 {
            let before = table.buckets();
            table.insert(key, ());
            assert!(table.buckets() - before <= 1);
        }
        assert!(table.overflowing() * 10 < table.buckets(), "{} of {}", table.overflowing(), table.buckets());
    }
}
