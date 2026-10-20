//! Times the five hash tables of `sequoia::hash` against
//! `std::collections::HashMap`: `cargo bench --bench hash`, or with group
//! names (`cargo bench --bench hash -- hit miss`) for only those groups.
//!
//! Each table maps 1,000,000 u64 keys, in no order, to u64 values. All
//! hash with SipHash (`RandomState`), as `HashMap` does by default, so the
//! times compare the tables, not the hash function.
//!
//! - `insert`: every key into an empty table, which grows as it goes.
//! - `hit`: looks every key up, in another order.
//! - `miss`: looks up as many keys the table does not have.
//! - `remove`: removes every key, in another order.
//! - `churn`: removes half the keys and inserts as many new ones, then looks
//!   up all of both: lookups after removes, past tombstones for linear probing.
//! - `load`: linear probing alone, filled to just under max loads of 50%,
//!   70% (the default) and 90%: how much longer walks get as it fills up.
//! - `pages`: the hash files of `sequoia::storage::hash` (extendible and
//!   linear hashing), against a B+ tree, on 200,000 index entries (an
//!   8-byte key, then an 8-byte id) in pages, through a buffer pool of 256
//!   frames, as the database has: inserted, found, missed and deleted.
//!
//! Each prints the fastest of a few runs.

mod common;

use std::collections::HashMap;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sequoia::hash::{Chained, Cuckoo, Extendible, HashTable, LinearHash, LinearProbe};
use sequoia::storage::hash::{self as disk_hash, HashFile, HashScheme};
use sequoia::storage::{BPlusTree, BufferPool, DiskManager};

const KEYS: usize = 1_000_000;
const RUNS: usize = 5;
const GROUPS: [&str; 5] = ["insert", "hit", "miss", "remove", "churn"];

struct Keys {
    present: Vec<u64>,
    /// `present` in another order.
    shuffled: Vec<u64>,
    absent: Vec<u64>,
}

fn main() {
    let keys = Keys { present: random(1, KEYS), shuffled: shuffled(&random(1, KEYS)), absent: random(2, KEYS) };

    let groups: Vec<&str> = GROUPS.into_iter().filter(|group| common::wanted(group)).collect();
    if !groups.is_empty() {
        let columns: Vec<(&str, Vec<Duration>)> = vec![
            ("HashMap", times(HashMap::<u64, u64>::new, &keys, &groups)),
            ("LinearProbe", times(LinearProbe::<u64, u64>::new, &keys, &groups)),
            ("Cuckoo", times(Cuckoo::<u64, u64>::new, &keys, &groups)),
            ("Chained", times(Chained::<u64, u64>::new, &keys, &groups)),
            ("Extendible", times(Extendible::<u64, u64>::new, &keys, &groups)),
            ("LinearHash", times(LinearHash::<u64, u64>::new, &keys, &groups)),
        ];
        print!("{:<12}", format!("{KEYS} keys"));
        for (name, _) in &columns {
            print!("{name:>13}");
        }
        println!();
        for (row, group) in groups.iter().enumerate() {
            print!("  {group:<10}");
            for (_, times) in &columns {
                print!("{:>13}", common::shown(times[row]).trim());
            }
            println!();
        }
    }

    if common::wanted("load") {
        load(&keys);
    }
    if common::wanted("pages") {
        pages(&keys);
    }
}

/// Entries on pages: in a B+ tree, and in a hash file of each scheme.
const PAGE_KEYS: usize = 200_000;

/// What the `pages` group times: a B+ tree or a hash file, through a pool.
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
            Paged::Hash(file) => file.insert(disk_hash::hash(&key.to_be_bytes()), &entry(key)).unwrap(),
        }
    }

    fn find(&self, key: u64) -> usize {
        let key = key.to_be_bytes();
        match self {
            Paged::Tree(tree) => tree.range(&key).take_while(|e| e.as_ref().unwrap().starts_with(&key)).count(),
            Paged::Hash(file) => file.find(disk_hash::hash(&key), &key).unwrap().len(),
        }
    }

    fn delete(&self, key: u64) {
        match self {
            Paged::Tree(tree) => _ = tree.delete(&entry(key)).unwrap(),
            Paged::Hash(file) => _ = file.delete(disk_hash::hash(&key.to_be_bytes()), &entry(key)).unwrap(),
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

fn pages(keys: &Keys) {
    let present = &keys.present[..PAGE_KEYS];
    let shuffled = shuffled(present);
    let absent = &keys.absent[..PAGE_KEYS];
    let filled = |scheme| {
        let paged = Paged::new(scheme);
        for &key in present {
            paged.insert(key);
        }
        paged
    };
    println!("\n{:<12}{:>13}{:>13}{:>13}", format!("{PAGE_KEYS} entries"), "B+ tree", "Extendible", "Linear");
    let schemes = [None, Some(HashScheme::Extendible), Some(HashScheme::Linear)];
    let full: Vec<Paged> = schemes.iter().map(|&scheme| filled(scheme)).collect();
    // Times a group on the scheme of each column, by its number.
    type Timer<'a> = Box<dyn Fn(usize) -> Duration + 'a>;
    let rows: [(&str, Timer); 4] = [
        ("insert", Box::new(|i| fastest(|| Paged::new(schemes[i]), |paged| present.iter().for_each(|&k| paged.insert(k))))),
        ("hit", Box::new(|i| fastest(|| (), |_| shuffled.iter().for_each(|&k| assert_eq!(full[i].find(k), 1))))),
        ("miss", Box::new(|i| fastest(|| (), |_| absent.iter().for_each(|&k| assert_eq!(full[i].find(k), 0))))),
        ("delete", Box::new(|i| fastest(|| filled(schemes[i]), |paged| shuffled.iter().for_each(|&k| paged.delete(k))))),
    ];
    for (name, time) in rows {
        print!("  {name:<10}");
        for i in 0..schemes.len() {
            print!("{:>13}", common::shown(time(i)).trim());
        }
        println!();
    }
}

/// The time of each group on tables made by `new`.
fn times<T: HashTable<u64, u64>>(new: impl Fn() -> T, keys: &Keys, groups: &[&str]) -> Vec<Duration> {
    let filled = || {
        let mut table = new();
        for &key in &keys.present {
            table.insert(key, key);
        }
        table
    };
    let full = filled();
    groups
        .iter()
        .map(|&group| match group {
            "insert" => fastest(&new, |table| {
                for &key in &keys.present {
                    table.insert(key, key);
                }
            }),
            "hit" => fastest(|| (), |_| lookups(&full, &keys.shuffled)),
            "miss" => fastest(|| (), |_| lookups(&full, &keys.absent)),
            "remove" => fastest(filled, |table| {
                for key in &keys.shuffled {
                    black_box(table.remove(key));
                }
            }),
            "churn" => fastest(filled, |table| {
                let half = KEYS / 2;
                for key in &keys.shuffled[..half] {
                    table.remove(key);
                }
                for &key in &keys.absent[..half] {
                    table.insert(key, key);
                }
                lookups(&*table, &keys.present);
                lookups(&*table, &keys.absent[..half]);
            }),
            _ => unreachable!("a group of GROUPS"),
        })
        .collect()
}

/// Linear probing filled to just under each max load, then looked up.
fn load(keys: &Keys) {
    println!("\nload: LinearProbe filled to near its max load {:>10}{:>13}", "hit", "miss");
    for max_load in [50, 70, 90] {
        let mut table = LinearProbe::with_max_load(max_load);
        // Past 2^20 slots, the next insert that would make it grow is where to stop.
        let mut inserted = Vec::new();
        while table.slots() < 1 << 20 || (table.len() + 1) * 100 <= table.slots() * max_load {
            let key = keys.present.get(inserted.len()).copied().unwrap_or(inserted.len() as u64);
            table.insert(key, key);
            inserted.push(key);
        }
        // KEYS lookups of each kind, as the other groups make. The hits are
        // spread over every key inserted: keys inserted early, into an emptier
        // table, sit nearer their home slot than later ones.
        let hit: Vec<u64> = (0..KEYS).map(|i| inserted[i * inserted.len() / KEYS]).collect();
        let miss = fastest(|| (), |_| lookups(&table, &keys.absent));
        let hit = fastest(|| (), |_| lookups(&table, &hit));
        let load = 100.0 * table.len() as f64 / table.slots() as f64;
        print!("  max {max_load}%: {} keys in {} slots ({load:.1}% full)", table.len(), table.slots());
        println!("{:>13}{:>13}", common::shown(hit).trim(), common::shown(miss).trim());
    }
}

fn lookups(table: &impl HashTable<u64, u64>, keys: &[u64]) {
    for key in keys {
        black_box(table.get(key));
    }
}

/// The fastest of RUNS runs of `run` on what `setup` makes, timing only `run`.
fn fastest<T>(mut setup: impl FnMut() -> T, mut run: impl FnMut(&mut T)) -> Duration {
    (0..RUNS)
        .map(|_| {
            let mut input = setup();
            let start = Instant::now();
            run(&mut input);
            let time = start.elapsed();
            drop(black_box(input));
            time
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
