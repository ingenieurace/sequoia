//! Hash files: byte-string entries kept in buckets of pages, found by a hash
//! of their key, which indexes `USING HASH` are built on.
//!
//! A B+ tree finds a key by going down its levels, comparing keys on each;
//! a hash file goes from its root page straight to the one bucket the key's
//! hash picks, and compares only 32-bit hashes until one matches. It finds
//! only entries whose key equals one given: it has no order, so no ranges.
//!
//! An entry is any byte string that starts with its key, and is hashed by
//! that key alone, so the entries of equal keys share a bucket (an index's
//! entries are its keys followed by a row id). Entries are not checked for
//! being there already: the caller keeps them distinct.
//!
//! Two schemes, both made to grow one bucket at a time instead of hashing
//! every entry again into a table twice the size, as in-memory tables such
//! as `HashMap` do:
//!
//! - [`Extendible`]: a directory, indexed by the low bits of a hash, points
//!   to buckets, several slots to one; a full bucket splits in two, and only
//!   when no slot is left to split it by does the directory double, which
//!   copies page ids, not entries.
//! - [`Linear`]: buckets split in turn, one each time the file's entries
//!   outgrow a set load, whichever bucket overflowed, so no directory is
//!   needed: a hash's low bits name its bucket, one more of them for the
//!   buckets already split this round.
//!
//! [`HASH_SCHEME`] picks the scheme of new indexes; a file's root page says
//! which it was made with, so files of both open either way.
//!
//! A bucket is a chain of pages (see `bucket.rs`): entries of keys so
//! frequent that they fill a page, which splitting cannot separate, go on in
//! overflow pages. So a hash index suits keys with few rows each; one on a
//! key with many rows per value searches long chains to delete.
//!
//! **Concurrency.** Any number of threads may find, insert and delete at
//! once, by latch crabbing as in the B+ tree: the root page's latch, then
//! the bucket's first page's, which guards its whole chain, released in the
//! order taken. Lookups read-latch both; an extendible file's inserts and
//! deletes write-latch only the bucket unless they split or merge one,
//! which they do again with the root write-latched. A linear file counts
//! its entries' bytes in its root, so each of its changes write-latches it.

mod bucket;
mod directory;
mod extendible;
mod linear;

pub use extendible::Extendible;
pub use linear::Linear;

use std::sync::Arc;

use crate::error::{Result, err};
use crate::storage::buffer::pool::BufferPool;
use crate::storage::io::disk::PageId;

/// The scheme of the hash files indexes `USING HASH` make. Edit this line to
/// switch, like `MEMTABLE` in `storage/lsm/memtable.rs`; files made with
/// the other one still open.
pub const HASH_SCHEME: HashScheme = HashScheme::Extendible;

/// The longest entry a hash file takes: as long as a B+ tree's key.
pub const MAX_ENTRY_SIZE: usize = crate::storage::btree::MAX_KEY_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashScheme {
    Extendible,
    Linear,
}

impl HashScheme {
    pub const ALL: [HashScheme; 2] = [HashScheme::Extendible, HashScheme::Linear];
}

/// The first byte of each scheme's root page.
const EXTENDIBLE: u8 = b'E';
const LINEAR: u8 = b'L';

pub enum HashFile {
    Extendible(Extendible),
    Linear(Linear),
}

impl HashFile {
    /// Creates an empty file of this scheme.
    pub fn create(pool: Arc<BufferPool>, scheme: HashScheme) -> Result<Self> {
        Ok(match scheme {
            HashScheme::Extendible => HashFile::Extendible(Extendible::create(pool)?),
            HashScheme::Linear => HashFile::Linear(Linear::create(pool)?),
        })
    }

    /// Opens the file whose root page is `root`, of whichever scheme it was made with.
    pub fn open(pool: Arc<BufferPool>, root: PageId) -> Result<Self> {
        let kind = pool.fetch_read(root)?[0];
        Ok(match kind {
            EXTENDIBLE => HashFile::Extendible(Extendible::open(pool, root)),
            LINEAR => HashFile::Linear(Linear::open(pool, root)),
            _ => return err(format!("corrupted hash file: root page {} has kind {kind}", root.0)),
        })
    }

    pub fn scheme(&self) -> HashScheme {
        match self {
            HashFile::Extendible(_) => HashScheme::Extendible,
            HashFile::Linear(_) => HashScheme::Linear,
        }
    }

    pub fn root_page(&self) -> PageId {
        match self {
            HashFile::Extendible(file) => file.root_page(),
            HashFile::Linear(file) => file.root_page(),
        }
    }

    /// Adds an entry whose key hashes to `hash`.
    pub fn insert(&self, hash: u32, entry: &[u8]) -> Result<()> {
        if entry.len() > MAX_ENTRY_SIZE {
            return err(format!("index entry is {} bytes, the maximum is {MAX_ENTRY_SIZE}", entry.len()));
        }
        match self {
            HashFile::Extendible(file) => file.insert(hash, entry),
            HashFile::Linear(file) => file.insert(hash, entry),
        }
    }

    /// Removes an entry whose key hashes to `hash`. Returns false if it was not there.
    pub fn delete(&self, hash: u32, entry: &[u8]) -> Result<bool> {
        match self {
            HashFile::Extendible(file) => file.delete(hash, entry),
            HashFile::Linear(file) => file.delete(hash, entry),
        }
    }

    /// The entries that start with `key`, which hashes to `hash`, in no order.
    pub fn find(&self, hash: u32, key: &[u8]) -> Result<Vec<Vec<u8>>> {
        match self {
            HashFile::Extendible(file) => file.find(hash, key),
            HashFile::Linear(file) => file.find(hash, key),
        }
    }

    /// Returns all of the file's pages to the free list.
    pub fn destroy(self) -> Result<()> {
        match self {
            HashFile::Extendible(file) => file.destroy(),
            HashFile::Linear(file) => file.destroy(),
        }
    }
}

/// The hash of a key, as hash files store it. It is written to disk with
/// every entry, so it must never change: unlike `RandomState`, it has no
/// seed, and it is defined here, not by the standard library.
///
/// Eight bytes at a time are mixed into the state by a multiplication, and
/// the result finished by MurmurHash3's 64-bit finalizer, so that every bit
/// of the key moves the low bits, which pick a bucket.
pub fn hash(key: &[u8]) -> u32 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = (key.len() as u64).wrapping_mul(K);
    let (chunks, rest) = key.as_chunks::<8>();
    for chunk in chunks {
        h = (h ^ u64::from_le_bytes(*chunk)).wrapping_mul(K).rotate_left(31);
    }
    if !rest.is_empty() {
        let mut word = [0; 8];
        word[..rest.len()].copy_from_slice(rest);
        h = (h ^ u64::from_le_bytes(word)).wrapping_mul(K).rotate_left(31);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^= h >> 33;
    h as u32
}

/// A mask of the low `bits` bits.
fn mask(bits: u32) -> u32 {
    if bits >= 32 { u32::MAX } else { (1 << bits) - 1 }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use super::*;
    use crate::storage::DiskManager;

    pub(super) fn pool(frames: usize) -> Arc<BufferPool> {
        Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), frames))
    }

    /// An entry: a key of `key`'s 4 bytes and some padding, then `id`.
    pub(super) fn entry(key: u32, id: u32) -> (u32, Vec<u8>, Vec<u8>) {
        let mut k = key.to_be_bytes().to_vec();
        k.resize(4 + (key % 40) as usize, b'.');
        let mut e = k.clone();
        e.extend_from_slice(&id.to_be_bytes());
        (hash(&k), k, e)
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn sorted(mut entries: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        entries.sort();
        entries
    }

    #[test]
    fn hashes_are_fixed_and_spread() {
        // Stored on disk: these must never change.
        assert_eq!((hash(b"sequoia"), hash(b"a key nine bytes or more")), (149_225_573, 3_947_121_480));
        assert_eq!(hash(b"a"), hash(b"a"));
        assert_ne!(hash(b"a"), hash(b"a\0"), "the length counts");
        // Consecutive integer keys spread evenly over 64 buckets.
        let mut buckets = [0; 64];
        for i in 0..64_000u64 {
            buckets[(hash(&i.to_be_bytes()) & 63) as usize] += 1;
        }
        assert!(buckets.iter().all(|&n| (800..1200).contains(&n)), "{buckets:?}");
    }

    /// Random inserts, deletes and lookups on a file of each scheme, checked
    /// against a map of keys to their entries, with keys from few to many
    /// entries each.
    #[test]
    fn random_changes_match_a_map() {
        for scheme in HashScheme::ALL {
            for keys in [5, 300, 5000] {
                let pool = pool(64);
                let file = HashFile::create(pool.clone(), scheme).unwrap();
                let mut model: HashMap<Vec<u8>, BTreeSet<Vec<u8>>> = HashMap::new();
                let mut rng = Rng(keys + 7);
                for step in 0..12_000 {
                    let (h, k, e) = entry(rng.below(keys) as u32, rng.below(40) as u32);
                    let present = model.get(&k).is_some_and(|set| set.contains(&e));
                    if rng.below(3) > 0 {
                        if !present {
                            file.insert(h, &e).unwrap();
                            model.entry(k.clone()).or_default().insert(e);
                        }
                    } else {
                        assert_eq!(file.delete(h, &e).unwrap(), present, "{scheme:?} step {step}");
                        if present {
                            model.get_mut(&k).unwrap().remove(&e);
                        }
                    }
                    if step % 7 == 0 {
                        let want: Vec<Vec<u8>> = model.get(&k).map_or(vec![], |set| set.iter().cloned().collect());
                        assert_eq!(sorted(file.find(h, &k).unwrap()), want, "{scheme:?} {keys} keys, step {step}");
                    }
                }
                for (k, set) in &model {
                    let want: Vec<Vec<u8>> = set.iter().cloned().collect();
                    assert_eq!(sorted(file.find(hash(k), k).unwrap()), want);
                }
            }
        }
    }

    #[test]
    fn grows_and_shrinks_back_freeing_its_pages() {
        for scheme in HashScheme::ALL {
            let pool = pool(32);
            let empty = pool.disk().page_count();
            let file = HashFile::create(pool.clone(), scheme).unwrap();
            let start = pool.disk().page_count() - empty;
            let entries: Vec<_> = (0..20_000).map(|i| entry(i, i)).collect();
            for (h, _, e) in &entries {
                file.insert(*h, e).unwrap();
            }
            assert!(pool.disk().page_count() > 170, "{scheme:?} grew");
            for (h, k, e) in &entries {
                assert_eq!(file.find(*h, k).unwrap(), std::slice::from_ref(e));
            }
            for (h, _, e) in &entries {
                assert!(file.delete(*h, e).unwrap());
            }
            let total = (pool.disk().page_count() - empty) as usize;
            let used = total - free_pages(&pool);
            assert!(used <= start as usize + 2, "{scheme:?}: {used} pages still in use, {start} at the start");
            file.destroy().unwrap();
            // Counting free pages adds one to the file, which it frees again.
            let total = (pool.disk().page_count() - empty) as usize;
            assert_eq!(free_pages(&pool), total, "{scheme:?}");
        }
    }

    /// The pages on the free list, found by allocating them all.
    fn free_pages(pool: &BufferPool) -> usize {
        let count = pool.disk().page_count();
        let mut taken = Vec::new();
        loop {
            let id = pool.disk().allocate_page().unwrap();
            if id.0 >= count {
                pool.disk().deallocate_page(id).unwrap();
                break;
            }
            taken.push(id);
        }
        let n = taken.len();
        for id in taken {
            pool.disk().deallocate_page(id).unwrap();
        }
        n
    }

    #[test]
    fn many_entries_of_one_key_overflow_and_drain() {
        for scheme in HashScheme::ALL {
            let pool = pool(16);
            let file = HashFile::create(pool.clone(), scheme).unwrap();
            let (h, k, _) = entry(42, 0);
            let others: Vec<_> = (100..400).map(|i| entry(i, 0)).collect();
            for id in 0..3000u32 {
                file.insert(h, &[k.as_slice(), &id.to_be_bytes()].concat()).unwrap();
            }
            for (h, _, e) in &others {
                file.insert(*h, e).unwrap();
            }
            assert_eq!(file.find(h, &k).unwrap().len(), 3000);
            for (h, k, e) in &others {
                assert_eq!(file.find(*h, k).unwrap(), std::slice::from_ref(e), "{scheme:?}");
            }
            for id in (0..3000u32).rev() {
                assert!(file.delete(h, &[k.as_slice(), &id.to_be_bytes()].concat()).unwrap());
            }
            assert!(file.find(h, &k).unwrap().is_empty());
            assert_eq!(file.find(others[7].0, &others[7].1).unwrap().len(), 1);
        }
    }

    #[test]
    fn reopens_from_its_root_in_its_own_scheme() {
        for scheme in HashScheme::ALL {
            let pool = pool(16);
            let file = HashFile::create(pool.clone(), scheme).unwrap();
            for i in 0..2000 {
                let (h, _, e) = entry(i, i);
                file.insert(h, &e).unwrap();
            }
            let root = file.root_page();
            drop(file);
            pool.flush_all().unwrap();
            let file = HashFile::open(pool.clone(), root).unwrap();
            assert_eq!(file.scheme(), scheme);
            for i in 0..2000 {
                let (h, k, e) = entry(i, i);
                assert_eq!(file.find(h, &k).unwrap(), [e]);
            }
        }
    }

    #[test]
    fn concurrent_inserts_deletes_and_finds_keep_every_entry() {
        for scheme in HashScheme::ALL {
            let pool = pool(64);
            let file = Arc::new(HashFile::create(pool, scheme).unwrap());
            // Entries every thread leaves alone, found throughout.
            for i in 0..500 {
                let (h, _, e) = entry(i * 8, 0);
                file.insert(h, &e).unwrap();
            }
            let threads: Vec<_> = (0..4u32)
                .map(|t| {
                    let file = file.clone();
                    std::thread::spawn(move || {
                        for round in 0..3 {
                            let mine: Vec<_> = (0..1500).map(|i| entry(i * 8 + 1 + t, round)).collect();
                            for (h, _, e) in &mine {
                                file.insert(*h, e).unwrap();
                            }
                            for (i, (h, k, e)) in mine.iter().enumerate() {
                                assert_eq!(file.find(*h, k).unwrap(), std::slice::from_ref(e));
                                let (h, k, e) = entry(i as u32 % 500 * 8, 0);
                                assert_eq!(file.find(h, &k).unwrap(), [e]);
                            }
                            for (h, _, e) in &mine {
                                assert!(file.delete(*h, e).unwrap());
                            }
                        }
                    })
                })
                .collect();
            for thread in threads {
                thread.join().unwrap();
            }
            for i in 0..500 {
                let (h, k, e) = entry(i * 8, 0);
                assert_eq!(file.find(h, &k).unwrap(), [e]);
            }
            for i in 0..1500 {
                let (h, k, _) = entry(i * 8 + 1, 0);
                assert!(file.find(h, &k).unwrap().is_empty());
            }
        }
    }
}
