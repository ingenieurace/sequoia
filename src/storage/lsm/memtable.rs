//! The MemTable: an LSM tree's writes since its last flush, in a sorted map
//! in memory, which may be any of four (see `trie/mod.rs` for the tries):
//!
//! - a skip list (`skiplist.rs`): O(log n) levels of linked lists;
//! - a trie: a node per byte of each key;
//! - a radix tree: a trie with its one-child paths compressed;
//! - an adaptive radix tree (ART): a radix tree whose nodes size their
//!   child arrays to how many children they have.
//!
//! A lookup in a trie takes a step per byte of the key, not per level of a
//! structure that grows with the entries, and compares single bytes rather
//! than whole keys. Keys are written in key order when the MemTable is
//! flushed, and read in key order from any key on by scans, which all four
//! do as they are.

use super::skiplist::SkipList;
use crate::trie::{self, Adaptive, Art, List, RadixTree, SortedMap, Trie};

/// The MemTable of `LsmConfig::default`, used by every LSM table. Edit this
/// line to switch, like `COMPACTION` in `tree.rs`.
pub const MEMTABLE: MemTableKind = MemTableKind::Art;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemTableKind {
    SkipList,
    Trie,
    RadixTree,
    Art,
}

impl MemTableKind {
    pub const ALL: [MemTableKind; 4] = [MemTableKind::SkipList, MemTableKind::Trie, MemTableKind::RadixTree, MemTableKind::Art];
}

/// A value, or `None` for a tombstone.
type Value = Option<Vec<u8>>;

/// Each in a box: a trie keeps its root node in place.
pub enum MemTable {
    SkipList(Box<SkipList<Vec<u8>, Value>>),
    Trie(Box<Trie<Value>>),
    RadixTree(Box<RadixTree<Value>>),
    Art(Box<Art<Value>>),
}

/// Runs `$body` with `$map` bound to whichever map the MemTable is.
macro_rules! each {
    ($memtable:expr, $map:ident => $body:expr) => {
        match $memtable {
            MemTable::SkipList($map) => $body,
            MemTable::Trie($map) => $body,
            MemTable::RadixTree($map) => $body,
            MemTable::Art($map) => $body,
        }
    };
}

impl MemTable {
    pub fn new(kind: MemTableKind) -> Self {
        match kind {
            MemTableKind::SkipList => MemTable::SkipList(Box::default()),
            MemTableKind::Trie => MemTable::Trie(Box::default()),
            MemTableKind::RadixTree => MemTable::RadixTree(Box::default()),
            MemTableKind::Art => MemTable::Art(Box::default()),
        }
    }

    /// Sets a key's value, or tombstone, returning the one it replaces.
    pub fn insert(&mut self, key: Vec<u8>, value: Value) -> Option<Value> {
        each!(self, map => SortedMap::insert(&mut **map, key, value))
    }

    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        each!(self, map => SortedMap::get(&**map, key))
    }

    pub fn last_key(&self) -> Option<&[u8]> {
        each!(self, map => SortedMap::last_key(&**map))
    }

    /// The entries with keys `start` or above, in key order.
    pub fn iter_from(&self, start: &[u8]) -> Iter<'_> {
        match self {
            MemTable::SkipList(map) => Iter::SkipList(SortedMap::iter_from(&**map, start)),
            MemTable::Trie(map) => Iter::List(map.iter_from(start)),
            MemTable::RadixTree(map) => Iter::List(map.iter_from(start)),
            MemTable::Art(map) => Iter::Adaptive(map.iter_from(start)),
        }
    }

    pub fn iter(&self) -> Iter<'_> {
        self.iter_from(&[])
    }

    pub fn len(&self) -> usize {
        each!(self, map => SortedMap::len(&**map))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&mut self) {
        each!(self, map => SortedMap::clear(&mut **map))
    }
}

pub enum Iter<'a> {
    SkipList(<SkipList<Vec<u8>, Value> as SortedMap<Value>>::Iter<'a>),
    List(trie::Iter<'a, Value, List<Value>>),
    Adaptive(trie::Iter<'a, Value, Adaptive<Value>>),
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a [u8], &'a Value);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Iter::SkipList(iter) => iter.next(),
            Iter::List(iter) => iter.next(),
            Iter::Adaptive(iter) => iter.next(),
        }
    }
}
