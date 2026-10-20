//! Times the LSM tree's two compaction strategies: `cargo bench --bench
//! lsm`, or with group names (`cargo bench --bench lsm -- read`) for only
//! those groups. Prints the fastest of a few runs of each case under level
//! and universal compaction, side by side.
//!
//! `write` loads 100,000 entries of an 8-byte key and a 100-byte value, in
//! key order, in no order, and over and over the same 10,000 keys, each into
//! a new tree. Beside the time it prints the write amplification: the bytes
//! of SSTable pages written, by flushes and merges, over the bytes of the
//! entries. `read` looks keys up in a tree loaded in no order, keys it has
//! and keys between them, which only Bloom filters and key ranges rule out,
//! and scans it.
//!
//! The trees live in a buffer pool of 256 frames over a disk in memory,
//! below SQL and its tables: what this times is the tree alone.

mod common;

use std::sync::Arc;
use std::time::Duration;

use sequoia::storage::lsm::tree::Compaction;
use sequoia::storage::{BufferPool, DiskManager, LsmConfig, LsmTree, PAGE_SIZE};

const ENTRIES: u64 = 100_000;
const VALUE: usize = 100;
const LOOKUPS: u64 = 10_000;

fn main() {
    let strategies = [("level", Compaction::Level), ("universal", Compaction::Universal)];
    if common::wanted("write") {
        header("write", &strategies);
        let loads: [(&str, fn(u64) -> u64); 3] = [
            ("100,000 keys in order", |i| i),
            ("100,000 keys in no order", scatter),
            ("10,000 keys, 10 times each", |i| scatter(i) % 10_000),
        ];
        for (name, key) in loads {
            print!("  {name:<34}");
            for (_, compaction) in strategies {
                let (time, written) = common::fastest(|| {
                    let mut tree = tree(compaction);
                    for i in 0..ENTRIES {
                        tree.put(encode(key(i)), value(i)).unwrap();
                    }
                    tree.pages_written()
                });
                let amplification = (written * PAGE_SIZE) as f64 / (ENTRIES as usize * (8 + VALUE)) as f64;
                print!("{:>26}", format!("{}, {amplification:.1}x written", ms(time)));
            }
            println!();
        }
    }
    if common::wanted("read") {
        header("read", &strategies);
        // Even keys only, so that odd ones fall between them.
        let trees: Vec<LsmTree> = strategies
            .iter()
            .map(|&(_, compaction)| {
                let mut tree = tree(compaction);
                for i in 0..ENTRIES {
                    tree.put(encode(scatter(i) * 2), value(i)).unwrap();
                }
                tree
            })
            .collect();
        let cases: [(&str, &dyn Fn(&LsmTree) -> usize); 3] = [
            ("10,000 keys it has", &|tree| (0..LOOKUPS).filter(|&i| tree.get(&encode(scatter(i) * 2)).unwrap().is_some()).count()),
            ("10,000 keys it lacks", &|tree| {
                (0..LOOKUPS).filter(|&i| tree.get(&encode(scatter(i) * 2 + 1)).unwrap().is_some()).count()
            }),
            ("scan every key", &|tree| tree.scan().map(Result::unwrap).count()),
        ];
        for (name, case) in cases {
            print!("  {name:<34}");
            for tree in &trees {
                let (time, found) = common::fastest(|| case(tree));
                print!("{:>26}", format!("{}, {found} found", ms(time)));
            }
            println!();
        }
    }
}

fn header(group: &str, strategies: &[(&str, Compaction)]) {
    print!("\n{group:<36}");
    for (name, _) in strategies {
        print!("{name:>26}");
    }
    println!();
}

fn ms(time: Duration) -> String {
    common::shown(time).trim().to_string()
}

/// A new tree, in a pool of its own, compacted by this strategy.
fn tree(compaction: Compaction) -> LsmTree {
    let pool = Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 256));
    LsmTree::create(pool, LsmConfig { compaction, ..LsmConfig::default() }).unwrap()
}

/// Key `i` of ENTRIES, in no order: `i * m mod p` for a prime `p` above them.
fn scatter(i: u64) -> u64 {
    i * 48_271 % 1_000_003
}

/// A key as bytes that sort as the number does.
fn encode(key: u64) -> Vec<u8> {
    key.to_be_bytes().to_vec()
}

fn value(i: u64) -> Vec<u8> {
    let mut value = format!("value {i} ").into_bytes();
    value.resize(VALUE, b'.');
    value
}
