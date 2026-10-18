//! Page replacement policies for the buffer pool.
//!
//! A replacer tracks resident pages by id and picks which one to evict when
//! the pool needs a frame. It knows nothing about frames or pins: the pool
//! passes a predicate saying which pages may be evicted right now.
//!
//! - [`ArcReplacer`]: Adaptive Replacement Cache, the default.
//! - [`ClockReplacer`]: CLOCK, an approximation of LRU.

use std::collections::{BTreeMap, HashMap};

use super::disk::PageId;

pub trait Replacer: Send {
    /// The page was accessed: loaded into the pool or hit while resident.
    fn record_access(&mut self, page: PageId);

    /// Chooses a resident page to evict among those for which `evictable`
    /// returns true. Does not evict it: the pool calls [`Replacer::evict`]
    /// once the page is gone.
    fn victim(&mut self, evictable: &dyn Fn(PageId) -> bool) -> Option<PageId>;

    /// The page was evicted to make room for another.
    fn evict(&mut self, page: PageId);

    /// The page left the pool for good (it was deleted); forget all about it.
    fn remove(&mut self, page: PageId);
}

// ---- ARC ----

/// Adaptive Replacement Cache (Megiddo and Modha, FAST 2003).
///
/// ```text
///          recency                      frequency
///   B1 (ghost) <-- T1 (resident) | T2 (resident) --> B2 (ghost)
///                  accessed once | accessed at least twice
/// ```
///
/// - A new page enters T1. Any further access moves it to T2, so a
///   sequential scan only churns T1 and cannot push hot pages out of T2.
/// - Evicted pages are remembered, without their data, in ghost lists: B1
///   for T1 and B2 for T2.
/// - `target` (`p` in the paper) is the size T1 aims for. A hit in B1 means
///   T1 was too small, so it grows; a hit in B2 means T2 was too small, so
///   it shrinks. Eviction takes from T1 while T1 is above its target.
/// - T1 + B1 never exceed `capacity`, and all four lists never exceed
///   twice that.
///
/// The paper's REPLACE also breaks a tie (T1 exactly at target) using the
/// incoming page; here eviction happens before the pool knows which page
/// comes next, so a tie evicts from T2.
pub struct ArcReplacer {
    capacity: usize,
    target: usize,
    /// Each list maps an access stamp to a page: the first entry is the
    /// least recently used, the last the most recently used.
    lists: [BTreeMap<u64, PageId>; 4],
    /// Which list each tracked page is in, and its stamp there.
    entries: HashMap<PageId, (List, u64)>,
    next_stamp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum List {
    T1,
    T2,
    B1,
    B2,
}

use List::{B1, B2, T1, T2};

impl ArcReplacer {
    pub fn new(capacity: usize) -> Self {
        Self { capacity, target: 0, lists: Default::default(), entries: HashMap::new(), next_stamp: 0 }
    }

    fn len(&self, list: List) -> usize {
        self.lists[list as usize].len()
    }

    /// Makes `page` the most recently used entry of `list`.
    fn push(&mut self, page: PageId, list: List) {
        self.next_stamp += 1;
        self.lists[list as usize].insert(self.next_stamp, page);
        self.entries.insert(page, (list, self.next_stamp));
    }

    /// Removes `page` from whichever list holds it, and returns that list.
    fn take(&mut self, page: PageId) -> Option<List> {
        let (list, stamp) = self.entries.remove(&page)?;
        self.lists[list as usize].remove(&stamp);
        Some(list)
    }

    /// Forgets the least recently used entry of `list`, if any.
    fn drop_lru(&mut self, list: List) {
        if let Some((_, page)) = self.lists[list as usize].pop_first() {
            self.entries.remove(&page);
        }
    }
}

impl Replacer for ArcReplacer {
    fn record_access(&mut self, page: PageId) {
        match self.take(page) {
            Some(T1 | T2) => {}
            Some(B1) => {
                // `page` was in B1 a moment ago, hence the +1.
                let step = (self.len(B2) / (self.len(B1) + 1)).max(1);
                self.target = (self.target + step).min(self.capacity);
            }
            Some(B2) => {
                let step = (self.len(B1) / (self.len(B2) + 1)).max(1);
                self.target = self.target.saturating_sub(step);
            }
            None => {
                // A brand new page: make room in the directory, then add it to T1.
                if self.len(T1) + self.len(B1) >= self.capacity {
                    self.drop_lru(B1);
                } else if self.entries.len() >= 2 * self.capacity {
                    self.drop_lru(B2);
                }
                self.push(page, T1);
                return;
            }
        }
        self.push(page, T2);
    }

    fn victim(&mut self, evictable: &dyn Fn(PageId) -> bool) -> Option<PageId> {
        let order = if self.len(T1) > self.target { [T1, T2] } else { [T2, T1] };
        // If every page in the preferred list is pinned, fall back to the other.
        order.into_iter().find_map(|list| self.lists[list as usize].values().copied().find(|&p| evictable(p)))
    }

    fn evict(&mut self, page: PageId) {
        match self.take(page) {
            Some(T1) => self.push(page, B1),
            Some(T2) => self.push(page, B2),
            _ => {}
        }
    }

    fn remove(&mut self, page: PageId) {
        self.take(page);
    }
}

// ---- CLOCK ----

/// CLOCK: resident pages sit on a ring, each with a reference bit that every
/// access sets. To find a victim, the hand sweeps the ring, clearing set
/// bits, and stops at the first evictable page whose bit is already clear.
pub struct ClockReplacer {
    /// Resident pages and their reference bits, in clock order.
    ring: Vec<Option<(PageId, bool)>>,
    slots: HashMap<PageId, usize>,
    /// Empty slots of `ring`.
    free: Vec<usize>,
    hand: usize,
}

impl ClockReplacer {
    pub fn new(capacity: usize) -> Self {
        Self { ring: vec![None; capacity], slots: HashMap::new(), free: (0..capacity).rev().collect(), hand: 0 }
    }
}

impl Replacer for ClockReplacer {
    fn record_access(&mut self, page: PageId) {
        if let Some(&slot) = self.slots.get(&page) {
            self.ring[slot] = Some((page, true));
            return;
        }
        let slot = self.free.pop().expect("more resident pages than the replacer's capacity");
        self.ring[slot] = Some((page, true));
        self.slots.insert(page, slot);
    }

    /// Two full turns suffice: the first may only clear reference bits.
    fn victim(&mut self, evictable: &dyn Fn(PageId) -> bool) -> Option<PageId> {
        let n = self.ring.len();
        for _ in 0..2 * n {
            let slot = self.hand;
            self.hand = (self.hand + 1) % n;
            let Some((page, referenced)) = &mut self.ring[slot] else { continue };
            if !evictable(*page) {
                continue;
            }
            if !*referenced {
                return Some(*page);
            }
            *referenced = false;
        }
        None
    }

    fn evict(&mut self, page: PageId) {
        self.remove(page);
    }

    fn remove(&mut self, page: PageId) {
        if let Some(slot) = self.slots.remove(&page) {
            self.ring[slot] = None;
            self.free.push(slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn p(id: u32) -> PageId {
        PageId(id)
    }

    /// Drives a replacer like a pool of `capacity` frames with nothing pinned,
    /// and returns the number of hits.
    fn simulate(replacer: &mut dyn Replacer, capacity: usize, accesses: &[u32]) -> usize {
        let mut resident = HashSet::new();
        let mut hits = 0;
        for &id in accesses {
            if resident.contains(&p(id)) {
                hits += 1;
            } else if resident.len() == capacity {
                let victim = replacer.victim(&|_| true).expect("nothing is pinned");
                replacer.evict(victim);
                resident.remove(&victim);
            }
            resident.insert(p(id));
            replacer.record_access(p(id));
        }
        hits
    }

    /// A hot set used twice per round, interleaved with scans that never
    /// repeat and are longer than the cache.
    fn hot_set_with_scans() -> Vec<u32> {
        let mut accesses = Vec::new();
        let mut next_cold = 100;
        for _ in 0..50 {
            accesses.extend([1, 2, 3, 1, 2, 3]);
            accesses.extend(next_cold..next_cold + 6);
            next_cold += 6;
        }
        accesses
    }

    #[test]
    fn arc_resists_scans_better_than_clock() {
        let accesses = hot_set_with_scans();
        let arc = simulate(&mut ArcReplacer::new(4), 4, &accesses);
        let clock = simulate(&mut ClockReplacer::new(4), 4, &accesses);

        // From the second round on, ARC keeps the hot set in T2 and only churns T1.
        assert_eq!(arc, 3 + 6 * 49);
        // Each scan flushes CLOCK's hot set, leaving only the repeat within a round.
        assert_eq!(clock, 3 * 50);
    }

    #[test]
    fn arc_second_access_promotes_to_t2() {
        let mut arc = ArcReplacer::new(4);
        arc.record_access(p(1));
        arc.record_access(p(2));
        assert_eq!(arc.entries[&p(1)].0, T1);
        arc.record_access(p(1));
        assert_eq!(arc.entries[&p(1)].0, T2);

        // T1 is above its target of 0, so it is evicted from first.
        assert_eq!(arc.victim(&|_| true), Some(p(2)));
        assert_eq!(arc.victim(&|page| page != p(2)), Some(p(1)), "falls back to T2");
    }

    #[test]
    fn arc_ghost_hits_adapt_the_target() {
        let mut arc = ArcReplacer::new(2);
        // 1 is used twice (T2); 2 is evicted from T1 to make room for 3.
        simulate(&mut arc, 2, &[1, 1, 2, 3]);
        assert_eq!(arc.entries[&p(2)].0, B1);

        // 2 comes back, evicting 3: T1 was too small, so its target grows.
        assert_eq!(arc.victim(&|_| true), Some(p(3)));
        arc.evict(p(3));
        arc.record_access(p(2));
        assert_eq!((arc.target, arc.entries[&p(2)].0), (1, T2));

        // T1 is empty, so 1 is evicted from T2. It comes back: T2 was too
        // small, so T1's target shrinks.
        assert_eq!(arc.victim(&|_| true), Some(p(1)));
        arc.evict(p(1));
        assert_eq!(arc.entries[&p(1)].0, B2);
        arc.record_access(p(1));
        assert_eq!((arc.target, arc.entries[&p(1)].0), (0, T2));
    }

    #[test]
    fn arc_directory_stays_bounded() {
        let capacity = 8;
        let mut arc = ArcReplacer::new(capacity);
        let accesses: Vec<u32> = (0..1000).map(|i| (i * 7919) % 50).collect();
        simulate(&mut arc, capacity, &accesses);

        assert!(arc.len(T1) + arc.len(T2) <= capacity);
        assert!(arc.len(T1) + arc.len(B1) <= capacity);
        assert!(arc.entries.len() <= 2 * capacity);
        assert!(arc.target <= capacity);
    }

    #[test]
    fn arc_remove_forgets_ghosts() {
        let mut arc = ArcReplacer::new(2);
        simulate(&mut arc, 2, &[1, 1, 2, 3]);
        assert_eq!(arc.entries[&p(2)].0, B1);
        for id in 1..=3 {
            arc.remove(p(id));
        }
        assert!(arc.entries.is_empty());
        assert!(arc.lists.iter().all(BTreeMap::is_empty));
    }

    #[test]
    fn clock_gives_recently_used_pages_a_second_chance() {
        let mut clock = ClockReplacer::new(3);
        // When 4 arrives every page is referenced: one full turn clears all
        // bits, then the hand evicts the first page (1) and stops after it.
        // 3 is used again; 2 is not, so 5 replaces 2.
        simulate(&mut clock, 3, &[1, 2, 3, 4, 3, 5]);
        let resident: HashSet<PageId> = clock.slots.keys().copied().collect();
        assert_eq!(resident, HashSet::from([p(3), p(4), p(5)]));
    }

    #[test]
    fn victims_must_be_evictable() {
        for replacer in [&mut ArcReplacer::new(2) as &mut dyn Replacer, &mut ClockReplacer::new(2)] {
            replacer.record_access(p(1));
            replacer.record_access(p(2));
            assert_eq!(replacer.victim(&|page| page == p(2)), Some(p(2)));
            assert_eq!(replacer.victim(&|_| false), None);
        }
    }
}
