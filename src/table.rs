use std::collections::HashSet;
use std::sync::Arc;

use crate::error::{Error, Result, err};
use crate::storage::{
    BufferPool, ColumnConfig, ColumnStore, LsmConfig, LsmTree, MAX_TUPLE_SIZE, PageId, Position, RecordId, TableHeap,
    tuple,
};
use crate::types::{Column, StorageKind, Value};

pub type Row = Vec<Value>;

/// Identifies a row within its table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowId {
    /// Where a heap row is stored.
    Record(RecordId),
    /// An LSM row's key.
    Key(Vec<u8>),
    /// Where a column store row is.
    Position(Position),
}

/// A table: a schema plus the storage holding its encoded rows. Enforces
/// column types and primary keys.
///
/// Every write validates all rows before touching storage, so a constraint
/// violation leaves the table unchanged.
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    storage: Storage,
}

enum Storage {
    Heap(TableHeap),
    /// Rows are keyed by their primary key or, in a table without one, by a
    /// hidden rowid: 1, 2, 3, ... in insertion order.
    Lsm { tree: LsmTree, next_rowid: i64 },
    Column(ColumnStore),
}

impl Table {
    /// Creates an empty table.
    pub fn create(pool: Arc<BufferPool>, name: String, columns: Vec<Column>, kind: StorageKind) -> Result<Self> {
        let storage = match kind {
            StorageKind::Heap => Storage::Heap(TableHeap::create(pool)?),
            StorageKind::Lsm => Storage::Lsm { tree: LsmTree::create(pool, LsmConfig::default())?, next_rowid: 1 },
            StorageKind::Column => {
                Storage::Column(ColumnStore::create(pool, columns.clone(), ColumnConfig::default())?)
            }
        };
        Ok(Table { name, columns, storage })
    }

    /// Opens a table from the pages [`Table::root_pages`] returned.
    pub fn open(
        pool: Arc<BufferPool>,
        name: String,
        columns: Vec<Column>,
        kind: StorageKind,
        (first, last): (PageId, PageId),
    ) -> Result<Self> {
        let storage = match kind {
            StorageKind::Heap => Storage::Heap(TableHeap::open(pool, first, last)?),
            StorageKind::Lsm => {
                let tree = LsmTree::open(pool, first, LsmConfig::default())?;
                let next_rowid = match (primary_key(&columns), tree.max_key()) {
                    (None, Some(max)) => tuple::decode_int_key(&max)? + 1,
                    _ => 1,
                };
                Storage::Lsm { tree, next_rowid }
            }
            StorageKind::Column => {
                Storage::Column(ColumnStore::open(pool, first, columns.clone(), ColumnConfig::default())?)
            }
        };
        Ok(Table { name, columns, storage })
    }

    pub fn storage_kind(&self) -> StorageKind {
        match self.storage {
            Storage::Heap(_) => StorageKind::Heap,
            Storage::Lsm { .. } => StorageKind::Lsm,
            Storage::Column(_) => StorageKind::Column,
        }
    }

    /// The pages the catalog records to open the table again: a heap's first
    /// and last page, or the manifest page of an LSM tree or column store twice.
    pub fn root_pages(&self) -> (PageId, PageId) {
        match &self.storage {
            Storage::Heap(heap) => (heap.first_page(), heap.last_page()),
            Storage::Lsm { tree, .. } => (tree.manifest_page(), tree.manifest_page()),
            Storage::Column(store) => (store.manifest_page(), store.manifest_page()),
        }
    }

    pub fn column_index(&self, name: &str) -> Result<usize> {
        column_index(&self.columns, name)
    }

    /// Iterates over all rows with their ids: a heap in storage order, an LSM
    /// tree in key order, a column store row group by row group.
    pub fn scan(&self) -> Box<dyn Iterator<Item = Result<(RowId, Row)>> + '_> {
        self.scan_columns(&vec![true; self.columns.len()])
    }

    /// Like [`Table::scan`], but only the columns marked in `needed` must be
    /// read. Unneeded columns may hold anything, NULL usually: a column store
    /// skips their chunks, while heaps and LSM trees return whole rows.
    pub fn scan_columns(&self, needed: &[bool]) -> Box<dyn Iterator<Item = Result<(RowId, Row)>> + '_> {
        assert_eq!(needed.len(), self.columns.len());
        match &self.storage {
            Storage::Heap(heap) => Box::new(heap.scan().map(|item| {
                let (rid, bytes) = item?;
                Ok((RowId::Record(rid), tuple::decode(&self.columns, &bytes)?))
            })),
            Storage::Lsm { tree, .. } => Box::new(tree.scan().map(|item| {
                let (key, bytes) = item?;
                Ok((RowId::Key(key), tuple::decode(&self.columns, &bytes)?))
            })),
            Storage::Column(store) => Box::new(
                store.scan(needed.to_vec()).map(|item| item.map(|(position, row)| (RowId::Position(position), row))),
            ),
        }
    }

    pub fn insert(&mut self, rows: Vec<Row>) -> Result<()> {
        let encoded = self.encode_all(&rows)?;
        self.check_primary_key(&HashSet::new(), &rows)?;
        let pk = primary_key(&self.columns);
        match &mut self.storage {
            Storage::Heap(heap) => {
                encoded.iter().try_for_each(|bytes| check_heap_size(bytes))?;
                for bytes in encoded {
                    heap.insert(&bytes)?;
                }
            }
            Storage::Lsm { tree, next_rowid } => {
                let keys: Vec<Vec<u8>> = (0..).zip(&rows).map(|(i, row)| match pk {
                    Some(pk) => tuple::encode_key(&row[pk]),
                    None => tuple::encode_key(&Value::Int(*next_rowid + i)),
                })
                .collect();
                keys.iter().zip(&encoded).try_for_each(|(key, bytes)| LsmTree::check_size(key, bytes))?;
                for (key, bytes) in keys.into_iter().zip(encoded) {
                    tree.put(key, bytes)?;
                }
                *next_rowid += rows.len() as i64;
            }
            Storage::Column(store) => {
                encoded.iter().try_for_each(|bytes| check_heap_size(bytes))?;
                for bytes in encoded {
                    store.insert(&bytes)?;
                }
            }
        }
        Ok(())
    }

    /// Replaces each row with the given id by a new value.
    ///
    /// A heap row stays where it is if its page has room for the new value;
    /// otherwise it is deleted and inserted again, possibly in another page.
    /// An LSM row is simply written again, under a new key if its primary key changed.
    /// A column store row is deleted and inserted again into the delta store.
    pub fn update(&mut self, changes: Vec<(RowId, Row)>) -> Result<()> {
        let (ids, rows): (Vec<RowId>, Vec<Row>) = changes.into_iter().unzip();
        let encoded = self.encode_all(&rows)?;
        self.check_primary_key(&ids.iter().cloned().collect(), &rows)?;
        let pk = primary_key(&self.columns);
        match &mut self.storage {
            Storage::Heap(heap) => {
                encoded.iter().try_for_each(|bytes| check_heap_size(bytes))?;
                let rids: Vec<RecordId> = ids.into_iter().map(RowId::into_record).collect::<Result<_>>()?;
                let mut moved = Vec::new();
                for (rid, bytes) in rids.into_iter().zip(encoded) {
                    if !heap.update(rid, &bytes)? {
                        moved.push((rid, bytes));
                    }
                }
                // Delete every moved row first, so a reused slot can't be mistaken for a pending one.
                for (rid, _) in &moved {
                    heap.delete(*rid)?;
                }
                for (_, bytes) in moved {
                    heap.insert(&bytes)?;
                }
            }
            Storage::Lsm { tree, .. } => {
                let mut deletes = Vec::new();
                let mut puts = Vec::new();
                for ((id, row), bytes) in ids.into_iter().zip(&rows).zip(encoded) {
                    let old = id.into_key()?;
                    let new = pk.map_or_else(|| old.clone(), |pk| tuple::encode_key(&row[pk]));
                    LsmTree::check_size(&new, &bytes)?;
                    if new != old {
                        deletes.push(old);
                    }
                    puts.push((new, bytes));
                }
                // Deletes first, so a row may take over a key another row is leaving.
                for key in deletes {
                    tree.delete(key)?;
                }
                for (key, bytes) in puts {
                    tree.put(key, bytes)?;
                }
            }
            Storage::Column(store) => {
                encoded.iter().try_for_each(|bytes| check_heap_size(bytes))?;
                let positions: Vec<Position> = ids.into_iter().map(RowId::into_position).collect::<Result<_>>()?;
                // Deletes first: an insert may migrate the delta store, moving its rows.
                for position in positions {
                    store.delete(position)?;
                }
                for bytes in encoded {
                    store.insert(&bytes)?;
                }
            }
        }
        Ok(())
    }

    pub fn delete(&mut self, ids: &[RowId]) -> Result<()> {
        for id in ids.iter().cloned() {
            match &mut self.storage {
                Storage::Heap(heap) => {
                    heap.delete(id.into_record()?)?;
                }
                Storage::Lsm { tree, .. } => tree.delete(id.into_key()?)?,
                Storage::Column(store) => store.delete(id.into_position()?)?,
            }
        }
        Ok(())
    }

    /// Frees all of the table's pages.
    pub fn destroy(self) -> Result<()> {
        match self.storage {
            Storage::Heap(heap) => heap.destroy(),
            Storage::Lsm { tree, .. } => tree.destroy(),
            Storage::Column(store) => store.destroy(),
        }
    }

    /// Encodes rows, which also checks their types.
    fn encode_all(&self, rows: &[Row]) -> Result<Vec<Vec<u8>>> {
        rows.iter().map(|row| tuple::encode(&self.columns, row)).collect()
    }

    /// Checks that `new_rows`, together with the existing rows except those in
    /// `replaced`, have unique non-null primary keys. A heap is scanned in
    /// full, a column store reads only its key column; an LSM tree is
    /// searched for each new key.
    fn check_primary_key(&self, replaced: &HashSet<RowId>, new_rows: &[Row]) -> Result<()> {
        let Some(pk) = primary_key(&self.columns) else {
            return Ok(());
        };
        let mut seen = HashSet::new();
        if let Storage::Heap(_) | Storage::Column(_) = self.storage {
            let needed: Vec<bool> = (0..self.columns.len()).map(|i| i == pk).collect();
            for item in self.scan_columns(&needed) {
                let (id, row) = item?;
                if !replaced.contains(&id) {
                    seen.insert(row[pk].clone());
                }
            }
        }
        for row in new_rows {
            let key = &row[pk];
            if key.is_null() {
                return err(format!("primary key {} cannot be NULL", self.columns[pk].name));
            }
            let stored = match &self.storage {
                Storage::Heap(_) | Storage::Column(_) => false, // already in `seen`
                Storage::Lsm { tree, .. } => {
                    let key = tuple::encode_key(key);
                    !replaced.contains(&RowId::Key(key.clone())) && tree.get(&key)?.is_some()
                }
            };
            if stored || !seen.insert(key.clone()) {
                return err(format!("duplicate primary key: {}", key.to_sql()));
            }
        }
        Ok(())
    }
}

impl RowId {
    fn into_record(self) -> Result<RecordId> {
        match self {
            RowId::Record(rid) => Ok(rid),
            _ => err("expected a heap row"),
        }
    }

    fn into_position(self) -> Result<Position> {
        match self {
            RowId::Position(position) => Ok(position),
            _ => err("expected a column store row"),
        }
    }

    fn into_key(self) -> Result<Vec<u8>> {
        match self {
            RowId::Key(key) => Ok(key),
            _ => err("expected an LSM row"),
        }
    }
}

fn check_heap_size(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_TUPLE_SIZE {
        return err(format!("row is {} bytes, the maximum is {MAX_TUPLE_SIZE}", bytes.len()));
    }
    Ok(())
}

fn primary_key(columns: &[Column]) -> Option<usize> {
    columns.iter().position(|c| c.primary_key)
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
