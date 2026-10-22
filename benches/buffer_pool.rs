//! Times the buffer pool's page replacement policies: `cargo bench --bench
//! buffer_pool`, or with pattern names (`cargo bench --bench buffer_pool --
//! loop scans`) for only those patterns.
//!
//! Pages are read through a pool of 256 frames, as a database's is, in
//! patterns of 200,000 reads over 2,048 pages, eight times what the pool
//! holds, under each policy: ARC (the default), LRU, LRU-2 and CLOCK. Each
//! pattern runs once to fill the pool, and then again, timed; for each it
//! prints the share of reads that found their page in the pool, and the
//! fastest of a few runs.
//!
//! The pages are on a disk in memory, so a miss costs a copy of 4 KB rather
//! than a read from a file: the times show what a policy itself costs, and
//! the hit rates what it would save.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sequoia::storage::{BufferPool, DiskManager, PageId, Policy, Replacer};

const FRAMES: usize = 256;
const PAGES: usize = 2_048;
const READS: usize = 200_000;

fn main() {
    let patterns: Vec<(&str, &str, Vec<usize>)> = vec![
        (
            "sequential",
            "every page in order, again and again",
            (0..READS).map(|i| i % PAGES).collect(),
        ),
        // A little more than the pool: LRU evicts each page just before it is read again.
        (
            "loop",
            "300 pages in order, again and again",
            (0..READS).map(|i| i % 300).collect(),
        ),
        (
            "uniform",
            "pages at random",
            random(READS).map(|r| r % PAGES).collect(),
        ),
        ("skewed", "90% of reads to 200 pages", skewed()),
        ("scans", "128 hot pages, between scans", scans()),
    ];
    let policies = [
        ("ARC", Policy::Arc),
        ("LRU", Policy::LruK(1)),
        ("LRU-2", Policy::LruK(2)),
        ("CLOCK", Policy::Clock),
    ];

    let disk = Arc::new(DiskManager::in_memory());
    let pages = make_pages(&disk);
    print!(
        "{:<52}",
        format!("{READS} reads of {PAGES} pages, {FRAMES} frames")
    );
    for (name, _) in &policies {
        print!("{name:>18}");
    }
    println!();
    for (pattern, about, reads) in &patterns {
        if !common::wanted(pattern) {
            continue;
        }
        print!("  {:<50}", format!("{pattern}: {about}"));
        for (_, policy) in &policies {
            let misses = Arc::new(AtomicUsize::new(0));
            let replacer = Counting {
                inner: policy.replacer(FRAMES),
                misses: misses.clone(),
            };
            let pool = BufferPool::with_replacer(disk.clone(), FRAMES, Box::new(replacer));
            read(&pool, &pages, reads);
            let (time, misses) = common::fastest(|| {
                misses.store(0, Ordering::Relaxed);
                read(&pool, &pages, reads);
                misses.load(Ordering::Relaxed)
            });
            let hits = 100.0 * (1.0 - misses as f64 / reads.len() as f64);
            print!(
                "{:>18}",
                format!("{hits:5.1}% {}", common::shown(time).trim())
            );
        }
        println!();
    }
}

/// Writes PAGES pages to the disk, through a pool of their own, and returns their ids.
fn make_pages(disk: &Arc<DiskManager>) -> Vec<PageId> {
    let pool = BufferPool::new(disk.clone(), FRAMES);
    let pages = (0..PAGES)
        .map(|i| {
            let mut page = pool.new_page().unwrap();
            page[..8].copy_from_slice(&(i as u64).to_le_bytes());
            page.id()
        })
        .collect();
    pool.flush_all().unwrap();
    disk.commit().unwrap();
    pages
}

/// Reads the pages at these positions, one after another.
fn read(pool: &BufferPool, pages: &[PageId], reads: &[usize]) {
    for &i in reads {
        let page = pool.fetch_read(pages[i]).unwrap();
        std::hint::black_box(page[0]);
    }
}

/// A replacer that counts the pages it evicts: once the pool is full, each
/// read that misses evicts one.
struct Counting {
    inner: Box<dyn Replacer>,
    misses: Arc<AtomicUsize>,
}

impl Replacer for Counting {
    fn record_access(&mut self, page: PageId) {
        self.inner.record_access(page)
    }

    fn victim(&mut self, evictable: &dyn Fn(PageId) -> bool) -> Option<PageId> {
        self.inner.victim(evictable)
    }

    fn evict(&mut self, page: PageId) {
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.inner.evict(page)
    }

    fn remove(&mut self, page: PageId) {
        self.inner.remove(page)
    }
}

/// `n` pseudo-random numbers, the same each run (xorshift).
fn random(n: usize) -> impl Iterator<Item = usize> {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    (0..n).map(move |_| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x as usize
    })
}

/// 90% of reads to the first 200 pages, which fit in the pool, and the rest
/// to all the others.
fn skewed() -> Vec<usize> {
    random(READS)
        .map(|r| match r % 10 {
            0 => 200 + (r / 10) % (PAGES - 200),
            _ => (r / 10) % 200,
        })
        .collect()
}

/// A thousand reads of 128 hot pages at random, then a scan of a thousand
/// other pages, in turn: the scans read each page once, and push hot pages
/// out of an LRU pool.
fn scans() -> Vec<usize> {
    let mut random = random(READS);
    let mut scan = (128..PAGES).cycle();
    (0..READS)
        .map(|i| match (i / 1000) % 2 {
            0 => random.next().unwrap() % 128,
            _ => scan.next().unwrap(),
        })
        .collect()
}
