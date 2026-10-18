//! B+ tree: a sorted set of byte-string keys stored in pages.
//!
//! ```text
//!                  root (internal)
//!                 [  k3  |  k6  ]
//!                /       |       \
//!     leaf [k1 k2] -> [k3 k4 k5] -> [k6 k7] -> (none)
//! ```
//!
//! Keys are kept in leaves, sorted, and the leaves are linked left to right
//! so a range scan walks them in order. An internal node with keys
//! `k0 .. kn-1` has children `c0 .. cn`: child `ci` holds the keys from
//! `k(i-1)` (inclusive) up to `ki` (exclusive).
//!
//! A node is one page, and holds as many keys as fit:
//!
//! ```text
//! 0       kind: 0 = leaf, 1 = internal
//! 1..3    key count n
//! 3..7    leaf: next leaf (u32::MAX = none); internal: first child c0
//! 7..     leaf: n x (u16 length, key bytes)
//!         internal: n x (u16 length, key bytes, u32 child to the right of the key)
//! ```
//!
//! A node is read into memory whole, changed there and written back whole:
//! simple, not fast. An insert into a full node splits it in two and adds a
//! key to its parent. A delete that leaves a node under a quarter full merges
//! it with a neighbour if both fit in one page, and otherwise moves keys
//! between the two so they are about equally full.
//!
//! The root never moves, so the page id the catalog records stays valid: a
//! root that splits moves its keys into two new pages and becomes their
//! parent, and a root left with one child takes over that child's keys.

use std::sync::Arc;

use crate::storage::buffer::pool::BufferPool;
use crate::storage::io::disk::{PAGE_USABLE, Page, PageId, decode_id, encode_id, read_u32, write_u32};
use crate::error::{Result, err};

/// The longest key the tree accepts. Small enough that a full node always
/// splits into two halves that fit in a page.
pub const MAX_KEY_SIZE: usize = 1024;

const HEADER_SIZE: usize = 7;
const LEAF: u8 = 0;
const INTERNAL: u8 = 1;

pub struct BPlusTree {
    pool: Arc<BufferPool>,
    root: PageId,
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Leaf { keys: Vec<Vec<u8>>, next: Option<PageId> },
    Internal { keys: Vec<Vec<u8>>, children: Vec<PageId> },
}

impl BPlusTree {
    /// Creates an empty tree: a root page that is an empty leaf.
    pub fn create(pool: Arc<BufferPool>) -> Result<Self> {
        let mut page = pool.new_page()?;
        Node::Leaf { keys: Vec::new(), next: None }.write(&mut page);
        let root = page.id();
        drop(page);
        Ok(BPlusTree { pool, root })
    }

    /// Opens the tree whose root page is `root`.
    pub fn open(pool: Arc<BufferPool>, root: PageId) -> Self {
        BPlusTree { pool, root }
    }

    pub fn root_page(&self) -> PageId {
        self.root
    }

    pub fn check_key(key: &[u8]) -> Result<()> {
        if key.len() > MAX_KEY_SIZE {
            return err(format!("index key is {} bytes, the maximum is {MAX_KEY_SIZE}", key.len()));
        }
        Ok(())
    }

    /// Adds a key. Returns false if it was already there.
    pub fn insert(&mut self, key: &[u8]) -> Result<bool> {
        Self::check_key(key)?;
        let mut root = self.read(self.root)?;
        let Some(inserted) = self.insert_into(&mut root, key)? else {
            return Ok(false);
        };
        if root.size() <= PAGE_USABLE {
            self.write(self.root, &root)?;
        } else {
            // Split the root's contents into two new pages and point the root at them.
            let (left, separator, right) = root.split();
            let left_id = self.allocate(&left)?;
            let right_id = self.allocate(&right)?;
            if let Node::Leaf { .. } = left {
                self.write(left_id, &left.with_next(Some(right_id)))?;
            }
            self.write(self.root, &Node::Internal { keys: vec![separator], children: vec![left_id, right_id] })?;
        }
        Ok(inserted)
    }

    /// Removes a key. Returns false if it was not there.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        let mut root = self.read(self.root)?;
        if !self.delete_from(&mut root, key)? {
            return Ok(false);
        }
        // A root with a single child takes over the child's keys.
        while let Node::Internal { keys, children } = &root
            && keys.is_empty()
        {
            let child = children[0];
            root = self.read(child)?;
            self.pool.delete_page(child)?;
        }
        self.write(self.root, &root)?;
        Ok(true)
    }

    /// Iterates over the keys `>= start` in order.
    ///
    /// Each leaf is read when the scan reaches it, so changes made during the
    /// scan may be seen. Callers that modify the tree should collect first.
    pub fn range(&self, start: &[u8]) -> Range<'_> {
        Range { tree: self, start: Some(start.to_vec()), next_leaf: None, pending: Vec::new().into_iter() }
    }

    /// Returns all of the tree's pages to the free list.
    pub fn destroy(self) -> Result<()> {
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            if let Node::Internal { children, .. } = self.read(id)? {
                stack.extend(children);
            }
            self.pool.delete_page(id)?;
        }
        Ok(())
    }

    /// Inserts into the subtree whose root is `node`, which the caller writes
    /// back. Returns `None` if the key was already there. A child that
    /// overflows is split here, so `node` may be left too big for a page.
    fn insert_into(&self, node: &mut Node, key: &[u8]) -> Result<Option<bool>> {
        match node {
            Node::Leaf { keys, .. } => match keys.binary_search_by(|k| k.as_slice().cmp(key)) {
                Ok(_) => Ok(None),
                Err(i) => {
                    keys.insert(i, key.to_vec());
                    Ok(Some(true))
                }
            },
            Node::Internal { keys, children } => {
                let i = child_index(keys, key);
                let mut child = self.read(children[i])?;
                let Some(inserted) = self.insert_into(&mut child, key)? else {
                    return Ok(None);
                };
                if child.size() <= PAGE_USABLE {
                    self.write(children[i], &child)?;
                } else {
                    let (left, separator, right) = child.split();
                    let right_id = self.allocate(&right)?;
                    self.write(children[i], &left.with_next(Some(right_id)))?;
                    keys.insert(i, separator);
                    children.insert(i + 1, right_id);
                }
                Ok(Some(inserted))
            }
        }
    }

    /// Deletes from the subtree whose root is `node`, which the caller writes
    /// back. A child left under a quarter full is rebalanced with a neighbour.
    fn delete_from(&self, node: &mut Node, key: &[u8]) -> Result<bool> {
        match node {
            Node::Leaf { keys, .. } => match keys.binary_search_by(|k| k.as_slice().cmp(key)) {
                Ok(i) => {
                    keys.remove(i);
                    Ok(true)
                }
                Err(_) => Ok(false),
            },
            Node::Internal { keys, children } => {
                let i = child_index(keys, key);
                let mut child = self.read(children[i])?;
                if !self.delete_from(&mut child, key)? {
                    return Ok(false);
                }
                self.write(children[i], &child)?;
                if child.size() < PAGE_USABLE / 4 {
                    // Pair the child with its right neighbour, or its left one if it is the last.
                    self.rebalance(keys, children, i.min(children.len() - 2))?;
                }
                Ok(true)
            }
        }
    }

    /// Rebalances the children `i` and `i + 1` of an internal node: merges
    /// them if they fit in one page, and otherwise splits their keys evenly.
    fn rebalance(&self, keys: &mut Vec<Vec<u8>>, children: &mut Vec<PageId>, i: usize) -> Result<()> {
        let (left_id, right_id) = (children[i], children[i + 1]);
        let merged = match (self.read(left_id)?, self.read(right_id)?) {
            (Node::Leaf { keys: mut left, .. }, Node::Leaf { keys: right, next }) => {
                left.extend(right);
                Node::Leaf { keys: left, next }
            }
            (
                Node::Internal { keys: mut left, children: mut left_children },
                Node::Internal { keys: right, children: right_children },
            ) => {
                // The separator comes down between the two halves.
                left.push(keys[i].clone());
                left.extend(right);
                left_children.extend(right_children);
                Node::Internal { keys: left, children: left_children }
            }
            _ => return err("corrupted B+ tree: siblings of different kinds"),
        };

        if merged.size() <= PAGE_USABLE {
            self.write(left_id, &merged)?;
            self.pool.delete_page(right_id)?;
            keys.remove(i);
            children.remove(i + 1);
        } else {
            let (left, separator, right) = merged.split();
            self.write(left_id, &left.with_next(Some(right_id)))?;
            self.write(right_id, &right)?;
            keys[i] = separator;
        }
        Ok(())
    }

    fn read(&self, id: PageId) -> Result<Node> {
        Node::read(&*self.pool.fetch_read(id)?)
    }

    fn write(&self, id: PageId, node: &Node) -> Result<()> {
        node.write(&mut *self.pool.fetch_write(id)?);
        Ok(())
    }

    fn allocate(&self, node: &Node) -> Result<PageId> {
        let mut page = self.pool.new_page()?;
        node.write(&mut page);
        Ok(page.id())
    }
}

/// Which child of an internal node with these keys may hold `key`.
fn child_index(keys: &[Vec<u8>], key: &[u8]) -> usize {
    keys.partition_point(|k| k.as_slice() <= key)
}

impl Node {
    fn keys(&self) -> &[Vec<u8>] {
        match self {
            Node::Leaf { keys, .. } | Node::Internal { keys, .. } => keys,
        }
    }

    /// Bytes the node takes in a page.
    fn size(&self) -> usize {
        HEADER_SIZE + self.keys().iter().map(|k| self.entry_size(k)).sum::<usize>()
    }

    /// Bytes one key takes in this node.
    fn entry_size(&self, key: &[u8]) -> usize {
        match self {
            Node::Leaf { .. } => 2 + key.len(),
            Node::Internal { .. } => 2 + key.len() + 4,
        }
    }

    /// Splits a node into a left half, the separator its parent gets, and a
    /// right half, putting about half of the bytes on each side. A leaf keeps
    /// its separator as the right half's first key, while an internal node
    /// moves it up. The left half of a leaf still links to the old next leaf.
    fn split(self) -> (Node, Vec<u8>, Node) {
        let total = self.size();
        let mut mid = 0;
        let mut bytes = HEADER_SIZE;
        for key in self.keys() {
            if bytes >= total / 2 {
                break;
            }
            bytes += self.entry_size(key);
            mid += 1;
        }
        // Both halves keep at least one key; an internal node also moves one up.
        let last = match self {
            Node::Leaf { .. } => self.keys().len() - 1,
            Node::Internal { .. } => self.keys().len() - 2,
        };
        let mid = mid.clamp(1, last);
        match self {
            Node::Leaf { mut keys, next } => {
                let right = keys.split_off(mid);
                let separator = right[0].clone();
                (Node::Leaf { keys, next }, separator, Node::Leaf { keys: right, next })
            }
            Node::Internal { mut keys, mut children } => {
                let right_keys = keys.split_off(mid + 1);
                let separator = keys.pop().expect("mid >= 1");
                let right_children = children.split_off(mid + 1);
                (
                    Node::Internal { keys, children },
                    separator,
                    Node::Internal { keys: right_keys, children: right_children },
                )
            }
        }
    }

    /// Points a leaf at a new next leaf; leaves an internal node unchanged.
    fn with_next(self, next: Option<PageId>) -> Node {
        match self {
            Node::Leaf { keys, .. } => Node::Leaf { keys, next },
            internal => internal,
        }
    }

    fn read(page: &Page) -> Result<Node> {
        let count = u16::from_le_bytes([page[1], page[2]]) as usize;
        let first = read_u32(page, 3);
        let mut pos = HEADER_SIZE;
        let mut keys = Vec::with_capacity(count);
        let mut children = vec![PageId(first)];
        let child_size = if page[0] == INTERNAL { 4 } else { 0 };
        for _ in 0..count {
            if pos + 2 > PAGE_USABLE {
                return err("corrupted B+ tree node");
            }
            let len = u16::from_le_bytes([page[pos], page[pos + 1]]) as usize;
            pos += 2;
            if pos + len + child_size > PAGE_USABLE {
                return err("corrupted B+ tree node");
            }
            keys.push(page[pos..pos + len].to_vec());
            pos += len;
            if page[0] == INTERNAL {
                children.push(PageId(read_u32(page, pos)));
                pos += 4;
            }
        }
        match page[0] {
            LEAF => Ok(Node::Leaf { keys, next: decode_id(first) }),
            INTERNAL => Ok(Node::Internal { keys, children }),
            kind => err(format!("corrupted B+ tree node: kind {kind}")),
        }
    }

    fn write(&self, page: &mut Page) {
        debug_assert!(self.size() <= PAGE_USABLE);
        let (kind, first, children) = match self {
            Node::Leaf { next, .. } => (LEAF, encode_id(*next), &[][..]),
            Node::Internal { children, .. } => (INTERNAL, children[0].0, &children[1..]),
        };
        page[0] = kind;
        page[1..3].copy_from_slice(&(self.keys().len() as u16).to_le_bytes());
        write_u32(page, 3, first);
        let mut pos = HEADER_SIZE;
        for (i, key) in self.keys().iter().enumerate() {
            page[pos..pos + 2].copy_from_slice(&(key.len() as u16).to_le_bytes());
            pos += 2;
            page[pos..pos + key.len()].copy_from_slice(key);
            pos += key.len();
            if let Some(child) = children.get(i) {
                write_u32(page, pos, child.0);
                pos += 4;
            }
        }
    }
}

pub struct Range<'a> {
    tree: &'a BPlusTree,
    /// Set until the first leaf is found.
    start: Option<Vec<u8>>,
    next_leaf: Option<PageId>,
    /// Keys of the current leaf not yet returned.
    pending: std::vec::IntoIter<Vec<u8>>,
}

impl Range<'_> {
    /// Descends to the leaf that may hold `start`, keeping its keys `>= start`.
    fn seek(&mut self, start: &[u8]) -> Result<()> {
        let mut node = self.tree.read(self.tree.root)?;
        while let Node::Internal { keys, children } = node {
            node = self.tree.read(children[child_index(&keys, start)])?;
        }
        let Node::Leaf { mut keys, next } = node else { unreachable!() };
        keys.drain(..keys.partition_point(|k| k.as_slice() < start));
        self.next_leaf = next;
        self.pending = keys.into_iter();
        Ok(())
    }
}

impl Iterator for Range<'_> {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(start) = self.start.take()
            && let Err(e) = self.seek(&start)
        {
            return Some(Err(e));
        }
        loop {
            if let Some(key) = self.pending.next() {
                return Some(Ok(key));
            }
            let id = self.next_leaf.take()?;
            match self.tree.read(id) {
                Ok(Node::Leaf { keys, next }) => {
                    self.next_leaf = next;
                    self.pending = keys.into_iter();
                }
                Ok(_) => return Some(err("corrupted B+ tree: a leaf links to an internal node")),
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::storage::DiskManager;

    fn pool(frames: usize) -> Arc<BufferPool> {
        Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), frames))
    }

    fn key(i: u32) -> Vec<u8> {
        let mut bytes = i.to_be_bytes().to_vec();
        bytes.resize(4 + (i % 50) as usize, b'.');
        bytes
    }

    fn all(tree: &BPlusTree) -> Vec<Vec<u8>> {
        tree.range(&[]).collect::<Result<_>>().unwrap()
    }

    /// Height of the tree, checking that every leaf is at the same depth.
    fn height(tree: &BPlusTree, id: PageId) -> usize {
        match tree.read(id).unwrap() {
            Node::Leaf { .. } => 1,
            Node::Internal { children, .. } => {
                let heights: BTreeSet<usize> = children.iter().map(|&c| height(tree, c)).collect();
                assert_eq!(heights.len(), 1, "unbalanced");
                heights.first().unwrap() + 1
            }
        }
    }

    /// A small xorshift generator, so the tests need no dependencies.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u32) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as u32
        }
    }

    #[test]
    fn inserts_split_nodes_and_keep_keys_sorted() {
        let mut tree = BPlusTree::create(pool(8)).unwrap();
        for i in (0..20_000).rev() {
            assert!(tree.insert(&key(i)).unwrap());
        }
        assert!(!tree.insert(&key(42)).unwrap(), "already there");
        assert_eq!(all(&tree), (0..20_000).map(key).collect::<Vec<_>>());
        assert_eq!(height(&tree, tree.root), 3, "20000 keys need two levels of internal nodes");
    }

    #[test]
    fn range_starts_at_the_first_key_not_below_start() {
        let mut tree = BPlusTree::create(pool(8)).unwrap();
        for i in (0..2000u32).step_by(2) {
            tree.insert(&i.to_be_bytes()).unwrap();
        }
        let from = |start: u32| tree.range(&start.to_be_bytes()).next().map(|k| k.unwrap());
        assert_eq!(from(0), Some(0u32.to_be_bytes().to_vec()));
        assert_eq!(from(1001), Some(1002u32.to_be_bytes().to_vec()));
        assert_eq!(from(1998), Some(1998u32.to_be_bytes().to_vec()));
        assert_eq!(from(1999), None);
        assert_eq!(tree.range(&1000u32.to_be_bytes()).count(), 500);
    }

    #[test]
    fn deletes_merge_nodes_and_free_their_pages() {
        let pool = pool(8);
        let mut tree = BPlusTree::create(pool.clone()).unwrap();
        for i in 0..5000 {
            tree.insert(&key(i)).unwrap();
        }
        let pages = pool.disk().page_count();
        for i in 0..5000 {
            assert!(tree.delete(&key(i)).unwrap());
        }
        assert!(!tree.delete(&key(0)).unwrap(), "already deleted");
        assert!(all(&tree).is_empty());
        assert_eq!(height(&tree, tree.root), 1, "back to a single leaf");

        // The freed pages are handed out again before the file grows.
        for i in 0..5000 {
            tree.insert(&key(i)).unwrap();
        }
        assert_eq!(pool.disk().page_count(), pages);
    }

    #[test]
    fn random_inserts_and_deletes_match_a_btreeset() {
        let mut tree = BPlusTree::create(pool(4)).unwrap();
        let mut expected = BTreeSet::new();
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for round in 0..20_000 {
            let k = key(rng.below(3000));
            if rng.below(3) == 0 {
                assert_eq!(tree.delete(&k).unwrap(), expected.remove(&k), "round {round}");
            } else {
                assert_eq!(tree.insert(&k).unwrap(), expected.insert(k), "round {round}");
            }
        }
        assert_eq!(all(&tree), expected.into_iter().collect::<Vec<_>>());
        height(&tree, tree.root);
    }

    #[test]
    fn large_keys_still_split() {
        let mut tree = BPlusTree::create(pool(8)).unwrap();
        let big = |i: u8| vec![i; MAX_KEY_SIZE];
        for i in 0..100 {
            tree.insert(&big(i)).unwrap();
        }
        assert_eq!(all(&tree), (0..100).map(big).collect::<Vec<_>>());
        for i in 0..100 {
            tree.delete(&big(i)).unwrap();
        }
        assert!(all(&tree).is_empty());
        assert!(tree.insert(&vec![0; MAX_KEY_SIZE + 1]).is_err());
    }

    #[test]
    fn reopens_from_its_root_and_destroys_all_pages() {
        let pool = pool(8);
        let mut tree = BPlusTree::create(pool.clone()).unwrap();
        for i in 0..3000 {
            tree.insert(&key(i)).unwrap();
        }
        let root = tree.root_page();
        let pages = pool.disk().page_count();

        let tree = BPlusTree::open(pool.clone(), root);
        assert_eq!(all(&tree).len(), 3000);
        tree.destroy().unwrap();

        let mut other = BPlusTree::create(pool.clone()).unwrap();
        for i in 0..3000 {
            other.insert(&key(i)).unwrap();
        }
        assert_eq!(pool.disk().page_count(), pages);
    }
}
