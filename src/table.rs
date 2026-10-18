use std::collections::HashSet;

use crate::error::{Error, Result, err};
use crate::storage::{MAX_TUPLE_SIZE, RecordId, TableHeap, tuple};
use crate::types::{Column, Value};

pub type Row = Vec<Value>;

/// A table: a schema plus a heap of encoded rows. Enforces column types and primary keys.
///
/// Every write validates all rows before touching the heap, so a constraint
/// violation leaves the table unchanged.
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    heap: TableHeap,
}

impl Table {
    pub fn new(name: String, columns: Vec<Column>, heap: TableHeap) -> Self {
        Table { name, columns, heap }
    }

    pub fn heap(&self) -> &TableHeap {
        &self.heap
    }

    pub fn column_index(&self, name: &str) -> Result<usize> {
        column_index(&self.columns, name)
    }

    /// Iterates over all rows with their locations.
    pub fn scan(&self) -> impl Iterator<Item = Result<(RecordId, Row)>> + '_ {
        self.heap.scan().map(|item| {
            let (rid, bytes) = item?;
            Ok((rid, tuple::decode(&self.columns, &bytes)?))
        })
    }

    pub fn insert(&mut self, rows: Vec<Row>) -> Result<()> {
        let encoded = self.encode_all(&rows)?;
        self.check_primary_key(&HashSet::new(), &rows)?;
        for bytes in encoded {
            self.heap.insert(&bytes)?;
        }
        Ok(())
    }

    /// Replaces each row at the given location with a new value.
    pub fn update(&mut self, changes: Vec<(RecordId, Row)>) -> Result<()> {
        let (rids, rows): (HashSet<RecordId>, Vec<Row>) = changes.into_iter().unzip();
        let encoded = self.encode_all(&rows)?;
        self.check_primary_key(&rids, &rows)?;
        // Delete everything first, so a reused slot can't be mistaken for a pending one.
        for &rid in &rids {
            self.heap.delete(rid)?;
        }
        for bytes in encoded {
            self.heap.insert(&bytes)?;
        }
        Ok(())
    }

    pub fn delete(&mut self, rids: &[RecordId]) -> Result<()> {
        for &rid in rids {
            self.heap.delete(rid)?;
        }
        Ok(())
    }

    /// Frees all of the table's pages.
    pub fn destroy(self) -> Result<()> {
        self.heap.destroy()
    }

    /// Encodes rows, which also checks their types and sizes.
    fn encode_all(&self, rows: &[Row]) -> Result<Vec<Vec<u8>>> {
        rows.iter()
            .map(|row| {
                let bytes = tuple::encode(&self.columns, row)?;
                if bytes.len() > MAX_TUPLE_SIZE {
                    return err(format!("row is {} bytes, the maximum is {MAX_TUPLE_SIZE}", bytes.len()));
                }
                Ok(bytes)
            })
            .collect()
    }

    /// Checks that `new_rows`, together with the existing rows except those at
    /// `replaced`, have unique non-null primary keys. Scans the whole table.
    fn check_primary_key(&self, replaced: &HashSet<RecordId>, new_rows: &[Row]) -> Result<()> {
        let Some(pk) = self.columns.iter().position(|c| c.primary_key) else {
            return Ok(());
        };
        let mut seen = HashSet::new();
        for item in self.scan() {
            let (rid, row) = item?;
            if !replaced.contains(&rid) {
                seen.insert(row[pk].clone());
            }
        }
        for row in new_rows {
            let key = &row[pk];
            if key.is_null() {
                return err(format!("primary key {} cannot be NULL", self.columns[pk].name));
            }
            if !seen.insert(key.clone()) {
                return err(format!("duplicate primary key: {}", key.to_sql()));
            }
        }
        Ok(())
    }
}

/// Rejects duplicate column names and multiple primary keys.
pub fn validate_schema(columns: &[Column]) -> Result<()> {
    for (i, column) in columns.iter().enumerate() {
        if columns[..i].iter().any(|c| c.name == column.name) {
            return err(format!("duplicate column: {}", column.name));
        }
    }
    if columns.iter().filter(|c| c.primary_key).count() > 1 {
        return err("a table can have only one primary key");
    }
    Ok(())
}

pub fn column_index(columns: &[Column], name: &str) -> Result<usize> {
    columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| Error(format!("unknown column: {name}")))
}
