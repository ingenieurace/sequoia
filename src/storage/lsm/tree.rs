//! Log-structured merge tree: a key-value store that never updates data in place.
//!
//! ```text
//! put / delete --> log (on disk) + MemTable (in memory)
//!                        | log full: write the MemTable out sorted
//!                        v
//! level 0:   SSTable  SSTable  SSTable  SSTable    newest first; key ranges overlap
//!                        | 4 of them: merge with level 1
//!                        v
//! level 1:   one SSTable, up to `level1_pages` pages
//!                        | too big: merge into level 2
//!                        v
//! level 2:   one SSTable, up to `fanout` times bigger ... and so on
//! ```
//!
//! Every write goes to the MemTable, a skip list in memory, and is appended to
//! the log, so it survives a crash: opening the tree replays the log into the
//! MemTable. A deletion is a write too, of a tombstone. When the log reaches
//! `log_bytes`, the MemTable is written out as a new level-0 SSTable and the
//! log starts over.
//!
//! Reads check the MemTable, then each SSTable from newest to oldest; the first
//! one that knows the key has its latest value. Each SSTable's summary (key
//! range and Bloom filter) skips most SSTables without reading them.
//!
//! Compaction sort-merges SSTables into one, keeping only the newest entry for
//! each key and dropping tombstones once nothing older lies below them. The
//! strategy is set by [`COMPACTION`]:
//!
//! - Level (the picture above): level 0 is merged into level 1 once it has
//!   `level0_tables` SSTables; a deeper level is merged into the next once it
//!   outgrows its limit. Below level 0 a level is a single SSTable, so its key
//!   ranges never overlap. Suits read-heavy workloads: few SSTables to check.
//! - Universal: every SSTable is a sorted run in one level (stored as level
//!   0), newest first, with overlapping key ranges. Once there are `max_runs`
//!   of them, the newest adjacent runs of similar size are merged, or every
//!   run if the newer ones take `max_space_amp` percent of the oldest's space
//!   or more. Suits insert-heavy workloads: each entry is rewritten fewer
//!   times (see `pages_written`), but reads may check more SSTables.
//!
//! A tree written with one strategy opens fine with the other: universal
//! treats the deeper levels as older runs, and level merges an oversized
//! level 0 into level 1.
//!
//! Where everything lives is recorded in the manifest page:
//!
//! ```text
//! 0..4    log first page
//! 4..8    level-0 count n, then n page ids (each SSTable's first page), newest first
//!         level count m (levels 1..=m), then m page ids (u32::MAX = empty level)
//! ```
//!
//! A flush or compaction writes its new pages, then rewrites the manifest page,
//! and only then frees the old log or SSTables. The statement doing it commits
//! all of that at once (see `storage/io/wal.rs`), so a crash never leaves it half done.

use std::sync::Arc;

use crate::storage::buffer::pool::BufferPool;
use crate::storage::io::disk::{PAGE_USABLE, PageId, decode_id, encode_id, read_u32, write_u32};
use crate::storage::heap::TableHeap;
use crate::storage::page::MAX_TUPLE_SIZE;
use super::skiplist::SkipList;
use super::sstable::{self, Entry, SsTable};
use crate::error::{Result, err};

/// The compaction strategy of `LsmConfig::default`, used by every LSM table.
/// Edit this line to switch, like `REPLACEMENT_POLICY` in `storage/buffer/pool.rs`:
///
/// - `Compaction::Level`: level compaction
/// - `Compaction::Universal`: universal compaction
pub const COMPACTION: Compaction = Compaction::Level;

/// Bytes before each log record's entry: its sequence number.
const SEQ_SIZE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compaction {
    Level,
    Universal,
}

#[derive(Debug, Clone, Copy)]
pub struct LsmConfig {
    /// Log size, in bytes, at which the MemTable is written out.
    pub log_bytes: usize,
    pub compaction: Compaction,
    /// Level: level-0 SSTables that trigger a merge into level 1.
    pub level0_tables: usize,
    /// Level: largest level 1, in pages, before it is merged into level 2.
    pub level1_pages: usize,
    /// Level: how many times bigger each level may be than the one above.
    pub fanout: usize,
    /// Universal: sorted runs that trigger a compaction.
    pub max_runs: usize,
    /// Universal: merge every run once the newer runs take this many percent
    /// of the oldest run's pages.
    pub max_space_amp: usize,
}

impl Default for LsmConfig {
    fn default() -> Self {
        LsmConfig {
            log_bytes: 64 * 1024,
            compaction: COMPACTION,
            level0_tables: 4,
            level1_pages: 64,
            fanout: 10,
            max_runs: 8,
            max_space_amp: 200,
        }
    }
}

pub struct LsmTree {
    pool: Arc<BufferPool>,
    config: LsmConfig,
    manifest: PageId,
    /// Each key's latest value since the last flush; `None` is a tombstone.
    memtable: SkipList<Vec<u8>, Option<Vec<u8>>>,
    /// Holds the MemTable's writes as records `(seq, entry)`.
    log: TableHeap,
    log_bytes: usize,
    next_seq: u64,
    /// Newest first.
    level0: Vec<SsTable>,
    /// `levels[i]` is level `i + 1`.
    levels: Vec<Option<SsTable>>,
    /// SSTable pages written by flushes and compactions since the tree was
    /// opened. Compared with the data written, this is the write amplification.
    pages_written: usize,
}

impl LsmTree {
    /// Creates an empty tree.
    pub fn create(pool: Arc<BufferPool>, config: LsmConfig) -> Result<Self> {
        let log = TableHeap::create(pool.clone())?;
        pool.flush_page(log.first_page())?;
        let manifest = pool.new_page()?.id();
        let tree = LsmTree {
            pool,
            config,
            manifest,
            memtable: SkipList::new(),
            log,
            log_bytes: 0,
            next_seq: 0,
            level0: Vec::new(),
            levels: Vec::new(),
            pages_written: 0,
        };
        tree.write_manifest()?;
        Ok(tree)
    }

    /// Opens a tree from its manifest page and replays its log.
    pub fn open(pool: Arc<BufferPool>, manifest: PageId, config: LsmConfig) -> Result<Self> {
        let (log_page, level0, levels) = {
            let page = pool.fetch_read(manifest)?;
            let mut at = 0;
            let mut next = || {
                let value = read_u32(&page, at);
                at += 4;
                value
            };
            let log_page = PageId(next());
            let level0: Vec<PageId> = (0..next()).map(|_| PageId(next())).collect();
            let levels: Vec<Option<PageId>> = (0..next()).map(|_| decode_id(next())).collect();
            (log_page, level0, levels)
        };

        let mut tree = LsmTree {
            pool: pool.clone(),
            config,
            manifest,
            memtable: SkipList::new(),
            log: TableHeap::open(pool.clone(), log_page, log_page)?,
            log_bytes: 0,
            next_seq: 0,
            level0: level0.into_iter().map(|id| SsTable::open(pool.clone(), id)).collect::<Result<_>>()?,
            levels: levels
                .into_iter()
                .map(|id| id.map(|id| SsTable::open(pool.clone(), id)).transpose())
                .collect::<Result<_>>()?,
            pages_written: 0,
        };

        // Records may sit out of order in the log's pages; the sequence numbers restore it.
        let mut records = Vec::new();
        for item in tree.log.scan() {
            let (_, record) = item?;
            let Some((seq, entry)) = record.split_first_chunk::<SEQ_SIZE>() else {
                return err("corrupted LSM log record");
            };
            let (key, value) = sstable::decode_entry(entry)?;
            records.push((u64::from_le_bytes(*seq), key.to_vec(), value.map(<[u8]>::to_vec)));
            tree.log_bytes += record.len();
        }
        records.sort_by_key(|(seq, _, _)| *seq);
        tree.next_seq = records.last().map_or(0, |(seq, _, _)| seq + 1);
        for (_, key, value) in records {
            tree.memtable.insert(key, value);
        }
        Ok(tree)
    }

    pub fn manifest_page(&self) -> PageId {
        self.manifest
    }

    pub fn pages_written(&self) -> usize {
        self.pages_written
    }

    /// Fails if a key and value are too big to store together.
    pub fn check_size(key: &[u8], value: &[u8]) -> Result<()> {
        let max = MAX_TUPLE_SIZE - SEQ_SIZE;
        let len = sstable::entry_len(key, value);
        if len > max {
            return err(format!("row is {len} bytes with its key, the maximum is {max}"));
        }
        Ok(())
    }

    /// Returns the latest value of a key, or `None` if it has none or was deleted.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(value) = self.memtable.get(key) {
            return Ok(value.clone());
        }
        for table in self.tables() {
            if let Some(value) = table.get(key)? {
                return Ok(value);
            }
        }
        Ok(None)
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        Self::check_size(&key, &value)?;
        self.write(key, Some(value))
    }

    pub fn delete(&mut self, key: Vec<u8>) -> Result<()> {
        self.write(key, None)
    }

    /// Iterates over the live keys and their values in key order.
    pub fn scan(&self) -> impl Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_ {
        let memtable = self.memtable.iter().map(|(k, v)| Ok((k.clone(), v.clone())));
        let mut sources: Vec<Source> = vec![Box::new(memtable)];
        sources.extend(self.tables().map(|table| Box::new(table.iter()) as Source));
        Merge::new(sources).filter_map(|item| match item {
            Ok((key, Some(value))) => Some(Ok((key, value))),
            Ok((_, None)) => None,
            Err(e) => Some(Err(e)),
        })
    }

    /// The largest key ever written that may still be stored, deleted or not.
    pub fn max_key(&self) -> Option<Vec<u8>> {
        let stored = self.tables().map(SsTable::max_key);
        self.memtable.last_key().map(Vec::as_slice).into_iter().chain(stored).max().map(<[u8]>::to_vec)
    }

    /// Frees every page of the tree.
    pub fn destroy(self) -> Result<()> {
        self.log.destroy()?;
        for table in self.level0.into_iter().chain(self.levels.into_iter().flatten()) {
            table.destroy()?;
        }
        self.pool.delete_page(self.manifest)
    }

    /// All SSTables from newest to oldest.
    fn tables(&self) -> impl Iterator<Item = &SsTable> {
        self.level0.iter().chain(self.levels.iter().flatten())
    }

    fn write(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) -> Result<()> {
        let mut record = self.next_seq.to_le_bytes().to_vec();
        record.extend(sstable::encode_entry(&key, value.as_deref()));
        self.log.insert(&record)?;
        self.next_seq += 1;
        self.log_bytes += record.len();
        self.memtable.insert(key, value);

        if self.log_bytes >= self.config.log_bytes {
            self.flush()?;
            self.compact()?;
        }
        Ok(())
    }

    /// Writes the MemTable out as a new level-0 SSTable and starts a new log.
    fn flush(&mut self) -> Result<()> {
        let entries = self.memtable.iter().map(|(k, v)| Ok((k.clone(), v.clone())));
        let table = SsTable::write(self.pool.clone(), entries)?;
        self.pages_written += table.as_ref().map_or(0, SsTable::page_count);
        let new_log = TableHeap::create(self.pool.clone())?;
        self.pool.flush_page(new_log.first_page())?;

        self.level0.extend(table);
        self.level0.rotate_right(1); // newest first
        let old_log = std::mem::replace(&mut self.log, new_log);
        self.memtable.clear();
        self.log_bytes = 0;
        self.next_seq = 0;

        self.write_manifest()?;
        old_log.destroy()
    }

    /// Merges SSTables as the compaction strategy decides, then frees the old ones.
    fn compact(&mut self) -> Result<()> {
        let garbage = match self.config.compaction {
            Compaction::Level => self.compact_levels()?,
            Compaction::Universal => self.compact_universal()?,
        };
        if garbage.is_empty() {
            return Ok(());
        }
        self.write_manifest()?;
        garbage.into_iter().try_for_each(SsTable::destroy)
    }

    /// Merges levels that are over their limits, from the top down. Returns the
    /// SSTables merged away.
    fn compact_levels(&mut self) -> Result<Vec<SsTable>> {
        let mut garbage = Vec::new();

        if self.level0.len() >= self.config.level0_tables {
            if self.levels.is_empty() {
                self.levels.push(None);
            }
            let inputs: Vec<&SsTable> = self.level0.iter().chain(&self.levels[0]).collect();
            let merged = self.merge(&inputs, self.is_bottom(0))?;
            self.pages_written += merged.as_ref().map_or(0, SsTable::page_count);
            garbage.append(&mut self.level0);
            garbage.extend(std::mem::replace(&mut self.levels[0], merged));
        }

        let mut i = 0;
        while i < self.levels.len() {
            let limit = self.config.level1_pages * self.config.fanout.pow(i as u32);
            if self.levels[i].as_ref().is_some_and(|table| table.page_count() > limit) {
                if i + 1 == self.levels.len() {
                    self.levels.push(None);
                }
                let inputs: Vec<&SsTable> = self.levels[i].iter().chain(&self.levels[i + 1]).collect();
                let merged = self.merge(&inputs, self.is_bottom(i + 1))?;
                self.pages_written += merged.as_ref().map_or(0, SsTable::page_count);
                garbage.extend(self.levels[i].take());
                garbage.extend(std::mem::replace(&mut self.levels[i + 1], merged));
            }
            i += 1;
        }
        Ok(garbage)
    }

    /// Merges the newest sorted runs once there are too many. Returns the
    /// SSTables merged away.
    fn compact_universal(&mut self) -> Result<Vec<SsTable>> {
        // Runs from level compaction: deeper levels are older.
        self.level0.extend(self.levels.drain(..).flatten());
        let runs = self.level0.len();
        if runs < self.config.max_runs {
            return Ok(Vec::new());
        }

        let sizes: Vec<usize> = self.level0.iter().map(SsTable::page_count).collect();
        let newer: usize = sizes[..runs - 1].iter().sum();
        let range = if newer * 100 >= sizes[runs - 1] * self.config.max_space_amp {
            // Space amplification: much of the data may be stale copies.
            0..runs
        } else {
            // Size ratio: the newest stretch of at least two runs where each is no
            // bigger than the newer ones before it together. Runs of similar
            // size are merged, so each entry is rewritten only a few times.
            let similar = (0..runs).find_map(|start| {
                let (mut end, mut total) = (start + 1, sizes[start]);
                while end < runs && sizes[end] <= total {
                    total += sizes[end];
                    end += 1;
                }
                (end - start >= 2).then_some(start..end)
            });
            // Otherwise the newest runs, just enough to get below `max_runs`.
            similar.unwrap_or(0..runs + 2 - self.config.max_runs)
        };

        // Merged runs are adjacent, so the result keeps its place in the newest-first order.
        let inputs: Vec<&SsTable> = self.level0[range.clone()].iter().collect();
        let merged = self.merge(&inputs, range.end == runs)?;
        self.pages_written += merged.as_ref().map_or(0, SsTable::page_count);
        Ok(self.level0.splice(range, merged).collect())
    }

    /// Whether no level below `levels[i]` holds anything.
    fn is_bottom(&self, i: usize) -> bool {
        self.levels[i + 1..].iter().all(Option::is_none)
    }

    /// Sort-merges SSTables (newest first) into a new one, keeping the newest
    /// entry for each key. Tombstones are dropped if nothing lies below.
    fn merge(&self, inputs: &[&SsTable], drop_tombstones: bool) -> Result<Option<SsTable>> {
        let sources = inputs.iter().map(|table| Box::new(table.iter()) as Source).collect();
        let entries = Merge::new(sources).filter(|item| !(drop_tombstones && matches!(item, Ok((_, None)))));
        SsTable::write(self.pool.clone(), entries)
    }

    /// Rewrites the manifest page.
    fn write_manifest(&self) -> Result<()> {
        let mut ids = vec![self.log.first_page().0, self.level0.len() as u32];
        ids.extend(self.level0.iter().map(|table| table.first_page().0));
        ids.push(self.levels.len() as u32);
        ids.extend(self.levels.iter().map(|table| encode_id(table.as_ref().map(SsTable::first_page))));
        assert!(ids.len() * 4 <= PAGE_USABLE, "manifest does not fit in a page");

        let mut page = self.pool.fetch_write(self.manifest)?;
        for (i, &id) in ids.iter().enumerate() {
            write_u32(&mut page, i * 4, id);
        }
        Ok(())
    }
}

type Source<'a> = Box<dyn Iterator<Item = Result<Entry>> + 'a>;

/// Merges sorted sources into one sorted stream. Sources are ordered newest
/// first: when several hold the same key, only the newest one's entry is kept.
struct Merge<'a> {
    sources: Vec<Source<'a>>,
    /// The next entry of each source; `None` once it is exhausted.
    heads: Vec<Option<Entry>>,
    started: bool,
}

impl<'a> Merge<'a> {
    fn new(sources: Vec<Source<'a>>) -> Self {
        let heads = sources.iter().map(|_| None).collect();
        Merge { sources, heads, started: false }
    }

    fn advance(&mut self, i: usize) -> Result<()> {
        self.heads[i] = self.sources[i].next().transpose()?;
        Ok(())
    }
}

impl Iterator for Merge<'_> {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.started {
            self.started = true;
            if let Err(e) = (0..self.sources.len()).try_for_each(|i| self.advance(i)) {
                return Some(Err(e));
            }
        }

        // The smallest key; on a tie `min_by` picks the first, i.e. the newest source.
        let newest = (0..self.heads.len())
            .filter(|&i| self.heads[i].is_some())
            .min_by(|&a, &b| self.heads[a].as_ref().unwrap().0.cmp(&self.heads[b].as_ref().unwrap().0))?;
        let entry = self.heads[newest].take().unwrap();

        // Skip older versions of the same key, then move every source used past it.
        for i in 0..self.heads.len() {
            let used = i == newest || self.heads[i].as_ref().is_some_and(|(key, _)| *key == entry.0);
            if used && let Err(e) = self.advance(i) {
                return Some(Err(e));
            }
        }
        Some(Ok(entry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DiskManager;

    fn pool() -> Arc<BufferPool> {
        Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 64))
    }

    /// Flushes after about 20 writes, and keeps level 1 to 4 pages.
    fn small() -> LsmConfig {
        LsmConfig {
            log_bytes: 2000,
            compaction: Compaction::Level,
            level0_tables: 3,
            level1_pages: 4,
            fanout: 2,
            max_runs: 4,
            max_space_amp: 200,
        }
    }

    fn small_universal() -> LsmConfig {
        LsmConfig { compaction: Compaction::Universal, ..small() }
    }

    fn key(i: u32) -> Vec<u8> {
        i.to_be_bytes().to_vec()
    }

    fn value(i: u32, version: u8) -> Vec<u8> {
        vec![version; 50 + (i % 7) as usize]
    }

    fn scan(tree: &LsmTree) -> Vec<(Vec<u8>, Vec<u8>)> {
        tree.scan().collect::<Result<_>>().unwrap()
    }

    #[test]
    fn get_put_delete_in_the_memtable() {
        let mut tree = LsmTree::create(pool(), LsmConfig::default()).unwrap();
        tree.put(key(1), b"one".to_vec()).unwrap();
        tree.put(key(2), b"two".to_vec()).unwrap();
        tree.put(key(1), b"uno".to_vec()).unwrap();
        tree.delete(key(2)).unwrap();

        assert_eq!(tree.get(&key(1)).unwrap(), Some(b"uno".to_vec()));
        assert_eq!(tree.get(&key(2)).unwrap(), None);
        assert_eq!(scan(&tree), vec![(key(1), b"uno".to_vec())]);
        assert!(tree.level0.is_empty(), "nothing flushed yet");
    }

    #[test]
    fn newest_version_wins_with_level_compaction() {
        let tree = check_newest_version_wins(small());
        assert!(tree.levels.len() >= 2, "compacted down at least two levels");
    }

    #[test]
    fn newest_version_wins_with_universal_compaction() {
        let tree = check_newest_version_wins(small_universal());
        assert!(tree.levels.is_empty() && tree.level0.len() < 4);
    }

    /// Writes several versions of each key with flushes and compactions in
    /// between, checks that reads see the latest, and returns the tree.
    fn check_newest_version_wins(config: LsmConfig) -> LsmTree {
        let mut tree = LsmTree::create(pool(), config).unwrap();
        for version in 0..3u8 {
            for i in 0..600 {
                tree.put(key(i), value(i, version)).unwrap();
            }
        }
        // Delete every third key, then overwrite some deleted ones again.
        for i in (0..600).step_by(3) {
            tree.delete(key(i)).unwrap();
        }
        for i in (0..600).step_by(9) {
            tree.put(key(i), value(i, 9)).unwrap();
        }

        let expected: Vec<(Vec<u8>, Vec<u8>)> = (0..600)
            .filter_map(|i| match (i % 3, i % 9) {
                (_, 0) => Some((key(i), value(i, 9))),
                (0, _) => None,
                _ => Some((key(i), value(i, 2))),
            })
            .collect();
        assert_eq!(scan(&tree), expected);
        for i in 0..600 {
            let want = expected.iter().find(|(k, _)| *k == key(i)).map(|(_, v)| v.clone());
            assert_eq!(tree.get(&key(i)).unwrap(), want, "key {i}");
        }
        tree
    }

    #[test]
    fn level0_is_merged_and_levels_stay_bounded() {
        let mut tree = LsmTree::create(pool(), small()).unwrap();
        for i in 0..2000 {
            tree.put(key(i), value(i, 1)).unwrap();
            assert!(tree.level0.len() < 3, "level 0 is merged as soon as it has 3 tables");
        }
        for (i, level) in tree.levels.iter().enumerate().take(tree.levels.len() - 1) {
            let limit = 4 * 2usize.pow(i as u32);
            assert!(level.as_ref().is_none_or(|t| t.page_count() <= limit), "level {} is too big", i + 1);
        }
        assert_eq!(scan(&tree).len(), 2000);
    }

    #[test]
    fn universal_keeps_runs_below_the_limit_and_rewrites_less() {
        // Default ratios (4 level-0 tables, fanout 10, 8 runs) on a small scale.
        let level = LsmConfig { log_bytes: 2000, compaction: Compaction::Level, level1_pages: 16, ..Default::default() };
        let universal = LsmConfig { log_bytes: 2000, compaction: Compaction::Universal, ..Default::default() };
        let mut level = LsmTree::create(pool(), level).unwrap();
        let mut universal = LsmTree::create(pool(), universal).unwrap();
        for i in 0..3000u32 {
            // Scattered keys, so key ranges overlap.
            let k = i.wrapping_mul(2_654_435_761).to_be_bytes().to_vec();
            level.put(k.clone(), value(i, 1)).unwrap();
            universal.put(k, value(i, 1)).unwrap();
            assert!(universal.level0.len() < 8, "universal merges as soon as there are 8 runs");
            assert!(universal.levels.is_empty());
        }
        assert_eq!(scan(&universal), scan(&level));
        assert!(
            universal.pages_written() < level.pages_written(),
            "universal {} pages, level {} pages",
            universal.pages_written(),
            level.pages_written()
        );
    }

    #[test]
    fn universal_full_merge_drops_tombstones() {
        let mut tree = LsmTree::create(pool(), small_universal()).unwrap();
        for i in 0..100 {
            tree.put(key(i), value(i, 1)).unwrap();
        }
        for i in 0..100 {
            tree.delete(key(i)).unwrap();
        }
        for i in 1000..1400 {
            tree.put(key(i), value(i, 1)).unwrap();
        }
        let stored: usize = tree.tables().map(|t| t.iter().count()).sum();
        assert_eq!(scan(&tree).len(), 400);
        assert!(stored < 500, "most of the 100 tombstones are gone: {stored} entries stored");
    }

    #[test]
    fn switching_strategies_keeps_the_data() {
        let pool = pool();
        let fill = |tree: &mut LsmTree, from: u32| {
            for i in from..from + 1000 {
                tree.put(key(i), value(i, 1)).unwrap();
            }
            pool.flush_all().unwrap();
        };

        let mut tree = LsmTree::create(pool.clone(), small()).unwrap();
        fill(&mut tree, 0);
        assert!(!tree.levels.is_empty());
        let manifest = tree.manifest_page();

        let mut tree = LsmTree::open(pool.clone(), manifest, small_universal()).unwrap();
        fill(&mut tree, 1000);
        assert!(tree.levels.is_empty(), "levels became runs");

        let mut tree = LsmTree::open(pool.clone(), manifest, small()).unwrap();
        fill(&mut tree, 2000);
        assert!(tree.level0.len() < 3);
        let keys: Vec<Vec<u8>> = scan(&tree).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, (0..3000).map(key).collect::<Vec<_>>());
    }

    #[test]
    fn tombstones_are_dropped_at_the_bottom() {
        let mut tree = LsmTree::create(pool(), small()).unwrap();
        for i in 0..100 {
            tree.put(key(i), value(i, 1)).unwrap();
        }
        for i in 0..100 {
            tree.delete(key(i)).unwrap();
        }
        // Push the deletions all the way down with unrelated writes.
        for i in 1000..1400 {
            tree.put(key(i), value(i, 1)).unwrap();
        }
        let stored: usize = tree.tables().map(|t| t.iter().count()).sum();
        let live = scan(&tree).len();
        assert_eq!(live, 400);
        assert!(stored < live + 100, "most of the 100 tombstones are gone: {stored} entries stored");
    }

    #[test]
    fn reopening_replays_the_log_and_finds_the_sstables() {
        let pool = pool();
        let manifest = {
            let mut tree = LsmTree::create(pool.clone(), small()).unwrap();
            for i in 0..500 {
                tree.put(key(i), value(i, 1)).unwrap();
            }
            tree.delete(key(7)).unwrap();
            tree.put(key(8), b"latest".to_vec()).unwrap();
            assert!(!tree.memtable.is_empty() && !tree.levels.is_empty());
            pool.flush_all().unwrap();
            tree.manifest_page()
        };

        let tree = LsmTree::open(pool, manifest, small()).unwrap();
        assert_eq!(scan(&tree).len(), 499);
        assert_eq!(tree.get(&key(7)).unwrap(), None);
        assert_eq!(tree.get(&key(8)).unwrap(), Some(b"latest".to_vec()));
        assert_eq!(tree.get(&key(499)).unwrap(), Some(value(499, 1)));
        assert_eq!(tree.max_key(), Some(key(499)));
    }

    #[test]
    fn destroy_frees_every_page() {
        let pool = pool();
        let fill = |pool: &Arc<BufferPool>| {
            let mut tree = LsmTree::create(pool.clone(), small()).unwrap();
            for i in 0..500 {
                tree.put(key(i), value(i, 1)).unwrap();
            }
            tree
        };
        fill(&pool).destroy().unwrap();
        let pages = pool.disk().page_count();
        fill(&pool).destroy().unwrap();
        assert_eq!(pool.disk().page_count(), pages);
    }

    #[test]
    fn rejects_entries_too_big_for_a_page() {
        let mut tree = LsmTree::create(pool(), LsmConfig::default()).unwrap();
        let max = MAX_TUPLE_SIZE - SEQ_SIZE - 3 - 4;
        assert!(tree.put(key(1), vec![0; max + 1]).is_err());
        tree.put(key(1), vec![0; max]).unwrap();
    }
}

