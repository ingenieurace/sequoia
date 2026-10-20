//! Skip list: the sorted map in memory that holds an LSM tree's MemTable.
//!
//! ```text
//! level 3   head ---------------------------> 40 ----------------------> nil
//! level 2   head -------------> 20 ---------> 40 ----------------------> nil
//! level 1   head ---> 10 -----> 20 ---------> 40 -----> 50 ------------> nil
//! level 0   head ---> 10 -----> 20 --> 30 --> 40 -----> 50 --> 60 -----> nil
//! ```
//!
//! Every node is on level 0, a sorted linked list of all the entries. A node
//! is also on each level above with probability 1/4, so each level skips over
//! about three quarters of the nodes of the one below. A search starts at the
//! top level of the head and moves right while the next key is smaller, then
//! drops a level: about log4(n) levels with a few steps each, O(log n) on
//! average. An insert makes the same search, remembering the last node it
//! passed on each level, and links the new node in after those.
//!
//! Nodes live in a `Vec` and point at each other by index, so no unsafe code
//! is needed. Node 0 is the head, which holds no entry; as no node ever points
//! back at it, index 0 also serves as "no next node". Entries are never
//! removed one by one: the MemTable records a delete as a tombstone value, and
//! is emptied all at once when it is written out.
//!
//! Levels are drawn from a fixed-seed generator, so the same inserts always
//! build the same list.

use std::borrow::Borrow;

use crate::trie::SortedMap;

/// Levels a node can be on: enough for 4^12, about 16 million, entries.
const MAX_LEVEL: usize = 12;
const HEAD: usize = 0;
/// The `next` of the last node on a level. The head is never anyone's next,
/// and as it holds no entry, a search that reaches it finds nothing.
const NIL: usize = HEAD;

pub struct SkipList<K, V> {
    /// The head first, then each entry in insertion order.
    nodes: Vec<Node<K, V>>,
    /// Levels in use: the height of the tallest node.
    levels: usize,
    /// State of the xorshift generator that draws node heights.
    rng: u64,
}

struct Node<K, V> {
    /// `None` only for the head.
    entry: Option<(K, V)>,
    /// The next node on each level this node is on.
    next: Vec<usize>,
}

impl<K: Ord, V> SkipList<K, V> {
    pub fn new() -> Self {
        let head = Node { entry: None, next: vec![NIL; MAX_LEVEL] };
        SkipList { nodes: vec![head], levels: 1, rng: 0x9E37_79B9_7F4A_7C15 }
    }

    pub fn len(&self) -> usize {
        self.nodes.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get<Q: Ord + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        let node = self.nodes[self.search(key, &mut [HEAD; MAX_LEVEL])].next[0];
        match &self.nodes[node].entry {
            Some((k, v)) if k.borrow() == key => Some(v),
            _ => None,
        }
    }

    /// Sets the value of a key, returning the one it replaces.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let mut before = [HEAD; MAX_LEVEL];
        let next = self.nodes[self.search(&key, &mut before)].next[0];
        if let Some((k, v)) = &mut self.nodes[next].entry
            && *k == key
        {
            return Some(std::mem::replace(v, value));
        }

        let height = self.random_height();
        // Levels new to the list start at the head, which `before` already says.
        self.levels = self.levels.max(height);
        let id = self.nodes.len();
        let next = before[..height].iter().enumerate().map(|(level, &node)| self.nodes[node].next[level]).collect();
        self.nodes.push(Node { entry: Some((key, value)), next });
        for (level, &node) in before[..height].iter().enumerate() {
            self.nodes[node].next[level] = id;
        }
        None
    }

    /// The largest key, found by going as far right as possible on each level.
    pub fn last_key(&self) -> Option<&K> {
        let mut node = HEAD;
        for level in (0..self.levels).rev() {
            while self.nodes[node].next[level] != NIL {
                node = self.nodes[node].next[level];
            }
        }
        self.nodes[node].entry.as_ref().map(|(k, _)| k)
    }

    /// Iterates over the entries in key order: level 0 from the head.
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter { list: self, node: self.nodes[HEAD].next[0] }
    }

    /// Iterates over the entries with keys `>= start` in key order: level 0
    /// from the first such node, found as a lookup finds a key.
    pub fn iter_from<Q: Ord + ?Sized>(&self, start: &Q) -> Iter<'_, K, V>
    where
        K: Borrow<Q>,
    {
        Iter { list: self, node: self.nodes[self.search(start, &mut [HEAD; MAX_LEVEL])].next[0] }
    }

    pub fn clear(&mut self) {
        self.nodes.truncate(1);
        self.nodes[HEAD].next.fill(NIL);
        self.levels = 1;
    }

    /// Finds the last node with a key below `key`, filling `before` with the
    /// last node passed on each level in use.
    fn search<Q: Ord + ?Sized>(&self, key: &Q, before: &mut [usize; MAX_LEVEL]) -> usize
    where
        K: Borrow<Q>,
    {
        let mut node = HEAD;
        for level in (0..self.levels).rev() {
            loop {
                let next = self.nodes[node].next[level];
                match &self.nodes[next].entry {
                    Some((k, _)) if k.borrow() < key => node = next,
                    _ => break,
                }
            }
            before[level] = node;
        }
        node
    }

    /// 1, plus 1 more with probability 1/4 each time, up to `MAX_LEVEL`.
    fn random_height(&mut self) -> usize {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        // Two random bits per level: both zero (1 in 4) goes one level up.
        let mut bits = self.rng;
        let mut height = 1;
        while height < MAX_LEVEL && bits & 3 == 0 {
            height += 1;
            bits >>= 2;
        }
        height
    }
}

impl<V> SortedMap<V> for SkipList<Vec<u8>, V> {
    type Iter<'a>
        = std::iter::Map<Iter<'a, Vec<u8>, V>, fn((&'a Vec<u8>, &'a V)) -> (&'a [u8], &'a V)>
    where
        V: 'a;

    fn insert(&mut self, key: Vec<u8>, value: V) -> Option<V> {
        SkipList::insert(self, key, value)
    }

    fn get(&self, key: &[u8]) -> Option<&V> {
        SkipList::get(self, key)
    }

    fn iter_from(&self, start: &[u8]) -> Self::Iter<'_> {
        SkipList::iter_from(self, start).map(|(k, v)| (k.as_slice(), v))
    }

    fn last_key(&self) -> Option<&[u8]> {
        SkipList::last_key(self).map(Vec::as_slice)
    }

    fn len(&self) -> usize {
        SkipList::len(self)
    }

    fn clear(&mut self) {
        SkipList::clear(self)
    }
}

impl<K: Ord, V> Default for SkipList<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Iter<'a, K, V> {
    list: &'a SkipList<K, V>,
    node: usize,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.node == NIL {
            return None;
        }
        let node = &self.list.nodes[self.node];
        self.node = node.next[0];
        node.entry.as_ref().map(|(k, v)| (k, v))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A different generator from the list's, for the test's own choices.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    #[test]
    fn behaves_like_a_btreemap() {
        let mut list = SkipList::new();
        let mut expected = BTreeMap::new();
        let mut rng = Rng(7);
        for round in 0..20_000 {
            let key = rng.below(5000);
            assert_eq!(list.insert(key, round), expected.insert(key, round), "round {round}");
            if round % 997 == 0 {
                let probe = rng.below(5000);
                assert_eq!(list.get(&probe), expected.get(&probe));
                assert_eq!(list.last_key(), expected.keys().next_back());
            }
        }
        assert_eq!(list.len(), expected.len());
        assert!(list.iter().eq(expected.iter()), "same entries in the same order");
        for key in 0..5000 {
            assert_eq!(list.get(&key), expected.get(&key));
        }
    }

    #[test]
    fn looks_keys_up_by_borrowed_form() {
        let mut list: SkipList<Vec<u8>, Option<Vec<u8>>> = SkipList::new();
        list.insert(b"b".to_vec(), Some(b"2".to_vec()));
        list.insert(b"a".to_vec(), None);
        list.insert(b"ab".to_vec(), Some(b"3".to_vec()));
        assert_eq!(list.get(b"a".as_slice()), Some(&None), "a tombstone is still an entry");
        assert_eq!(list.get(b"ab".as_slice()), Some(&Some(b"3".to_vec())));
        assert_eq!(list.get(b"c".as_slice()), None);
        let keys: Vec<&[u8]> = list.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(keys, [b"a".as_slice(), b"ab", b"b"]);
    }

    #[test]
    fn levels_thin_out_by_a_quarter() {
        let mut list = SkipList::new();
        for i in 0..100_000 {
            list.insert(i, ());
        }
        let on_level = |level: usize| list.nodes[1..].iter().filter(|node| node.next.len() > level).count();
        assert_eq!(on_level(0), 100_000);
        for level in 1..4 {
            let ratio = on_level(level) as f64 / on_level(level - 1) as f64;
            assert!((0.2..0.3).contains(&ratio), "level {level}: {ratio}");
        }
        assert!(list.levels <= MAX_LEVEL && list.levels >= 7, "about log4(100000) = 8 levels");
    }

    #[test]
    fn clear_empties_and_the_list_fills_again() {
        let mut list = SkipList::new();
        assert_eq!((list.last_key(), list.iter().next()), (None, None));
        for i in (0..100).rev() {
            list.insert(i, i * 2);
        }
        assert_eq!(list.last_key(), Some(&99));
        list.clear();
        assert!(list.is_empty());
        assert_eq!((list.get(&5), list.last_key(), list.iter().next()), (None, None, None));
        list.insert(3, 1);
        assert_eq!(list.iter().collect::<Vec<_>>(), [(&3, &1)]);
    }
}
