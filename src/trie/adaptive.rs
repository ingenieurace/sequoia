//! Children of an adaptive radix tree (ART): one of four kinds of node,
//! picked by how many children there are, as Leis et al. describe.
//!
//! ```text
//! Node4    up to 4    keys [b1 b2 b3 _]   children [c1 c2 c3 _]    bytes sorted, searched in order
//! Node16   up to 16   keys [16 bytes]     children [16]            bytes sorted, binary search
//! Node48   up to 48   slot [256 bytes]    children [48]            slot[byte] = where its child is, + 1
//! Node256  up to 256                      children [256]           children[byte]
//! ```
//!
//! A node that is full grows into the next kind when a child is added, and
//! one with few children left shrinks into the kind before: Node16 at 3,
//! Node48 at 12 and Node256 at 37, short of where the smaller kind fills up,
//! so that a node at the edge does not change kind with every add and remove.
//!
//! The four trade size for speed. A list of `(byte, child)` pairs is the
//! smallest a node can be, but searching it takes time; an array of 256
//! children finds any of them at once, but most nodes have few children, and
//! 256 pointers for a node with 2 is mostly wasted. Node48 finds a child at
//! once with a byte per possible child and a pointer only per actual one.

use std::array;
use std::mem;

use super::{Children, Node};

type Child<V> = Box<Node<V, Adaptive<V>>>;

pub enum Adaptive<V> {
    N4(Small<V, 4>),
    N16(Box<Small<V, 16>>),
    N48(Box<Node48<V>>),
    N256(Box<Node256<V>>),
}

impl<V> Default for Adaptive<V> {
    fn default() -> Self {
        Adaptive::N4(Small::new())
    }
}

/// Up to `N` children, under the bytes in `keys`, sorted.
pub struct Small<V, const N: usize> {
    len: usize,
    keys: [u8; N],
    children: [Option<Child<V>>; N],
}

impl<V, const N: usize> Small<V, N> {
    fn new() -> Self {
        Small {
            len: 0,
            keys: [0; N],
            children: array::from_fn(|_| None),
        }
    }

    fn find(&self, byte: u8) -> Result<usize, usize> {
        self.keys[..self.len].binary_search(&byte)
    }

    fn add(&mut self, byte: u8, child: Child<V>) {
        let i = self.find(byte).expect_err("a byte with no child");
        self.keys.copy_within(i..self.len, i + 1);
        self.children[i..=self.len].rotate_right(1);
        self.keys[i] = byte;
        self.children[i] = Some(child);
        self.len += 1;
    }

    fn remove(&mut self, byte: u8) -> Option<Child<V>> {
        let i = self.find(byte).ok()?;
        let child = self.children[i].take();
        self.keys.copy_within(i + 1..self.len, i);
        self.children[i..self.len].rotate_left(1);
        self.len -= 1;
        child
    }

    /// Moves the children, in order, into a larger or smaller kind of node.
    fn drain(&mut self) -> impl Iterator<Item = (u8, Child<V>)> + '_ {
        let len = mem::take(&mut self.len);
        self.keys[..len]
            .iter()
            .zip(&mut self.children[..len])
            .map(|(&b, c)| (b, c.take().expect("in use")))
    }
}

pub struct Node48<V> {
    len: usize,
    /// For each byte, where its child is in `children`, plus 1; 0 for none.
    slots: [u8; 256],
    children: [Option<Child<V>>; 48],
}

impl<V> Node48<V> {
    fn new() -> Self {
        Node48 {
            len: 0,
            slots: [0; 256],
            children: array::from_fn(|_| None),
        }
    }

    fn add(&mut self, byte: u8, child: Child<V>) {
        let free = self
            .children
            .iter()
            .position(Option::is_none)
            .expect("not full");
        self.children[free] = Some(child);
        self.slots[byte as usize] = free as u8 + 1;
        self.len += 1;
    }

    fn child(&self, byte: usize) -> Option<&Child<V>> {
        match self.slots[byte] {
            0 => None,
            slot => self.children[slot as usize - 1].as_ref(),
        }
    }
}

pub struct Node256<V> {
    len: usize,
    children: [Option<Child<V>>; 256],
}

impl<V> Children<V> for Adaptive<V> {
    fn len(&self) -> usize {
        match self {
            Adaptive::N4(n) => n.len,
            Adaptive::N16(n) => n.len,
            Adaptive::N48(n) => n.len,
            Adaptive::N256(n) => n.len,
        }
    }

    fn get(&self, byte: u8) -> Option<&Node<V, Self>> {
        let child = match self {
            Adaptive::N4(n) => n.find(byte).ok().and_then(|i| n.children[i].as_ref()),
            Adaptive::N16(n) => n.find(byte).ok().and_then(|i| n.children[i].as_ref()),
            Adaptive::N48(n) => n.child(byte as usize),
            Adaptive::N256(n) => n.children[byte as usize].as_ref(),
        };
        child.map(|c| &**c)
    }

    fn get_mut(&mut self, byte: u8) -> Option<&mut Node<V, Self>> {
        let child = match self {
            Adaptive::N4(n) => n.find(byte).ok().and_then(|i| n.children[i].as_mut()),
            Adaptive::N16(n) => n.find(byte).ok().and_then(|i| n.children[i].as_mut()),
            Adaptive::N48(n) => match n.slots[byte as usize] {
                0 => None,
                slot => n.children[slot as usize - 1].as_mut(),
            },
            Adaptive::N256(n) => n.children[byte as usize].as_mut(),
        };
        child.map(|c| &mut **c)
    }

    fn add(&mut self, byte: u8, child: Child<V>) {
        // A full node grows into the next kind first.
        match self {
            Adaptive::N4(n) if n.len == 4 => {
                let mut bigger = Small::new();
                n.drain().for_each(|(b, c)| bigger.add(b, c));
                *self = Adaptive::N16(Box::new(bigger));
            }
            Adaptive::N16(n) if n.len == 16 => {
                let mut bigger = Box::new(Node48::new());
                n.drain().for_each(|(b, c)| bigger.add(b, c));
                *self = Adaptive::N48(bigger);
            }
            Adaptive::N48(n) if n.len == 48 => {
                let mut bigger = Box::new(Node256 {
                    len: n.len,
                    children: array::from_fn(|_| None),
                });
                for byte in 0..256 {
                    if let slot @ 1.. = n.slots[byte] {
                        bigger.children[byte] = n.children[slot as usize - 1].take();
                    }
                }
                *self = Adaptive::N256(bigger);
            }
            _ => {}
        }
        match self {
            Adaptive::N4(n) => n.add(byte, child),
            Adaptive::N16(n) => n.add(byte, child),
            Adaptive::N48(n) => n.add(byte, child),
            Adaptive::N256(n) => {
                n.children[byte as usize] = Some(child);
                n.len += 1;
            }
        }
    }

    fn remove(&mut self, byte: u8) -> Option<Child<V>> {
        let child = match self {
            Adaptive::N4(n) => n.remove(byte),
            Adaptive::N16(n) => n.remove(byte),
            Adaptive::N48(n) => {
                let slot = mem::take(&mut n.slots[byte as usize]);
                let child = n.children[slot.checked_sub(1)? as usize].take();
                n.len -= 1;
                child
            }
            Adaptive::N256(n) => {
                let child = n.children[byte as usize].take()?;
                n.len -= 1;
                Some(child)
            }
        };
        // A node with few children left shrinks into the kind before.
        match self {
            Adaptive::N16(n) if n.len <= 3 => {
                let mut smaller = Small::new();
                n.drain().for_each(|(b, c)| smaller.add(b, c));
                *self = Adaptive::N4(smaller);
            }
            Adaptive::N48(n) if n.len <= 12 => {
                let mut smaller = Small::new();
                for byte in 0..256 {
                    if let slot @ 1.. = n.slots[byte] {
                        smaller.add(
                            byte as u8,
                            n.children[slot as usize - 1].take().expect("in use"),
                        );
                    }
                }
                *self = Adaptive::N16(Box::new(smaller));
            }
            Adaptive::N256(n) if n.len <= 37 => {
                let mut smaller = Box::new(Node48::new());
                for (byte, child) in n.children.iter_mut().enumerate() {
                    if let Some(child) = child.take() {
                        smaller.add(byte as u8, child);
                    }
                }
                *self = Adaptive::N48(smaller);
            }
            _ => {}
        }
        child
    }

    fn next(&self, from: u16) -> Option<(u8, &Node<V, Self>)> {
        let found = match self {
            Adaptive::N4(n) => next_small(n, from),
            Adaptive::N16(n) => next_small(n, from),
            Adaptive::N48(n) => (from as usize..256).find_map(|b| n.child(b).map(|c| (b as u8, c))),
            Adaptive::N256(n) => {
                (from as usize..256).find_map(|b| n.children[b].as_ref().map(|c| (b as u8, c)))
            }
        };
        found.map(|(b, c)| (b, &**c))
    }

    fn last(&self) -> Option<&Node<V, Self>> {
        let last = match self {
            Adaptive::N4(n) => n.len.checked_sub(1).and_then(|i| n.children[i].as_ref()),
            Adaptive::N16(n) => n.len.checked_sub(1).and_then(|i| n.children[i].as_ref()),
            Adaptive::N48(n) => (0..256).rev().find_map(|b| n.child(b)),
            Adaptive::N256(n) => n.children.iter().rev().find_map(Option::as_ref),
        };
        last.map(|c| &**c)
    }
}

fn next_small<V, const N: usize>(n: &Small<V, N>, from: u16) -> Option<(u8, &Child<V>)> {
    let i = n.keys[..n.len].partition_point(|&b| (b as u16) < from);
    (i < n.len).then(|| (n.keys[i], n.children[i].as_ref().expect("in use")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trie::{Art, SortedMap};

    fn kind(children: &Adaptive<()>) -> &'static str {
        match children {
            Adaptive::N4(_) => "Node4",
            Adaptive::N16(_) => "Node16",
            Adaptive::N48(_) => "Node48",
            Adaptive::N256(_) => "Node256",
        }
    }

    #[test]
    fn nodes_grow_and_shrink_with_their_children() {
        let mut art = Art::new();
        let mut kinds = Vec::new();
        for byte in 0..=255u8 {
            art.insert(vec![byte], ());
            let kind = kind(&art.root.children);
            if kinds.last() != Some(&kind) {
                kinds.push(kind);
            }
        }
        assert_eq!(kinds, ["Node4", "Node16", "Node48", "Node256"]);
        let mut shrunk = Vec::new();
        for byte in (0..=255u8).rev() {
            art.remove(&[byte]);
            let kind = kind(&art.root.children);
            if shrunk.last().map(|(k, _)| *k) != Some(kind) {
                shrunk.push((kind, art.root.children.len()));
            }
            assert!(
                art.iter_from(&[]).map(|(k, _)| k[0]).eq(0..byte),
                "in order after removing {byte}"
            );
        }
        assert_eq!(
            shrunk,
            [
                ("Node256", 255),
                ("Node48", 37),
                ("Node16", 12),
                ("Node4", 3)
            ]
        );
    }
}
