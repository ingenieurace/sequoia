//! Secondary indexes: a B+ tree over one column's values.
//!
//! Each entry is one key of the tree, the column's value followed by the id of
//! the row holding it:
//!
//! ```text
//! [value][row id]
//!
//! INT    8 bytes big-endian, sign bit flipped
//! BOOL   1 byte
//! TEXT   UTF-8 bytes with each 0x00 written as 0x00 0xFF, then 0x00 0x00
//! ```
//!
//! The value encoding sorts like the values and no value's encoding is a
//! prefix of another's, so the entries of one value are next to each other,
//! in value order, and are found by a scan of the keys starting with its
//! encoding. Adding the row id makes every entry unique even when many rows
//! share a value. NULLs are not indexed: no comparison matches them.
//!
//! The index stores row ids as bytes; the table decides what they mean.

use std::sync::Arc;

use crate::error::{Result, err};
use crate::storage::{BPlusTree, BufferPool, PageId, btree::MAX_KEY_SIZE, tuple};
use crate::types::{DataType, Value};

/// One end of a range of values.
#[derive(Debug, Clone, PartialEq)]
pub struct Bound {
    pub value: Value,
    pub inclusive: bool,
}

pub struct Index {
    pub name: String,
    /// Index of the indexed column in the table's schema.
    pub column: usize,
    pub data_type: DataType,
    /// Whether two rows may not have the same non-NULL value.
    pub unique: bool,
    tree: BPlusTree,
}

impl Index {
    pub fn create(
        pool: Arc<BufferPool>,
        name: String,
        column: usize,
        data_type: DataType,
        unique: bool,
    ) -> Result<Self> {
        Ok(Index { name, column, data_type, unique, tree: BPlusTree::create(pool)? })
    }

    pub fn open(
        pool: Arc<BufferPool>,
        name: String,
        column: usize,
        data_type: DataType,
        unique: bool,
        root: PageId,
    ) -> Self {
        Index { name, column, data_type, unique, tree: BPlusTree::open(pool, root) }
    }

    pub fn root_page(&self) -> PageId {
        self.tree.root_page()
    }

    /// Fails if a row with this value and an id `id_len` bytes long would make too long an entry.
    pub fn check(&self, value: &Value, id_len: usize) -> Result<()> {
        if value.is_null() {
            return Ok(());
        }
        let len = encode_value(value).len() + id_len;
        if len > MAX_KEY_SIZE {
            return err(format!(
                "value is too long for index {}: {len} bytes, the maximum is {MAX_KEY_SIZE}",
                self.name
            ));
        }
        Ok(())
    }

    pub fn insert(&mut self, value: &Value, row_id: &[u8]) -> Result<()> {
        if !value.is_null() {
            self.tree.insert(&[encode_value(value), row_id.to_vec()].concat())?;
        }
        Ok(())
    }

    pub fn delete(&mut self, value: &Value, row_id: &[u8]) -> Result<()> {
        if !value.is_null() {
            self.tree.delete(&[encode_value(value), row_id.to_vec()].concat())?;
        }
        Ok(())
    }

    /// The ids of the rows holding `value`.
    pub fn get(&self, value: &Value) -> Result<Vec<Vec<u8>>> {
        let bound = Bound { value: value.clone(), inclusive: true };
        self.range(Some(&bound), Some(&bound))
    }

    /// The ids of the rows whose value lies between the bounds, in value order.
    /// A missing bound leaves that end open.
    pub fn range(&self, lower: Option<&Bound>, upper: Option<&Bound>) -> Result<Vec<Vec<u8>>> {
        let lower = lower.map(|b| (encode_value(&b.value), b.inclusive));
        let upper = upper.map(|b| (encode_value(&b.value), b.inclusive));
        let start = lower.as_ref().map_or(&[][..], |(value, _)| value.as_slice());

        let mut ids = Vec::new();
        for key in self.tree.range(start) {
            let key = key?;
            let len = value_len(&key, self.data_type)?;
            let value = &key[..len];
            if let Some((bound, false)) = &lower
                && value == bound.as_slice()
            {
                continue;
            }
            if let Some((bound, inclusive)) = &upper
                && (value > bound.as_slice() || (!inclusive && value == bound.as_slice()))
            {
                break;
            }
            ids.push(key[len..].to_vec());
        }
        Ok(ids)
    }

    /// Frees all of the index's pages.
    pub fn destroy(self) -> Result<()> {
        self.tree.destroy()
    }
}

/// Encodes a non-NULL value as described in the module docs.
fn encode_value(value: &Value) -> Vec<u8> {
    match value {
        Value::Text(s) => {
            let mut bytes = Vec::with_capacity(s.len() + 2);
            for &b in s.as_bytes() {
                bytes.push(b);
                if b == 0 {
                    bytes.push(0xFF);
                }
            }
            bytes.extend_from_slice(&[0, 0]);
            bytes
        }
        other => tuple::encode_key(other),
    }
}

/// The length of the value at the start of an index entry.
fn value_len(key: &[u8], data_type: DataType) -> Result<usize> {
    let len = match data_type {
        DataType::Int | DataType::Real => 8,
        DataType::Bool => 1,
        DataType::Text => {
            let mut i = 0;
            loop {
                match key.get(i..i + 2) {
                    Some([0, 0]) => break i + 2,
                    Some([0, _]) => i += 2,
                    Some(_) => i += 1,
                    None => return err("corrupted index entry"),
                }
            }
        }
    };
    if len > key.len() {
        return err("corrupted index entry");
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DiskManager;

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    fn index(data_type: DataType) -> Index {
        let pool = Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 16));
        Index::create(pool, "i".into(), 0, data_type, false).unwrap()
    }

    #[test]
    fn text_encoding_sorts_like_the_values_and_is_prefix_free() {
        let values = ["", "\0", "\0\0", "\0a", "a", "a\0", "a\0b", "ab", "b", "é"];
        for pair in values.windows(2) {
            let (a, b) = (encode_value(&text(pair[0])), encode_value(&text(pair[1])));
            assert!(a < b, "{pair:?}");
            assert!(!b.starts_with(&a), "{pair:?}");
        }
        for s in values {
            let mut key = encode_value(&text(s));
            let len = key.len();
            key.extend_from_slice(&[0, 0, 7]);
            assert_eq!(value_len(&key, DataType::Text).unwrap(), len, "{s:?}");
        }
    }

    #[test]
    fn rows_sharing_a_value_are_found_together() {
        let mut index = index(DataType::Text);
        for (i, name) in ["bob", "al", "alice", "bob", "al\0", "carol", "bob"].iter().enumerate() {
            index.insert(&text(name), &[i as u8]).unwrap();
        }
        index.insert(&Value::Null, &[99]).unwrap();
        assert_eq!(index.get(&text("bob")).unwrap(), vec![vec![0], vec![3], vec![6]]);
        assert_eq!(index.get(&text("al")).unwrap(), vec![vec![1]]);
        assert!(index.get(&text("b")).unwrap().is_empty());

        index.delete(&text("bob"), &[3]).unwrap();
        assert_eq!(index.get(&text("bob")).unwrap(), vec![vec![0], vec![6]]);
    }

    #[test]
    fn ranges_honour_inclusive_and_exclusive_bounds() {
        let mut index = index(DataType::Int);
        for n in -5..=5i64 {
            index.insert(&Value::Int(n), &[n as u8, 0]).unwrap();
            index.insert(&Value::Int(n), &[n as u8, 1]).unwrap();
        }
        let bound = |n, inclusive| Some(Bound { value: Value::Int(n), inclusive });
        let values = |lower: Option<Bound>, upper: Option<Bound>| -> Vec<i64> {
            let ids = index.range(lower.as_ref(), upper.as_ref()).unwrap();
            ids.iter().step_by(2).map(|id| id[0] as i8 as i64).collect()
        };
        assert_eq!(values(bound(-1, true), bound(2, true)), vec![-1, 0, 1, 2]);
        assert_eq!(values(bound(-1, false), bound(2, false)), vec![0, 1]);
        assert_eq!(values(None, bound(-4, true)), vec![-5, -4]);
        assert_eq!(values(bound(4, false), None), vec![5]);
        assert_eq!(values(bound(3, false), bound(3, true)), Vec::<i64>::new());
        assert_eq!(values(None, None).len(), 11);
    }

    #[test]
    fn entries_longer_than_a_tree_key_are_rejected() {
        let index = index(DataType::Text);
        index.check(&text(&"x".repeat(1000)), 6).unwrap();
        assert!(index.check(&text(&"x".repeat(MAX_KEY_SIZE)), 6).is_err());
        index.check(&Value::Null, 6).unwrap();
    }
}
