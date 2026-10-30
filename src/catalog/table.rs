use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

use super::index::{Bound, Index, encode_keys, own_expr};
use super::stats::TableStats;
use crate::datetime;
use crate::error::{Error, Result, err};
use crate::mvcc::{Claim, Holding, Snapshot, Transactions};
use crate::predicate::Predicate;
use crate::sql::ast::{Constraints, CreateIndex, Expr};
use crate::sql::eval::{Field, eval, expr_at_time, locate};
use crate::storage::version::{All, Fate, HEADER, Header, Horizon, Visibility, Xid};
use crate::storage::{
    BufferPool, ColumnChunk, ColumnConfig, ColumnStore, LsmConfig, LsmTree, MAX_TUPLE_SIZE, PageId,
    Position, RecordId, SharedScan, TableHeap, sort_key, tuple,
};
use crate::types::{Column, StorageKind, Value, column_index};

pub type Row = Vec<Value>;

/// Bytes of a heap row's id in an index: page id and slot, big-endian.
const RECORD_ID_LEN: usize = 6;

/// Bytes of an LSM version's key after its primary key: its xmin, inverted.
const XMIN_LEN: usize = 8;

/// Updates and deletes that make a table vacuum itself after a commit,
/// besides a fifth of its rows (see [`Table::tidy`]).
const AUTOVACUUM_CHANGES: u64 = 1000;

/// Identifies a version of a row within its table.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowId {
    /// Where a heap version is stored.
    Record(RecordId),
    /// An LSM version's key: its primary key, then its xmin (see [`version_key`]).
    Key(Vec<u8>),
    /// Where a column store row is.
    Position(Position),
}

/// A table: a schema plus the storage holding its encoded rows, and the
/// indexes over them. Enforces column types, primary keys, unique indexes,
/// NOT NULL columns and CHECK conditions, and gives columns their defaults
/// (see [`Constraints`]; a UNIQUE constraint is kept by a unique index).
///
/// Every stored row is a version, with the header of `storage/version.rs`:
/// reads see the versions their snapshot sees (see `mvcc/snapshot.rs`), and
/// writes leave the old versions for the snapshots that still read them.
/// An update writes a new version, with index entries of its own; an LSM
/// tree keys each version by its primary key and its xmin, so that a key's
/// versions sort together, newest first. A version its own transaction
/// wrote in the same statement or before, under the same id, is changed or
/// removed in place, since no one else has seen it.
///
/// Every write validates all rows before touching storage, so a constraint
/// violation leaves the table unchanged.
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// What CREATE TABLE asked of the rows besides the columns' types.
    pub constraints: Constraints,
    /// The same, made ready to check rows by.
    rules: Rules,
    pool: Arc<BufferPool>,
    txns: Arc<Transactions>,
    storage: Storage,
    /// Kept up to date by every write. Only heap and LSM tables have any: a
    /// column store moves its rows when it migrates them, which would change
    /// their ids.
    indexes: Vec<Index>,
    /// How many rows the table has, or [`UNCOUNTED`]: see [`Table::row_count`].
    rows: AtomicU64,
    /// What the last ANALYZE learned, if the table was analyzed.
    stats: Option<TableStats>,
    /// Versions updates and deletes left behind since the last vacuum.
    changed: u64,
    /// Written before rows had versions: opened only for an upgrade to
    /// copy its rows (see [`Table::legacy_rows`]).
    legacy: bool,
}

/// A row count not counted yet.
const UNCOUNTED: u64 = u64::MAX;

/// A table's constraints, made ready to check rows by.
struct Rules {
    /// Whether each column, by position, was declared NOT NULL.
    not_null: Vec<bool>,
    /// Each CHECK's name and condition, its columns found among the table's.
    checks: Vec<(String, Expr)>,
    /// The table's columns, for the conditions to read.
    fields: Vec<Field>,
}

impl Rules {
    /// Checks that the constraints name only the table's columns, that
    /// CHECK conditions read nothing but the row (no subquery, aggregate or
    /// current time), and that defaults read no column.
    fn new(table: &str, columns: &[Column], constraints: &Constraints) -> Result<Rules> {
        let known = |name: &str, what: &str| {
            column_index(columns, name)
                .map_err(|_| Error(format!("unknown column in {what}: {name}")))
        };
        let mut not_null = vec![false; columns.len()];
        for name in &constraints.not_null {
            not_null[known(name, "NOT NULL")?] = true;
        }
        for (name, default) in &constraints.defaults {
            known(name, "DEFAULT")?;
            let mut problem = None;
            default.visit(&mut |e| match e {
                Expr::Column(_) | Expr::Outer(_) => {
                    _ = problem.get_or_insert("cannot read a column")
                }
                Expr::Aggregate(..) => _ = problem.get_or_insert("cannot use an aggregate"),
                Expr::Window(_) => _ = problem.get_or_insert("cannot use a window function"),
                Expr::Subquery(_) | Expr::Exists(_) | Expr::InSubquery(..) => {
                    _ = problem.get_or_insert("cannot use a subquery")
                }
                _ => {}
            });
            if let Some(problem) = problem {
                return err(format!("the default of column {name} {problem}"));
            }
        }
        for unique in &constraints.unique {
            for name in &unique.columns {
                known(name, "UNIQUE")?;
            }
        }
        let fields: Vec<Field> = columns.iter().map(|c| Field::named(&c.name)).collect();
        let checks = constraints
            .checks
            .iter()
            .map(|check| {
                let condition = own_expr(&check.condition, table, columns, "a CHECK")?;
                Ok((check.name.clone(), locate(&condition, &fields)))
            })
            .collect::<Result<_>>()?;
        Ok(Rules {
            not_null,
            checks,
            fields,
        })
    }

    /// Checks that each default is a value its column can hold, as it would
    /// be now.
    fn check_defaults(&self, columns: &[Column], constraints: &Constraints) -> Result<()> {
        for (i, value) in defaults(columns, constraints, datetime::now())?
            .into_iter()
            .enumerate()
        {
            let column = &columns[i];
            let fault = |problem: String| {
                Error(format!("the default of column {}: {problem}", column.name))
            };
            let value = match value.fits(column.data_type) {
                true => value,
                false => value.assign(column.data_type).map_err(|e| fault(e.0))?,
            };
            if !value.fits(column.data_type) {
                return Err(fault(format!(
                    "{} is not a {}",
                    value.to_sql(),
                    column.data_type
                )));
            }
        }
        Ok(())
    }

    /// Checks rows against the NOT NULL columns and the CHECK conditions: a
    /// condition may be true or NULL, but not false.
    fn check(&self, columns: &[Column], rows: &[Row]) -> Result<()> {
        for row in rows {
            for (value, (column, not_null)) in row.iter().zip(columns.iter().zip(&self.not_null)) {
                if *not_null && value.is_null() {
                    return err(format!("column {} cannot be NULL", column.name));
                }
            }
            for (name, condition) in &self.checks {
                match eval(condition, &self.fields, row)? {
                    Value::Bool(true) | Value::Null => {}
                    Value::Bool(false) => {
                        return err(format!("new row violates check constraint {name}"));
                    }
                    other => {
                        return err(format!(
                            "check constraint {name} must be a condition, not {}",
                            other.to_sql()
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Every column's default at time `now`, or NULL for a column without one.
pub(crate) fn defaults(columns: &[Column], constraints: &Constraints, now: i64) -> Result<Row> {
    columns
        .iter()
        .map(|column| match constraints.default_of(&column.name) {
            Some(default) => eval(&expr_at_time(default, now)?, &[], &[]),
            None => Ok(Value::Null),
        })
        .collect()
}

/// The rows of a heap table, read by threads together: see [`Table::shared_scan`].
pub struct SharedTableScan {
    pages: SharedScan,
    columns: Vec<Column>,
    needed: Vec<bool>,
    snapshot: Snapshot,
}

impl SharedTableScan {
    /// The rows the snapshot sees of the next page no thread has taken, or
    /// None once all are.
    pub fn next_rows(&self) -> Result<Option<Vec<Row>>> {
        let rows = self.pages.next_page(|bytes| {
            let (header, row) = Header::read(bytes)?;
            if !self.snapshot.sees_version(header) {
                return Ok(None);
            }
            tuple::decode_columns(&self.columns, row, &self.needed).map(Some)
        })?;
        Ok(rows.map(|rows| rows.into_iter().flatten().collect()))
    }
}

/// Lookups of a heap table's rows by one column, through an index on it,
/// that threads make together: see [`Table::shared_lookup`].
pub struct SharedLookup {
    index: Index,
    pool: Arc<BufferPool>,
    columns: Vec<Column>,
    column: usize,
    needed: Vec<bool>,
    covering: bool,
    snapshot: Snapshot,
}

impl SharedLookup {
    /// The rows whose column equals `value` that the snapshot sees, as an
    /// index join's lookup finds them: none for NULL, or for a value that
    /// equals no value of the column's type; from the index's entries if it
    /// covers the columns needed, else from the heap, whole.
    pub fn find(&self, value: &Value) -> Result<Vec<Row>> {
        if value.is_null() {
            return Ok(Vec::new());
        }
        let Some(value) = value.coerce(self.columns[self.column].data_type) else {
            return Ok(Vec::new());
        };
        let entries = self.index.scan(std::slice::from_ref(&value), None, None)?;
        let mut rows = Vec::with_capacity(entries.len());
        for entry in &entries {
            let rid = record_id(self.index.row_id(entry)?)?;
            let bytes = TableHeap::get_in(&self.pool, rid)?
                .ok_or_else(|| Error(format!("index points to a missing row: {rid:?}")))?;
            let (header, row) = Header::read(&bytes)?;
            if !self.snapshot.sees_version(header) {
                continue;
            }
            rows.push(match self.covering {
                true => self.index.row(entry, &self.needed)?,
                false => tuple::decode(&self.columns, row)?,
            });
        }
        Ok(rows)
    }
}

/// A heap row's id, as an index entry ends with it.
fn record_id(bytes: &[u8]) -> Result<RecordId> {
    match <[u8; RECORD_ID_LEN]>::try_from(bytes) {
        Ok([a, b, c, d, e, f]) => Ok(RecordId {
            page_id: PageId(u32::from_be_bytes([a, b, c, d])),
            slot: u16::from_be_bytes([e, f]),
        }),
        Err(_) => err("corrupted index entry: bad record id"),
    }
}

/// The key of an LSM version: its primary key as a sort key (see
/// `storage/sort_key.rs`), whose bytes sort as the values do and end where
/// they can be told to, then its xmin inverted, so that the versions of a
/// key come together, newest first.
fn version_key(key: &Value, xmin: Xid) -> Vec<u8> {
    let mut bytes = encode_keys(std::slice::from_ref(key));
    bytes.extend_from_slice(&(Xid::MAX - xmin).to_be_bytes());
    bytes
}

/// The primary key part of an LSM version's key.
fn primary_part(key: &[u8]) -> &[u8] {
    &key[..key.len().saturating_sub(XMIN_LEN)]
}

/// The key of another version of the same LSM row.
fn with_xmin(key: &[u8], xmin: Xid) -> Vec<u8> {
    let mut bytes = primary_part(key).to_vec();
    bytes.extend_from_slice(&(Xid::MAX - xmin).to_be_bytes());
    bytes
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

/// The versions a scan reads, with their headers.
type Versions<'a> = Box<dyn Iterator<Item = Result<(RowId, Header, Row)>> + 'a>;

/// What a scan does with one stored version.
enum Step {
    Row(RowId, Header, Row),
    /// Not seen, or not wanted: on to the next.
    Skip,
    /// Past the range scanned: the scan ends.
    Stop,
}

fn steps<'a>(steps: impl Iterator<Item = Result<Step>> + 'a) -> Versions<'a> {
    Box::new(
        steps
            .take_while(|step| !matches!(step, Ok(Step::Stop)))
            .filter_map(|step| match step {
                Ok(Step::Row(id, header, row)) => Some(Ok((id, header, row))),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            }),
    )
}

impl Table {
    /// Creates an empty table. Fails if a constraint names a column the
    /// table does not have, a CHECK reads anything but the row, or a default
    /// reads a column or is not of its column's type.
    pub fn create(
        pool: Arc<BufferPool>,
        txns: Arc<Transactions>,
        name: String,
        columns: Vec<Column>,
        constraints: Constraints,
        kind: StorageKind,
    ) -> Result<Self> {
        let rules = Rules::new(&name, &columns, &constraints)?;
        rules.check_defaults(&columns, &constraints)?;
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
            constraints,
            rules,
            pool,
            txns,
            storage,
            indexes: Vec::new(),
            rows: AtomicU64::new(0),
            stats: None,
            changed: 0,
            legacy: false,
        })
    }

    /// Opens a table from the pages [`Table::root_pages`] returned. Its
    /// indexes are added afterwards with [`Table::open_index`]. A `legacy`
    /// table was written before rows had versions, and can only be read
    /// with [`Table::legacy_rows`].
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        pool: Arc<BufferPool>,
        txns: Arc<Transactions>,
        name: String,
        columns: Vec<Column>,
        constraints: Constraints,
        kind: StorageKind,
        (first, last): (PageId, PageId),
        legacy: bool,
    ) -> Result<Self> {
        let rules = Rules::new(&name, &columns, &constraints)?;
        let storage = match kind {
            StorageKind::Heap => Storage::Heap(TableHeap::open(pool.clone(), first, last)?),
            StorageKind::Lsm => {
                let tree = LsmTree::open(pool.clone(), first, LsmConfig::default())?;
                let next_rowid = match (primary_key(&columns), tree.max_key(), legacy) {
                    (None, Some(max), true) => tuple::decode_int_key(&max)? + 1,
                    (None, Some(max), false) => {
                        match sort_key::decode(primary_part(&max), &mut 0)? {
                            Value::Int(n) => n + 1,
                            _ => return err("corrupted LSM table: a rowid is not an INT"),
                        }
                    }
                    _ => 1,
                };
                Storage::Lsm { tree, next_rowid }
            }
            StorageKind::Column => Storage::Column(ColumnStore::open(
                pool.clone(),
                first,
                columns.clone(),
                ColumnConfig::default(),
                legacy,
            )?),
        };
        Ok(Table {
            name,
            columns,
            constraints,
            rules,
            pool,
            txns,
            storage,
            indexes: Vec::new(),
            rows: AtomicU64::new(UNCOUNTED),
            stats: None,
            changed: 0,
            legacy,
        })
    }

    pub fn storage_kind(&self) -> StorageKind {
        match self.storage {
            Storage::Heap(_) => StorageKind::Heap,
            Storage::Lsm { .. } => StorageKind::Lsm,
            Storage::Column(_) => StorageKind::Column,
        }
    }

    /// A scan of every row `snapshot` sees that threads share, a page at a
    /// time, if the table is a heap; the columns not marked in `needed` are
    /// left NULL. It holds no borrow of the table, which must not change
    /// while it runs.
    pub fn shared_scan(&self, snapshot: &Snapshot, needed: &[bool]) -> Option<SharedTableScan> {
        assert_eq!(needed.len(), self.columns.len());
        match &self.storage {
            Storage::Heap(heap) => Some(SharedTableScan {
                pages: heap.shared_scan(),
                columns: self.columns.clone(),
                needed: needed.to_vec(),
                snapshot: snapshot.clone(),
            }),
            _ => None,
        }
    }

    /// Lookups by `column` through the index at `index` (whose first key it
    /// is) that threads make together, if the table is a heap; `needed`
    /// marks the columns read above, as for [`Table::shared_scan`]. Like
    /// that scan, it holds no borrow of the table, which must not change
    /// while it runs.
    pub fn shared_lookup(
        &self,
        snapshot: &Snapshot,
        index: usize,
        column: usize,
        needed: &[bool],
    ) -> Option<SharedLookup> {
        let Storage::Heap(_) = &self.storage else {
            return None;
        };
        let index = self.indexes[index].clone();
        Some(SharedLookup {
            covering: index.covers(needed),
            index,
            pool: self.pool.clone(),
            columns: self.columns.clone(),
            column,
            needed: needed.to_vec(),
            snapshot: snapshot.clone(),
        })
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

    /// What the last ANALYZE learned about the table's values (see
    /// `stats.rs`), if it had any rows then: an empty table tells nothing
    /// of the values it will hold.
    pub fn stats(&self) -> Option<&TableStats> {
        self.stats.as_ref().filter(|stats| stats.rows > 0)
    }

    pub fn set_stats(&mut self, stats: Option<TableStats>) {
        self.stats = stats;
    }

    /// Builds an index over the table's rows: an entry for every version
    /// stored, for whichever snapshot reads it. Fails, leaving no index
    /// behind, if the table is a column table, or the definition names what
    /// the table does not have, or the index is unique and two rows that are
    /// there for `snapshot` (see [`Snapshot::holds`]) share their keys, or
    /// an entry is too long.
    pub fn create_index(&mut self, definition: &CreateIndex, snapshot: &Snapshot) -> Result<()> {
        if let Storage::Column(_) = self.storage {
            return err("COLUMN tables cannot have indexes");
        }
        let mut index = Index::create(self.pool.clone(), definition, &self.columns)?;
        let mut build = || {
            let mut taken = HashSet::new();
            let all = vec![true; self.columns.len()];
            for item in self.versions(All, &all, &[], &Access::Scan) {
                let (id, header, row) = item?;
                let id = id.to_bytes();
                let Some(entry) = index.entry(&row, &id)? else {
                    continue;
                };
                index.check(&row, id.len())?;
                if index.unique
                    && let Some(keys) = unique_keys(&index, &row)?
                    && snapshot.holds(header)?
                    && !taken.insert(encode_keys(&keys))
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

    /// Gives the table another name, other names of columns, or other
    /// constraints, keeping its rows as they are stored: for an ALTER TABLE
    /// that changes no column's place or type. Its indexes become those of
    /// `kept`, each opened at its root page under its new definition; the
    /// others are freed. Fails, changing nothing, if the constraints do not
    /// hold for the new columns.
    pub fn redefine(
        &mut self,
        name: String,
        columns: Vec<Column>,
        constraints: Constraints,
        kept: &[(CreateIndex, PageId)],
    ) -> Result<()> {
        assert!(
            columns
                .iter()
                .zip(&self.columns)
                .all(|(new, old)| new.data_type == old.data_type)
                && columns.len() == self.columns.len(),
            "the rows are stored as they were"
        );
        let rules = Rules::new(&name, &columns, &constraints)?;
        rules.check_defaults(&columns, &constraints)?;
        let indexes = kept
            .iter()
            .map(|(definition, root)| Index::open(self.pool.clone(), definition, &columns, *root))
            .collect::<Result<Vec<_>>>()?;
        for index in std::mem::take(&mut self.indexes) {
            if !kept.iter().any(|(_, root)| *root == index.root_page()) {
                index.destroy()?;
            }
        }
        self.indexes = indexes;
        self.name = name;
        self.columns = columns;
        self.constraints = constraints;
        self.rules = rules;
        Ok(())
    }

    /// Checks the rows `snapshot` sees against the table's NOT NULL columns
    /// and CHECK conditions: for a constraint an ALTER TABLE adds.
    pub fn check_rows(&self, snapshot: &Snapshot) -> Result<()> {
        for item in self.scan(snapshot) {
            let (_, row) = item?;
            self.rules
                .check(&self.columns, std::slice::from_ref(&row))
                .map_err(|e| Error(format!("a row of {} breaks it: {}", self.name, e.0)))?;
        }
        Ok(())
    }

    /// Removes an index and frees its pages.
    pub fn drop_index(&mut self, name: &str) -> Result<()> {
        let Some(i) = self.indexes.iter().position(|index| index.name == name) else {
            return err(format!("unknown index: {name}"));
        };
        self.indexes.remove(i).destroy()
    }

    /// Iterates over the rows `snapshot` sees, with their ids: a heap in
    /// storage order, an LSM tree in key order, a column store row group by
    /// row group.
    pub fn scan(&self, snapshot: &Snapshot) -> Box<dyn Iterator<Item = Result<(RowId, Row)>> + '_> {
        self.scan_filtered(
            snapshot,
            &vec![true; self.columns.len()],
            &[],
            &Access::Scan,
        )
    }

    /// Like [`Table::scan`], but only the columns marked in `needed` must be
    /// read, and the rows are read the way `access` says, which for heaps and
    /// LSM trees may be through an index (or an LSM tree's primary key), in
    /// index order. Unneeded columns may hold anything, NULL usually: a
    /// column store skips their chunks, and heaps and LSM trees skip their
    /// bytes. Rows that fail one of `predicates` may be left out: a column
    /// store evaluates them on its compressed chunks and skips those rows,
    /// ignoring `access`. An access path must come from `predicates`, so
    /// that it leaves out only rows they rule out.
    pub fn scan_filtered(
        &self,
        snapshot: &Snapshot,
        needed: &[bool],
        predicates: &[Predicate],
        access: &Access,
    ) -> Box<dyn Iterator<Item = Result<(RowId, Row)>> + '_> {
        Box::new(
            self.versions(snapshot.clone(), needed, predicates, access)
                .map(|item| item.map(|(id, _, row)| (id, row))),
        )
    }

    /// The versions `vis` sees, read as [`Table::scan_filtered`] reads rows,
    /// with their headers.
    fn versions<'a, V: Visibility + Clone + 'a>(
        &'a self,
        vis: V,
        needed: &[bool],
        predicates: &[Predicate],
        access: &Access,
    ) -> Versions<'a> {
        assert_eq!(needed.len(), self.columns.len());
        let columns = &self.columns;
        let decode = {
            let needed = needed.to_vec();
            move |bytes: &[u8]| tuple::decode_columns(columns, bytes, &needed)
        };
        // A stored version, if `vis` sees it.
        let seen = {
            let vis = vis.clone();
            move |bytes: &[u8]| -> Result<Option<(Header, Row)>> {
                let (header, row) = Header::read(bytes)?;
                match vis.visible(header)? {
                    true => Ok(Some((header, decode(row)?))),
                    false => Ok(None),
                }
            }
        };
        let step = |id: RowId, seen: Option<(Header, Row)>| match seen {
            Some((header, row)) => Step::Row(id, header, row),
            None => Step::Skip,
        };
        match (access.clone(), &self.storage) {
            (Access::Scan, Storage::Heap(heap)) => steps(
                heap.scan_map(seen)
                    .map(move |item| item.map(|(rid, seen)| step(RowId::Record(rid), seen))),
            ),
            (Access::Scan, Storage::Lsm { tree, .. }) => {
                steps(tree.scan_map(&[], move |key, bytes| {
                    Ok(step(RowId::Key(key.to_vec()), seen(bytes)?))
                }))
            }
            (_, Storage::Column(store)) => Box::new(
                store
                    .scan(needed.to_vec(), predicates.to_vec(), vis)
                    .map(|item| {
                        item.map(|(position, header, row)| (RowId::Position(position), header, row))
                    }),
            ),
            (Access::Key(value), Storage::Lsm { tree, .. }) => {
                let prefix = encode_keys(std::slice::from_ref(&value));
                steps(tree.scan_map(&prefix.clone(), move |key, bytes| {
                    if primary_part(key) != prefix.as_slice() {
                        return Ok(Step::Stop);
                    }
                    Ok(step(RowId::Key(key.to_vec()), seen(bytes)?))
                }))
            }
            (Access::KeyRange { lower, upper }, Storage::Lsm { tree, .. }) => {
                // Keys are encoded so that their bytes sort as the values do.
                let encode = |b: Bound| (encode_keys(std::slice::from_ref(&b.value)), b.inclusive);
                let (lower, upper) = (lower.map(encode), upper.map(encode));
                let start = lower.as_ref().map_or(Vec::new(), |(key, _)| key.clone());
                steps(tree.scan_map(&start, move |key, bytes| {
                    let primary = primary_part(key);
                    if let Some((bound, false)) = &lower
                        && primary == bound.as_slice()
                    {
                        return Ok(Step::Skip);
                    }
                    if let Some((bound, inclusive)) = &upper
                        && (primary > bound.as_slice()
                            || (!inclusive && primary == bound.as_slice()))
                    {
                        return Ok(Step::Stop);
                    }
                    Ok(step(RowId::Key(key.to_vec()), seen(bytes)?))
                }))
            }
            (Access::Key(_) | Access::KeyRange { .. }, Storage::Heap(_)) => {
                unreachable!("only an LSM tree has primary key lookups")
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
                steps(entries.into_iter().map(move |entry| {
                    let id = self.row_id(index.row_id(&entry)?.to_vec())?;
                    let Some((header, bytes)) = self.version(&id)? else {
                        return err(format!("index points to a missing row: {id:?}"));
                    };
                    if !vis.visible(header)? {
                        return Ok(Step::Skip);
                    }
                    let row = match covering {
                        true => index.row(&entry, &needed)?,
                        false => tuple::decode_columns(&self.columns, &bytes[HEADER..], &needed)?,
                    };
                    Ok(Step::Row(id, header, row))
                }))
            }
        }
    }

    /// The rows [`Table::scan_filtered`] reads, left in their columns, if the
    /// table keeps them so: a column table does (see
    /// [`ColumnStore::scan_by_column`]), the others do not.
    pub fn scan_by_column(
        &self,
        snapshot: &Snapshot,
        needed: &[bool],
        predicates: &[Predicate],
        chunk: usize,
    ) -> Option<Box<dyn Iterator<Item = Result<ColumnChunk>> + '_>> {
        match &self.storage {
            Storage::Column(store) => Some(Box::new(store.scan_by_column(
                needed.to_vec(),
                predicates.to_vec(),
                chunk,
                snapshot.clone(),
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
    /// first call after the table is opened counts the newest version of each
    /// row, its transaction committed or not (see [`Transactions::newest`]),
    /// and writes keep the count from then on, as they are made,
    /// whether their transactions commit or not: an estimate. Opening the
    /// catalog again, as a rollback does, forgets it.
    pub fn row_count(&self) -> Result<u64> {
        let rows = self.rows.load(Relaxed);
        if rows != UNCOUNTED {
            return Ok(rows);
        }
        let mut rows = 0;
        let none = vec![false; self.columns.len()];
        for item in self.versions(self.txns.newest(), &none, &[], &Access::Scan) {
            item?;
            rows += 1;
        }
        self.rows.store(rows, Relaxed);
        Ok(rows)
    }

    /// Makes the next [`Table::row_count`] count the rows again.
    pub fn forget_row_count(&self) {
        self.rows.store(UNCOUNTED, Relaxed);
    }

    /// Adds `change` to the row count, if it was counted.
    fn count(&self, change: i64) {
        let _ = self.rows.fetch_update(Relaxed, Relaxed, |rows| {
            (rows != UNCOUNTED).then(|| rows.saturating_add_signed(change))
        });
    }

    /// Inserts rows, as versions written by `snapshot`'s id.
    pub fn insert(&mut self, snapshot: &Snapshot, rows: Vec<Row>) -> Result<()> {
        let xid = snapshot.xid()?;
        let rows = self.coerce_all(rows)?;
        self.rules.check(&self.columns, &rows)?;
        let stored = self.encode_all(&rows, xid)?;
        self.check_unique(snapshot, &HashSet::new(), &rows, &[])?;
        let pk = primary_key(&self.columns);
        let keys: Vec<Vec<u8>> = match &self.storage {
            Storage::Lsm { next_rowid, .. } => (0..)
                .zip(&rows)
                .map(|(i, row)| match pk {
                    Some(pk) => version_key(&row[pk], xid),
                    None => version_key(&Value::Int(*next_rowid + i), xid),
                })
                .collect(),
            _ => Vec::new(),
        };
        self.check_sizes(&keys, &stored)?;
        self.check_index_entries(&rows, |i| keys.get(i).map_or(RECORD_ID_LEN, Vec::len))?;
        self.clear_keys(snapshot, keys.iter())?;

        let ids: Vec<RowId> = match &mut self.storage {
            Storage::Heap(heap) => stored
                .iter()
                .map(|bytes| Ok(RowId::Record(heap.insert(bytes)?)))
                .collect::<Result<_>>()?,
            Storage::Lsm { tree, next_rowid } => {
                for (key, bytes) in keys.iter().cloned().zip(stored) {
                    tree.put(key, bytes)?;
                }
                *next_rowid += rows.len() as i64;
                keys.into_iter().map(RowId::Key).collect()
            }
            Storage::Column(store) => {
                for bytes in stored {
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

    /// Replaces each version with the given id, whose stored value is the
    /// first row given, by the second, written by `snapshot`'s id.
    ///
    /// The old version gets its xmax, and the new one is stored apart: in a
    /// heap wherever there is room, in an LSM tree under the key of its
    /// primary key and xmin, in a column store in the delta store. Only a
    /// version `snapshot` may change in place ([`Claim::Own`]: one this same
    /// id wrote, or any when no one else can see it) is replaced (a heap row
    /// stays where it is if its page has room), and only then do its index
    /// entries change; a new version gets entries of its own.
    pub fn update(&mut self, snapshot: &Snapshot, changes: Vec<(RowId, Row, Row)>) -> Result<()> {
        let xid = snapshot.xid()?;
        let (old_rows, rows): (Vec<(RowId, Row)>, Vec<Row>) = changes
            .into_iter()
            .map(|(id, old, new)| ((id, old), new))
            .unzip();
        let rows = self.coerce_all(rows)?;
        self.rules.check(&self.columns, &rows)?;
        let ids: Vec<RowId> = old_rows.iter().map(|(id, _)| id.clone()).collect();
        let stored = self.encode_all(&rows, xid)?;
        self.check_unique(snapshot, &ids.iter().cloned().collect(), &rows, &old_rows)?;
        let claims: Vec<Claim> = ids
            .iter()
            .map(|id| snapshot.claim(self.header(id)?))
            .collect::<Result<_>>()?;
        let pk = primary_key(&self.columns);
        let new_keys: Vec<Vec<u8>> = match &self.storage {
            Storage::Lsm { .. } => ids
                .iter()
                .zip(&rows)
                .map(|(id, row)| match pk {
                    Some(pk) => Ok(version_key(&row[pk], xid)),
                    None => Ok(with_xmin(id.key()?, xid)),
                })
                .collect::<Result<_>>()?,
            _ => Vec::new(),
        };
        self.check_sizes(&new_keys, &stored)?;
        self.check_index_entries(&rows, |i| new_keys.get(i).map_or(RECORD_ID_LEN, Vec::len))?;
        // The keys own versions leave hold the versions this replaces.
        let leaving: HashSet<&RowId> = ids
            .iter()
            .zip(&claims)
            .filter_map(|(id, claim)| (*claim == Claim::Own).then_some(id))
            .collect();
        let taken: Vec<Vec<u8>> = new_keys
            .iter()
            .filter(|key| !leaving.contains(&RowId::Key(key.to_vec())))
            .cloned()
            .collect();
        self.clear_keys(snapshot, taken.iter())?;

        let new_ids: Vec<RowId> = match &mut self.storage {
            Storage::Heap(heap) => {
                let rids: Vec<RecordId> = ids
                    .into_iter()
                    .map(RowId::into_record)
                    .collect::<Result<_>>()?;
                let mut new_rids = rids.clone();
                // The rows that need a slot of their own: new versions, and
                // own versions their pages have no room for any more.
                let mut placed = Vec::new();
                for (i, claim) in claims.iter().enumerate() {
                    match claim {
                        Claim::Own if heap.update(rids[i], &stored[i])? => {}
                        Claim::Own => placed.push(i),
                        Claim::Mark => {
                            mark_record(heap, rids[i], xid)?;
                            placed.push(i);
                        }
                    }
                }
                // Delete every moved row first, so a reused slot can't be mistaken for a pending one.
                for &i in &placed {
                    if claims[i] == Claim::Own {
                        heap.delete(rids[i])?;
                    }
                }
                for i in placed {
                    new_rids[i] = heap.insert(&stored[i])?;
                }
                new_rids.into_iter().map(RowId::Record).collect()
            }
            Storage::Lsm { tree, .. } => {
                let old_keys: Vec<Vec<u8>> = ids
                    .into_iter()
                    .map(RowId::into_key)
                    .collect::<Result<_>>()?;
                // Own versions leaving their keys first, so a row may take
                // over a key another row is leaving.
                for (i, claim) in claims.iter().enumerate() {
                    if *claim == Claim::Own && old_keys[i] != new_keys[i] {
                        tree.delete(old_keys[i].clone())?;
                    }
                }
                for (i, claim) in claims.iter().enumerate() {
                    if *claim == Claim::Mark {
                        mark_key(tree, &old_keys[i], xid)?;
                    }
                }
                for (key, bytes) in new_keys.iter().cloned().zip(stored) {
                    tree.put(key, bytes)?;
                }
                new_keys.into_iter().map(RowId::Key).collect()
            }
            Storage::Column(store) => {
                let positions: Vec<Position> = ids
                    .into_iter()
                    .map(RowId::into_position)
                    .collect::<Result<_>>()?;
                for (position, claim) in positions.into_iter().zip(&claims) {
                    match claim {
                        Claim::Own => store.delete(position)?,
                        Claim::Mark => store.set_xmax(position, xid)?,
                    }
                }
                for bytes in stored {
                    store.insert(&bytes)?;
                }
                Vec::new() // no indexes to update
            }
        };
        self.changed += claims.iter().filter(|&&c| c == Claim::Mark).count() as u64;
        if self.indexes.is_empty() {
            return Ok(());
        }
        // An own version's entry that is the same for the old and new row
        // stays: its keys, included columns and row id are unchanged, or the
        // row was and is left out. Old entries go first, so a new entry may
        // reuse a key an old one is leaving. A new version only adds its own.
        let new_rows: Vec<(RowId, Row)> = new_ids.into_iter().zip(rows).collect();
        for index in &mut self.indexes {
            let mut changed = Vec::new();
            for (((old_id, old), (new_id, new)), claim) in
                old_rows.iter().zip(&new_rows).zip(&claims)
            {
                let new = index.entry(new, &new_id.to_bytes())?;
                let old = match claim {
                    Claim::Own => index.entry(old, &old_id.to_bytes())?,
                    Claim::Mark => None,
                };
                if old != new {
                    changed.push((old, new));
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

    /// Deletes the versions with these ids, given with their stored values
    /// of every column their index entries are made from (see
    /// [`Table::indexed_columns`]; the other columns' values are not read):
    /// sets their xmax, or removes those `snapshot`'s id wrote itself, with
    /// their index entries.
    pub fn delete(&mut self, snapshot: &Snapshot, rows: &[(RowId, Row)]) -> Result<()> {
        let xid = snapshot.xid()?;
        let claims: Vec<Claim> = rows
            .iter()
            .map(|(id, _)| snapshot.claim(self.header(id)?))
            .collect::<Result<_>>()?;
        let mut removed = Vec::new();
        for ((id, row), claim) in rows.iter().zip(claims) {
            match (claim, &mut self.storage) {
                (Claim::Own, Storage::Heap(heap)) => {
                    heap.delete(id.clone().into_record()?)?;
                    removed.push((id.clone(), row.clone()));
                }
                (Claim::Own, Storage::Lsm { tree, .. }) => {
                    tree.delete(id.clone().into_key()?)?;
                    removed.push((id.clone(), row.clone()));
                }
                (Claim::Own, Storage::Column(store)) => {
                    store.delete(id.clone().into_position()?)?
                }
                (Claim::Mark, Storage::Heap(heap)) => {
                    mark_record(heap, id.clone().into_record()?, xid)?
                }
                (Claim::Mark, Storage::Lsm { tree, .. }) => mark_key(tree, id.key()?, xid)?,
                (Claim::Mark, Storage::Column(store)) => {
                    store.set_xmax(id.clone().into_position()?, xid)?
                }
            }
        }
        self.changed += (rows.len() - removed.len()) as u64;
        self.remove_from_indexes(&removed)?;
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

    /// Removes the versions no transaction can see any more (see
    /// [`Horizon`]), with their index entries, and moves a column table's
    /// settled rows into row groups. Returns how many versions it removed.
    pub fn vacuum(&mut self, horizon: &dyn Horizon) -> Result<usize> {
        let dead_ones = |bytes: &[u8]| -> Result<Option<Row>> {
            let (header, row) = Header::read(bytes)?;
            match horizon.fate(header) {
                Fate::Dead => tuple::decode(&self.columns, row).map(Some),
                _ => Ok(None),
            }
        };
        let mut dead = Vec::new();
        match &self.storage {
            Storage::Heap(heap) => {
                for item in heap.scan() {
                    let (rid, bytes) = item?;
                    if let Some(row) = dead_ones(&bytes)? {
                        dead.push((RowId::Record(rid), row));
                    }
                }
            }
            Storage::Lsm { tree, .. } => {
                for item in tree.scan() {
                    let (key, bytes) = item?;
                    if let Some(row) = dead_ones(&bytes)? {
                        dead.push((RowId::Key(key), row));
                    }
                }
            }
            Storage::Column(_) => {}
        }
        let removed = match &mut self.storage {
            Storage::Heap(heap) => {
                for (id, _) in &dead {
                    heap.delete(id.clone().into_record()?)?;
                }
                dead.len()
            }
            Storage::Lsm { tree, .. } => {
                for (id, _) in &dead {
                    tree.delete(id.clone().into_key()?)?;
                }
                dead.len()
            }
            Storage::Column(store) => store.vacuum(horizon)?,
        };
        self.remove_from_indexes(&dead)?;
        self.changed = 0;
        Ok(removed)
    }

    /// What a table does after a commit: a column table moves its delta
    /// store's settled rows into row groups once it has enough of them, and
    /// a table vacuums itself once updates and deletes have left more than
    /// [`AUTOVACUUM_CHANGES`] versions behind, and a fifth of its rows.
    /// Returns whether it changed anything.
    pub fn tidy(&mut self) -> Result<bool> {
        let rows = self.rows.load(Relaxed);
        let rows = if rows == UNCOUNTED { 0 } else { rows };
        if self.changed > AUTOVACUUM_CHANGES + rows / 5 {
            self.vacuum(&self.txns.horizon())?;
            return Ok(true);
        }
        if let Storage::Column(store) = &mut self.storage
            && store.wants_migration()
        {
            let groups = store.group_count();
            store.migrate(&self.txns.horizon())?;
            return Ok(store.group_count() != groups);
        }
        Ok(false)
    }

    /// The rows of a table written before rows had versions, every one, for
    /// an upgrade to copy into a new table.
    pub fn legacy_rows(&self) -> Result<Vec<Row>> {
        assert!(self.legacy, "a table of today has versions");
        match &self.storage {
            Storage::Heap(heap) => heap
                .scan()
                .map(|item| tuple::decode(&self.columns, &item?.1))
                .collect(),
            Storage::Lsm { tree, .. } => tree
                .scan()
                .map(|item| tuple::decode(&self.columns, &item?.1))
                .collect(),
            Storage::Column(store) => store
                .scan(vec![true; self.columns.len()], Vec::new(), All)
                .map(|item| item.map(|(_, _, row)| row))
                .collect(),
        }
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

    /// Reads one version of a heap or LSM table by id, its header and its
    /// stored bytes (the header first); `None` if there is none.
    fn version(&self, id: &RowId) -> Result<Option<(Header, Vec<u8>)>> {
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
            .map(|bytes| Ok((Header::read(&bytes)?.0, bytes)))
            .transpose()
    }

    /// The header of the version with this id.
    fn header(&self, id: &RowId) -> Result<Header> {
        if let (Storage::Column(store), RowId::Position(position)) = (&self.storage, id) {
            return store.header(*position);
        }
        match self.version(id)? {
            Some((header, _)) => Ok(header),
            None => err(format!("row {id:?} of table {} is gone", self.name)),
        }
    }

    /// Removes the index entries of the LSM versions stored now under keys
    /// that new versions are about to take: versions no one sees any more.
    /// Only a statement that changes versions in place can meet one, as it
    /// writes every version with the one xmin [`FROZEN`](crate::storage::version::FROZEN)
    /// (see [`Snapshot::alone`]), and a version of the same row that a
    /// transaction deleted before may lie under it still.
    fn clear_keys<'k>(
        &mut self,
        snapshot: &Snapshot,
        keys: impl Iterator<Item = &'k Vec<u8>>,
    ) -> Result<()> {
        let Storage::Lsm { tree, .. } = &self.storage else {
            return Ok(());
        };
        let mut dead = Vec::new();
        for key in keys {
            let Some(bytes) = tree.get(key)? else {
                continue;
            };
            let (header, row) = Header::read(&bytes)?;
            if snapshot.sees_version(header) {
                return err("a row would have two versions under one key");
            }
            dead.push((RowId::Key(key.clone()), tuple::decode(&self.columns, row)?));
        }
        self.remove_from_indexes(&dead)
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

    /// Checks that stored rows fit: in an LSM tree with their keys, elsewhere in a page.
    fn check_sizes(&self, keys: &[Vec<u8>], stored: &[Vec<u8>]) -> Result<()> {
        match &self.storage {
            Storage::Lsm { .. } => keys
                .iter()
                .zip(stored)
                .try_for_each(|(key, bytes)| LsmTree::check_size(key, bytes)),
            _ => stored.iter().try_for_each(|bytes| check_heap_size(bytes)),
        }
    }

    /// Turns the bytes an index stores back into a row id.
    fn row_id(&self, bytes: Vec<u8>) -> Result<RowId> {
        match &self.storage {
            Storage::Heap(_) => record_id(&bytes).map(RowId::Record),
            Storage::Lsm { .. } => Ok(RowId::Key(bytes)),
            Storage::Column(_) => err("COLUMN tables cannot have indexes"),
        }
    }

    /// A row of every column's default, as a statement that began at `now`
    /// gives it; NULL for a column without one.
    pub fn defaults(&self, now: i64) -> Result<Row> {
        defaults(&self.columns, &self.constraints, now)
    }

    /// Rows with each value made one of its column's type as it is stored
    /// (see [`Value::assign`]): an INT stored in a REAL column, 2.0 in an INT
    /// one, a number rounded to a DECIMAL column's places (an error if it has
    /// too many digits). Others are left for encoding to reject.
    pub fn coerce_all(&self, mut rows: Vec<Row>) -> Result<Vec<Row>> {
        for row in &mut rows {
            for (value, column) in row.iter_mut().zip(&self.columns) {
                if !value.fits(column.data_type) {
                    *value = value
                        .assign(column.data_type)
                        .map_err(|e| Error(format!("column {}: {}", column.name, e.0)))?;
                }
            }
        }
        Ok(rows)
    }

    /// Encodes rows as versions written by `xid`, which also checks their types.
    fn encode_all(&self, rows: &[Row], xid: Xid) -> Result<Vec<Vec<u8>>> {
        rows.iter()
            .map(|row| Ok(Header::new(xid).with(&tuple::encode(&self.columns, row)?)))
            .collect()
    }

    /// Checks that `new_rows`, together with the rows there now except those
    /// in `replaced`, have non-null primary keys, unique primary keys, and
    /// unique keys in every unique index. A stored version counts if it is
    /// there for `snapshot` ([`Snapshot::holds`]): committed and not
    /// deleted, whether `snapshot` sees it or not, or its own. `old_rows`
    /// are the rows the new ones replace, in the same order, if known: keys
    /// a row keeps are unique among the stored rows already, and are checked
    /// only against the new ones.
    fn check_unique(
        &self,
        snapshot: &Snapshot,
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
            self.check_unique_index(snapshot, index, is_pk, replaced, new_rows, old_rows)?;
        }
        if let Some(pk) = pk
            && !self
                .indexes
                .iter()
                .any(|index| index.unique && index.is_on_column(pk))
        {
            self.check_unique_key(snapshot, pk, replaced, new_rows, old_rows)?;
        }
        Ok(())
    }

    /// Checks the keys of a unique index: among the new rows it indexes, and
    /// against the stored versions it lists.
    fn check_unique_index(
        &self,
        snapshot: &Snapshot,
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
                    let id = self.row_id(bytes)?;
                    if !replaced.contains(&id) {
                        stored |= snapshot.holds(self.header(&id)?)?;
                    }
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
    /// searched for the versions of each new key, a column store reads only
    /// its key column, and a heap is scanned in full (only in files from
    /// before heap tables had a primary key index).
    fn check_unique_key(
        &self,
        snapshot: &Snapshot,
        column: usize,
        replaced: &HashSet<RowId>,
        new_rows: &[Row],
        old_rows: &[(RowId, Row)],
    ) -> Result<()> {
        let mut seen = HashSet::new();
        if !matches!(self.storage, Storage::Lsm { .. }) {
            let needed: Vec<bool> = (0..self.columns.len()).map(|i| i == column).collect();
            for item in self.versions(Holding(snapshot), &needed, &[], &Access::Scan) {
                let (id, _, row) = item?;
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
                    let prefix = encode_keys(std::slice::from_ref(value));
                    let mut stored = false;
                    for item in tree.scan_from(&prefix) {
                        let (key, bytes) = item?;
                        if primary_part(&key) != prefix.as_slice() {
                            break;
                        }
                        if !replaced.contains(&RowId::Key(key)) {
                            stored |= snapshot.holds(Header::read(&bytes)?.0)?;
                        }
                    }
                    stored
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

/// Sets the xmax of a heap version, in place.
fn mark_record(heap: &mut TableHeap, rid: RecordId, xmax: Xid) -> Result<()> {
    let Some(mut bytes) = heap.get(rid)? else {
        return err(format!("row {rid:?} is gone"));
    };
    Header::set_xmax(&mut bytes, xmax);
    match heap.update(rid, &bytes)? {
        true => Ok(()),
        false => err("cannot rewrite a row's header in place"),
    }
}

/// Sets the xmax of an LSM version, writing it again under its key.
fn mark_key(tree: &mut LsmTree, key: &[u8], xmax: Xid) -> Result<()> {
    let Some(mut bytes) = tree.get(key)? else {
        return err("an LSM row is gone");
    };
    Header::set_xmax(&mut bytes, xmax);
    tree.put(key.to_vec(), bytes)
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

    fn key(&self) -> Result<&[u8]> {
        match self {
            RowId::Key(key) => Ok(key),
            _ => err("expected an LSM row"),
        }
    }
}

/// Checks that a stored row, its version header included, fits in a page;
/// the sizes the error gives leave the header out.
fn check_heap_size(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_TUPLE_SIZE {
        return err(format!(
            "row is {} bytes, the maximum is {}",
            bytes.len() - HEADER,
            MAX_TUPLE_SIZE - HEADER
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
