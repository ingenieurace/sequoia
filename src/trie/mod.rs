//! Tries: sorted maps from byte strings that branch on one byte of the key
//! at each level, so a lookup takes as many steps as the key has bytes,
//! however many keys there are. Keys that share a prefix share the path to
//! it, and a node's subtree holds exactly the keys that start with its path.
//!
//! Three of them, each the one before with one idea more:
//!
//! - [`Trie`]: a node for every byte of every key, its children in a sorted list.
//! - [`RadixTree`]: path compression. A node keeps the bytes that no key
//!   branches on as its `prefix`, so a chain of one-child nodes is one node.
//! - [`Art`] (adaptive radix tree): a node's children are in an array sized
//!   for how many it has, of 4, 16, 48 or 256, as small as it can be and as
//!   quick to search as a node of that size can be (see `adaptive.rs`).
//!
//! ```text
//! keys: "car", "cart", "cat", "dog"
//!
//! Trie                       RadixTree / Art
//! root                       root
//!  c ── a ── r* ── t*         c "a" ── r* ── t*
//!  │         └ t*             │        └ t*
//!  d ── o ── g*               d "og"*
//! ```
//!
//! (`*` marks a node where a key ends; `"a"` is a prefix.) A node may end a
//! key and have children too, as "car" and "cart" need: keys need not be
//! prefix-free. Its own key sorts before its children's, which are longer,
//! so visiting a node, then its children in byte order, visits keys in order.
//!
//! Each node where a key ends keeps the whole key, so iterating borrows keys
//! rather than building them. One tree does all three: the kind of children
//! ([`Children`]) and whether paths are compressed are its type parameters.

mod adaptive;
mod list;

pub use adaptive::Adaptive;
pub use list::List;

use std::cmp::Ordering;
use std::collections::{BTreeMap, btree_map};
use std::mem;
use std::ops::Bound;

/// A sorted map from byte strings, as an LSM tree's MemTable needs: every
/// structure here is one, and so are `BTreeMap` and the skip list.
pub trait SortedMap<V> {
    type Iter<'a>: Iterator<Item = (&'a [u8], &'a V)>
    where
        Self: 'a,
        V: 'a;

    /// Sets the value of a key, returning the one it replaces.
    fn insert(&mut self, key: Vec<u8>, value: V) -> Option<V>;
    fn get(&self, key: &[u8]) -> Option<&V>;
    /// The entries whose keys are `start` or above, in key order.
    fn iter_from(&self, start: &[u8]) -> Self::Iter<'_>;
    fn last_key(&self) -> Option<&[u8]>;
    fn len(&self) -> usize;
    fn clear(&mut self);

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A node's children, each under the byte that leads to it.
pub trait Children<V>: Default + Sized {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn get(&self, byte: u8) -> Option<&Node<V, Self>>;
    fn get_mut(&mut self, byte: u8) -> Option<&mut Node<V, Self>>;
    /// Adds a child under a byte that has none.
    fn add(&mut self, byte: u8, child: Box<Node<V, Self>>);
    fn remove(&mut self, byte: u8) -> Option<Box<Node<V, Self>>>;
    /// The child under the smallest byte that is `from` or above (`from`
    /// may be 256, past every byte), and its byte.
    fn next(&self, from: u16) -> Option<(u8, &Node<V, Self>)>;
    /// The child under the largest byte.
    fn last(&self) -> Option<&Node<V, Self>>;
}

pub struct Node<V, C> {
    /// Bytes every key below shares after the byte that leads here. Always
    /// empty in a [`Trie`], which does not compress paths.
    prefix: Vec<u8>,
    /// The key that ends here, whole, and its value.
    entry: Option<(Vec<u8>, V)>,
    children: C,
}

impl<V, C: Children<V>> Node<V, C> {
    fn new(prefix: Vec<u8>) -> Self {
        Node {
            prefix,
            entry: None,
            children: C::default(),
        }
    }
}

/// A trie whose children are of kind `C`, with paths compressed if `COMPRESS`.
pub struct Tree<V, C, const COMPRESS: bool> {
    root: Node<V, C>,
    len: usize,
}

/// A node per byte, children in a sorted list (see `list.rs`).
pub type Trie<V> = Tree<V, List<V>, false>;
/// Paths compressed, children in a sorted list.
pub type RadixTree<V> = Tree<V, List<V>, true>;
/// Paths compressed, children in arrays sized for how many there are.
pub type Art<V> = Tree<V, Adaptive<V>, true>;

impl<V, C: Children<V>, const COMPRESS: bool> Default for Tree<V, C, COMPRESS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V, C: Children<V>, const COMPRESS: bool> Tree<V, C, COMPRESS> {
    pub fn new() -> Self {
        Tree {
            root: Node::new(Vec::new()),
            len: 0,
        }
    }

    /// The nodes of the tree, for seeing what a kind of tree costs.
    pub fn nodes(&self) -> usize {
        fn count<V, C: Children<V>>(node: &Node<V, C>) -> usize {
            let mut n = 1;
            let mut from = 0;
            while let Some((byte, child)) = node.children.next(from) {
                n += count(child);
                from = byte as u16 + 1;
            }
            n
        }
        count(&self.root)
    }

    /// Removes a key, returning its value. A node left with no key and no
    /// children goes; in a radix tree, one left with no key and one child
    /// takes that child's place, its prefix joined to the child's.
    pub fn remove(&mut self, key: &[u8]) -> Option<V> {
        let value = Self::remove_below(&mut self.root, key, 0)?;
        self.len -= 1;
        Some(value)
    }

    /// Removes `key` from the subtree of `node`, whose path is `key[..depth]`.
    fn remove_below(node: &mut Node<V, C>, key: &[u8], depth: usize) -> Option<V> {
        if depth == key.len() {
            return node.entry.take().map(|(_, value)| value);
        }
        let byte = key[depth];
        let child = node.children.get_mut(byte)?;
        let below = depth + 1 + child.prefix.len();
        if !key[depth + 1..].starts_with(&child.prefix) {
            return None;
        }
        let value = Self::remove_below(child, key, below)?;
        if child.entry.is_none() {
            match child.children.len() {
                0 => drop(node.children.remove(byte)),
                1 if COMPRESS => {
                    let only = child.children.next(0).expect("one child").0;
                    let mut grandchild = child.children.remove(only).expect("it is there");
                    let mut prefix = mem::take(&mut child.prefix);
                    prefix.push(only);
                    prefix.append(&mut grandchild.prefix);
                    grandchild.prefix = prefix;
                    *child = *grandchild;
                }
                _ => {}
            }
        }
        Some(value)
    }
}

impl<V, C: Children<V>, const COMPRESS: bool> SortedMap<V> for Tree<V, C, COMPRESS> {
    type Iter<'a>
        = Iter<'a, V, C>
    where
        Self: 'a,
        V: 'a;

    /// Follows the key down. Where no child leads on, the rest of the key
    /// becomes a new node (a chain of them, one per byte, in a [`Trie`]).
    /// Where a child's prefix and the key part ways, the child splits: a new
    /// node takes the bytes they share, and the child keeps the rest below it.
    fn insert(&mut self, key: Vec<u8>, value: V) -> Option<V> {
        let mut node = &mut self.root;
        let mut depth = 0;
        loop {
            if depth == key.len() {
                let old = node.entry.replace((key, value)).map(|(_, old)| old);
                self.len += old.is_none() as usize;
                return old;
            }
            let byte = key[depth];
            let rest = &key[depth + 1..];
            let Some(child) = node.children.get_mut(byte) else {
                let leaf = leaf::<V, C, COMPRESS>(rest);
                node.children.add(byte, Box::new(leaf));
                // Down the new path to where the key ends.
                node = node.children.get_mut(byte).expect("just added");
                depth += 1 + node.prefix.len();
                continue;
            };
            let shared = child
                .prefix
                .iter()
                .zip(rest)
                .take_while(|(a, b)| a == b)
                .count();
            if shared < child.prefix.len() {
                let branch = child.prefix[shared];
                let head = child.prefix[..shared].to_vec();
                let mut old = mem::replace(child, Node::new(head));
                old.prefix.drain(..=shared);
                child.children.add(branch, Box::new(old));
            }
            node = node.children.get_mut(byte).expect("found above");
            depth += 1 + shared;
        }
    }

    fn get(&self, key: &[u8]) -> Option<&V> {
        let mut node = &self.root;
        let mut depth = 0;
        while depth < key.len() {
            node = node.children.get(key[depth])?;
            if !key[depth + 1..].starts_with(&node.prefix) {
                return None;
            }
            depth += 1 + node.prefix.len();
        }
        node.entry.as_ref().map(|(_, v)| v)
    }

    /// Goes down toward `start` as a lookup would, keeping on a stack each
    /// node passed and the byte after the one taken from it: the iterator
    /// goes on from there, through the subtrees right of the path, which
    /// hold the keys above `start`. A node whose prefix sorts above
    /// `start`'s bytes is wholly above it, one below wholly below.
    fn iter_from(&self, start: &[u8]) -> Iter<'_, V, C> {
        let mut iter = Iter {
            stack: Vec::new(),
            pending: None,
        };
        let mut node = &self.root;
        let mut depth = 0;
        loop {
            let rest = &start[depth.min(start.len())..];
            let shared = &rest[..rest.len().min(node.prefix.len())];
            match node.prefix[..shared.len()].cmp(shared) {
                Ordering::Less => return iter,
                Ordering::Greater => break,
                // `start` ends within or at the end of the path: all of it is `start` or above.
                Ordering::Equal if rest.len() <= node.prefix.len() => break,
                Ordering::Equal => {}
            }
            depth += node.prefix.len();
            let byte = start[depth];
            iter.stack.push((node, byte as u16 + 1));
            match node.children.get(byte) {
                Some(child) => {
                    node = child;
                    depth += 1;
                }
                None => return iter,
            }
        }
        iter.pending = node.entry.as_ref();
        iter.stack.push((node, 0));
        iter
    }

    fn last_key(&self) -> Option<&[u8]> {
        let mut node = &self.root;
        while let Some(child) = node.children.last() {
            node = child;
        }
        node.entry.as_ref().map(|(k, _)| k.as_slice())
    }

    fn len(&self) -> usize {
        self.len
    }

    fn clear(&mut self) {
        *self = Self::new();
    }
}

/// A node for the rest of a key that no node holds yet: one node with all
/// of it as its prefix, or in a [`Trie`], a chain of one node per byte.
fn leaf<V, C: Children<V>, const COMPRESS: bool>(rest: &[u8]) -> Node<V, C> {
    match rest.split_first() {
        Some((&first, more)) if !COMPRESS => {
            let mut node: Node<V, C> = Node::new(Vec::new());
            node.children
                .add(first, Box::new(leaf::<V, C, COMPRESS>(more)));
            node
        }
        _ => Node::new(if COMPRESS { rest.to_vec() } else { Vec::new() }),
    }
}

/// Visits each node, then its children in byte order: keys in order.
pub struct Iter<'a, V, C> {
    /// The nodes being visited, each with the byte of the next child to visit.
    stack: Vec<(&'a Node<V, C>, u16)>,
    /// The entry of the node just reached, to return first.
    pending: Option<&'a (Vec<u8>, V)>,
}

impl<'a, V, C: Children<V>> Iterator for Iter<'a, V, C> {
    type Item = (&'a [u8], &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((key, value)) = self.pending.take() {
                return Some((key, value));
            }
            let (node, from) = self.stack.last_mut()?;
            match node.children.next(*from) {
                Some((byte, child)) => {
                    *from = byte as u16 + 1;
                    self.pending = child.entry.as_ref();
                    self.stack.push((child, 0));
                }
                None => {
                    self.stack.pop();
                }
            }
        }
    }
}

impl<V> SortedMap<V> for BTreeMap<Vec<u8>, V> {
    type Iter<'a>
        = std::iter::Map<
        btree_map::Range<'a, Vec<u8>, V>,
        fn((&'a Vec<u8>, &'a V)) -> (&'a [u8], &'a V),
    >
    where
        V: 'a;

    fn insert(&mut self, key: Vec<u8>, value: V) -> Option<V> {
        BTreeMap::insert(self, key, value)
    }

    fn get(&self, key: &[u8]) -> Option<&V> {
        BTreeMap::get(self, key)
    }

    fn iter_from(&self, start: &[u8]) -> Self::Iter<'_> {
        self.range::<[u8], _>((Bound::Included(start), Bound::Unbounded))
            .map(|(k, v)| (k.as_slice(), v))
    }

    fn last_key(&self) -> Option<&[u8]> {
        self.last_key_value().map(|(k, _)| k.as_slice())
    }

    fn len(&self) -> usize {
        BTreeMap::len(self)
    }

    fn clear(&mut self) {
        BTreeMap::clear(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Random inserts, lookups, removes and scans on `map`, over short keys
    /// from a few bytes so that many share prefixes or are prefixes of
    /// others, checked against a `BTreeMap`.
    fn agrees_with_btreemap<C: Children<u64>, const COMPRESS: bool>(
        mut map: Tree<u64, C, COMPRESS>,
    ) {
        let mut expected = BTreeMap::new();
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut random = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let key = |r: u64| -> Vec<u8> {
            let alphabet = [0u8, 1, 97, 98, 255];
            (0..r % 6)
                .map(|i| alphabet[(r >> (8 + i * 3)) as usize % alphabet.len()])
                .collect()
        };
        for step in 0..30_000u64 {
            let r = random();
            let k = key(r);
            match r >> 60 {
                0..=6 => assert_eq!(
                    map.insert(k.clone(), step),
                    expected.insert(k.clone(), step),
                    "insert {k:?}"
                ),
                7..=10 => assert_eq!(map.remove(&k), expected.remove(&k), "remove {k:?}"),
                11..=13 => assert_eq!(map.get(&k), expected.get(&k), "get {k:?}"),
                _ => {
                    let got: Vec<_> = map.iter_from(&k).take(8).collect();
                    let want: Vec<_> = SortedMap::iter_from(&expected, &k).take(8).collect();
                    assert_eq!(got, want, "iter_from {k:?}");
                }
            }
            assert_eq!(map.len(), expected.len());
            assert_eq!(map.last_key(), SortedMap::last_key(&expected));
        }
        assert!(map.iter_from(&[]).eq(SortedMap::iter_from(&expected, &[])));
        // Wider bytes, to fill nodes of every size, then empty it.
        for i in 0..5000u64 {
            map.insert(i.to_be_bytes()[5..].to_vec(), i);
            expected.insert(i.to_be_bytes()[5..].to_vec(), i);
        }
        assert!(map.iter_from(&[]).eq(SortedMap::iter_from(&expected, &[])));
        let keys: Vec<Vec<u8>> = expected.keys().cloned().collect();
        for k in keys {
            assert_eq!(map.remove(&k), expected.remove(&k));
        }
        assert!(map.is_empty() && map.last_key().is_none());
        assert_eq!(map.nodes(), 1, "only the root is left");
    }

    #[test]
    fn trie_agrees_with_btreemap() {
        agrees_with_btreemap(Trie::new());
    }

    #[test]
    fn radix_tree_agrees_with_btreemap() {
        agrees_with_btreemap(RadixTree::new());
    }

    #[test]
    fn art_agrees_with_btreemap() {
        agrees_with_btreemap(Art::new());
    }

    #[test]
    fn compressing_paths_saves_nodes() {
        let keys = ["car", "cart", "cat", "dog"];
        let (mut trie, mut radix) = (Trie::new(), RadixTree::new());
        for key in keys {
            trie.insert(key.into(), ());
            radix.insert(key.into(), ());
        }
        assert_eq!(trie.nodes(), 9, "a node per byte, and the root");
        assert_eq!(radix.nodes(), 6, "root, c+a, r, t of cart, t of cat, d+og");
        assert_eq!(radix.root.children.get(b'c').unwrap().prefix, b"a");
        let keys_in_order: Vec<&[u8]> = radix.iter_from(b"cars").map(|(k, _)| k).collect();
        assert_eq!(keys_in_order, [&b"cart"[..], b"cat", b"dog"]);
    }
}
