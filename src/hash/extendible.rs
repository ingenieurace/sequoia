//! Extendible hashing: buckets of at most `BUCKET_SIZE` entries, and a
//! directory that points to them by the first bits of a hash.
//!
//! ```text
//! global depth 2           bucket   local depth   hashes starting with
//! directory  00 --------->   A          1           0
//!            01 ----/
//!            10 --------->   B          2           10
//!            11 --------->   C          2           11
//! ```
//!
//! The directory has 2^global depth entries: entry i is for hashes whose
//! first global-depth bits are i. A bucket of local depth d holds the hashes
//! that start with some d bits, and 2^(global - d) entries point to it.
//!
//! When a key's bucket is full, the bucket splits in two by its hashes'
//! next bit, and only its entries move. If its local depth was the global
//! depth, no two entries pointed to it, so the directory doubles first:
//! entry i becomes entries 2i and 2i+1, both pointing where i did. Then the
//! entries that pointed to the bucket and have the next bit set point to the
//! new one. If all the entries' hashes share that bit, one half is still
//! full, and the insert splits again.
//!
//! Removing entries never merges buckets or shrinks the directory.

use std::hash::{BuildHasher, Hash, RandomState};
use std::mem;

use super::HashTable;

const BUCKET_SIZE: usize = 16;

struct Bucket<K, V> {
    /// How many first bits of a hash pick this bucket.
    depth: u32,
    /// Each entry with its key's hash.
    entries: Vec<(u64, K, V)>,
}

pub struct Extendible<K, V, S = RandomState> {
    /// Indexes into `buckets`, by the first `global_depth` bits of a hash.
    directory: Vec<usize>,
    buckets: Vec<Bucket<K, V>>,
    global_depth: u32,
    len: usize,
    hasher: S,
}

impl<K, V> Extendible<K, V> {
    pub fn new() -> Self {
        Self::with_hasher(RandomState::new())
    }
}

impl<K, V, S> Extendible<K, V, S> {
    pub fn with_hasher(hasher: S) -> Self {
        let bucket = Bucket { depth: 0, entries: Vec::with_capacity(BUCKET_SIZE) };
        Extendible { directory: vec![0], buckets: vec![bucket], global_depth: 0, len: 0, hasher }
    }

    pub fn global_depth(&self) -> u32 {
        self.global_depth
    }

    pub fn buckets(&self) -> usize {
        self.buckets.len()
    }

    /// The directory entry for a hash: its first `global_depth` bits.
    fn entry(&self, hash: u64) -> usize {
        hash.checked_shr(64 - self.global_depth).unwrap_or(0) as usize
    }

    /// Splits the bucket that `hash` picks in two, doubling the directory first if it must.
    fn split(&mut self, hash: u64) {
        let b = self.directory[self.entry(hash)];
        let depth = self.buckets[b].depth;
        if depth == self.global_depth {
            self.directory = self.directory.iter().flat_map(|&b| [b, b]).collect();
            self.global_depth += 1;
        }

        // The bit after the bucket's first `depth` tells its entries apart.
        let next_bit = |h: u64| (h >> (63 - depth)) & 1 == 1;
        let (moved, kept) = mem::take(&mut self.buckets[b].entries).into_iter().partition(|(h, ..)| next_bit(*h));
        self.buckets[b] = Bucket { depth: depth + 1, entries: kept };
        let new = self.buckets.len();
        self.buckets.push(Bucket { depth: depth + 1, entries: moved });

        // The entries pointing to the bucket are the 2^(global - depth) whose
        // first `depth` bits are the bucket's: a run of them, of which the
        // second half has the next bit set.
        let span = 1 << (self.global_depth - depth);
        let start = self.entry(hash) & !(span - 1);
        for slot in &mut self.directory[start + span / 2..start + span] {
            *slot = new;
        }
    }
}

impl<K, V, S: Default> Default for Extendible<K, V, S> {
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> Extendible<K, V, S> {
    /// The bucket and position in it of the entry for `key`, if any.
    fn find(&self, hash: u64, key: &K) -> Option<(usize, usize)> {
        let b = self.directory[self.entry(hash)];
        self.buckets[b].entries.iter().position(|(h, k, _)| *h == hash && k == key).map(|i| (b, i))
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> HashTable<K, V> for Extendible<K, V, S> {
    fn insert(&mut self, key: K, value: V) -> Option<V> {
        let hash = self.hasher.hash_one(&key);
        if let Some((b, i)) = self.find(hash, &key) {
            return Some(mem::replace(&mut self.buckets[b].entries[i].2, value));
        }
        loop {
            let b = self.directory[self.entry(hash)];
            if self.buckets[b].entries.len() < BUCKET_SIZE {
                self.buckets[b].entries.push((hash, key, value));
                self.len += 1;
                return None;
            }
            self.split(hash);
        }
    }

    fn get(&self, key: &K) -> Option<&V> {
        let (b, i) = self.find(self.hasher.hash_one(key), key)?;
        Some(&self.buckets[b].entries[i].2)
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let (b, i) = self.find(self.hasher.hash_one(key), key)?;
        self.len -= 1;
        Some(self.buckets[b].entries.swap_remove(i).2)
    }

    fn len(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_points_each_hash_to_its_bucket() {
        let mut table = Extendible::new();
        for key in 0..20_000 {
            table.insert(key, ());
        }
        let global = table.global_depth;
        assert!(global > 0);
        assert_eq!(table.directory.len(), 1 << global);
        for (b, bucket) in table.buckets.iter().enumerate() {
            assert!(bucket.depth <= global);
            assert!(bucket.entries.len() <= BUCKET_SIZE);
            let pointers = table.directory.iter().filter(|&&slot| slot == b).count();
            assert_eq!(pointers, 1 << (global - bucket.depth), "bucket {b}");
            for (hash, ..) in &bucket.entries {
                assert_eq!(table.directory[table.entry(*hash)], b);
            }
        }
    }
}
