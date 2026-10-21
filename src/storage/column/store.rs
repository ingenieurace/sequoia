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
//! 4..8    column count c, with bit 31 set if the group has a zone map
//!         c chunk first page ids, in schema order
//!         the zone map's first page id, if it has one
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
//!
//! **Zone maps.** Before reading any of a group's chunks, a scan checks its
//! predicates against the group's zone map: each column's least and
//! greatest values and how many are NULL, counted when the group was
//! written, and kept in memory. A group with a predicate no row can satisfy
//! (`k < 100` where the least `k` is 5000, or any predicate on a column of
//! NULLs) is skipped without reading a page, and a predicate every row
//! satisfies is not evaluated. Deletes leave a zone map as it was: true of
//! the rows left too, if less tight. It pays on columns whose values come
//! in order of insertion, as ids and times do, where each group holds a
//! narrow range. [`ColumnStore::zone_stats`] counts the groups skipped. Groups
//! written before zone maps existed have none, and are read as before.
//!
//! A zone map is stored like a chunk (see below), in its own pages:
//!
//! ```text
//! c x u32         each column's NULL count
//! u32 + bytes     the least value of each column, as a row (see `tuple.rs`):
//!                 NULL for a column of NULLs alone
//! u32 + bytes     the greatest
//! ```
//!
//! [`ColumnStore::scan`] then puts each row together from its columns' values;
//! [`ColumnStore::scan_by_column`] leaves them in their columns, for the
//! vectorized executor, which works on columns itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::storage::buffer::pool::BufferPool;
use crate::storage::io::disk::{PAGE_USABLE, PageId, decode_id, encode_id, read_u32, write_u32};
use super::encoding;
use crate::storage::heap::{RecordId, TableHeap};
use crate::storage::tuple;
use crate::error::{Result, err};
use crate::predicate::{Compare, Predicate};
use crate::types::{Column, Value};

const CHUNK_HEADER: usize = 6;
/// The bit of a row group header's column count that says it has a zone map.
const ZONED: u32 = 1 << 31;
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

/// Rows handed over a column at a time: how many, and each column's values
/// for them, if it was read.
#[derive(Debug)]
pub struct ColumnChunk {
    pub rows: usize,
    pub columns: Vec<Option<Vec<Value>>>,
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
    /// Row groups scans read, and skipped by their zone maps, since the
    /// store was opened.
    groups_read: AtomicU64,
    groups_skipped: AtomicU64,
}

struct RowGroup {
    header: PageId,
    rows: usize,
    /// Each column's chunk, in schema order.
    chunks: Vec<PageId>,
    /// Each column's zone, in schema order, and the first page they are
    /// stored in; none for a group written before zone maps.
    zones: Option<(Vec<Zone>, PageId)>,
    /// Where the delete bitmap starts in the header page.
    bitmap_at: usize,
    deleted: Vec<u8>,
    live: usize,
}

/// What a column of a row group holds, as a zone map keeps it: its least and
/// greatest values (none if every one is NULL) and how many are NULL.
#[derive(Debug, Clone, PartialEq)]
struct Zone {
    range: Option<(Value, Value)>,
    nulls: u32,
}

/// Which rows of a row group a predicate may hold for, by its zone.
#[derive(Debug, PartialEq)]
enum Holds {
    /// None of them: the group can be skipped.
    None,
    /// Some, maybe: the chunk must be read to tell which.
    Some,
    /// All of them, NULLs aside, and there are none.
    All,
}

impl Zone {
    /// The zone of a column's values.
    fn of(values: &[Value]) -> Zone {
        let nulls = values.iter().filter(|v| v.is_null()).count() as u32;
        let mut present = values.iter().filter(|v| !v.is_null());
        let range = present.next().map(|first| {
            present.fold((first, first), |(min, max), v| {
                (if v.sort_cmp(min).is_lt() { v } else { min }, if v.sort_cmp(max).is_gt() { v } else { max })
            })
        });
        Zone { range: range.map(|(min, max)| (min.clone(), max.clone())), nulls }
    }

    /// Which rows `column op literal` may hold for. A value never compared
    /// with the literal is NULL, and a NULL satisfies no predicate.
    fn holds(&self, op: Compare, literal: &Value) -> Holds {
        let Some((min, max)) = &self.range else { return Holds::None };
        // A literal of another type: the chunk's filter reports the error.
        let (Some(low), Some(high)) = (min.compare(literal), max.compare(literal)) else { return Holds::Some };
        let (none, all) = match op {
            Compare::Eq => (low.is_gt() || high.is_lt(), low.is_eq() && high.is_eq()),
            Compare::NotEq => (low.is_eq() && high.is_eq(), low.is_gt() || high.is_lt()),
            Compare::Lt => (low.is_ge(), high.is_lt()),
            Compare::LtEq => (low.is_gt(), high.is_le()),
            Compare::Gt => (high.is_le(), low.is_gt()),
            Compare::GtEq => (high.is_lt(), low.is_ge()),
        };
        match (none, all && self.nulls == 0) {
            (true, _) => Holds::None,
            (false, true) => Holds::All,
            (false, false) => Holds::Some,
        }
    }
}

/// Encodes the zones of a row group's columns, as its zone map stores them.
fn encode_zones(columns: &[Column], zones: &[Zone]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    zones.iter().for_each(|zone| bytes.extend_from_slice(&zone.nulls.to_le_bytes()));
    for end in [0, 1] {
        let row: Vec<Value> = zones
            .iter()
            .map(|zone| zone.range.as_ref().map_or(Value::Null, |(min, max)| [min, max][end].clone()))
            .collect();
        let row = tuple::encode(columns, &row)?;
        bytes.extend_from_slice(&(row.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&row);
    }
    Ok(bytes)
}

fn decode_zones(columns: &[Column], bytes: &[u8]) -> Result<Vec<Zone>> {
    fn corrupted<T>() -> Result<T> {
        err("corrupted zone map")
    }
    let mut pos = 0;
    let u32_at = |pos: &mut usize| -> Result<u32> {
        let Some(word) = bytes.get(*pos..*pos + 4) else { return corrupted() };
        *pos += 4;
        Ok(u32::from_le_bytes(word.try_into().expect("4 bytes")))
    };
    let nulls = (0..columns.len()).map(|_| u32_at(&mut pos)).collect::<Result<Vec<_>>>()?;
    let mut ends = Vec::new();
    for _ in 0..2 {
        let len = u32_at(&mut pos)? as usize;
        let Some(row) = bytes.get(pos..pos + len) else { return corrupted() };
        ends.push(tuple::decode(columns, row)?);
        pos += len;
    }
    if pos != bytes.len() {
        return corrupted();
    }
    let (min, max) = (ends.swap_remove(0), ends.swap_remove(0));
    Ok(min
        .into_iter()
        .zip(max)
        .zip(nulls)
        .map(|((min, max), nulls)| Zone { range: (!min.is_null()).then_some((min, max)), nulls })
        .collect())
}

impl ColumnStore {
    /// Creates an empty store. Fails if a row group header for this many
    /// columns would not fit in a page.
    pub fn create(pool: Arc<BufferPool>, columns: Vec<Column>, config: ColumnConfig) -> Result<Self> {
        let max_columns = (PAGE_USABLE - 12 - config.group_rows.div_ceil(8)) / 4;
        if columns.len() > max_columns {
            return err(format!("a column table can have at most {max_columns} columns"));
        }
        let delta = TableHeap::create(pool.clone())?;
        pool.flush_page(delta.first_page())?;
        let manifest = pool.new_page()?.id();
        let store = ColumnStore {
            pool,
            columns,
            config,
            manifest,
            delta,
            delta_rows: 0,
            groups: Vec::new(),
            groups_read: AtomicU64::new(0),
            groups_skipped: AtomicU64::new(0),
        };
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
        let groups = headers.into_iter().map(|id| RowGroup::open(&pool, id, &columns)).collect::<Result<_>>()?;
        let delta = TableHeap::open(pool.clone(), delta_page, delta_page)?;
        let delta_rows = delta.scan().try_fold(0, |n, item| item.map(|_| n + 1))?;
        let (groups_read, groups_skipped) = (AtomicU64::new(0), AtomicU64::new(0));
        Ok(ColumnStore { pool, columns, config, manifest, delta, delta_rows, groups, groups_read, groups_skipped })
    }

    /// How many row groups scans have read, and how many their zone maps let
    /// them skip, since the store was opened.
    pub fn zone_stats(&self) -> (u64, u64) {
        (self.groups_read.load(Relaxed), self.groups_skipped.load(Relaxed))
    }

    /// Whether a scan with these predicates must read the group, counted.
    fn must_read(&self, group: &RowGroup, predicates: &[Predicate]) -> bool {
        let read = group.may_match(predicates);
        let counter = if read { &self.groups_read } else { &self.groups_skipped };
        counter.fetch_add(1, Relaxed);
        read
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
        let delta = self.delta_rows(predicates.clone());
        let zone_predicates = predicates.clone();
        let read = self.groups.iter().filter(move |group| self.must_read(group, &zone_predicates));
        let groups = read.flat_map(move |group| {
            let rows = group.read(&self.pool, &self.columns, &needed, &predicates);
            let (rows, error) = match rows {
                Ok(rows) => (rows, None),
                Err(e) => (Vec::new(), Some(Err(e))),
            };
            rows.into_iter().map(Ok).chain(error)
        });
        groups.chain(delta)
    }

    /// The rows [`ColumnStore::scan`] returns, in its order, left in their
    /// columns: each row group's as it decodes them, then the delta store's
    /// `chunk` rows at a time, taken apart into columns. A chunk holds at
    /// least one row.
    pub fn scan_by_column(
        &self,
        needed: Vec<bool>,
        predicates: Vec<Predicate>,
        chunk: usize,
    ) -> impl Iterator<Item = Result<ColumnChunk>> + '_ {
        let mut delta = self.delta_rows(predicates.clone());
        let width = self.columns.len();
        let (mut error, mut done) = (None, false);
        let delta = std::iter::from_fn(move || {
            if done {
                return error.take().map(Err);
            }
            let mut columns: Vec<Vec<Value>> = vec![Vec::new(); width];
            let mut rows = 0;
            while rows < chunk {
                match delta.next() {
                    Some(Ok((_, row))) => {
                        columns.iter_mut().zip(row).for_each(|(column, value)| column.push(value));
                        rows += 1;
                    }
                    Some(Err(e)) => {
                        (error, done) = (Some(e), true);
                        break;
                    }
                    None => {
                        done = true;
                        break;
                    }
                }
            }
            match rows {
                0 => error.take().map(Err),
                rows => Some(Ok(ColumnChunk { rows, columns: columns.into_iter().map(Some).collect() })),
            }
        });
        let groups = self.groups.iter().filter_map(move |group| {
            if !self.must_read(group, &predicates) {
                return None;
            }
            group.read_columns(&self.pool, &self.columns, &needed, &predicates).transpose()
        });
        groups.chain(delta)
    }

    /// The delta store's rows that satisfy every one of `predicates`, whole.
    fn delta_rows(&self, predicates: Vec<Predicate>) -> impl Iterator<Item = Result<(Position, Vec<Value>)>> + '_ {
        self.delta.scan().filter_map(move |item| {
            let read = || {
                let (rid, bytes) = item?;
                Ok((Position::Delta(rid), tuple::decode(&self.columns, &bytes)?))
            };
            match read() {
                Ok((_, row)) if !predicates.iter().all(|p| p.matches(&row)) => None,
                result => Some(result),
            }
        })
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
        let zones: Vec<Zone> = (0..columns.len())
            .map(|i| Zone::of(&rows.iter().map(|row| row[i].clone()).collect::<Vec<_>>()))
            .collect();
        let zone_page = write_chunk(pool, &encode_zones(columns, &zones)?)?;
        let deleted = vec![0; rows.len().div_ceil(8)];

        let mut page = pool.new_page()?;
        write_u32(&mut page, 0, rows.len() as u32);
        write_u32(&mut page, 4, chunks.len() as u32 | ZONED);
        for (i, chunk) in chunks.iter().enumerate() {
            write_u32(&mut page, 8 + 4 * i, chunk.0);
        }
        write_u32(&mut page, 8 + 4 * chunks.len(), zone_page.0);
        let header = page.id();
        drop(page);
        pool.flush_page(header)?;
        let bitmap_at = 12 + 4 * chunks.len();
        Ok(RowGroup { header, rows: rows.len(), chunks, zones: Some((zones, zone_page)), bitmap_at, deleted, live: rows.len() })
    }

    fn open(pool: &BufferPool, header: PageId, columns: &[Column]) -> Result<Self> {
        let page = pool.fetch_read(header)?;
        let rows = read_u32(&page, 0) as usize;
        let count = read_u32(&page, 4);
        if (count & !ZONED) as usize != columns.len() {
            return err(format!("corrupted row group at page {}: wrong column count", header.0));
        }
        let chunks: Vec<PageId> = (0..columns.len()).map(|i| PageId(read_u32(&page, 8 + 4 * i))).collect();
        let zone_page = (count & ZONED != 0).then(|| PageId(read_u32(&page, 8 + 4 * chunks.len())));
        let bitmap_at = 8 + 4 * chunks.len() + if zone_page.is_some() { 4 } else { 0 };
        let end = bitmap_at + rows.div_ceil(8);
        if end > PAGE_USABLE {
            return err(format!("corrupted row group at page {}: too many rows", header.0));
        }
        let deleted = page[bitmap_at..end].to_vec();
        drop(page);
        let live = rows - deleted.iter().map(|b| b.count_ones() as usize).sum::<usize>();
        let zones = match zone_page {
            Some(first) => Some((decode_zones(columns, &read_chunk(pool, first)?)?, first)),
            None => None,
        };
        Ok(RowGroup { header, rows, chunks, zones, bitmap_at, deleted, live })
    }

    /// Whether some row of the group may satisfy every predicate, by its zone map.
    fn may_match(&self, predicates: &[Predicate]) -> bool {
        let Some((zones, _)) = &self.zones else { return true };
        predicates.iter().all(|p| zones[p.column].holds(p.op, &p.literal) != Holds::None)
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
        let at = self.bitmap_at + row / 8;
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
        let Some((rows, values)) = self.select(pool, columns, needed, predicates)? else { return Ok(Vec::new()) };
        let mut values: Vec<Option<std::vec::IntoIter<Value>>> =
            values.into_iter().map(|column| column.map(Vec::into_iter)).collect();
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

    /// Like [`RowGroup::read`], but leaves the rows' values in their columns;
    /// None if no row is left.
    fn read_columns(
        &self,
        pool: &BufferPool,
        columns: &[Column],
        needed: &[bool],
        predicates: &[Predicate],
    ) -> Result<Option<ColumnChunk>> {
        let selected = self.select(pool, columns, needed, predicates)?;
        Ok(selected.map(|(rows, columns)| ColumnChunk { rows: rows.len(), columns }))
    }

    /// The positions of the rows not deleted that satisfy every predicate,
    /// and the needed columns' values for them; None if no row is left.
    #[allow(clippy::type_complexity)]
    fn select(
        &self,
        pool: &BufferPool,
        columns: &[Column],
        needed: &[bool],
        predicates: &[Predicate],
    ) -> Result<Option<(Vec<usize>, Vec<Option<Vec<Value>>>)>> {
        let mut chunks: Vec<Option<Vec<u8>>> = vec![None; columns.len()];
        let mut selected: Vec<u8> = self.deleted.iter().map(|b| !b).collect();
        if !self.rows.is_multiple_of(8) {
            *selected.last_mut().unwrap() &= (1 << (self.rows % 8)) - 1;
        }
        // Find the matching rows on the compressed chunks, stopping once none
        // is left, but for predicates the zone map says every row satisfies.
        let zones = self.zones.as_ref().map(|(zones, _)| zones);
        let unsure = |p: &&Predicate| zones.is_none_or(|zones| zones[p.column].holds(p.op, &p.literal) != Holds::All);
        for p in predicates.iter().filter(unsure) {
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
            return Ok(None);
        }

        // Then decode the needed columns, materializing only those rows.
        let mut values = Vec::with_capacity(columns.len());
        for (i, column) in columns.iter().enumerate() {
            values.push(if needed[i] {
                let bytes = match chunks[i].take() {
                    Some(bytes) => bytes,
                    None => read_chunk(pool, self.chunks[i])?,
                };
                Some(encoding::decode_selected(column.data_type, &bytes, self.rows, &selected)?)
            } else {
                None
            });
        }
        Ok(Some((rows, values)))
    }

    fn destroy(self, pool: &BufferPool) -> Result<()> {
        for chunk in self.chunks.into_iter().chain(self.zones.map(|(_, first)| first)) {
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
    fn zones_bound_what_predicates_may_hold_for() {
        use Value::{Int, Null, Real, Text};
        let zone = Zone::of(&[Int(5), Null, Int(9), Int(7)]);
        assert_eq!(zone, Zone { range: Some((Int(5), Int(9))), nulls: 1 });
        let holds = |zone: &Zone, op, literal: Value| zone.holds(op, &literal);
        for (op, literal, expected) in [
            (Compare::Lt, 5, Holds::None),
            (Compare::Lt, 6, Holds::Some),
            (Compare::LtEq, 4, Holds::None),
            (Compare::Gt, 9, Holds::None),
            (Compare::GtEq, 9, Holds::Some),
            (Compare::Eq, 10, Holds::None),
            (Compare::Eq, 7, Holds::Some),
            (Compare::NotEq, 7, Holds::Some),
        ] {
            assert_eq!(holds(&zone, op, Int(literal)), expected, "{op:?} {literal}");
        }
        // Every value satisfies it, but a NULL would not.
        assert_eq!(holds(&zone, Compare::GtEq, Int(5)), Holds::Some);
        let full = Zone::of(&[Int(5), Int(9)]);
        assert_eq!(holds(&full, Compare::GtEq, Int(5)), Holds::All);
        assert_eq!(holds(&full, Compare::NotEq, Int(4)), Holds::All);
        assert_eq!(holds(&Zone::of(&[Int(3), Int(3)]), Compare::NotEq, Int(3)), Holds::None);
        assert_eq!(holds(&Zone::of(&[Int(3), Int(3)]), Compare::Eq, Int(3)), Holds::All);
        // A column of NULLs satisfies nothing.
        assert_eq!(Zone::of(&[Null, Null]), Zone { range: None, nulls: 2 });
        assert_eq!(holds(&Zone::of(&[Null]), Compare::NotEq, Int(0)), Holds::None);
        // -0.0 and 0.0 are one value, as a REAL compares.
        assert_eq!(holds(&Zone::of(&[Real(-0.0), Real(2.5)]), Compare::Lt, Real(0.0)), Holds::None);
        assert_eq!(holds(&Zone::of(&[Real(-1.5), Real(0.0)]), Compare::Gt, Real(-0.0)), Holds::None);
        let texts = Zone::of(&[Text("oslo".into()), Text("lima".into()), Text("paris".into())]);
        assert_eq!(texts.range, Some((Text("lima".into()), Text("paris".into()))));
        assert_eq!(holds(&texts, Compare::Gt, Text("p".into())), Holds::Some);
        assert_eq!(holds(&texts, Compare::Gt, Text("pz".into())), Holds::None);
        // A literal of another type cannot be judged: the chunk's filter reports it.
        assert_eq!(holds(&full, Compare::Eq, Text("x".into())), Holds::Some);
    }

    #[test]
    fn zone_maps_skip_row_groups_and_survive_reopening() {
        let pool = pool();
        let mut store = ColumnStore::create(pool.clone(), columns(), small()).unwrap();
        insert(&mut store, 0..550);
        let predicate = |column, op, literal| Predicate { column, op, literal };
        let all = scan(&store, &[true, true, true]);
        let check = |store: &ColumnStore, predicates: Vec<Predicate>, skipped: u64| {
            let before = store.zone_stats();
            let expected: Vec<_> = all.iter().filter(|(_, r)| predicates.iter().all(|p| p.matches(r))).cloned().collect();
            let found: Vec<_> = store.scan(vec![true; 3], predicates.clone()).collect::<Result<_>>().unwrap();
            assert_eq!(found, expected, "{predicates:?}");
            let chunks: Vec<ColumnChunk> = store.scan_by_column(vec![true; 3], predicates.clone(), 64).collect::<Result<_>>().unwrap();
            assert_eq!(chunks.iter().map(|c| c.rows).sum::<usize>(), expected.len(), "{predicates:?}");
            let after = store.zone_stats();
            let (read, skip) = (after.0 - before.0, after.1 - before.1);
            assert_eq!((read + skip, skip), (10, 2 * skipped), "{predicates:?}: twice over the 5 groups");
        };
        // Ids grow with each group: 0..100, 100..200, ...
        check(&store, vec![predicate(0, Compare::Lt, Value::Int(150))], 3);
        check(&store, vec![predicate(0, Compare::Eq, Value::Int(321))], 4);
        check(&store, vec![predicate(0, Compare::GtEq, Value::Int(250)), predicate(0, Compare::Lt, Value::Int(300))], 4);
        check(&store, vec![predicate(0, Compare::Gt, Value::Int(10_000))], 5);
        // Every group holds every city, and NULLs: none is skipped.
        check(&store, vec![predicate(1, Compare::Eq, Value::Text("oslo".into()))], 0);
        check(&store, vec![predicate(2, Compare::NotEq, Value::Bool(true))], 0);
        // A predicate every row satisfies leaves out none.
        check(&store, vec![predicate(0, Compare::GtEq, Value::Int(0))], 0);

        // Deleting rows leaves the zone maps true of the rows left.
        let doomed: Vec<Position> =
            all.iter().filter(|(_, r)| matches!(r[0], Value::Int(i) if i < 120 || i % 7 == 0)).map(|(p, _)| *p).collect();
        doomed.iter().for_each(|&p| store.delete(p).unwrap());
        pool.flush_all().unwrap();
        let reopened = ColumnStore::open(pool, store.manifest_page(), columns(), small()).unwrap();
        let zones = |store: &ColumnStore| store.groups.iter().map(|g| g.zones.clone()).collect::<Vec<_>>();
        assert_eq!(zones(&reopened), zones(&store));
        // The first group lost every row, and was freed; the next keeps its zones, if less tight now.
        assert_eq!(reopened.group_count(), 4);
        assert_eq!(reopened.groups[0].zones.as_ref().unwrap().0[0], Zone { range: Some((Value::Int(100), Value::Int(199))), nulls: 0 });
        let left: Vec<i64> = (0..550).filter(|i| *i >= 120 && i % 7 != 0).collect();
        assert_eq!(ids(&reopened), left);
        let low: Vec<_> = reopened.scan(vec![true; 3], vec![predicate(0, Compare::Lt, Value::Int(130))]).collect::<Result<_>>().unwrap();
        assert_eq!(low.len(), left.iter().filter(|&&i| i < 130).count());
    }

    #[test]
    fn row_groups_without_zone_maps_are_read() {
        let pool = pool();
        let mut store = ColumnStore::create(pool.clone(), columns(), small()).unwrap();
        insert(&mut store, 0..250);
        let first = store.scan(vec![true, false, false], Vec::new()).next().unwrap().unwrap().0;
        store.delete(first).unwrap();
        // Rewrite each header as groups were written before zone maps: no
        // flag, no zone map page, the delete bitmap right after the chunks.
        for group in &store.groups {
            let mut page = pool.fetch_write(group.header).unwrap();
            let bitmap = page[group.bitmap_at..group.bitmap_at + group.deleted.len()].to_vec();
            write_u32(&mut page, 4, group.chunks.len() as u32);
            let at = 8 + 4 * group.chunks.len();
            page[at..at + bitmap.len()].copy_from_slice(&bitmap);
        }
        pool.flush_all().unwrap();
        let mut reopened = ColumnStore::open(pool, store.manifest_page(), columns(), small()).unwrap();
        assert!(reopened.groups.iter().all(|g| g.zones.is_none()));
        assert_eq!(ids(&reopened), (1..250).collect::<Vec<_>>());
        let low = Predicate { column: 0, op: Compare::Lt, literal: Value::Int(5) };
        assert_eq!(reopened.scan(vec![true; 3], vec![low]).count(), 4);
        assert_eq!(reopened.zone_stats().1, 0, "nothing to skip by");
        // Deletes find the bitmap where it is.
        let second = reopened.scan(vec![true, false, false], Vec::new()).next().unwrap().unwrap().0;
        reopened.delete(second).unwrap();
        assert_eq!(ids(&reopened), (2..250).collect::<Vec<_>>());
    }

    #[test]
    fn too_many_columns_are_rejected() {
        let wide: Vec<Column> = (0..2000)
            .map(|i| Column { name: format!("c{i}"), data_type: DataType::Int, primary_key: false })
            .collect();
        assert!(ColumnStore::create(pool(), wide, ColumnConfig::default()).is_err());
    }
}
