//! Times the tries of `sequoia::trie` against a `BTreeMap` and the LSM
//! tree's skip list: `cargo bench --bench trie`, or with group names
//! (`cargo bench --bench trie -- texts lsm`) for only those groups.
//!
//! - `ints`: 200,000 random 8-byte keys, which share little but their
//!   first bytes, as an LSM table's INT keys are.
//! - `texts`: 200,000 keys like `user/004217/email`, which share long
//!   prefixes.
//!
//! For each it times inserting every key into an empty map, looking every
//! key up (`hit`) and as many it does not have (`miss`), reading every
//! entry in order (`scan`), and 20,000 reads of the next 10 entries from a
//! key (`seek`), as an LSM scan from a key does; and it prints the memory
//! the map took, counted by the allocator.
//!
//! `lsm` loads 200,000 entries into an LSM tree with each kind of MemTable,
//! then looks them all up: with the default 64 KB log, flushed every few
//! hundred entries, and with an 8 MB one, which keeps tens of thousands.
//! Each prints the fastest of a few runs.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sequoia::storage::lsm::memtable::MemTableKind;
use sequoia::storage::lsm::skiplist::SkipList;
use sequoia::storage::{BufferPool, DiskManager, LsmConfig, LsmTree};
use sequoia::trie::{Art, RadixTree, SortedMap, Trie};

/// The system allocator, counting the bytes in use.
struct Counting;

static IN_USE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        IN_USE.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const KEYS: usize = 200_000;
const SEEKS: usize = 20_000;
const RUNS: usize = 3;
const MAPS: [&str; 5] = ["BTreeMap", "SkipList", "Trie", "RadixTree", "ART"];
const ROWS: [&str; 6] = ["insert", "hit", "miss", "scan", "seek", "memory"];

fn main() {
    let ints = |seed| random(seed).take(KEYS).map(|x| x.to_be_bytes().to_vec()).collect::<Vec<_>>();
    let fields = ["email", "name", "phone", "address", "created"];
    let texts = |seed| {
        random(seed)
            .take(KEYS)
            .map(|x| format!("user/{:06}/{}", x % 1_000_000, fields[(x >> 32) as usize % 5]).into_bytes())
            .collect::<Vec<_>>()
    };
    for (group, keys, absent) in [("ints", ints(1), ints(2)), ("texts", texts(1), texts(2))] {
        if !common::wanted(group) {
            continue;
        }
        let columns = [
            measure::<BTreeMap<Vec<u8>, u64>>(&keys, &absent),
            measure::<SkipList<Vec<u8>, u64>>(&keys, &absent),
            measure::<Trie<u64>>(&keys, &absent),
            measure::<RadixTree<u64>>(&keys, &absent),
            measure::<Art<u64>>(&keys, &absent),
        ];
        let length = keys.iter().map(Vec::len).sum::<usize>() as f64 / keys.len() as f64;
        print!("\n{group:<12}");
        MAPS.iter().for_each(|name| print!("{name:>13}"));
        println!("    ({KEYS} keys of {length:.1} bytes on average)");
        for (row, name) in ROWS.iter().enumerate() {
            print!("  {name:<10}");
            for column in &columns {
                let cell = match row {
                    5 => format!("{:.1} MB", column.memory as f64 / 1e6),
                    _ => common::shown(column.times[row]).trim().to_string(),
                };
                print!("{cell:>13}");
            }
            println!();
        }
    }
    if common::wanted("lsm") {
        lsm();
    }
}

struct Column {
    /// insert, hit, miss, scan, seek
    times: [Duration; 5],
    memory: usize,
}

fn measure<M: SortedMap<u64> + Default>(keys: &[Vec<u8>], absent: &[Vec<u8>]) -> Column {
    let filled = || {
        let mut map = M::default();
        for (i, key) in keys.iter().enumerate() {
            map.insert(key.clone(), i as u64);
        }
        map
    };
    let insert = fastest(|| {
        black_box(filled());
    });
    let before = IN_USE.load(Ordering::Relaxed);
    let map = filled();
    let memory = IN_USE.load(Ordering::Relaxed) - before;
    // The keys themselves were cloned into the map: count only what holds them.
    let memory = memory.saturating_sub(keys.iter().map(Vec::len).sum());

    let hit = fastest(|| keys.iter().for_each(|k| _ = black_box(map.get(k))));
    let miss = fastest(|| absent.iter().for_each(|k| _ = black_box(map.get(k))));
    let scan = fastest(|| _ = black_box(map.iter_from(&[]).count()));
    let seek = fastest(|| {
        for key in absent.iter().take(SEEKS) {
            black_box(map.iter_from(key).take(10).count());
        }
    });
    Column { times: [insert, hit, miss, scan, seek], memory }
}

/// Loads an LSM tree with each kind of MemTable, then looks every key up.
fn lsm() {
    println!("\n{:<26}{:>16}{:>16}", "lsm: 200000 entries", "load", "get");
    for log_bytes in [64 << 10, 8 << 20] {
        for kind in MemTableKind::ALL {
            let config = LsmConfig { log_bytes, memtable: kind, ..LsmConfig::default() };
            let keys: Vec<Vec<u8>> = random(3).take(KEYS).map(|x| x.to_be_bytes().to_vec()).collect();
            let load = || {
                let pool = Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 256));
                let mut tree = LsmTree::create(pool, config).unwrap();
                for key in &keys {
                    tree.put(key.clone(), vec![7; 100]).unwrap();
                }
                tree
            };
            let load_time = fastest(|| _ = black_box(load()));
            let tree = load();
            let get_time = fastest(|| keys.iter().for_each(|k| _ = black_box(tree.get(k).unwrap())));
            let label = format!("{} KB log, {kind:?}", log_bytes >> 10);
            println!("  {label:<24}{:>16}{:>16}", common::shown(load_time).trim(), common::shown(get_time).trim());
        }
    }
}

fn fastest(mut f: impl FnMut()) -> Duration {
    (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .min()
        .expect("RUNS > 0")
}

/// Pseudo-random numbers, the same each run for the same seed (xorshift).
fn random(seed: u64) -> impl Iterator<Item = u64> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    std::iter::repeat_with(move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    })
}
