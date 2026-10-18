//! Column store: rows grouped and split by column (PAX), with a delta store
//! for new rows.
//!
//! ```text
//! insert --> delta store: a table heap of whole rows (NSM)
//!                | `group_rows` rows: migrated
//!                v
//! row group:  [id chunk][name chunk][age chunk] ...  + delete bitmap
//! row group:  [id chunk][name chunk][age chunk] ...  + delete bitmap
//! ```
//!
//! New rows go to the delta store, where they are cheap to insert, update and
//! delete. Once it holds `group_rows` rows they are migrated into a new row
//! group and the delta store starts over; a row is in one place or the other,
//! never both. A row group stores each column's values together in its own
//! chunk, compressed by [`encoding`](super::encoding), so a scan reads only the
//! chunks of the columns it needs. Row groups are never rewritten: deleting a
//! row sets its bit in the group's delete bitmap, updating it deletes it and
//! inserts the new value into the delta store. A row group whose rows are all
//! deleted is freed.
//!
//! The manifest page lists where everything is:
//!
//! ```text
//! 0..4    delta store first page
//! 4..8    row group count n, then n header page ids, oldest first
//! ```
//!
//! A row group header page:
//!
//! ```text
//! 0..4    row count r
//! 4..8    column count c, then c chunk first page ids, in schema order
//!         delete bitmap, ceil(r / 8) bytes; bit i set = row i is deleted
//! ```
//!
//! A chunk is a linked list of pages, each holding the next piece of its bytes:
//!
//! ```text
//! 0..4    next page (u32::MAX = none)
//! 4..6    bytes in this page
//! 6..     bytes
//! ```
//!
//! A migration writes the row group, then rewrites the manifest, and only then
//! frees the old delta store, as an LSM flush does.
//!
//! A scan may be given [`Predicate`]s (`column op literal`, ANDed). In a row
//! group it evaluates them on the compressed chunks first (see
//! [`encoding::filter`]), ANDs the bitmaps they return with the live rows, and
//! decodes the other columns only for the rows left (late materialization). A
//! group with no row left is skipped without decoding anything.

use std::sync::Arc;

use crate::storage::buffer::pool::BufferPool;
use crate::storage::io::disk::{PAGE_USABLE, PageId, decode_id, encode_id, read_u32, write_u32};
use super::encoding;
use crate::storage::heap::{RecordId, TableHeap};
use crate::storage::tuple;
use crate::error::{Result, err};
use crate::predicate::Predicate;
use crate::types::{Column, Value};

const CHUNK_HEADER: usize = 6;
const CHUNK_CAPACITY: usize = PAGE_USABLE - CHUNK_HEADER;

#[derive(Debug, Clone, Copy)]
pub struct ColumnConfig {
    /// Rows in the delta store that trigger a migration into a row group.
    pub group_rows: usize,
}

impl Default for ColumnConfig {
    fn default() -> Self {
        ColumnConfig { group_rows: 1024 }
    }
}

/// Where a row of a column store is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Position {
    Delta(RecordId),
    /// Row `row` of the row group with this header page.
    Group { header: PageId, row: u32 },
}

pub struct ColumnStore {
    pool: Arc<BufferPool>,
    columns: Vec<Column>,
    config: ColumnConfig,
    manifest: PageId,
    delta: TableHeap,
    delta_rows: usize,
    /// Oldest first.
    groups: Vec<RowGroup>,
}

struct RowGroup {
    header: PageId,
    rows: usize,
    /// Each column's chunk, in schema order.
    chunks: Vec<PageId>,
    deleted: Vec<u8>,
    live: usize,
}

impl ColumnStore {
    /// Creates an empty store. Fails if a row group header for this many
    /// columns would not fit in a page.
    pub fn create(pool: Arc<BufferPool>, columns: Vec<Column>, config: ColumnConfig) -> Result<Self> {
        let max_columns = (PAGE_USABLE - 8 - config.group_rows.div_ceil(8)) / 4;
        if columns.len() > max_columns {
            return err(format!("a column table can have at most {max_columns} columns"));
        }
        let delta = TableHeap::create(pool.clone())?;
        pool.flush_page(delta.first_page())?;
        let manifest = pool.new_page()?.id();
        let store = ColumnStore { pool, columns, config, manifest, delta, delta_rows: 0, groups: Vec::new() };
        store.write_manifest()?;
        Ok(store)
    }

    /// Opens a store from its manifest page.
    pub fn open(pool: Arc<BufferPool>, manifest: PageId, columns: Vec<Column>, config: ColumnConfig) -> Result<Self> {
        let (delta_page, headers) = {
            let page = pool.fetch_read(manifest)?;
            let count = read_u32(&page, 4) as usize;
            if 8 + 4 * count > PAGE_USABLE {
                return err("corrupted column store manifest");
            }
            (PageId(read_u32(&page, 0)), (0..count).map(|i| PageId(read_u32(&page, 8 + 4 * i))).collect::<Vec<_>>())
        };
        let groups = headers.into_iter().map(|id| RowGroup::open(&pool, id, columns.len())).collect::<Result<_>>()?;
        let delta = TableHeap::open(pool.clone(), delta_page, delta_page)?;
        let delta_rows = delta.scan().try_fold(0, |n, item| item.map(|_| n + 1))?;
        Ok(ColumnStore { pool, columns, config, manifest, delta, delta_rows, groups })
    }

    pub fn manifest_page(&self) -> PageId {
        self.manifest
    }

    /// Row groups in the store.
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Adds an encoded row (see [`tuple::encode`]) to the delta store,
    /// migrating the delta store into a row group once it is full.
    pub fn insert(&mut self, row: &[u8]) -> Result<()> {
        self.delta.insert(row)?;
        self.delta_rows += 1;
        if self.delta_rows >= self.config.group_rows {
            self.migrate()?;
        }
        Ok(())
    }

    pub fn delete(&mut self, position: Position) -> Result<()> {
        match position {
            Position::Delta(rid) => {
                if !self.delta.delete(rid)? {
                    return err("no such row in the delta store");
                }
                self.delta_rows -= 1;
            }
            Position::Group { header, row } => {
                let Some(index) = self.groups.iter().position(|g| g.header == header) else {
                    return err(format!("no row group at page {}", header.0));
                };
                let group = &mut self.groups[index];
                group.delete(&self.pool, row as usize)?;
                if group.live == 0 {
                    let group = self.groups.remove(index);
                    self.write_manifest()?;
                    group.destroy(&self.pool)?;
                }
            }
        }
        Ok(())
    }

    /// Iterates over the rows that satisfy every one of `predicates`: the row
    /// groups oldest first, then the delta store. Only the columns marked in
    /// `needed` are read from row groups; the others are NULL. Rows from the
    /// delta store are always whole.
    pub fn scan(
        &self,
        needed: Vec<bool>,
        predicates: Vec<Predicate>,
    ) -> impl Iterator<Item = Result<(Position, Vec<Value>)>> + '_ {
        let delta_predicates = predicates.clone();
        let groups = self.groups.iter().flat_map(move |group| {
            let rows = group.read(&self.pool, &self.columns, &needed, &predicates);
            let (rows, error) = match rows {
                Ok(rows) => (rows, None),
                Err(e) => (Vec::new(), Some(Err(e))),
            };
            rows.into_iter().map(Ok).chain(error)
        });
        let delta = self.delta.scan().filter_map(move |item| {
            let read = || {
                let (rid, bytes) = item?;
                Ok((Position::Delta(rid), tuple::decode(&self.columns, &bytes)?))
            };
            match read() {
                Ok((_, row)) if !delta_predicates.iter().all(|p| p.matches(&row)) => None,
                result => Some(result),
            }
        });
        groups.chain(delta)
    }

    /// Frees every page of the store.
    pub fn destroy(self) -> Result<()> {
        self.delta.destroy()?;
        for group in self.groups {
            group.destroy(&self.pool)?;
        }
        self.pool.delete_page(self.manifest)
    }

    /// Moves every row of the delta store into a new row group.
    fn migrate(&mut self) -> Result<()> {
        if 8 + 4 * (self.groups.len() + 1) > PAGE_USABLE {
            return err("column table is full: too many row groups");
        }
        let rows = self
            .delta
            .scan()
            .map(|item| tuple::decode(&self.columns, &item?.1))
            .collect::<Result<Vec<_>>>()?;
        let group = RowGroup::write(&self.pool, &self.columns, &rows)?;
        let new_delta = TableHeap::create(self.pool.clone())?;
        self.pool.flush_page(new_delta.first_page())?;

        self.groups.push(group);
        let old_delta = std::mem::replace(&mut self.delta, new_delta);
        self.delta_rows = 0;
        self.write_manifest()?;
        old_delta.destroy()
    }

    /// Rewrites the manifest page.
    fn write_manifest(&self) -> Result<()> {
        let mut page = self.pool.fetch_write(self.manifest)?;
        write_u32(&mut page, 0, self.delta.first_page().0);
        write_u32(&mut page, 4, self.groups.len() as u32);
        for (i, group) in self.groups.iter().enumerate() {
            write_u32(&mut page, 8 + 4 * i, group.header.0);
        }
        Ok(())
    }
}

impl RowGroup {
    /// Writes rows as a new row group: one compressed chunk per column, then the header.
    fn write(pool: &BufferPool, columns: &[Column], rows: &[Vec<Value>]) -> Result<Self> {
        let mut chunks = Vec::with_capacity(columns.len());
        for (i, column) in columns.iter().enumerate() {
            let values: Vec<Value> = rows.iter().map(|row| row[i].clone()).collect();
            chunks.push(write_chunk(pool, &encoding::encode(column.data_type, &values)?)?);
        }
        let deleted = vec![0; rows.len().div_ceil(8)];

        let mut page = pool.new_page()?;
        write_u32(&mut page, 0, rows.len() as u32);
        write_u32(&mut page, 4, chunks.len() as u32);
        for (i, chunk) in chunks.iter().enumerate() {
            write_u32(&mut page, 8 + 4 * i, chunk.0);
        }
        let header = page.id();
        drop(page);
        pool.flush_page(header)?;
        Ok(RowGroup { header, rows: rows.len(), chunks, deleted, live: rows.len() })
    }

    fn open(pool: &BufferPool, header: PageId, column_count: usize) -> Result<Self> {
        let page = pool.fetch_read(header)?;
        let rows = read_u32(&page, 0) as usize;
        if read_u32(&page, 4) as usize != column_count {
            return err(format!("corrupted row group at page {}: wrong column count", header.0));
        }
        let chunks = (0..column_count).map(|i| PageId(read_u32(&page, 8 + 4 * i))).collect();
        let start = 8 + 4 * column_count;
        let end = start + rows.div_ceil(8);
        if end > PAGE_USABLE {
            return err(format!("corrupted row group at page {}: too many rows", header.0));
        }
        let deleted = page[start..end].to_vec();
        let live = rows - deleted.iter().map(|b| b.count_ones() as usize).sum::<usize>();
        Ok(RowGroup { header, rows, chunks, deleted, live })
    }

    fn is_deleted(&self, row: usize) -> bool {
        encoding::bit(&self.deleted, row)
    }

    /// Sets a row's bit in the delete bitmap, in memory and in the header page.
    fn delete(&mut self, pool: &BufferPool, row: usize) -> Result<()> {
        if row >= self.rows || self.is_deleted(row) {
            return err(format!("no row {row} in the row group at page {}", self.header.0));
        }
        self.deleted[row / 8] |= 1 << (row % 8);
        self.live -= 1;
        let at = 8 + 4 * self.chunks.len() + row / 8;
        pool.fetch_write(self.header)?[at] = self.deleted[row / 8];
        Ok(())
    }

    /// Returns the rows not deleted that satisfy every predicate, with the
    /// needed columns decoded.
    fn read(
        &self,
        pool: &BufferPool,
        columns: &[Column],
        needed: &[bool],
        predicates: &[Predicate],
    ) -> Result<Vec<(Position, Vec<Value>)>> {
        let mut chunks: Vec<Option<Vec<u8>>> = vec![None; columns.len()];
        let mut selected: Vec<u8> = self.deleted.iter().map(|b| !b).collect();
        if !self.rows.is_multiple_of(8) {
            *selected.last_mut().unwrap() &= (1 << (self.rows % 8)) - 1;
        }
        // Find the matching rows on the compressed chunks, stopping once none is left.
        for p in predicates {
            if selected.iter().all(|&b| b == 0) {
                break;
            }
            if chunks[p.column].is_none() {
                chunks[p.column] = Some(read_chunk(pool, self.chunks[p.column])?);
            }
            let bytes = chunks[p.column].as_deref().unwrap();
            let matching = encoding::filter(columns[p.column].data_type, bytes, self.rows, p.op, &p.literal)?;
            selected.iter_mut().zip(matching).for_each(|(s, m)| *s &= m);
        }
        let rows: Vec<usize> = (0..self.rows).filter(|&row| encoding::bit(&selected, row)).collect();
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        // Then decode the needed columns, materializing only those rows.
        let mut values: Vec<Option<std::vec::IntoIter<Value>>> = Vec::with_capacity(columns.len());
        for (i, column) in columns.iter().enumerate() {
            values.push(if needed[i] {
                let bytes = match chunks[i].take() {
                    Some(bytes) => bytes,
                    None => read_chunk(pool, self.chunks[i])?,
                };
                Some(encoding::decode_selected(column.data_type, &bytes, self.rows, &selected)?.into_iter())
            } else {
                None
            });
        }
        Ok(rows
            .into_iter()
            .map(|row| {
                let values = values
                    .iter_mut()
                    .map(|column| column.as_mut().map_or(Value::Null, |c| c.next().expect("one value per row")))
                    .collect();
                (Position::Group { header: self.header, row: row as u32 }, values)
            })
            .collect())
    }

    fn destroy(self, pool: &BufferPool) -> Result<()> {
        for chunk in self.chunks {
            free_chunk(pool, chunk)?;
        }
        pool.delete_page(self.header)
    }
}

/// Writes bytes to a new chain of pages, flushed, and returns its first page.
fn write_chunk(pool: &BufferPool, bytes: &[u8]) -> Result<PageId> {
    let mut next = None;
    // Back to front, so each page knows the one after it.
    let pieces: Vec<&[u8]> = if bytes.is_empty() { vec![bytes] } else { bytes.chunks(CHUNK_CAPACITY).collect() };
    for piece in pieces.into_iter().rev() {
        let mut page = pool.new_page()?;
        write_u32(&mut page, 0, encode_id(next));
        page[4..6].copy_from_slice(&(piece.len() as u16).to_le_bytes());
        page[CHUNK_HEADER..CHUNK_HEADER + piece.len()].copy_from_slice(piece);
        let id = page.id();
        drop(page);
        pool.flush_page(id)?;
        next = Some(id);
    }
    Ok(next.expect("at least one piece"))
}

fn read_chunk(pool: &BufferPool, first: PageId) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut next = Some(first);
    while let Some(id) = next {
        let page = pool.fetch_read(id)?;
        let len = u16::from_le_bytes([page[4], page[5]]) as usize;
        if len > CHUNK_CAPACITY {
            return err(format!("corrupted column chunk at page {}", id.0));
        }
        bytes.extend_from_slice(&page[CHUNK_HEADER..CHUNK_HEADER + len]);
        next = decode_id(read_u32(&page, 0));
    }
    Ok(bytes)
}

fn free_chunk(pool: &BufferPool, first: PageId) -> Result<()> {
    let mut next = Some(first);
    while let Some(id) = next {
        next = decode_id(read_u32(&*pool.fetch_read(id)?, 0));
        pool.delete_page(id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predicate::Compare;
    use crate::storage::DiskManager;
    use crate::types::DataType;

    fn pool() -> Arc<BufferPool> {
        Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 64))
    }

    fn columns() -> Vec<Column> {
        [("id", DataType::Int), ("city", DataType::Text), ("active", DataType::Bool)]
            .map(|(name, data_type)| Column { name: name.into(), data_type, primary_key: false })
            .into()
    }

    fn row(i: i64) -> Vec<Value> {
        let city = ["lima", "oslo", "paris"][i as usize % 3];
        vec![Value::Int(i), Value::Text(city.into()), if i % 5 == 0 { Value::Null } else { Value::Bool(i % 2 == 0) }]
    }

    fn small() -> ColumnConfig {
        ColumnConfig { group_rows: 100 }
    }

    fn insert(store: &mut ColumnStore, range: std::ops::Range<i64>) {
        for i in range {
            store.insert(&tuple::encode(&columns(), &row(i)).unwrap()).unwrap();
        }
    }

    fn scan(store: &ColumnStore, needed: &[bool]) -> Vec<(Position, Vec<Value>)> {
        store.scan(needed.to_vec(), Vec::new()).collect::<Result<_>>().unwrap()
    }

    fn ids(store: &ColumnStore) -> Vec<i64> {
        let mut ids: Vec<i64> = scan(store, &[true, false, false])
            .into_iter()
            .map(|(_, row)| match row[0] {
                Value::Int(n) => n,
                _ => panic!("id is an INT"),
            })
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn full_delta_stores_become_row_groups() {
        let mut store = ColumnStore::create(pool(), columns(), small()).unwrap();
        insert(&mut store, 0..250);
        assert_eq!(store.group_count(), 2);
        assert_eq!(store.delta_rows, 50);

        let all = scan(&store, &[true, true, true]);
        assert_eq!(all.len(), 250);
        for (_, values) in &all {
            let Value::Int(i) = values[0] else { panic!() };
            assert_eq!(*values, row(i));
        }
    }

    #[test]
    fn scans_read_only_the_needed_columns() {
        let mut store = ColumnStore::create(pool(), columns(), small()).unwrap();
        insert(&mut store, 0..100);
        let rows = scan(&store, &[false, true, false]);
        assert_eq!(rows[4].1, vec![Value::Null, Value::Text("oslo".into()), Value::Null]);
    }

    #[test]
    fn scans_evaluate_pushed_down_predicates() {
        let mut store = ColumnStore::create(pool(), columns(), small()).unwrap();
        insert(&mut store, 0..250);
        let all = scan(&store, &[true, true, true]);
        store.delete(all.iter().find(|(_, r)| r[0] == Value::Int(7)).unwrap().0).unwrap();

        let predicate = |column, op, literal| Predicate { column, op, literal };
        let cases = vec![
            vec![predicate(1, Compare::Eq, Value::Text("oslo".into()))],
            vec![predicate(1, Compare::Lt, Value::Text("m".into())), predicate(0, Compare::GtEq, Value::Int(120))],
            vec![predicate(2, Compare::NotEq, Value::Bool(false))],
            vec![predicate(0, Compare::Gt, Value::Int(1000))],
            vec![predicate(0, Compare::LtEq, Value::Int(10)), predicate(0, Compare::Gt, Value::Int(3))],
        ];
        let all = scan(&store, &[true, true, true]);
        for predicates in cases {
            let matches = |row: &[Value]| predicates.iter().all(|p| p.matches(row));
            let expected: Vec<_> = all.iter().filter(|(_, r)| matches(r)).cloned().collect();
            let found: Vec<_> = store.scan(vec![true; 3], predicates.clone()).collect::<Result<_>>().unwrap();
            assert_eq!(found, expected, "{predicates:?}");
        }

        // Rows from row groups and from the delta store, the deleted row left out.
        let ids = |rows: Vec<(Position, Vec<Value>)>| rows.into_iter().map(|(_, r)| r[0].clone()).collect::<Vec<_>>();
        let low = predicate(0, Compare::Lt, Value::Int(9));
        let high = predicate(0, Compare::GtEq, Value::Int(248));
        let rows = |p: &Predicate| store.scan(vec![true, false, false], vec![p.clone()]).collect::<Result<_>>();
        let rows = |p: &Predicate| rows(p).unwrap();
        assert_eq!(ids(rows(&low)), [0, 1, 2, 3, 4, 5, 6, 8].map(Value::Int));
        assert_eq!(ids(rows(&high)), [248, 249].map(Value::Int));

        // The predicate's column need not be returned (rows from the delta store are whole).
        let paris = predicate(1, Compare::Eq, Value::Text("paris".into()));
        let rows = store.scan(vec![true, false, false], vec![paris]).collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(rows.len(), 83);
        assert!(rows.iter().all(|(_, r)| matches!(r[0], Value::Int(i) if i % 3 == 2)));
        assert!(rows.iter().all(|(p, r)| matches!(p, Position::Delta(_)) || r[1] == Value::Null));
    }

    #[test]
    fn compressed_chunks_are_small() {
        let pool = pool();
        let mut store = ColumnStore::create(pool.clone(), columns(), ColumnConfig::default()).unwrap();
        insert(&mut store, 0..1024);
        assert_eq!(store.group_count(), 1);
        let rows: usize = (0..1024).map(|i| tuple::encode(&columns(), &row(i)).unwrap().len()).sum();
        let chunks: usize = store.groups[0].chunks.iter().map(|&c| read_chunk(&pool, c).unwrap().len()).sum();
        // Delta ids, a dictionary of three cities and two bits per BOOL.
        assert!(chunks * 20 < rows, "{chunks} bytes compressed, {rows} as rows");
    }

    #[test]
    fn deletes_mark_the_bitmap_and_survive_reopening() {
        let pool = pool();
        let mut store = ColumnStore::create(pool.clone(), columns(), small()).unwrap();
        insert(&mut store, 0..150);
        let doomed: Vec<Position> = scan(&store, &[true, false, false])
            .into_iter()
            .filter(|(_, row)| matches!(row[0], Value::Int(i) if i % 10 == 0))
            .map(|(position, _)| position)
            .collect();
        doomed.iter().for_each(|&p| store.delete(p).unwrap());
        assert!(store.delete(doomed[0]).is_err(), "already deleted");
        pool.flush_all().unwrap();

        let reopened = ColumnStore::open(pool, store.manifest_page(), columns(), small()).unwrap();
        assert_eq!(ids(&reopened), (0..150).filter(|i| i % 10 != 0).collect::<Vec<_>>());
        assert_eq!(reopened.delta_rows, 45);
    }

    #[test]
    fn empty_row_groups_are_freed() {
        let pool = pool();
        let mut store = ColumnStore::create(pool.clone(), columns(), small()).unwrap();
        insert(&mut store, 0..100);
        let pages = pool.disk().page_count();
        for (position, _) in scan(&store, &[false, false, false]) {
            store.delete(position).unwrap();
        }
        assert_eq!(store.group_count(), 0);
        insert(&mut store, 0..100);
        assert_eq!(pool.disk().page_count(), pages, "freed pages are reused");
    }

    #[test]
    fn chunks_can_span_pages() {
        let pool = pool();
        let bytes: Vec<u8> = (0..10_000u32).map(|i| (i * 31) as u8).collect();
        let first = write_chunk(&pool, &bytes).unwrap();
        assert_eq!(read_chunk(&pool, first).unwrap(), bytes);
        assert!(read_chunk(&pool, write_chunk(&pool, &[]).unwrap()).unwrap().is_empty());
        let pages = pool.disk().page_count();
        free_chunk(&pool, first).unwrap();
        write_chunk(&pool, &bytes).unwrap();
        assert_eq!(pool.disk().page_count(), pages);
    }

    #[test]
    fn destroy_frees_every_page() {
        let pool = pool();
        let fill = || {
            let mut store = ColumnStore::create(pool.clone(), columns(), small()).unwrap();
            insert(&mut store, 0..250);
            store
        };
        fill().destroy().unwrap();
        let pages = pool.disk().page_count();
        fill();
        assert_eq!(pool.disk().page_count(), pages, "freed pages are reused");
    }

    #[test]
    fn too_many_columns_are_rejected() {
        let wide: Vec<Column> = (0..2000)
            .map(|i| Column { name: format!("c{i}"), data_type: DataType::Int, primary_key: false })
            .collect();
        assert!(ColumnStore::create(pool(), wide, ColumnConfig::default()).is_err());
    }
}
