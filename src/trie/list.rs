//! Children in a list sorted by their bytes: as small as a node can be for
//! however many children it has, but a lookup is a binary search.

use super::{Children, Node};

type Child<V> = Box<Node<V, List<V>>>;

pub struct List<V> {
    children: Vec<(u8, Child<V>)>,
}

impl<V> Default for List<V> {
    fn default() -> Self {
        List { children: Vec::new() }
    }
}

impl<V> List<V> {
    fn find(&self, byte: u8) -> Result<usize, usize> {
        self.children.binary_search_by_key(&byte, |(b, _)| *b)
    }
}

impl<V> Children<V> for List<V> {
    fn len(&self) -> usize {
        self.children.len()
    }

    fn get(&self, byte: u8) -> Option<&Node<V, Self>> {
        self.find(byte).ok().map(|i| &*self.children[i].1)
    }

    fn get_mut(&mut self, byte: u8) -> Option<&mut Node<V, Self>> {
        self.find(byte).ok().map(|i| &mut *self.children[i].1)
    }

    fn add(&mut self, byte: u8, child: Box<Node<V, Self>>) {
        let i = self.find(byte).expect_err("a byte with no child");
        self.children.insert(i, (byte, child));
    }

    fn remove(&mut self, byte: u8) -> Option<Box<Node<V, Self>>> {
        self.find(byte).ok().map(|i| self.children.remove(i).1)
    }

    fn next(&self, from: u16) -> Option<(u8, &Node<V, Self>)> {
        let i = self.children.partition_point(|(b, _)| (*b as u16) < from);
        self.children.get(i).map(|(b, child)| (*b, &**child))
    }

    fn last(&self) -> Option<&Node<V, Self>> {
        self.children.last().map(|(_, child)| &**child)
    }
}
