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
//! Searches read the pages on their way down in place, without copying keys
//! out, and an insert or delete that changes only its leaf (most of them)
//! shifts that leaf's bytes in place and writes no other page. A change that
//! must split or merge nodes instead reads the nodes on its path into memory
//! whole, changes them there and writes them back whole. An insert into a
//! full node splits it in two and adds a key to its parent. A delete that
//! leaves a node under a quarter full merges it with a neighbour if both fit
//! in one page, and otherwise moves keys between the two so they are about
//! equally full; the parent's separator may grow then, and a parent that no
//! longer fits its page splits too. A range scan copies one leaf at a time
//! and takes its keys out as it reaches them.
//!
//! The root never moves, so the page id the catalog records stays valid: a
//! root that splits moves its keys into two new pages and becomes their
//! parent, and a root left with one child takes over that child's keys.
//!
//! **Concurrency.** Any number of threads may search, insert and delete at
//! once. Each page has a latch, its buffer pool frame's read-write lock, and
//! they are taken by latch crabbing: from the root down, a node's latch is
//! released only once its child's is held, so no change can slip between
//! them, and never upward, so no two threads wait for each other.
//!
//! ```text
//! search    read-latch root, read-latch child, release root, ...          down to the leaf
//! insert    as a search, but write-latch the leaf (its parent still held)  then change it in place
//!   or, if the leaf must split: write-latch from the root, releasing every
//!   ancestor at a node that is safe, which a split below cannot overflow
//! ```
//!
//! A delete does the same, where a safe node is one a merge below cannot
//! leave under a quarter full, nor a longer separator overflow. Above a
//! safe node nothing changes, so the latches held are exactly those of the
//! nodes that may. Sibling nodes a rebalance needs are latched with their
//! parent held, and a node is freed only while write-latched, its parent
//! too, so no one can be reading it.
//!
//! Scans do not follow leaf links, which a merge may free under them: on the
//! way down to a leaf a scan notes the smallest separator to its right, the
//! leaf's upper bound (B-link trees keep it in each node), and after the
//! leaf goes down again to the leaf holding that key. The keys of each leaf
//! are below that bound when they are copied, so a scan returns keys in
//! order, each once, and every key present for the whole scan.

use std::mem;
use std::sync::Arc;

use crate::error::{Result, err};
use crate::storage::buffer::pool::{BufferPool, PageWriteGuard};
use crate::storage::io::disk::{
    PAGE_SIZE, PAGE_USABLE, Page, PageId, decode_id, encode_id, read_u32, write_u32,
};

/// The longest key the tree accepts. Small enough that a full node always
/// splits into two halves that fit in a page.
pub const MAX_KEY_SIZE: usize = 1024;

const HEADER_SIZE: usize = 7;
const LEAF: u8 = 0;
const INTERNAL: u8 = 1;

/// A clone is another handle to the same tree, on the same pages: the
/// pages hold all of its state, so threads may each read through one.
#[derive(Clone)]
pub struct BPlusTree {
    pool: Arc<BufferPool>,
    root: PageId,
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Leaf {
        keys: Vec<Vec<u8>>,
        next: Option<PageId>,
    },
    Internal {
        keys: Vec<Vec<u8>>,
        children: Vec<PageId>,
    },
}

impl BPlusTree {
    /// Creates an empty tree: a root page that is an empty leaf.
    pub fn create(pool: Arc<BufferPool>) -> Result<Self> {
        let mut page = pool.new_page()?;
        Node::Leaf {
            keys: Vec::new(),
            next: None,
        }
        .write(&mut page);
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
            return err(format!(
                "index key is {} bytes, the maximum is {MAX_KEY_SIZE}",
                key.len()
            ));
        }
        Ok(())
    }

    /// Adds a key. Returns false if it was already there.
    pub fn insert(&self, key: &[u8]) -> Result<bool> {
        Self::check_key(key)?;
        {
            let mut page = self.leaf_for_write(key)?;
            let (at, found, end) = leaf_search(&page, key)?;
            if found {
                return Ok(false);
            }
            let size = 2 + key.len();
            if end + size <= PAGE_USABLE {
                page.copy_within(at..end, at + size);
                page[at..at + 2].copy_from_slice(&(key.len() as u16).to_le_bytes());
                page[at + 2..at + size].copy_from_slice(key);
                let n = count(&page);
                set_count(&mut page, n + 1);
                return Ok(true);
            }
        }
        // The leaf is full: split it, and its parents if need be.
        self.restructure(key, Change::Insert)
    }

    /// Removes a key. Returns false if it was not there.
    pub fn delete(&self, key: &[u8]) -> Result<bool> {
        {
            let mut page = self.leaf_for_write(key)?;
            let (at, found, end) = leaf_search(&page, key)?;
            if !found {
                return Ok(false);
            }
            // A leaf left at least a quarter full, or the root, needs no rebalancing.
            let size = 2 + key.len();
            if page.id() == self.root || end - size >= PAGE_USABLE / 4 {
                page.copy_within(at + size..end, at);
                let n = count(&page);
                set_count(&mut page, n - 1);
                return Ok(true);
            }
        }
        self.restructure(key, Change::Delete)
    }

    /// Iterates over the keys `>= start` in order.
    ///
    /// Each leaf is read when the scan reaches it, so changes made during the
    /// scan may be seen. Callers that modify the tree should collect first.
    pub fn range(&self, start: &[u8]) -> Range<'_> {
        Range {
            tree: self,
            next: Some(start.to_vec()),
            leaf: None,
            left: 0,
            pos: 0,
            bound: None,
        }
    }

    /// The leaf that holds `key` or would, write-latched. The way down is
    /// read-latched, each node until its child is; the leaf's parent until
    /// the leaf is write-latched, so that no split or merge, which would
    /// need the parent's write latch, can move `key` out of the leaf
    /// meanwhile.
    fn leaf_for_write(&self, key: &[u8]) -> Result<PageWriteGuard<'_>> {
        'again: loop {
            let mut parent = None;
            let mut id = self.root;
            loop {
                let page = self.pool.fetch_read(id)?;
                if page[0] == LEAF {
                    drop(page);
                    let leaf = self.pool.fetch_write(id)?;
                    // Only the root, which has no parent, can stop being a
                    // leaf meanwhile: by splitting. Then go down again.
                    if leaf[0] != LEAF {
                        continue 'again;
                    }
                    drop(parent);
                    return Ok(leaf);
                }
                id = child_of(&page, key)?.0;
                parent = Some(page);
            }
        }
    }

    /// Makes a change that may split or merge nodes. Write-latches the way
    /// down, releasing the nodes above each safe one (see [`Node::safe`]),
    /// changes the leaf, and then, from the leaf up, splits each node that
    /// overflows and rebalances each that underflows, which changes only
    /// the nodes held. Writes back only the nodes that changed.
    fn restructure(&self, key: &[u8], change: Change) -> Result<bool> {
        let mut path: Vec<Level> = Vec::new();
        let mut id = self.root;
        loop {
            let guard = self.pool.fetch_write(id)?;
            let node = Node::read(&guard)?;
            if node.safe(change, key.len(), id == self.root) {
                path.clear(); // releases the ancestors, from the top
            }
            let child = match &node {
                Node::Internal { keys, children } => {
                    Some(child_index(keys, key)).map(|i| (i, children[i]))
                }
                Node::Leaf { .. } => None,
            };
            path.push(Level {
                id,
                guard,
                node,
                child: child.map_or(0, |(i, _)| i),
                changed: false,
            });
            match child {
                Some((_, child)) => id = child,
                None => break,
            }
        }

        let leaf = path.last_mut().expect("the leaf");
        let Node::Leaf { keys, .. } = &mut leaf.node else {
            unreachable!("the path ends at a leaf")
        };
        match (keys.binary_search_by(|k| k.as_slice().cmp(key)), change) {
            (Err(i), Change::Insert) => keys.insert(i, key.to_vec()),
            (Ok(i), Change::Delete) => _ = keys.remove(i),
            _ => return Ok(false),
        }
        leaf.changed = true;

        while let Some(mut level) = path.pop() {
            let parent = path.last_mut();
            let root = level.id == self.root;
            if level.node.size() > PAGE_USABLE {
                let (left, separator, right) = mem::replace(&mut level.node, Node::empty()).split();
                if root {
                    // The root moves its keys into two new pages and becomes their parent.
                    let right_id = self.allocate(&right)?;
                    let left_id = self.allocate(&left.with_next(Some(right_id)))?;
                    level.node = Node::Internal {
                        keys: vec![separator],
                        children: vec![left_id, right_id],
                    };
                } else {
                    let parent = parent.expect("an unsafe node's parent is held");
                    let right_id = self.allocate(&right)?;
                    level.node = left.with_next(Some(right_id));
                    let Node::Internal { keys, children } = &mut parent.node else {
                        unreachable!("a parent")
                    };
                    keys.insert(parent.child, separator);
                    children.insert(parent.child + 1, right_id);
                    parent.changed = true;
                }
            } else if change == Change::Delete && !root && level.node.size() < PAGE_USABLE / 4 {
                // Only a delete rebalances: a node an insert finds under a
                // quarter full (with long keys, a split may leave one so)
                // only grows, and its parent may not be held.
                let parent = parent.expect("an unsafe node's parent is held");
                parent.changed = true;
                if self.rebalance(&mut level, parent)? {
                    // Merged into its left sibling. Its latch goes with the
                    // page: no one can reach it through the parent now.
                    let id = level.id;
                    drop(level);
                    self.pool.delete_unreachable_page(id)?;
                    continue;
                }
            }
            if root {
                // A root with a single child takes over the child's keys.
                while let Node::Internal { keys, children } = &level.node
                    && keys.is_empty()
                {
                    let child = children[0];
                    level.node = Node::read(&*self.pool.fetch_write(child)?)?;
                    self.pool.delete_unreachable_page(child)?;
                }
            }
            if level.changed {
                level.node.write(&mut level.guard);
            }
        }
        Ok(true)
    }

    /// Rebalances a node left under a quarter full with a sibling, which is
    /// write-latched under their parent: the one to its right, or to its left
    /// if it is the last child. Merges the two into the left one if they fit
    /// in one page, freeing the right one, and otherwise splits their keys
    /// evenly. Returns true if `level` merged into its left sibling, for the
    /// caller to free its page.
    fn rebalance(&self, level: &mut Level, parent: &mut Level) -> Result<bool> {
        let Node::Internal { keys, children } = &mut parent.node else {
            unreachable!("a parent")
        };
        let i = parent.child;
        let pair = i.min(children.len() - 2); // the pair is children `pair` and `pair + 1`
        let sibling_id = children[if pair == i { i + 1 } else { pair }];
        let mut sibling = self.pool.fetch_write(sibling_id)?;
        let node = mem::replace(&mut level.node, Node::empty());
        let (left, right) = match pair == i {
            true => (node, Node::read(&sibling)?),
            false => (Node::read(&sibling)?, node),
        };
        let merged = match (left, right) {
            (Node::Leaf { keys: mut left, .. }, Node::Leaf { keys: right, next }) => {
                left.extend(right);
                Node::Leaf { keys: left, next }
            }
            (
                Node::Internal {
                    keys: mut left,
                    children: mut left_children,
                },
                Node::Internal {
                    keys: right,
                    children: right_children,
                },
            ) => {
                // The separator comes down between the two halves.
                left.push(keys[pair].clone());
                left.extend(right);
                left_children.extend(right_children);
                Node::Internal {
                    keys: left,
                    children: left_children,
                }
            }
            _ => return err("corrupted B+ tree: siblings of different kinds"),
        };
        level.changed = true;

        if merged.size() <= PAGE_USABLE {
            keys.remove(pair);
            children.remove(pair + 1);
            if pair == i {
                level.node = merged;
                drop(sibling);
                self.pool.delete_unreachable_page(sibling_id)?;
                Ok(false)
            } else {
                merged.write(&mut sibling);
                Ok(true)
            }
        } else {
            let (left, separator, right) = merged.split();
            keys[pair] = separator;
            if pair == i {
                level.node = left.with_next(Some(sibling_id));
                right.write(&mut sibling);
            } else {
                left.with_next(Some(level.id)).write(&mut sibling);
                level.node = right;
            }
            Ok(false)
        }
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

    fn read(&self, id: PageId) -> Result<Node> {
        Node::read(&*self.pool.fetch_read(id)?)
    }

    fn allocate(&self, node: &Node) -> Result<PageId> {
        let mut page = self.pool.new_page()?;
        node.write(&mut page);
        Ok(page.id())
    }
}

/// A change to the keys, for a restructuring to know which nodes it may change.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Change {
    Insert,
    Delete,
}

/// A node on a restructuring's way down, write-latched.
struct Level<'a> {
    id: PageId,
    guard: PageWriteGuard<'a>,
    node: Node,
    /// The child the way goes on to, in an internal node.
    child: usize,
    changed: bool,
}

/// The most an internal node's entry can take: a separator a split below
/// adds, or a rebalance swaps in.
const MAX_ENTRY: usize = 2 + MAX_KEY_SIZE + 4;

/// Of an internal node's page, the child that may hold `key`, and, if there
/// is one, the separator to its right: every key below that child is less.
fn child_of<'p>(page: &'p Page, key: &[u8]) -> Result<(PageId, Option<&'p [u8]>)> {
    if page[0] != INTERNAL {
        return err(format!("corrupted B+ tree node: kind {}", page[0]));
    }
    // The child to the right of the last key <= `key`.
    let mut child = read_u32(page, 3);
    let mut pos = HEADER_SIZE;
    for _ in 0..count(page) {
        let (k, next) = entry(page, pos, 4)?;
        if k > key {
            return Ok((PageId(child), Some(k)));
        }
        child = read_u32(page, next - 4);
        pos = next;
    }
    Ok((PageId(child), None))
}

/// Which child of an internal node with these keys may hold `key`.
fn child_index(keys: &[Vec<u8>], key: &[u8]) -> usize {
    keys.partition_point(|k| k.as_slice() <= key)
}

/// The number of keys in a node's page.
fn count(page: &Page) -> usize {
    u16::from_le_bytes([page[1], page[2]]) as usize
}

fn set_count(page: &mut Page, count: usize) {
    page[1..3].copy_from_slice(&(count as u16).to_le_bytes());
}

/// The key of the entry at `pos` in a node's page, and where the next entry
/// starts. `child_size` is 4 in an internal node, whose entries end with a
/// child, and 0 in a leaf.
fn entry(page: &Page, pos: usize, child_size: usize) -> Result<(&[u8], usize)> {
    if pos + 2 > PAGE_USABLE {
        return err("corrupted B+ tree node");
    }
    let len = u16::from_le_bytes([page[pos], page[pos + 1]]) as usize;
    let end = pos + 2 + len + child_size;
    if end > PAGE_USABLE {
        return err("corrupted B+ tree node");
    }
    Ok((&page[pos + 2..pos + 2 + len], end))
}

/// Looks for `key` in a leaf's page: where its entry is or would go,
/// whether it is there, and where the leaf's entries end.
fn leaf_search(page: &Page, key: &[u8]) -> Result<(usize, bool, usize)> {
    if page[0] != LEAF {
        return err("corrupted B+ tree: expected a leaf");
    }
    let mut pos = HEADER_SIZE;
    let mut place = None;
    for _ in 0..count(page) {
        let (k, next) = entry(page, pos, 0)?;
        if place.is_none() && k >= key {
            place = Some((pos, k == key));
        }
        pos = next;
    }
    let (at, found) = place.unwrap_or((pos, false));
    Ok((at, found, pos))
}

impl Node {
    /// A leaf with no keys, which a node taken out to change stands in for.
    fn empty() -> Node {
        Node::Leaf {
            keys: Vec::new(),
            next: None,
        }
    }

    /// Whether a change of `key` below this node cannot spread above it: an
    /// insert cannot overflow it, by adding the key to a leaf or a separator
    /// to an internal node, and a delete cannot leave it under a quarter full
    /// (the root: with no key) nor overflow it with a longer separator.
    fn safe(&self, change: Change, key_len: usize, root: bool) -> bool {
        let size = self.size();
        match (self, change) {
            (Node::Leaf { .. }, Change::Insert) => size + 2 + key_len <= PAGE_USABLE,
            (Node::Internal { .. }, Change::Insert) => size + MAX_ENTRY <= PAGE_USABLE,
            (Node::Leaf { .. }, Change::Delete) => {
                root || size.saturating_sub(2 + key_len) >= PAGE_USABLE / 4
            }
            (Node::Internal { keys, .. }, Change::Delete) => {
                let stays = if root {
                    keys.len() >= 2
                } else {
                    size.saturating_sub(MAX_ENTRY) >= PAGE_USABLE / 4
                };
                stays && size + MAX_ENTRY <= PAGE_USABLE
            }
        }
    }

    fn keys(&self) -> &[Vec<u8>] {
        match self {
            Node::Leaf { keys, .. } | Node::Internal { keys, .. } => keys,
        }
    }

    /// Bytes the node takes in a page.
    fn size(&self) -> usize {
        HEADER_SIZE
            + self
                .keys()
                .iter()
                .map(|k| self.entry_size(k))
                .sum::<usize>()
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
                (
                    Node::Leaf { keys, next },
                    separator,
                    Node::Leaf { keys: right, next },
                )
            }
            Node::Internal {
                mut keys,
                mut children,
            } => {
                let right_keys = keys.split_off(mid + 1);
                let separator = keys.pop().expect("mid >= 1");
                let right_children = children.split_off(mid + 1);
                (
                    Node::Internal { keys, children },
                    separator,
                    Node::Internal {
                        keys: right_keys,
                        children: right_children,
                    },
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
            LEAF => Ok(Node::Leaf {
                keys,
                next: decode_id(first),
            }),
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
    /// Where to go down to next: the start, then each leaf's upper bound.
    next: Option<Vec<u8>>,
    /// A copy of the current leaf's page, once there is one.
    leaf: Option<Box<Page>>,
    /// Keys of the current leaf not yet returned, and where the next one is.
    left: usize,
    pos: usize,
    /// The current leaf's upper bound; `None` for the last leaf.
    bound: Option<Vec<u8>>,
}

impl Range<'_> {
    /// Goes down to the leaf that may hold `start`, read-latching as a
    /// search does and noting the leaf's upper bound, copies it, and skips
    /// its keys below `start`.
    fn seek(&mut self, start: &[u8]) -> Result<()> {
        let mut id = self.tree.root;
        let mut bound = None;
        let mut parent = None;
        let page = loop {
            let page = self.tree.pool.fetch_read(id)?;
            drop(parent);
            if page[0] == LEAF {
                break page;
            }
            let (child, right) = child_of(&page, start)?;
            // A separator lower down is tighter than one above.
            if let Some(right) = right {
                bound = Some(right.to_vec());
            }
            id = child;
            parent = Some(page);
        };
        let leaf = self.leaf.get_or_insert_with(|| Box::new([0; PAGE_SIZE]));
        leaf.copy_from_slice(&*page);
        drop(page);
        self.bound = bound;
        self.left = count(leaf);
        self.pos = HEADER_SIZE;
        while self.left > 0 {
            let (key, next) = entry(leaf, self.pos, 0)?;
            if key >= start {
                break;
            }
            self.pos = next;
            self.left -= 1;
        }
        Ok(())
    }

    fn step(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(leaf) = &self.leaf
                && self.left > 0
            {
                let (key, next) = entry(leaf, self.pos, 0)?;
                let key = key.to_vec();
                self.pos = next;
                self.left -= 1;
                return Ok(Some(key));
            }
            // The next leaf holds the keys from this one's upper bound on.
            let Some(start) = self.next.take() else {
                return Ok(None);
            };
            self.seek(&start)?;
            self.next = self.bound.take();
        }
    }
}

impl Iterator for Range<'_> {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.step() {
            Ok(key) => key.map(Ok),
            Err(e) => {
                self.next = None;
                self.left = 0;
                Some(Err(e))
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

    /// Checks every node below `id`: its keys sorted and within the bounds
    /// its parent gives it, its page not overfull, and every leaf at the
    /// same depth. Returns the height. (A node may be under a quarter full:
    /// with long keys, even halves of a split can be.)
    fn check(tree: &BPlusTree, id: PageId, low: Option<&[u8]>, high: Option<&[u8]>) -> usize {
        let node = tree.read(id).unwrap();
        let keys = node.keys();
        assert!(node.size() <= PAGE_USABLE, "node {} overflows", id.0);
        assert!(
            keys.windows(2).all(|w| w[0] < w[1]),
            "node {} unsorted",
            id.0
        );
        assert!(keys.iter().all(
            |k| low.is_none_or(|l| k.as_slice() >= l) && high.is_none_or(|h| k.as_slice() < h)
        ));
        match node {
            Node::Leaf { .. } => 1,
            Node::Internal { keys, children } => {
                let bound = |i: usize| keys.get(i).map(Vec::as_slice);
                let heights: BTreeSet<usize> = (0..children.len())
                    .map(|i| {
                        check(
                            tree,
                            children[i],
                            if i == 0 { low } else { bound(i - 1) },
                            bound(i).or(high),
                        )
                    })
                    .collect();
                assert_eq!(heights.len(), 1, "unbalanced below {}", id.0);
                heights.first().unwrap() + 1
            }
        }
    }

    #[test]
    fn concurrent_inserts_deletes_and_scans_keep_every_key() {
        const WRITERS: u32 = 8;
        // Per writer: 32,000 in all at the peak, three levels deep, so
        // internal nodes split, and merge again as two in three go.
        const KEYS: u32 = 4000;
        // A small pool, so pages are evicted and read back while latched in turn.
        let tree = BPlusTree::create(pool(128)).unwrap();
        // Keys every reader must always find: inserted first, never removed.
        let stable: Vec<Vec<u8>> = (0..200).map(|i| key(1_000_000 + i)).collect();
        for k in &stable {
            tree.insert(k).unwrap();
        }
        let done = std::sync::atomic::AtomicBool::new(false);
        let done = &done;
        let (tree, stable) = (&tree, &stable);
        std::thread::scope(|s| {
            // Each writer owns the keys i with i % WRITERS == w: inserts them all,
            // deletes two in three, and inserts back one in three of those.
            let writers: Vec<_> = (0..WRITERS)
                .map(|w| {
                    s.spawn(move || {
                        let mine = (0..KEYS).map(|i| i * WRITERS + w);
                        for i in mine.clone() {
                            assert!(tree.insert(&key(i)).unwrap());
                        }
                        for i in mine.clone().filter(|i| i % 3 != 0) {
                            assert!(tree.delete(&key(i)).unwrap(), "{i} was there");
                        }
                        for i in mine.filter(|i| i % 9 == 1) {
                            assert!(tree.insert(&key(i)).unwrap());
                        }
                    })
                })
                .collect();
            for _ in 0..2 {
                s.spawn(move || {
                    while !done.load(std::sync::atomic::Ordering::Relaxed) {
                        let keys = all(tree);
                        assert!(
                            keys.windows(2).all(|w| w[0] < w[1]),
                            "a scan returns keys in order, each once"
                        );
                        let found: BTreeSet<&Vec<u8>> = keys.iter().collect();
                        assert!(
                            stable.iter().all(|k| found.contains(k)),
                            "a scan misses no key that stays"
                        );
                        for k in stable.iter().step_by(17) {
                            assert_eq!(tree.range(k).next().unwrap().unwrap(), *k);
                        }
                    }
                });
            }
            let results: Vec<_> = writers.into_iter().map(|w| w.join()).collect();
            // Stop the readers before reporting a writer's panic, which would otherwise wait for them.
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            results.into_iter().for_each(|r| r.unwrap());
        });

        let mut expected: BTreeSet<Vec<u8>> = (0..KEYS * WRITERS)
            .filter(|i| i % 3 == 0 || i % 9 == 1)
            .map(key)
            .collect();
        expected.extend(stable.iter().cloned());
        assert_eq!(all(tree), expected.into_iter().collect::<Vec<_>>());
        check(tree, tree.root, None, None);
    }

    #[test]
    fn a_parent_a_longer_separator_overflows_splits() {
        // Long and short keys mixed: a rebalance below may swap a short
        // separator for a long one, overflowing its parent, as seed 17 did
        // before parents were split then.
        for seed in 1..40u64 {
            let tree = BPlusTree::create(pool(16)).unwrap();
            let mut expected = BTreeSet::new();
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            for _ in 0..8000 {
                let i = rng.below(300);
                let len = if rng.below(3) == 0 {
                    600 + rng.below(MAX_KEY_SIZE as u32 - 600)
                } else {
                    4 + rng.below(8)
                };
                let mut k = i.to_be_bytes().to_vec();
                k.resize(len as usize, b'x');
                if rng.below(2) == 0 {
                    assert_eq!(tree.delete(&k).unwrap(), expected.remove(&k));
                } else {
                    assert_eq!(tree.insert(&k).unwrap(), expected.insert(k));
                }
            }
            assert_eq!(
                all(&tree),
                expected.into_iter().collect::<Vec<_>>(),
                "seed {seed}"
            );
            check(&tree, tree.root, None, None);
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
        let tree = BPlusTree::create(pool(8)).unwrap();
        for i in (0..20_000).rev() {
            assert!(tree.insert(&key(i)).unwrap());
        }
        assert!(!tree.insert(&key(42)).unwrap(), "already there");
        assert_eq!(all(&tree), (0..20_000).map(key).collect::<Vec<_>>());
        assert_eq!(
            height(&tree, tree.root),
            3,
            "20000 keys need two levels of internal nodes"
        );
    }

    #[test]
    fn range_starts_at_the_first_key_not_below_start() {
        let tree = BPlusTree::create(pool(8)).unwrap();
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
        let tree = BPlusTree::create(pool.clone()).unwrap();
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
        let tree = BPlusTree::create(pool(4)).unwrap();
        let mut expected = BTreeSet::new();
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for round in 0..20_000 {
            let k = key(rng.below(3000));
            if rng.below(3) == 0 {
                assert_eq!(
                    tree.delete(&k).unwrap(),
                    expected.remove(&k),
                    "round {round}"
                );
            } else {
                assert_eq!(
                    tree.insert(&k).unwrap(),
                    expected.insert(k),
                    "round {round}"
                );
            }
        }
        assert_eq!(all(&tree), expected.into_iter().collect::<Vec<_>>());
        height(&tree, tree.root);
    }

    #[test]
    fn large_keys_still_split() {
        let tree = BPlusTree::create(pool(8)).unwrap();
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
        let tree = BPlusTree::create(pool.clone()).unwrap();
        for i in 0..3000 {
            tree.insert(&key(i)).unwrap();
        }
        let root = tree.root_page();
        let pages = pool.disk().page_count();

        let tree = BPlusTree::open(pool.clone(), root);
        assert_eq!(all(&tree).len(), 3000);
        tree.destroy().unwrap();

        let other = BPlusTree::create(pool.clone()).unwrap();
        for i in 0..3000 {
            other.insert(&key(i)).unwrap();
        }
        assert_eq!(pool.disk().page_count(), pages);
    }
}
