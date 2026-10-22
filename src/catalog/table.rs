use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

use super::index::{Bound, Index, encode_keys};
use crate::error::{Error, Result, err};
use crate::predicate::Predicate;
use crate::sql::ast::CreateIndex;
use crate::storage::{
    BufferPool, ColumnChunk, ColumnConfig, ColumnStore, LsmConfig, LsmTree, MAX_TUPLE_SIZE, PageId,
    Position, RecordId, SharedScan, TableHeap, tuple,
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
    /// How many rows the table has, or [`UNCOUNTED`]: see [`Table::row_count`].
    rows: AtomicU64,
}

/// A row count not counted yet.
const UNCOUNTED: u64 = u64::MAX;

/// The rows of a heap table, read by threads together: see [`Table::shared_scan`].
pub struct SharedTableScan {
    pages: SharedScan,
    columns: Vec<Column>,
    needed: Vec<bool>,
}

impl SharedTableScan {
    /// The rows of the next page no thread has taken, or None once all are.
    pub fn next_rows(&self) -> Result<Option<Vec<Row>>> {
        self.pages
            .next_page(|bytes| tuple::decode_columns(&self.columns, bytes, &self.needed))
    }
}

enum Storage {
    Heap(TableHeap),
    /// Rows are keyed by their primary key or, in a table without one, by a
    /// hidden rowid: 1, 2, 3, ... in insertion order.
    Lsm {
        tree: LsmTree,
        next_rowid: i64,
    },
    Column(ColumnStore),
}

/// How a scan finds its rows; `plan::access` picks one.
#[derive(Debug, Clone, PartialEq)]
pub enum Access {
    /// Read every row.
    Scan,
    /// Look one primary key up in an LSM tree.
    Key(Value),
    /// Read the rows of an LSM tree whose primary keys lie between two
    /// bounds, in key order. A missing bound leaves that end open.
    KeyRange {
        lower: Option<Bound>,
        upper: Option<Bound>,
    },
    /// Read the rows of an index whose first keys equal `equal` and whose
    /// next key lies between two bounds, in index order. A covering index
    /// holds every column the scan needs: the rows are read from its entries,
    /// not from the table.
    Index {
        index: usize,
        equal: Vec<Value>,
        lower: Option<Bound>,
        upper: Option<Bound>,
        covering: bool,
    },
}

impl Table {
    /// Creates an empty table.
    pub fn create(
        pool: Arc<BufferPool>,
        name: String,
        columns: Vec<Column>,
        kind: StorageKind,
    ) -> Result<Self> {
        let storage = match kind {
            StorageKind::Heap => Storage::Heap(TableHeap::create(pool.clone())?),
            StorageKind::Lsm => Storage::Lsm {
                tree: LsmTree::create(pool.clone(), LsmConfig::default())?,
                next_rowid: 1,
            },
            StorageKind::Column => Storage::Column(ColumnStore::create(
                pool.clone(),
                columns.clone(),
                ColumnConfig::default(),
            )?),
        };
        Ok(Table {
            name,
            columns,
            pool,
            storage,
            indexes: Vec::new(),
            rows: AtomicU64::new(0),
        })
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
            StorageKind::Column => Storage::Column(ColumnStore::open(
                pool.clone(),
                first,
                columns.clone(),
                ColumnConfig::default(),
            )?),
        };
        Ok(Table {
            name,
            columns,
            pool,
            storage,
            indexes: Vec::new(),
            rows: AtomicU64::new(UNCOUNTED),
        })
    }

    pub fn storage_kind(&self) -> StorageKind {
        match self.storage {
            Storage::Heap(_) => StorageKind::Heap,
            Storage::Lsm { .. } => StorageKind::Lsm,
            Storage::Column(_) => StorageKind::Column,
        }
    }

    /// A scan of every row that threads share, a page at a time, if the
    /// table is a heap; the columns not marked in `needed` are left NULL.
    /// It holds no borrow of the table, which must not change while it runs.
    pub fn shared_scan(&self, needed: &[bool]) -> Option<SharedTableScan> {
        assert_eq!(needed.len(), self.columns.len());
        match &self.storage {
            Storage::Heap(heap) => Some(SharedTableScan {
                pages: heap.shared_scan(),
                columns: self.columns.clone(),
                needed: needed.to_vec(),
            }),
            _ => None,
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
    /// if the table is a column table, or the definition names what the
    /// table does not have, or the index is unique and two indexed rows
    /// share their keys, or an entry is too long.
    pub fn create_index(&mut self, definition: &CreateIndex) -> Result<()> {
        if let Storage::Column(_) = self.storage {
            return err("COLUMN tables cannot have indexes");
        }
        let mut index = Index::create(self.pool.clone(), definition, &self.columns)?;
        let mut build = || {
            for item in self.scan() {
                let (id, row) = item?;
                let id = id.to_bytes();
                let Some(entry) = index.entry(&row, &id)? else {
                    continue;
                };
                index.check(&row, id.len())?;
                if index.unique
                    && let Some(keys) = unique_keys(&index, &row)?
                    && !index.find(&encode_keys(&keys))?.is_empty()
                {
                    let name = &index.name;
                    return err(format!(
                        "cannot create unique index {name}: duplicate value {}",
                        show(&keys)
                    ));
                }
                index.insert_entry(&entry)?;
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
    pub fn open_index(&mut self, definition: &CreateIndex, root: PageId) -> Result<()> {
        self.indexes.push(Index::open(
            self.pool.clone(),
            definition,
            &self.columns,
            root,
        )?);
        Ok(())
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
    /// skips their chunks, and heaps and LSM trees skip their bytes.
    pub fn scan_columns(
        &self,
        needed: &[bool],
    ) -> Box<dyn Iterator<Item = Result<(RowId, Row)>> + '_> {
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
        let decode = {
            let needed = needed.to_vec();
            move |bytes: &[u8]| tuple::decode_columns(&self.columns, bytes, &needed)
        };
        match (access.clone(), &self.storage) {
            (Access::Scan, Storage::Heap(heap)) => Box::new(
                heap.scan_map(decode)
                    .map(|item| item.map(|(rid, row)| (RowId::Record(rid), row))),
            ),
            (Access::Scan, Storage::Lsm { tree, .. }) => {
                Box::new(tree.scan_map(&[], move |key, bytes| {
                    Ok((RowId::Key(key.to_vec()), decode(bytes)?))
                }))
            }
            (_, Storage::Column(store)) => Box::new(
                store
                    .scan(needed.to_vec(), predicates.to_vec())
                    .map(|item| item.map(|(position, row)| (RowId::Position(position), row))),
            ),
            (Access::Key(value), _) => {
                let id = RowId::Key(tuple::encode_key(&value));
                Box::new(
                    self.get(&id)
                        .transpose()
                        .map(|row| Ok((id, row?)))
                        .into_iter(),
                )
            }
            (Access::KeyRange { lower, upper }, Storage::Lsm { tree, .. }) => {
                // Keys are encoded so that their bytes sort as the values do.
                let lower = lower.map(|b| (tuple::encode_key(&b.value), b.inclusive));
                let upper = upper.map(|b| (tuple::encode_key(&b.value), b.inclusive));
                let start = lower.as_ref().map_or(Vec::new(), |(key, _)| key.clone());
                let below = move |key: &[u8]| {
                    upper.as_ref().is_none_or(|(upper, inclusive)| {
                        key < upper.as_slice() || (*inclusive && key == upper)
                    })
                };
                fn key(item: &Result<(RowId, Row)>) -> Option<&[u8]> {
                    match item {
                        Ok((RowId::Key(key), _)) => Some(key),
                        _ => None,
                    }
                }
                let rows = tree
                    .scan_map(&start, move |key, bytes| Ok((RowId::Key(key.to_vec()), decode(bytes)?)))
                    .skip_while(move |item| {
                        matches!((key(item), &lower), (Some(key), Some((bound, false))) if key == bound.as_slice())
                    })
                    .take_while(move |item| key(item).is_none_or(&below));
                Box::new(rows)
            }
            (Access::KeyRange { .. }, Storage::Heap(_)) => {
                unreachable!("only an LSM tree has key ranges")
            }
            (
                Access::Index {
                    index,
                    equal,
                    lower,
                    upper,
                    covering,
                },
                _,
            ) => {
                let index = &self.indexes[index];
                let entries = match index.scan(&equal, lower.as_ref(), upper.as_ref()) {
                    Ok(entries) => entries,
                    Err(e) => return Box::new(std::iter::once(Err(e))),
                };
                let needed = needed.to_vec();
                Box::new(entries.into_iter().map(move |entry| {
                    let id = self.row_id(index.row_id(&entry)?.to_vec())?;
                    let row = match covering {
                        true => index.row(&entry, &needed)?,
                        false => self.get(&id)?.ok_or_else(|| {
                            Error(format!("index points to a missing row: {id:?}"))
                        })?,
                    };
                    Ok((id, row))
                }))
            }
        }
    }

    /// The rows [`Table::scan_filtered`] reads, left in their columns, if the
    /// table keeps them so: a column table does (see
    /// [`ColumnStore::scan_by_column`]), the others do not.
    pub fn scan_by_column(
        &self,
        needed: &[bool],
        predicates: &[Predicate],
        chunk: usize,
    ) -> Option<Box<dyn Iterator<Item = Result<ColumnChunk>> + '_>> {
        match &self.storage {
            Storage::Column(store) => Some(Box::new(store.scan_by_column(
                needed.to_vec(),
                predicates.to_vec(),
                chunk,
            ))),
            _ => None,
        }
    }

    /// For a column table, how many row groups scans have read, and how many
    /// their zone maps let them skip, since the table was opened.
    pub fn zone_stats(&self) -> Option<(u64, u64)> {
        match &self.storage {
            Storage::Column(store) => Some(store.zone_stats()),
            _ => None,
        }
    }

    /// How many rows the table has, for the planner to weigh plans by. The
    /// first call after the table is opened counts them with a scan (as an
    /// ANALYZE would), and writes keep the count from then on. Opening the
    /// catalog again, as a rollback does, forgets it.
    pub fn row_count(&self) -> Result<u64> {
        let rows = self.rows.load(Relaxed);
        if rows != UNCOUNTED {
            return Ok(rows);
        }
        let mut rows = 0;
        for item in self.scan_columns(&vec![false; self.columns.len()]) {
            item?;
            rows += 1;
        }
        self.rows.store(rows, Relaxed);
        Ok(rows)
    }

    /// Adds `change` to the row count, if it was counted.
    fn count(&self, change: i64) {
        let _ = self.rows.fetch_update(Relaxed, Relaxed, |rows| {
            (rows != UNCOUNTED).then(|| rows.saturating_add_signed(change))
        });
    }

    pub fn insert(&mut self, rows: Vec<Row>) -> Result<()> {
        let rows = self.coerce_all(rows);
        let encoded = self.encode_all(&rows)?;
        self.check_unique(&HashSet::new(), &rows, &[])?;
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
                keys.iter()
                    .zip(&encoded)
                    .try_for_each(|(key, bytes)| LsmTree::check_size(key, bytes))?;
            }
            _ => encoded
                .iter()
                .try_for_each(|bytes| check_heap_size(bytes))?,
        }
        self.check_index_entries(&rows, |i| keys.get(i).map_or(RECORD_ID_LEN, Vec::len))?;

        let ids: Vec<RowId> = match &mut self.storage {
            Storage::Heap(heap) => encoded
                .iter()
                .map(|bytes| Ok(RowId::Record(heap.insert(bytes)?)))
                .collect::<Result<_>>()?,
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
        let added = rows.len() as i64;
        self.add_to_indexes(ids.into_iter().zip(rows))?;
        self.count(added);
        Ok(())
    }

    /// Replaces each row with the given id, whose stored value is the first
    /// row given, by the second.
    ///
    /// A heap row stays where it is if its page has room for the new value;
    /// otherwise it is deleted and inserted again, possibly in another page.
    /// An LSM row is simply written again, under a new key if its primary key changed.
    /// A column store row is deleted and inserted again into the delta store.
    /// Index entries change only for rows whose indexed value or id does.
    pub fn update(&mut self, changes: Vec<(RowId, Row, Row)>) -> Result<()> {
        let (old_rows, rows): (Vec<(RowId, Row)>, Vec<Row>) = changes
            .into_iter()
            .map(|(id, old, new)| ((id, old), new))
            .unzip();
        let rows = self.coerce_all(rows);
        let ids: Vec<RowId> = old_rows.iter().map(|(id, _)| id.clone()).collect();
        let encoded = self.encode_all(&rows)?;
        self.check_unique(&ids.iter().cloned().collect(), &rows, &old_rows)?;
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
                new_keys
                    .iter()
                    .zip(&encoded)
                    .try_for_each(|(key, bytes)| LsmTree::check_size(key, bytes))?;
            }
            _ => encoded
                .iter()
                .try_for_each(|bytes| check_heap_size(bytes))?,
        }
        self.check_index_entries(&rows, |i| new_keys.get(i).map_or(RECORD_ID_LEN, Vec::len))?;

        let new_ids: Vec<RowId> = match &mut self.storage {
            Storage::Heap(heap) => {
                let rids: Vec<RecordId> = ids
                    .into_iter()
                    .map(RowId::into_record)
                    .collect::<Result<_>>()?;
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
                let positions: Vec<Position> = ids
                    .into_iter()
                    .map(RowId::into_position)
                    .collect::<Result<_>>()?;
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
        if self.indexes.is_empty() {
            return Ok(());
        }
        // An entry that is the same for the old and new row stays: its keys,
        // included columns and row id are unchanged, or the row was and is
        // left out. Old entries go first, so a new entry may reuse a key an
        // old one is leaving.
        let new_rows: Vec<(RowId, Row)> = new_ids.into_iter().zip(rows).collect();
        for index in &mut self.indexes {
            let mut changed = Vec::new();
            for ((old_id, old), (new_id, new)) in old_rows.iter().zip(&new_rows) {
                let entries = (
                    index.entry(old, &old_id.to_bytes())?,
                    index.entry(new, &new_id.to_bytes())?,
                );
                if entries.0 != entries.1 {
                    changed.push(entries);
                }
            }
            for old in changed.iter().filter_map(|(old, _)| old.as_ref()) {
                index.delete_entry(old)?;
            }
            for new in changed.iter().filter_map(|(_, new)| new.as_ref()) {
                index.insert_entry(new)?;
            }
        }
        Ok(())
    }

    /// Deletes the rows with these ids, given with their stored values of
    /// every column their index entries are made from (see
    /// [`Table::indexed_columns`]; the other columns' values are not read).
    pub fn delete(&mut self, rows: &[(RowId, Row)]) -> Result<()> {
        for (id, _) in rows.iter().cloned() {
            match &mut self.storage {
                Storage::Heap(heap) => {
                    heap.delete(id.into_record()?)?;
                }
                Storage::Lsm { tree, .. } => tree.delete(id.into_key()?)?,
                Storage::Column(store) => store.delete(id.into_position()?)?,
            }
        }
        self.remove_from_indexes(rows)?;
        self.count(-(rows.len() as i64));
        Ok(())
    }

    /// Which columns index entries are made from, that [`Table::delete`]
    /// needs the values of: index keys, included columns, and the columns of
    /// key expressions and partial indexes' conditions.
    pub fn indexed_columns(&self) -> Vec<bool> {
        let mut indexed = vec![false; self.columns.len()];
        for index in &self.indexes {
            index.reads().iter().for_each(|&i| indexed[i] = true);
        }
        indexed
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
            _ => {
                return err(format!(
                    "cannot read row {id:?} of table {} directly",
                    self.name
                ));
            }
        };
        bytes
            .map(|bytes| tuple::decode(&self.columns, &bytes))
            .transpose()
    }

    fn add_to_indexes(&mut self, rows: impl IntoIterator<Item = (RowId, Row)>) -> Result<()> {
        if self.indexes.is_empty() {
            return Ok(());
        }
        for (id, row) in rows {
            let id = id.to_bytes();
            for index in &mut self.indexes {
                index.insert(&row, &id)?;
            }
        }
        Ok(())
    }

    fn remove_from_indexes(&mut self, rows: &[(RowId, Row)]) -> Result<()> {
        if self.indexes.is_empty() {
            return Ok(());
        }
        for (id, row) in rows {
            let id = id.to_bytes();
            for index in &mut self.indexes {
                index.delete(row, &id)?;
            }
        }
        Ok(())
    }

    /// Checks that every index can take the rows; `id_len(i)` is the length of row `i`'s id.
    fn check_index_entries(&self, rows: &[Row], id_len: impl Fn(usize) -> usize) -> Result<()> {
        for index in &self.indexes {
            for (i, row) in rows.iter().enumerate() {
                index.check(row, id_len(i))?;
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

    /// Rows with each value made one of its column's type where it equals
    /// one (an INT stored in a REAL column, 2.0 in an INT one); others are
    /// left for encoding to reject. These are the values a row is stored with.
    pub fn coerce_all(&self, mut rows: Vec<Row>) -> Vec<Row> {
        for row in &mut rows {
            for (value, column) in row.iter_mut().zip(&self.columns) {
                if !value.fits(column.data_type)
                    && let Some(coerced) = value.coerce(column.data_type)
                {
                    *value = coerced;
                }
            }
        }
        rows
    }

    /// Encodes rows, which also checks their types.
    fn encode_all(&self, rows: &[Row]) -> Result<Vec<Vec<u8>>> {
        rows.iter()
            .map(|row| tuple::encode(&self.columns, row))
            .collect()
    }

    /// Checks that `new_rows`, together with the existing rows except those in
    /// `replaced`, have non-null primary keys, unique primary keys, and unique
    /// keys in every unique index. `old_rows` are the rows the new ones
    /// replace, in the same order, if known: keys a row keeps are unique
    /// among the stored rows already, and are checked only against the new ones.
    fn check_unique(
        &self,
        replaced: &HashSet<RowId>,
        new_rows: &[Row],
        old_rows: &[(RowId, Row)],
    ) -> Result<()> {
        let pk = primary_key(&self.columns);
        if let Some(pk) = pk
            && new_rows.iter().any(|row| row[pk].is_null())
        {
            return err(format!(
                "primary key {} cannot be NULL",
                self.columns[pk].name
            ));
        }
        for index in self.indexes.iter().filter(|index| index.unique) {
            let is_pk = pk.is_some_and(|pk| index.is_on_column(pk));
            self.check_unique_index(index, is_pk, replaced, new_rows, old_rows)?;
        }
        if let Some(pk) = pk
            && !self
                .indexes
                .iter()
                .any(|index| index.unique && index.is_on_column(pk))
        {
            self.check_unique_key(pk, replaced, new_rows, old_rows)?;
        }
        Ok(())
    }

    /// Checks the keys of a unique index: among the new rows it indexes, and
    /// against the stored ones it lists.
    fn check_unique_index(
        &self,
        index: &Index,
        is_pk: bool,
        replaced: &HashSet<RowId>,
        new_rows: &[Row],
        old_rows: &[(RowId, Row)],
    ) -> Result<()> {
        let mut seen = HashSet::new();
        for (i, row) in new_rows.iter().enumerate() {
            let Some(keys) = unique_keys(index, row)? else {
                continue;
            };
            let encoded = encode_keys(&keys);
            let kept = match old_rows.get(i) {
                Some((_, old)) => {
                    unique_keys(index, old)?.is_some_and(|old| encode_keys(&old) == encoded)
                }
                None => false,
            };
            let mut stored = false;
            if !kept {
                for bytes in index.find(&encoded)? {
                    stored |= !replaced.contains(&self.row_id(bytes)?);
                }
            }
            if stored || !seen.insert(encoded) {
                return match is_pk {
                    true => err(format!("duplicate primary key: {}", show(&keys))),
                    false => err(format!(
                        "duplicate value in unique index {}: {}",
                        index.name,
                        show(&keys)
                    )),
                };
            }
        }
        Ok(())
    }

    /// Checks a primary key that has no unique index: an LSM tree is
    /// searched for each new key, a column store reads only its key column,
    /// and a heap is scanned in full (only in files from before heap tables
    /// had a primary key index).
    fn check_unique_key(
        &self,
        column: usize,
        replaced: &HashSet<RowId>,
        new_rows: &[Row],
        old_rows: &[(RowId, Row)],
    ) -> Result<()> {
        let mut seen = HashSet::new();
        if !matches!(self.storage, Storage::Lsm { .. }) {
            let needed: Vec<bool> = (0..self.columns.len()).map(|i| i == column).collect();
            for item in self.scan_columns(&needed) {
                let (id, row) = item?;
                if !replaced.contains(&id) {
                    seen.insert(row[column].clone());
                }
            }
        }
        for (i, row) in new_rows.iter().enumerate() {
            let value = &row[column];
            let kept = old_rows
                .get(i)
                .is_some_and(|(_, old)| &old[column] == value);
            let stored = match &self.storage {
                Storage::Lsm { tree, .. } if !kept => {
                    let key = tuple::encode_key(value);
                    !replaced.contains(&RowId::Key(key.clone())) && tree.get(&key)?.is_some()
                }
                _ => false, // kept, or already in `seen`
            };
            if stored || !seen.insert(value.clone()) {
                return err(format!("duplicate primary key: {}", value.to_sql()));
            }
        }
        Ok(())
    }
}

/// A row's keys in a unique index, if they must be unique: the row is
/// indexed and none of them is NULL.
fn unique_keys(index: &Index, row: &[Value]) -> Result<Option<Vec<Value>>> {
    Ok(index
        .key_values(row)?
        .filter(|keys| !keys.iter().any(Value::is_null)))
}

/// Keys as an error message shows them: one as a literal, several in parentheses.
fn show(keys: &[Value]) -> String {
    match keys {
        [key] => key.to_sql(),
        keys => format!(
            "({})",
            keys.iter()
                .map(Value::to_sql)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

impl RowId {
    /// The bytes an index stores for this row.
    fn to_bytes(&self) -> Vec<u8> {
        match self {
            RowId::Record(rid) => {
                [&rid.page_id.0.to_be_bytes()[..], &rid.slot.to_be_bytes()].concat()
            }
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
        return err(format!(
            "row is {} bytes, the maximum is {MAX_TUPLE_SIZE}",
            bytes.len()
        ));
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
