//! Times the hash files of `sequoia::storage::hash` (extendible and linear
//! hashing) against a B+ tree: `cargo bench --bench hash`, or with group
//! names (`cargo bench --bench hash -- hit miss`) for only those groups.
//!
//! Each holds 200,000 index entries (an 8-byte key, then an 8-byte id) in
//! pages, through a buffer pool of 256 frames, as the database has.
//!
//! - `insert`: every entry into an empty one, which grows as it goes.
//! - `hit`: finds every key, in another order.
//! - `miss`: looks up as many keys it does not have.
//! - `delete`: deletes every entry, in another order.
//!
//! Each prints the fastest of a few runs.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use sequoia::storage::hash::{self, HashFile, HashScheme};
use sequoia::storage::{BPlusTree, BufferPool, DiskManager};

const KEYS: usize = 200_000;
const RUNS: usize = 5;
const GROUPS: [&str; 4] = ["insert", "hit", "miss", "delete"];
/// A B+ tree, then a hash file of each scheme.
const STRUCTURES: [Option<HashScheme>; 3] = [None, Some(HashScheme::Extendible), Some(HashScheme::Linear)];

fn main() {
    let present = random(1, KEYS);
    let shuffled = shuffled(&present);
    let absent = random(2, KEYS);
    let filled = |scheme| {
        let paged = Paged::new(scheme);
        for &key in &present {
            paged.insert(key);
        }
        paged
    };
    let full: Vec<Paged> = STRUCTURES.iter().map(|&scheme| filled(scheme)).collect();

    println!("{:<12}{:>13}{:>13}{:>13}", format!("{KEYS} entries"), "B+ tree", "Extendible", "Linear");
    for group in GROUPS.into_iter().filter(|group| common::wanted(group)) {
        print!("  {group:<10}");
        for (i, &scheme) in STRUCTURES.iter().enumerate() {
            let time = match group {
                "insert" => fastest(|| Paged::new(scheme), |paged| present.iter().for_each(|&k| paged.insert(k))),
                "hit" => fastest(|| (), |_| shuffled.iter().for_each(|&k| assert_eq!(full[i].find(k), 1))),
                "miss" => fastest(|| (), |_| absent.iter().for_each(|&k| assert_eq!(full[i].find(k), 0))),
                "delete" => fastest(|| filled(scheme), |paged| shuffled.iter().for_each(|&k| paged.delete(k))),
                _ => unreachable!("a group of GROUPS"),
            };
            print!("{:>13}", common::shown(time).trim());
        }
        println!();
    }
}

/// A B+ tree or a hash file, through a pool of its own.
enum Paged {
    Tree(BPlusTree),
    Hash(HashFile),
}

impl Paged {
    fn new(scheme: Option<HashScheme>) -> Paged {
        let pool = Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 256));
        match scheme {
            None => Paged::Tree(BPlusTree::create(pool).unwrap()),
            Some(scheme) => Paged::Hash(HashFile::create(pool, scheme).unwrap()),
        }
    }

    fn insert(&self, key: u64) {
        match self {
            Paged::Tree(tree) => _ = tree.insert(&entry(key)).unwrap(),
            Paged::Hash(file) => file.insert(hash::hash(&key.to_be_bytes()), &entry(key)).unwrap(),
        }
    }

    /// How many entries have this key.
    fn find(&self, key: u64) -> usize {
        let key = key.to_be_bytes();
        match self {
            Paged::Tree(tree) => tree.range(&key).take_while(|e| e.as_ref().unwrap().starts_with(&key)).count(),
            Paged::Hash(file) => file.find(hash::hash(&key), &key).unwrap().len(),
        }
    }

    fn delete(&self, key: u64) {
        match self {
            Paged::Tree(tree) => _ = tree.delete(&entry(key)).unwrap(),
            Paged::Hash(file) => _ = file.delete(hash::hash(&key.to_be_bytes()), &entry(key)).unwrap(),
        }
    }
}

/// An index entry for `key`: the key, then a row id.
fn entry(key: u64) -> [u8; 16] {
    let mut entry = [0; 16];
    entry[..8].copy_from_slice(&key.to_be_bytes());
    entry[8..].copy_from_slice(&key.rotate_left(17).to_le_bytes());
    entry
}

/// The fastest of RUNS runs of `run` on what `setup` makes, timing only `run`.
fn fastest<T>(mut setup: impl FnMut() -> T, mut run: impl FnMut(&mut T)) -> Duration {
    (0..RUNS)
        .map(|_| {
            let mut input = setup();
            let start = Instant::now();
            run(&mut input);
            start.elapsed()
        })
        .min()
        .expect("RUNS > 0")
}

/// `n` distinct pseudo-random keys, the same each run for the same `seed`
/// (xorshift: a full-period permutation, so no key repeats within a sequence).
fn random(seed: u64, n: usize) -> Vec<u64> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        })
        .collect()
}

/// `keys` in another order (Fisher-Yates, with a fixed seed).
fn shuffled(keys: &[u64]) -> Vec<u64> {
    let mut keys = keys.to_vec();
    let mut r = random(3, keys.len());
    for i in (1..keys.len()).rev() {
        let j = (r.pop().unwrap() % (i as u64 + 1)) as usize;
        keys.swap(i, j);
    }
    keys
}
