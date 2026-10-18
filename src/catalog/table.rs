use std::collections::HashSet;
use std::sync::Arc;

use super::index::{Bound, Index};
use crate::error::{Error, Result, err};
use crate::predicate::Predicate;
use crate::storage::{
    BufferPool, ColumnConfig, ColumnStore, LsmConfig, LsmTree, MAX_TUPLE_SIZE, PageId, Position,
    RecordId, TableHeap, tuple,
};
use crate::types::{Column, StorageKind, Value, column_index};

pub type Row = Vec<Value>;

/// Bytes of a heap row's id in an index: page id and slot, big-endian.
const RECORD_ID_LEN: usize = 6;

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

/// A table: a schema plus the storage holding its encoded rows, and the
/// indexes over them. Enforces column types, primary keys and unique indexes.
///
/// Every write validates all rows before touching storage, so a constraint
/// violation leaves the table unchanged.
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pool: Arc<BufferPool>,
    storage: Storage,
    /// Kept up to date by every write. Only heap and LSM tables have any: a
    /// column store moves its rows when it migrates them, which would change
    /// their ids.
    indexes: Vec<Index>,
}

enum Storage {
    Heap(TableHeap),
    /// Rows are keyed by their primary key or, in a table without one, by a
    /// hidden rowid: 1, 2, 3, ... in insertion order.
    Lsm { tree: LsmTree, next_rowid: i64 },
    Column(ColumnStore),
}

/// How a scan finds its rows; `plan::access` picks one.
#[derive(Debug, Clone, PartialEq)]
pub enum Access {
    /// Read every row.
    Scan,
    /// Look one primary key up in an LSM tree.
    Key(Value),
    /// Read the rows an index lists between two bounds.
    Index { index: usize, lower: Option<Bound>, upper: Option<Bound> },
}

impl Table {
    /// Creates an empty table.
    pub fn create(pool: Arc<BufferPool>, name: String, columns: Vec<Column>, kind: StorageKind) -> Result<Self> {
        let storage = match kind {
            StorageKind::Heap => Storage::Heap(TableHeap::create(pool.clone())?),
            StorageKind::Lsm => {
                Storage::Lsm { tree: LsmTree::create(pool.clone(), LsmConfig::default())?, next_rowid: 1 }
            }
            StorageKind::Column => {
                Storage::Column(ColumnStore::create(pool.clone(), columns.clone(), ColumnConfig::default())?)
            }
        };
        Ok(Table { name, columns, pool, storage, indexes: Vec::new() })
    }

    /// Opens a table from the pages [`Table::root_pages`] returned. Its
    /// indexes are added afterwards with [`Table::open_index`].
    pub fn open(
        pool: Arc<BufferPool>,
        name: String,
        columns: Vec<Column>,
        kind: StorageKind,
        (first, last): (PageId, PageId),
    ) -> Result<Self> {
        let storage = match kind {
            StorageKind::Heap => Storage::Heap(TableHeap::open(pool.clone(), first, last)?),
            StorageKind::Lsm => {
                let tree = LsmTree::open(pool.clone(), first, LsmConfig::default())?;
                let next_rowid = match (primary_key(&columns), tree.max_key()) {
                    (None, Some(max)) => tuple::decode_int_key(&max)? + 1,
                    _ => 1,
                };
                Storage::Lsm { tree, next_rowid }
            }
            StorageKind::Column => {
                Storage::Column(ColumnStore::open(pool.clone(), first, columns.clone(), ColumnConfig::default())?)
            }
        };
        Ok(Table { name, columns, pool, storage, indexes: Vec::new() })
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

    pub fn primary_key(&self) -> Option<usize> {
        primary_key(&self.columns)
    }

    pub fn indexes(&self) -> &[Index] {
        &self.indexes
    }

    /// Builds an index over the table's rows. Fails, leaving no index behind,
    /// if the table is a column table, or the index is unique and two rows
    /// share a value, or a value is too long to index.
    pub fn create_index(&mut self, name: String, column: usize, unique: bool) -> Result<()> {
        if let Storage::Column(_) = self.storage {
            return err("COLUMN tables cannot have indexes");
        }
        let data_type = self.columns[column].data_type;
        let mut index = Index::create(self.pool.clone(), name, column, data_type, unique)?;
        let mut build = || {
            for item in self.scan() {
                let (id, row) = item?;
                let (value, id) = (&row[column], id.to_bytes());
                index.check(value, id.len())?;
                if unique && !value.is_null() && !index.get(value)?.is_empty() {
                    let name = &index.name;
                    return err(format!("cannot create unique index {name}: duplicate value {}", value.to_sql()));
                }
                index.insert(value, &id)?;
            }
            Ok(())
        };
        match build() {
            Ok(()) => {
                self.indexes.push(index);
                Ok(())
            }
            Err(e) => {
                index.destroy()?;
                Err(e)
            }
        }
    }

    /// Adds an index the catalog recorded, which already holds the table's rows.
    pub fn open_index(&mut self, name: String, column: usize, unique: bool, root: PageId) {
        let data_type = self.columns[column].data_type;
        self.indexes.push(Index::open(self.pool.clone(), name, column, data_type, unique, root));
    }

    /// Removes an index and frees its pages.
    pub fn drop_index(&mut self, name: &str) -> Result<()> {
        let Some(i) = self.indexes.iter().position(|index| index.name == name) else {
            return err(format!("unknown index: {name}"));
        };
        self.indexes.remove(i).destroy()
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
        self.scan_filtered(needed, &[], &Access::Scan)
    }

    /// Like [`Table::scan_columns`], but reads the rows the way `access`
    /// says, which for heaps and LSM trees may be through an index (or an LSM
    /// tree's primary key), in index order. Rows that fail one of
    /// `predicates` may be left out: a column store evaluates them on its
    /// compressed chunks and skips those rows, ignoring `access`. An access
    /// path must come from `predicates`, so that it leaves out only rows they
    /// rule out.
    pub fn scan_filtered(
        &self,
        needed: &[bool],
        predicates: &[Predicate],
        access: &Access,
    ) -> Box<dyn Iterator<Item = Result<(RowId, Row)>> + '_> {
        assert_eq!(needed.len(), self.columns.len());
        match (access.clone(), &self.storage) {
            (Access::Scan, Storage::Heap(heap)) => Box::new(heap.scan().map(|item| {
                let (rid, bytes) = item?;
                Ok((RowId::Record(rid), tuple::decode(&self.columns, &bytes)?))
            })),
            (Access::Scan, Storage::Lsm { tree, .. }) => Box::new(tree.scan().map(|item| {
                let (key, bytes) = item?;
                Ok((RowId::Key(key), tuple::decode(&self.columns, &bytes)?))
            })),
            (_, Storage::Column(store)) => Box::new(
                store
                    .scan(needed.to_vec(), predicates.to_vec())
                    .map(|item| item.map(|(position, row)| (RowId::Position(position), row))),
            ),
            (Access::Key(value), _) => {
                let id = RowId::Key(tuple::encode_key(&value));
                Box::new(self.get(&id).transpose().map(|row| Ok((id, row?))).into_iter())
            }
            (Access::Index { index, lower, upper }, _) => {
                let ids = match self.indexes[index].range(lower.as_ref(), upper.as_ref()) {
                    Ok(ids) => ids,
                    Err(e) => return Box::new(std::iter::once(Err(e))),
                };
                Box::new(ids.into_iter().map(|bytes| {
                    let id = self.row_id(bytes)?;
                    let row = self.get(&id)?.ok_or_else(|| Error(format!("index points to a missing row: {id:?}")))?;
                    Ok((id, row))
                }))
            }
        }
    }

    pub fn insert(&mut self, rows: Vec<Row>) -> Result<()> {
        let encoded = self.encode_all(&rows)?;
        self.check_unique(&HashSet::new(), &rows)?;
        let pk = primary_key(&self.columns);
        let keys: Vec<Vec<u8>> = match &self.storage {
            Storage::Lsm { next_rowid, .. } => (0..)
                .zip(&rows)
                .map(|(i, row)| match pk {
                    Some(pk) => tuple::encode_key(&row[pk]),
                    None => tuple::encode_key(&Value::Int(*next_rowid + i)),
                })
                .collect(),
            _ => Vec::new(),
        };
        match &self.storage {
            Storage::Lsm { .. } => {
                keys.iter().zip(&encoded).try_for_each(|(key, bytes)| LsmTree::check_size(key, bytes))?;
            }
            _ => encoded.iter().try_for_each(|bytes| check_heap_size(bytes))?,
        }
        self.check_index_entries(&rows, |i| keys.get(i).map_or(RECORD_ID_LEN, Vec::len))?;

        let ids: Vec<RowId> = match &mut self.storage {
            Storage::Heap(heap) => {
                encoded.iter().map(|bytes| Ok(RowId::Record(heap.insert(bytes)?))).collect::<Result<_>>()?
            }
            Storage::Lsm { tree, next_rowid } => {
                for (key, bytes) in keys.iter().cloned().zip(encoded) {
                    tree.put(key, bytes)?;
                }
                *next_rowid += rows.len() as i64;
                keys.into_iter().map(RowId::Key).collect()
            }
            Storage::Column(store) => {
                for bytes in encoded {
                    store.insert(&bytes)?;
                }
                Vec::new() // no indexes to add the rows to
            }
        };
        self.add_to_indexes(ids.into_iter().zip(rows))
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
        self.check_unique(&ids.iter().cloned().collect(), &rows)?;
        let pk = primary_key(&self.columns);
        let new_keys: Vec<Vec<u8>> = match &self.storage {
            Storage::Lsm { .. } => ids
                .iter()
                .zip(&rows)
                .map(|(id, row)| match pk {
                    Some(pk) => Ok(tuple::encode_key(&row[pk])),
                    None => id.clone().into_key(),
                })
                .collect::<Result<_>>()?,
            _ => Vec::new(),
        };
        match &self.storage {
            Storage::Lsm { .. } => {
                new_keys.iter().zip(&encoded).try_for_each(|(key, bytes)| LsmTree::check_size(key, bytes))?;
            }
            _ => encoded.iter().try_for_each(|bytes| check_heap_size(bytes))?,
        }
        self.check_index_entries(&rows, |i| new_keys.get(i).map_or(RECORD_ID_LEN, Vec::len))?;
        let old_rows = self.indexed_rows(&ids)?;

        let new_ids: Vec<RowId> = match &mut self.storage {
            Storage::Heap(heap) => {
                let rids: Vec<RecordId> = ids.into_iter().map(RowId::into_record).collect::<Result<_>>()?;
                let mut new_rids = rids.clone();
                let mut moved = Vec::new();
                for (i, (rid, bytes)) in rids.iter().zip(encoded).enumerate() {
                    if !heap.update(*rid, &bytes)? {
                        moved.push((i, bytes));
                    }
                }
                // Delete every moved row first, so a reused slot can't be mistaken for a pending one.
                for (i, _) in &moved {
                    heap.delete(rids[*i])?;
                }
                for (i, bytes) in moved {
                    new_rids[i] = heap.insert(&bytes)?;
                }
                new_rids.into_iter().map(RowId::Record).collect()
            }
            Storage::Lsm { tree, .. } => {
                let mut deletes = Vec::new();
                for (id, new) in ids.into_iter().zip(&new_keys) {
                    let old = id.into_key()?;
                    if &old != new {
                        deletes.push(old);
                    }
                }
                // Deletes first, so a row may take over a key another row is leaving.
                for key in deletes {
                    tree.delete(key)?;
                }
                for (key, bytes) in new_keys.iter().cloned().zip(encoded) {
                    tree.put(key, bytes)?;
                }
                new_keys.into_iter().map(RowId::Key).collect()
            }
            Storage::Column(store) => {
                let positions: Vec<Position> = ids.into_iter().map(RowId::into_position).collect::<Result<_>>()?;
                // Deletes first: an insert may migrate the delta store, moving its rows.
                for position in positions {
                    store.delete(position)?;
                }
                for bytes in encoded {
                    store.insert(&bytes)?;
                }
                Vec::new() // no indexes to update
            }
        };
        // Old entries first, so a new entry may reuse a key an old one is leaving.
        self.remove_from_indexes(&old_rows)?;
        self.add_to_indexes(new_ids.into_iter().zip(rows))
    }

    pub fn delete(&mut self, ids: &[RowId]) -> Result<()> {
        let old_rows = self.indexed_rows(ids)?;
        for id in ids.iter().cloned() {
            match &mut self.storage {
                Storage::Heap(heap) => {
                    heap.delete(id.into_record()?)?;
                }
                Storage::Lsm { tree, .. } => tree.delete(id.into_key()?)?,
                Storage::Column(store) => store.delete(id.into_position()?)?,
            }
        }
        self.remove_from_indexes(&old_rows)
    }

    /// Frees all of the table's pages, its indexes' included.
    pub fn destroy(self) -> Result<()> {
        for index in self.indexes {
            index.destroy()?;
        }
        match self.storage {
            Storage::Heap(heap) => heap.destroy(),
            Storage::Lsm { tree, .. } => tree.destroy(),
            Storage::Column(store) => store.destroy(),
        }
    }

    /// Reads one row of a heap or LSM table by id; `None` if there is none.
    fn get(&self, id: &RowId) -> Result<Option<Row>> {
        let bytes = match (&self.storage, id) {
            (Storage::Heap(heap), RowId::Record(rid)) => heap.get(*rid)?,
            (Storage::Lsm { tree, .. }, RowId::Key(key)) => tree.get(key)?,
            _ => return err(format!("cannot read row {id:?} of table {} directly", self.name)),
        };
        bytes.map(|bytes| tuple::decode(&self.columns, &bytes)).transpose()
    }

    /// The current rows with the given ids, if the table has indexes to remove them from.
    fn indexed_rows(&self, ids: &[RowId]) -> Result<Vec<(RowId, Row)>> {
        if self.indexes.is_empty() {
            return Ok(Vec::new());
        }
        ids.iter()
            .map(|id| {
                let row = self.get(id)?.ok_or_else(|| Error(format!("missing row: {id:?}")))?;
                Ok((id.clone(), row))
            })
            .collect()
    }

    fn add_to_indexes(&mut self, rows: impl IntoIterator<Item = (RowId, Row)>) -> Result<()> {
        if self.indexes.is_empty() {
            return Ok(());
        }
        for (id, row) in rows {
            let id = id.to_bytes();
            for index in &mut self.indexes {
                index.insert(&row[index.column], &id)?;
            }
        }
        Ok(())
    }

    fn remove_from_indexes(&mut self, rows: &[(RowId, Row)]) -> Result<()> {
        for (id, row) in rows {
            let id = id.to_bytes();
            for index in &mut self.indexes {
                index.delete(&row[index.column], &id)?;
            }
        }
        Ok(())
    }

    /// Checks that every index can take the rows; `id_len(i)` is the length of row `i`'s id.
    fn check_index_entries(&self, rows: &[Row], id_len: impl Fn(usize) -> usize) -> Result<()> {
        for index in &self.indexes {
            for (i, row) in rows.iter().enumerate() {
                index.check(&row[index.column], id_len(i))?;
            }
        }
        Ok(())
    }

    /// Turns the bytes an index stores back into a row id.
    fn row_id(&self, bytes: Vec<u8>) -> Result<RowId> {
        match &self.storage {
            Storage::Heap(_) => match <[u8; RECORD_ID_LEN]>::try_from(bytes.as_slice()) {
                Ok([a, b, c, d, e, f]) => Ok(RowId::Record(RecordId {
                    page_id: PageId(u32::from_be_bytes([a, b, c, d])),
                    slot: u16::from_be_bytes([e, f]),
                })),
                Err(_) => err("corrupted index entry: bad record id"),
            },
            Storage::Lsm { .. } => Ok(RowId::Key(bytes)),
            Storage::Column(_) => err("COLUMN tables cannot have indexes"),
        }
    }

    /// Encodes rows, which also checks their types.
    fn encode_all(&self, rows: &[Row]) -> Result<Vec<Vec<u8>>> {
        rows.iter().map(|row| tuple::encode(&self.columns, row)).collect()
    }

    /// Checks that `new_rows`, together with the existing rows except those in
    /// `replaced`, have non-null primary keys and unique values in the primary
    /// key and every unique index.
    fn check_unique(&self, replaced: &HashSet<RowId>, new_rows: &[Row]) -> Result<()> {
        let pk = primary_key(&self.columns);
        if let Some(pk) = pk
            && new_rows.iter().any(|row| row[pk].is_null())
        {
            return err(format!("primary key {} cannot be NULL", self.columns[pk].name));
        }
        let mut columns: Vec<usize> = pk.into_iter().collect();
        for index in self.indexes.iter().filter(|index| index.unique) {
            if !columns.contains(&index.column) {
                columns.push(index.column);
            }
        }
        for column in columns {
            self.check_unique_column(column, pk == Some(column), replaced, new_rows)?;
        }
        Ok(())
    }

    /// Checks one column that must be unique. Uses a unique index on it if
    /// there is one. Otherwise the column is the primary key: an LSM tree is
    /// searched for each new key, a column store reads only its key column,
    /// and a heap is scanned in full (only in files from before heap tables
    /// had a primary key index).
    fn check_unique_column(
        &self,
        column: usize,
        is_pk: bool,
        replaced: &HashSet<RowId>,
        new_rows: &[Row],
    ) -> Result<()> {
        let index = self.indexes.iter().find(|index| index.unique && index.column == column);
        let mut seen = HashSet::new();
        if index.is_none() && !matches!(self.storage, Storage::Lsm { .. }) {
            let needed: Vec<bool> = (0..self.columns.len()).map(|i| i == column).collect();
            for item in self.scan_columns(&needed) {
                let (id, row) = item?;
                if !replaced.contains(&id) {
                    seen.insert(row[column].clone());
                }
            }
        }
        for row in new_rows {
            let value = &row[column];
            if value.is_null() {
                continue;
            }
            let stored = match (index, &self.storage) {
                (Some(index), _) => {
                    let mut stored = false;
                    for bytes in index.get(value)? {
                        stored |= !replaced.contains(&self.row_id(bytes)?);
                    }
                    stored
                }
                (None, Storage::Lsm { tree, .. }) => {
                    let key = tuple::encode_key(value);
                    !replaced.contains(&RowId::Key(key.clone())) && tree.get(&key)?.is_some()
                }
                (None, _) => false, // already in `seen`
            };
            if stored || !seen.insert(value.clone()) {
                let value = value.to_sql();
                return match (is_pk, index) {
                    (false, Some(index)) => err(format!("duplicate value in unique index {}: {value}", index.name)),
                    _ => err(format!("duplicate primary key: {value}")),
                };
            }
        }
        Ok(())
    }
}

impl RowId {
    /// The bytes an index stores for this row.
    fn to_bytes(&self) -> Vec<u8> {
        match self {
            RowId::Record(rid) => [&rid.page_id.0.to_be_bytes()[..], &rid.slot.to_be_bytes()].concat(),
            RowId::Key(key) => key.clone(),
            RowId::Position(_) => unreachable!("COLUMN tables cannot have indexes"),
        }
    }

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

pub fn primary_key(columns: &[Column]) -> Option<usize> {
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
