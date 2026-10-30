//! System catalog: which tables and indexes exist, their schemas, and where
//! their data lives.
//!
//! The catalog is itself a table heap whose first page is the DiskManager's
//! root page. Each table and each index is one catalog row:
//!
//! ```text
//! (sql TEXT, first_page INT, last_page INT)
//! ```
//!
//! where `sql` is the table's CREATE TABLE statement or the index's CREATE
//! INDEX statement, parsed again on open (the same idea as SQLite's
//! `sqlite_schema` table). A table's `USING` clause says how the rows are
//! stored: for a heap the two pages are its first and last page, for an LSM
//! tree or a column store both are its manifest page. For an index the first
//! is the root page of its B+ tree or hash file, which never moves, and the second is
//! [`INDEX_FORMAT`]. An index of an older format is built again, in the
//! format of today, when the catalog is opened: in files from before indexes
//! had several keys, included columns and NULL keys, the second page was the
//! root page again; in files from before DECIMAL, it was
//! [`PREVIOUS_INDEX_FORMAT`], and a REAL key's fraction had no 00 after it
//! (see `storage/sort_key.rs`).
//!
//! A heap table with a primary key gets a unique index on it, `<table>_pkey`,
//! when it is created; it is dropped with the table and not on its own.
//!
//! An analyzed table has one more row, its statistics (see `stats.rs`):
//! `sql` is the table's name and the statistics as text, the first page is
//! 0 and the second is [`STATS_FORMAT`]. ANALYZE replaces it.
//!
//! One row says where the commit log is (see `mvcc/clog.rs`): its first
//! page is the log's header page, and the second is [`LOG_FORMAT`]. The
//! catalog rows themselves have no versions: a statement that changes them
//! has the database to itself (see `mvcc/mod.rs`).
//!
//! **Upgrade.** A file from before rows had versions has no commit log row.
//! Opening it copies every table's rows into a new table of today, as
//! versions every transaction sees, and builds its indexes again.

use std::collections::BTreeMap;
use std::sync::Arc;

pub mod index;
pub mod stats;
pub mod table;

use crate::error::{Error, Result, err};
use crate::mvcc::{Snapshot, Transactions};
use crate::sql::ast::{
    Alter, ColumnRef, Constraints, CreateIndex, Expr, IndexKind, Statement, Written, quote_ident,
};
use crate::sql::eval::cast;
use crate::sql::parse;
use crate::storage::{BPlusTree, BufferPool, MAX_TUPLE_SIZE, PageId, RecordId, TableHeap, tuple};
use crate::types::{Column, DataType, StorageKind, Value};
use index::Index;
use stats::TableStats;
use table::{Row, Table, primary_key, validate_schema};

/// The second page of an index's catalog row: not a page, but a mark of the
/// format of its entries.
const INDEX_FORMAT: PageId = PageId(u32::MAX - 2);

/// The mark of the format before, whose keys a REAL with a fraction ended
/// without a 00.
const PREVIOUS_INDEX_FORMAT: PageId = PageId(u32::MAX);

/// The second page of a table's statistics row: not a page either.
const STATS_FORMAT: PageId = PageId(u32::MAX - 1);

/// The second page of the commit log's row.
const LOG_FORMAT: PageId = PageId(u32::MAX - 3);

pub struct Catalog {
    pool: Arc<BufferPool>,
    txns: Arc<Transactions>,
    /// Holds one row per table and one per index.
    heap: TableHeap,
    tables: BTreeMap<String, Entry>,
    /// Every index, by name; the index itself belongs to its table.
    indexes: BTreeMap<String, IndexEntry>,
}

struct IndexEntry {
    table: String,
    /// Location of this index's catalog row.
    rid: RecordId,
}

struct Entry {
    table: Table,
    /// Location of this table's catalog row.
    rid: RecordId,
    /// The table's last root page as currently recorded in the catalog row.
    saved_last_page: PageId,
    /// Location of the table's statistics row, once it is analyzed.
    stats_rid: Option<RecordId>,
}

/// A catalog row of an index: where it is, what it is, its root page and the format of its entries.
type IndexRow = (RecordId, CreateIndex, PageId, PageId);

impl Catalog {
    /// Loads the catalog, creating an empty one in a new database, and
    /// reads the commit log into `txns` (see [`Transactions::attach`]).
    pub fn open(pool: Arc<BufferPool>, txns: Arc<Transactions>) -> Result<Self> {
        let mut heap = match pool.disk().root_page() {
            Some(root) => TableHeap::open(pool.clone(), root, root)?,
            None => {
                let heap = TableHeap::create(pool.clone())?;
                // The root must not point at a page that was never written.
                pool.flush_page(heap.first_page())?;
                pool.disk().set_root_page(Some(heap.first_page()))?;
                heap
            }
        };

        let mut table_rows = Vec::new();
        let mut index_rows: Vec<IndexRow> = Vec::new();
        let mut stats_rows = Vec::new();
        let mut log = None;
        for item in heap.scan() {
            let (rid, bytes) = item?;
            let (sql, first, last) = decode_row(&bytes)?;
            if last == STATS_FORMAT {
                stats_rows.push((rid, sql));
                continue;
            }
            if last == LOG_FORMAT {
                log = Some(first);
                continue;
            }
            match decode_entry(&sql, first, last)? {
                (
                    Statement::CreateTable {
                        name,
                        columns,
                        constraints,
                        storage,
                    },
                    first,
                    last,
                ) => table_rows.push((rid, name, columns, constraints, storage, (first, last))),
                (Statement::CreateIndex(definition), root, format) => {
                    index_rows.push((rid, definition, root, format))
                }
                (statement, ..) => return err(format!("corrupted catalog row: {statement:?}")),
            }
        }
        let legacy = log.is_none() && !table_rows.is_empty();
        let root = txns.attach(&pool, log)?;
        if log.is_none() {
            heap.insert(&encode_entry("commit log".into(), (root, LOG_FORMAT))?)?;
        }

        let mut tables = BTreeMap::new();
        for (rid, name, columns, constraints, storage, pages) in table_rows {
            let table = Table::open(
                pool.clone(),
                txns.clone(),
                name.clone(),
                columns,
                constraints,
                storage,
                pages,
                legacy,
            )?;
            tables.insert(
                name,
                Entry {
                    table,
                    rid,
                    saved_last_page: pages.1,
                    stats_rid: None,
                },
            );
        }

        // Indexes are opened once every table is, as their rows may come first.
        let mut indexes = BTreeMap::new();
        let index_rows = match legacy {
            true => {
                upgrade(
                    &pool,
                    &txns,
                    &mut heap,
                    &mut tables,
                    index_rows,
                    &mut indexes,
                )?;
                Vec::new()
            }
            false => index_rows,
        };
        for (mut rid, definition, root, format) in index_rows {
            let (name, table) = (definition.name.clone(), definition.table.clone());
            let Some(entry) = tables.get_mut(&table) else {
                return err(format!(
                    "corrupted catalog: index {name} on unknown table {table}"
                ));
            };
            if format == INDEX_FORMAT {
                entry.table.open_index(&definition, root)?;
            } else {
                // Of an older format: built again.
                if format == PREVIOUS_INDEX_FORMAT {
                    entry.table.open_index(&definition, root)?;
                    entry.table.drop_index(&name)?;
                } else {
                    BPlusTree::open(pool.clone(), root).destroy()?;
                }
                entry.table.create_index(&definition, &txns.latest())?;
                let root = entry
                    .table
                    .indexes()
                    .last()
                    .expect("just created")
                    .root_page();
                heap.delete(rid)?;
                rid = heap.insert(&encode_entry(definition.to_string(), (root, INDEX_FORMAT))?)?;
            }
            indexes.insert(name, IndexEntry { table, rid });
        }
        for (rid, sql) in stats_rows {
            let (name, text) = sql.split_once(' ').unwrap_or((&sql, ""));
            let Some(entry) = tables.get_mut(name) else {
                return err(format!(
                    "corrupted catalog: statistics of unknown table {name}"
                ));
            };
            let stats = TableStats::decode(text)?;
            if stats.columns.len() != entry.table.columns.len() {
                return err(format!(
                    "corrupted catalog: statistics of {name} have the wrong columns"
                ));
            }
            entry.table.set_stats(Some(stats));
            entry.stats_rid = Some(rid);
        }
        Ok(Catalog {
            pool,
            txns,
            heap,
            tables,
            indexes,
        })
    }

    /// The transactions of the database the catalog belongs to.
    pub fn transactions(&self) -> &Arc<Transactions> {
        &self.txns
    }

    pub fn get(&self, name: &str) -> Result<&Table> {
        self.tables
            .get(name)
            .map(|e| &e.table)
            .ok_or_else(|| unknown_table(name))
    }

    pub fn get_mut(&mut self, name: &str) -> Result<&mut Table> {
        self.tables
            .get_mut(name)
            .map(|e| &mut e.table)
            .ok_or_else(|| unknown_table(name))
    }

    pub fn table_names(&self) -> Vec<&str> {
        self.tables.keys().map(String::as_str).collect()
    }

    pub fn create_table(
        &mut self,
        name: String,
        columns: Vec<Column>,
        constraints: Constraints,
        storage: StorageKind,
        snapshot: &Snapshot,
    ) -> Result<()> {
        if self.tables.contains_key(&name) {
            return err(format!("table already exists: {name}"));
        }
        validate_schema(&columns)?;
        let mut indexes = Vec::new();
        if let (StorageKind::Heap, Some(pk)) = (storage, primary_key(&columns)) {
            let key = [columns[pk].name.clone()];
            indexes.push(unique_index(&name, &pk_index_name(&name), &key));
        }
        if !constraints.unique.is_empty() && matches!(storage, StorageKind::Column) {
            return err("COLUMN tables cannot have UNIQUE constraints, which need an index");
        }
        for unique in &constraints.unique {
            indexes.push(unique_index(&name, &unique.name, &unique.columns));
        }
        for (i, index) in indexes.iter().enumerate() {
            if self.indexes.contains_key(&index.name)
                || indexes[..i].iter().any(|other| other.name == index.name)
            {
                return err(format!("index already exists: {}", index.name));
            }
        }

        let table = Table::create(
            self.pool.clone(),
            self.txns.clone(),
            name.clone(),
            columns,
            constraints,
            storage,
        )?;
        let rid = match encode_table_entry(&table).and_then(|bytes| self.heap.insert(&bytes)) {
            Ok(rid) => rid,
            Err(e) => {
                table.destroy()?;
                return Err(e);
            }
        };
        let saved_last_page = table.root_pages().1;
        self.tables.insert(
            name.clone(),
            Entry {
                table,
                rid,
                saved_last_page,
                stats_rid: None,
            },
        );

        for index in indexes {
            if let Err(e) = self.create_index(index, snapshot) {
                self.drop_table(&name)?;
                return Err(e);
            }
        }
        Ok(())
    }

    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        let entry = self
            .tables
            .remove(name)
            .ok_or_else(|| unknown_table(name))?;
        let indexes: Vec<String> = self
            .indexes
            .iter()
            .filter(|(_, index)| index.table == name)
            .map(|(n, _)| n.clone())
            .collect();
        for index in indexes {
            let rid = self.indexes.remove(&index).expect("listed above").rid;
            self.heap.delete(rid)?;
        }
        self.heap.delete(entry.rid)?;
        if let Some(rid) = entry.stats_rid {
            self.heap.delete(rid)?;
        }
        entry.table.destroy()
    }

    /// Computes the statistics of a table (see `stats.rs`) from the rows
    /// `snapshot` sees, and records them in place of those it had, halving
    /// their histograms and lists of common values until they fit in a
    /// catalog row.
    pub fn analyze(&mut self, name: &str, snapshot: &Snapshot) -> Result<()> {
        let entry = self
            .tables
            .get_mut(name)
            .ok_or_else(|| unknown_table(name))?;
        let mut stats = TableStats::collect(&entry.table, snapshot)?;
        let row = loop {
            let row = encode_entry(
                format!("{name} {}", stats.encode()),
                (PageId(0), STATS_FORMAT),
            )?;
            if row.len() <= MAX_TUPLE_SIZE {
                break row;
            }
            if !stats.shrink() {
                return err(format!(
                    "the statistics of {name} do not fit in a page: it has too many columns"
                ));
            }
        };
        let rid = match entry.stats_rid {
            Some(rid) if self.heap.update(rid, &row)? => rid,
            old => {
                if let Some(rid) = old {
                    self.heap.delete(rid)?;
                }
                self.heap.insert(&row)?
            }
        };
        entry.stats_rid = Some(rid);
        entry.table.set_stats(Some(stats));
        Ok(())
    }

    /// Builds an index on a table from its rows; a unique one checks the
    /// keys of the rows there for `snapshot`.
    pub fn create_index(&mut self, definition: CreateIndex, snapshot: &Snapshot) -> Result<()> {
        let (name, table) = (definition.name.clone(), definition.table.clone());
        if self.indexes.contains_key(&name) {
            return err(format!("index already exists: {name}"));
        }
        let entry = self
            .tables
            .get_mut(&table)
            .ok_or_else(|| unknown_table(&table))?;
        entry.table.create_index(&definition, snapshot)?;

        let root = entry
            .table
            .indexes()
            .last()
            .expect("just created")
            .root_page();
        match encode_entry(definition.to_string(), (root, INDEX_FORMAT))
            .and_then(|bytes| self.heap.insert(&bytes))
        {
            Ok(rid) => {
                self.indexes.insert(name, IndexEntry { table, rid });
                Ok(())
            }
            Err(e) => {
                entry.table.drop_index(&name)?;
                Err(e)
            }
        }
    }

    pub fn drop_index(&mut self, name: &str) -> Result<()> {
        let index = self
            .indexes
            .get(name)
            .ok_or_else(|| Error(format!("unknown index: {name}")))?;
        if name == pk_index_name(&index.table) {
            return err(format!(
                "cannot drop index {name}: the primary key of {} needs it",
                index.table
            ));
        }
        let table = &self.tables[&index.table].table;
        if table.constraints.unique.iter().any(|u| u.name == name) {
            return err(format!(
                "cannot drop index {name}: the UNIQUE constraint of {} of that name needs it",
                index.table
            ));
        }
        let index = self.indexes.remove(name).expect("found above");
        self.heap.delete(index.rid)?;
        self.tables
            .get_mut(&index.table)
            .expect("an index's table exists")
            .table
            .drop_index(name)
    }

    /// Changes a table as `action` says (see [`Alter`]), for a statement
    /// that began at `now` and reads `snapshot`. A change of where its
    /// columns are or what type (adding, dropping, `TYPE`) copies its rows
    /// into a new table of the new definition, its indexes built again; any
    /// other only rewrites its catalog rows, and checks the rows a new
    /// constraint must hold for.
    pub fn alter_table(
        &mut self,
        name: &str,
        action: Alter,
        snapshot: &Snapshot,
        now: i64,
    ) -> Result<()> {
        let table = self.get(name)?;
        let kind = table.storage_kind();
        let mut columns = table.columns.clone();
        let mut constraints = table.constraints.clone();
        let mut new_name = name.to_string();
        // Each index's definition, and its root page if it stays as it is.
        let mut indexes: Vec<(CreateIndex, Option<PageId>)> = self
            .index_definitions(name)?
            .into_iter()
            .map(|(definition, root)| (definition, Some(root)))
            .collect();
        let mut rewrite: Option<Box<dyn Fn(Row) -> Result<Row>>> = None;
        let mut check_rows = false;
        let position = |column: &str| table.column_index(column);
        // The names a constraint added may not take: the table's own, and every index's.
        let taken: Vec<String> = constraints
            .names()
            .into_iter()
            .chain(self.indexes.keys().cloned())
            .collect();
        let no_unique_in_columns = |written: &Written| match kind {
            StorageKind::Column if !written.unique.is_empty() => {
                err("COLUMN tables cannot have UNIQUE constraints, which need an index")
            }
            _ => Ok(()),
        };
        match action {
            Alter::RenameTo(to) => {
                if self.tables.contains_key(&to) {
                    return err(format!("table already exists: {to}"));
                }
                for (definition, _) in &mut indexes {
                    definition.table = to.clone();
                    if definition.name == pk_index_name(name) {
                        definition.name = pk_index_name(&to);
                    }
                }
                new_name = to;
            }
            Alter::RenameColumn { from, to } => {
                let i = position(&from)?;
                if position(&to).is_ok() {
                    return err(format!("column already exists: {to}"));
                }
                columns[i].name = to.clone();
                let rename = |column: &mut String| {
                    if *column == from {
                        *column = to.clone();
                    }
                };
                constraints.not_null.iter_mut().for_each(rename);
                constraints.defaults.iter_mut().for_each(|(c, _)| rename(c));
                for unique in &mut constraints.unique {
                    unique.columns.iter_mut().for_each(rename);
                }
                for check in &mut constraints.checks {
                    check.condition = renamed(&check.condition, &from, &to);
                }
                for (definition, _) in &mut indexes {
                    for key in &mut definition.keys {
                        *key = renamed(key, &from, &to);
                    }
                    definition.include.iter_mut().for_each(rename);
                    if let Some(predicate) = &mut definition.predicate {
                        *predicate = renamed(predicate, &from, &to);
                    }
                }
            }
            Alter::SetDefault { column, default } => {
                position(&column)?;
                constraints.defaults.retain(|(c, _)| *c != column);
                if let Some(default) = default {
                    constraints.defaults.push((column, default));
                }
            }
            Alter::SetNotNull { column, not_null } => {
                let i = position(&column)?;
                if !not_null && columns[i].primary_key {
                    return err(format!(
                        "column {column} is the primary key, which cannot be NULL"
                    ));
                }
                constraints.not_null.retain(|c| *c != column);
                if not_null {
                    constraints.not_null.push(column);
                    check_rows = true;
                }
            }
            Alter::AddConstraint(written) => {
                no_unique_in_columns(&written)?;
                let added = written.named(name, &taken)?;
                check_rows |= !added.checks.is_empty();
                for unique in &added.unique {
                    indexes.push((unique_index(name, &unique.name, &unique.columns), None));
                }
                constraints.checks.extend(added.checks);
                constraints.unique.extend(added.unique);
            }
            Alter::DropConstraint(constraint) => {
                if let Some(i) = constraints.checks.iter().position(|c| c.name == constraint) {
                    constraints.checks.remove(i);
                } else if let Some(i) = constraints.unique.iter().position(|u| u.name == constraint)
                {
                    constraints.unique.remove(i);
                    indexes.retain(|(definition, _)| definition.name != constraint);
                } else {
                    return err(format!("unknown constraint of {name}: {constraint}"));
                }
            }
            Alter::AddColumn {
                column,
                constraints: written,
            } => {
                if position(&column.name).is_ok() {
                    return err(format!("column already exists: {}", column.name));
                }
                if column.primary_key {
                    return err("a primary key column cannot be added to a table");
                }
                no_unique_in_columns(&written)?;
                let added = written.named(name, &taken)?;
                for unique in &added.unique {
                    indexes.push((unique_index(name, &unique.name, &unique.columns), None));
                }
                constraints.not_null.extend(added.not_null);
                constraints.defaults.extend(added.defaults);
                constraints.checks.extend(added.checks);
                constraints.unique.extend(added.unique);
                columns.push(column);
                // Every row gets the new column's default, as it would be now.
                let value = table::defaults(&columns, &constraints, now)?
                    .pop()
                    .expect("the column added");
                rewrite = Some(Box::new(move |mut row| {
                    row.push(value.clone());
                    Ok(row)
                }));
            }
            Alter::DropColumn(column) => {
                let i = position(&column)?;
                if columns[i].primary_key {
                    return err(format!(
                        "cannot drop column {column}: it is the primary key"
                    ));
                }
                if columns.len() == 1 {
                    return err(format!("cannot drop column {column}: {name} has no other"));
                }
                columns.remove(i);
                // What reads the column goes with it, as in PostgreSQL.
                constraints.not_null.retain(|c| *c != column);
                constraints.defaults.retain(|(c, _)| *c != column);
                constraints.checks.retain(|c| !reads(&c.condition, &column));
                constraints.unique.retain(|u| !u.columns.contains(&column));
                indexes.retain(|(definition, _)| {
                    !(definition.keys.iter().any(|key| reads(key, &column))
                        || definition.include.contains(&column)
                        || definition
                            .predicate
                            .as_ref()
                            .is_some_and(|p| reads(p, &column)))
                });
                rewrite = Some(Box::new(move |mut row| {
                    row.remove(i);
                    Ok(row)
                }));
            }
            Alter::SetType { column, data_type } => {
                let i = position(&column)?;
                columns[i].data_type = data_type;
                rewrite = Some(Box::new(move |mut row| {
                    row[i] = cast(&row[i], data_type)
                        .map_err(|e| Error(format!("column {column}: {}", e.0)))?;
                    Ok(row)
                }));
            }
        }
        for (i, (definition, _)) in indexes.iter().enumerate() {
            let other = self
                .indexes
                .get(&definition.name)
                .is_some_and(|index| index.table != name);
            if other || indexes[..i].iter().any(|(d, _)| d.name == definition.name) {
                return err(format!("index already exists: {}", definition.name));
            }
        }
        self.replace(
            name,
            Definition {
                name: new_name,
                columns,
                constraints,
                indexes,
                kind,
            },
            rewrite,
            check_rows,
            snapshot,
        )
    }

    /// Puts a table of a new definition in the place of the table `old`:
    /// the same, redefined, or with `rewrite`, a new table its rows are
    /// copied into, each changed by it; then rewrites their catalog rows.
    fn replace(
        &mut self,
        old: &str,
        definition: Definition,
        rewrite: Option<Box<dyn Fn(Row) -> Result<Row>>>,
        check_rows: bool,
        snapshot: &Snapshot,
    ) -> Result<()> {
        let entry = self.tables.remove(old).ok_or_else(|| unknown_table(old))?;
        let names: Vec<String> = self
            .indexes
            .iter()
            .filter(|(_, index)| index.table == old)
            .map(|(name, _)| name.clone())
            .collect();
        for name in names {
            let index = self.indexes.remove(&name).expect("listed above");
            self.heap.delete(index.rid)?;
        }
        self.heap.delete(entry.rid)?;
        if let Some(rid) = entry.stats_rid {
            self.heap.delete(rid)?;
        }
        let Definition {
            name,
            columns,
            constraints,
            indexes,
            kind,
        } = definition;
        let rewritten = rewrite.is_some();
        let mut table = match rewrite {
            None => {
                let mut table = entry.table;
                let kept: Vec<(CreateIndex, PageId)> = indexes
                    .iter()
                    .filter_map(|(definition, root)| Some((definition.clone(), (*root)?)))
                    .collect();
                table.redefine(name, columns, constraints, &kept)?;
                if check_rows {
                    table.check_rows(snapshot)?;
                }
                table
            }
            Some(rewrite) => {
                let rows = entry
                    .table
                    .scan(snapshot)
                    .map(|item| rewrite(item?.1))
                    .collect::<Result<Vec<_>>>()?;
                let mut table = Table::create(
                    self.pool.clone(),
                    self.txns.clone(),
                    name,
                    columns,
                    constraints,
                    kind,
                )?;
                table
                    .insert(snapshot, rows)
                    .map_err(|e| Error(format!("a row of {old} breaks it: {}", e.0)))?;
                entry.table.destroy()?;
                table
            }
        };
        for (definition, root) in &indexes {
            if rewritten || root.is_none() {
                table.create_index(definition, snapshot)?;
            }
        }
        table.tidy()?;

        let rid = self.heap.insert(&encode_table_entry(&table)?)?;
        for (definition, _) in &indexes {
            let root = table
                .indexes()
                .iter()
                .find(|index| index.name == definition.name)
                .expect("built or kept above")
                .root_page();
            let rid = self
                .heap
                .insert(&encode_entry(definition.to_string(), (root, INDEX_FORMAT))?)?;
            let entry = IndexEntry {
                table: table.name.clone(),
                rid,
            };
            self.indexes.insert(definition.name.clone(), entry);
        }
        let stats_rid = match table.stats() {
            Some(stats) => Some(self.heap.insert(&encode_entry(
                format!("{} {}", table.name, stats.encode()),
                (PageId(0), STATS_FORMAT),
            )?)?),
            None => None,
        };
        let saved_last_page = table.root_pages().1;
        self.tables.insert(
            table.name.clone(),
            Entry {
                table,
                rid,
                saved_last_page,
                stats_rid,
            },
        );
        Ok(())
    }

    /// The definitions of a table's indexes, read from their catalog rows,
    /// and their root pages.
    fn index_definitions(&self, table: &str) -> Result<Vec<(CreateIndex, PageId)>> {
        let mut found = Vec::new();
        for (name, index) in self
            .indexes
            .iter()
            .filter(|(_, index)| index.table == table)
        {
            let Some(bytes) = self.heap.get(index.rid)? else {
                return err(format!("corrupted catalog: no row for index {name}"));
            };
            let (sql, root, format) = decode_row(&bytes)?;
            match decode_entry(&sql, root, format)?.0 {
                Statement::CreateIndex(definition) => found.push((definition, root)),
                other => return err(format!("corrupted catalog row: {other:?}")),
            }
        }
        Ok(found)
    }

    /// Removes the versions of a table, or of every table, that no
    /// transaction can see any more (see [`Table::vacuum`]). Returns how
    /// many it removed.
    pub fn vacuum(&mut self, name: Option<&str>) -> Result<usize> {
        let horizon = self.txns.horizon();
        let mut removed = 0;
        match name {
            Some(name) => removed += self.get_mut(name)?.vacuum(&horizon)?,
            None => {
                for entry in self.tables.values_mut() {
                    removed += entry.table.vacuum(&horizon)?;
                }
            }
        }
        Ok(removed)
    }

    /// Makes every table count its rows again when the planner next asks,
    /// after a rollback whose writes the counts took in.
    pub fn forget_row_counts(&self) {
        for entry in self.tables.values() {
            entry.table.forget_row_count();
        }
    }

    /// Lets every table tidy up after a commit (see [`Table::tidy`]).
    /// Returns whether one changed anything.
    pub fn tidy(&mut self) -> Result<bool> {
        let mut changed = false;
        for entry in self.tables.values_mut() {
            changed |= entry.table.tidy()?;
        }
        Ok(changed)
    }

    /// Copies the committed pages from the write-ahead log into the database file.
    pub fn checkpoint(&self) -> Result<()> {
        self.pool.disk().checkpoint()
    }

    /// Records heaps that grew since the last save, and the transaction ids
    /// handed out, then writes every dirty page back to the disk, for it to
    /// commit or roll back.
    pub fn save(&mut self) -> Result<()> {
        for entry in self.tables.values_mut() {
            let last_page = entry.table.root_pages().1;
            if last_page != entry.saved_last_page {
                // Same length as before, so this rewrites one page in place.
                if !self
                    .heap
                    .update(entry.rid, &encode_table_entry(&entry.table)?)?
                {
                    return err("cannot update catalog row");
                }
                entry.saved_last_page = last_page;
            }
        }
        self.txns.save(&self.pool)?;
        self.pool.flush_all()
    }
}

/// Copies the rows of every table of a file from before rows had versions
/// into a new table of today, as versions every transaction sees, and
/// builds its indexes again; the old indexes are freed first.
fn upgrade(
    pool: &Arc<BufferPool>,
    txns: &Arc<Transactions>,
    heap: &mut TableHeap,
    tables: &mut BTreeMap<String, Entry>,
    index_rows: Vec<IndexRow>,
    indexes: &mut BTreeMap<String, IndexEntry>,
) -> Result<()> {
    for (_, definition, root, format) in &index_rows {
        let Some(entry) = tables.get(&definition.table) else {
            return err(format!(
                "corrupted catalog: index {} on unknown table {}",
                definition.name, definition.table
            ));
        };
        if *format == INDEX_FORMAT || *format == PREVIOUS_INDEX_FORMAT {
            Index::open(pool.clone(), definition, &entry.table.columns, *root)?.destroy()?;
        } else {
            BPlusTree::open(pool.clone(), *root).destroy()?;
        }
    }
    let snapshot = Snapshot::frozen(txns);
    for entry in tables.values_mut() {
        let old = &entry.table;
        let rows = old.legacy_rows()?;
        let mut table = Table::create(
            pool.clone(),
            txns.clone(),
            old.name.clone(),
            old.columns.clone(),
            old.constraints.clone(),
            old.storage_kind(),
        )?;
        table.insert(&snapshot, rows)?;
        table.tidy()?;
        std::mem::replace(&mut entry.table, table).destroy()?;
        heap.delete(entry.rid)?;
        entry.rid = heap.insert(&encode_table_entry(&entry.table)?)?;
        entry.saved_last_page = entry.table.root_pages().1;
    }
    for (rid, definition, _, _) in index_rows {
        let entry = tables.get_mut(&definition.table).expect("checked above");
        entry.table.create_index(&definition, &snapshot)?;
        let root = entry
            .table
            .indexes()
            .last()
            .expect("just created")
            .root_page();
        heap.delete(rid)?;
        let rid = heap.insert(&encode_entry(definition.to_string(), (root, INDEX_FORMAT))?)?;
        indexes.insert(
            definition.name.clone(),
            IndexEntry {
                table: definition.table,
                rid,
            },
        );
    }
    Ok(())
}

fn catalog_columns() -> Vec<Column> {
    [
        ("sql", DataType::Text),
        ("first_page", DataType::Int),
        ("last_page", DataType::Int),
    ]
    .map(|(name, data_type)| Column {
        name: name.into(),
        data_type,
        primary_key: false,
    })
    .into()
}

fn encode_table_entry(table: &Table) -> Result<Vec<u8>> {
    encode_entry(
        create_table_sql(
            &table.name,
            &table.columns,
            &table.constraints,
            table.storage_kind(),
        ),
        table.root_pages(),
    )
}

fn encode_entry(sql: String, (first, last): (PageId, PageId)) -> Result<Vec<u8>> {
    let row = [
        Value::Text(sql),
        Value::Int(first.0.into()),
        Value::Int(last.0.into()),
    ];
    tuple::encode(&catalog_columns(), &row)
}

/// Decodes a catalog row into its text and the two pages it records.
fn decode_row(bytes: &[u8]) -> Result<(String, PageId, PageId)> {
    let row = tuple::decode(&catalog_columns(), bytes)?;
    let [Value::Text(sql), Value::Int(first), Value::Int(last)] = row.as_slice() else {
        return err("corrupted catalog row");
    };
    Ok((sql.clone(), page_id(*first)?, page_id(*last)?))
}

/// Parses the statement of a table's or an index's catalog row.
fn decode_entry(sql: &str, first: PageId, last: PageId) -> Result<(Statement, PageId, PageId)> {
    let Some(statement) = parse(sql)?.pop() else {
        return err(format!("corrupted catalog row: {sql}"));
    };
    Ok((statement, first, last))
}

/// A table's definition as an ALTER TABLE leaves it.
struct Definition {
    name: String,
    columns: Vec<Column>,
    constraints: Constraints,
    /// Each index's definition, and its root page if the index stays as it
    /// is; none for one to build.
    indexes: Vec<(CreateIndex, Option<PageId>)>,
    kind: StorageKind,
}

/// The unique index named `name` that keeps a primary key or a UNIQUE
/// constraint of `table` on `columns`.
fn unique_index(table: &str, name: &str, columns: &[String]) -> CreateIndex {
    CreateIndex {
        name: name.to_string(),
        table: table.to_string(),
        using: IndexKind::BTree,
        keys: columns
            .iter()
            .map(|column| Expr::Column(ColumnRef::bare(column)))
            .collect(),
        include: Vec::new(),
        predicate: None,
        unique: true,
    }
}

/// An expression of a table's columns, with the column `from` called `to`.
fn renamed(expr: &Expr, from: &str, to: &str) -> Expr {
    expr.replace(&mut |e| match e {
        Expr::Column(column) if column.name == from => Some(Ok(Expr::Column(ColumnRef::bare(to)))),
        _ => None,
    })
    .expect("renaming a column cannot fail")
}

/// Whether an expression reads the column with this name.
fn reads(expr: &Expr, column: &str) -> bool {
    let mut found = false;
    expr.visit(&mut |e| {
        if let Expr::Column(c) = e
            && c.name == column
        {
            found = true;
        }
    });
    found
}

fn pk_index_name(table: &str) -> String {
    format!("{table}_pkey")
}

/// The CREATE TABLE statement a table's catalog row keeps: each column with
/// its `PRIMARY KEY`, `NOT NULL` and `DEFAULT (expr)`, then every CHECK and
/// UNIQUE constraint by name.
fn create_table_sql(
    name: &str,
    columns: &[Column],
    constraints: &Constraints,
    storage: StorageKind,
) -> String {
    let mut items: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut item = format!("{} {}", quote_ident(&c.name), c.data_type);
            if c.primary_key {
                item += " PRIMARY KEY";
            }
            if constraints.is_not_null(&c.name) {
                item += " NOT NULL";
            }
            if let Some(default) = constraints.default_of(&c.name) {
                item += &format!(" DEFAULT ({default})");
            }
            item
        })
        .collect();
    for check in &constraints.checks {
        let name = quote_ident(&check.name);
        items.push(format!("CONSTRAINT {name} CHECK ({})", check.condition));
    }
    for unique in &constraints.unique {
        let columns: Vec<String> = unique.columns.iter().map(|c| quote_ident(c)).collect();
        let name = quote_ident(&unique.name);
        items.push(format!("CONSTRAINT {name} UNIQUE ({})", columns.join(", ")));
    }
    let using = match storage {
        StorageKind::Heap => String::new(), // the default, as in files written before LSM existed
        other => format!(" USING {other}"),
    };
    format!(
        "CREATE TABLE {} ({}){using}",
        quote_ident(name),
        items.join(", ")
    )
}

fn page_id(raw: i64) -> Result<PageId> {
    u32::try_from(raw)
        .map(PageId)
        .map_err(|_| Error(format!("corrupted catalog: bad page id {raw}")))
}

fn unknown_table(name: &str) -> Error {
    Error(format!("unknown table: {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::version::{HEADER, Header};
    use crate::storage::{DiskManager, LsmConfig, LsmTree};

    fn open(pool: &Arc<BufferPool>) -> Catalog {
        Catalog::open(pool.clone(), Transactions::new()).unwrap()
    }

    fn create_index(catalog: &mut Catalog, sql: &str) {
        let Some(Statement::CreateIndex(definition)) = parse(sql).unwrap().pop() else {
            unreachable!()
        };
        let snapshot = Snapshot::frozen(catalog.transactions());
        catalog.create_index(definition, &snapshot).unwrap();
    }

    fn create_table(catalog: &mut Catalog, columns: Vec<Column>, rows: Vec<Vec<Value>>) {
        let snapshot = Snapshot::frozen(catalog.transactions());
        catalog
            .create_table(
                "t".into(),
                columns,
                Constraints::default(),
                StorageKind::Heap,
                &snapshot,
            )
            .unwrap();
        catalog
            .get_mut("t")
            .unwrap()
            .insert(&snapshot, rows)
            .unwrap();
    }

    #[test]
    fn indexes_from_before_null_keys_are_built_again_when_opened() {
        let pool = Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 64));
        let mut catalog = open(&pool);
        let columns = vec![
            Column {
                name: "id".into(),
                data_type: DataType::Int,
                primary_key: true,
            },
            Column {
                name: "n".into(),
                data_type: DataType::Int,
                primary_key: false,
            },
        ];
        let rows = (0..500).map(|i| {
            vec![
                Value::Int(i),
                if i % 5 == 0 {
                    Value::Null
                } else {
                    Value::Int(i % 7)
                },
            ]
        });
        create_table(&mut catalog, columns, rows.collect());
        create_index(&mut catalog, "CREATE INDEX t_n ON t (n)");

        // Record it as files from before did, its root page twice.
        let rid = catalog.indexes["t_n"].rid;
        let root = catalog.get("t").unwrap().indexes()[1].root_page();
        assert!(
            catalog
                .heap
                .update(
                    rid,
                    &encode_entry("CREATE INDEX t_n ON t (n)".into(), (root, root)).unwrap()
                )
                .unwrap()
        );
        catalog.save().unwrap();
        let pages = pool.disk().page_count();

        let catalog = open(&pool);
        let (sql, first, format) = decode_row(
            &catalog
                .heap
                .get(catalog.indexes["t_n"].rid)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let (sql, _, format) = decode_entry(&sql, first, format).unwrap();
        assert_eq!(format, INDEX_FORMAT);
        assert!(matches!(sql, Statement::CreateIndex(CreateIndex { name, .. }) if name == "t_n"));
        let index = &catalog.get("t").unwrap().indexes()[1];
        assert_eq!(
            index.scan(&[Value::Null], None, None).unwrap().len(),
            100,
            "NULL keys are in it now"
        );
        assert_eq!(index.scan(&[Value::Int(3)], None, None).unwrap().len(), 57);
        assert_eq!(
            pool.disk().page_count(),
            pages,
            "the old tree's pages were freed and reused"
        );

        // Once built again, it is opened as it is.
        let reopened = open(&pool);
        assert_eq!(
            reopened.get("t").unwrap().indexes()[1].root_page(),
            index.root_page()
        );
    }

    #[test]
    fn indexes_from_before_decimal_are_built_again_when_opened() {
        let pool = Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 64));
        let mut catalog = open(&pool);
        let column = |name: &str, data_type| Column {
            name: name.into(),
            data_type,
            primary_key: name == "id",
        };
        let columns = vec![column("id", DataType::Int), column("x", DataType::Real)];
        let rows = (0..300).map(|i| vec![Value::Int(i), Value::Real(i as f64 / 4.0)]);
        create_table(&mut catalog, columns, rows.collect());
        for sql in [
            "CREATE INDEX t_x ON t (x)",
            "CREATE INDEX t_h ON t USING HASH (x)",
        ] {
            create_index(&mut catalog, sql);
            // Record it as files from before DECIMAL did.
            let name = if sql.contains("HASH") { "t_h" } else { "t_x" };
            let rid = catalog.indexes[name].rid;
            let index = catalog.get("t").unwrap().indexes().last().unwrap();
            let row = encode_entry(sql.into(), (index.root_page(), PREVIOUS_INDEX_FORMAT)).unwrap();
            assert!(catalog.heap.update(rid, &row).unwrap());
        }
        catalog.save().unwrap();
        let pages = pool.disk().page_count();

        let catalog = open(&pool);
        for name in ["t_x", "t_h"] {
            let row = catalog
                .heap
                .get(catalog.indexes[name].rid)
                .unwrap()
                .unwrap();
            assert_eq!(decode_row(&row).unwrap().2, INDEX_FORMAT, "{name}");
        }
        for index in &catalog.get("t").unwrap().indexes()[1..] {
            let found = index.scan(&[Value::Real(12.25)], None, None).unwrap();
            assert_eq!(found.len(), 1, "{}", index.name);
        }
        assert_eq!(pool.disk().page_count(), pages, "the old pages were reused");
    }

    #[test]
    fn files_from_before_versions_are_upgraded_when_opened() {
        let pool = Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 64));
        let column = |name: &str, data_type| Column {
            name: name.into(),
            data_type,
            primary_key: name == "id",
        };
        let columns = vec![column("id", DataType::Int), column("name", DataType::Text)];
        let row = |i: i64| vec![Value::Int(i), Value::Text(format!("n{}", i % 10))];
        let rows: Vec<Vec<Value>> = (0..1500).map(row).collect();
        let mut catalog = open(&pool);
        for (table, using) in [("h", "HEAP"), ("l", "LSM"), ("c", "COLUMN")] {
            let snapshot = Snapshot::frozen(catalog.transactions());
            let storage = match using {
                "HEAP" => StorageKind::Heap,
                "LSM" => StorageKind::Lsm,
                _ => StorageKind::Column,
            };
            catalog
                .create_table(
                    table.into(),
                    columns.clone(),
                    Constraints::default(),
                    storage,
                    &snapshot,
                )
                .unwrap();
        }
        create_index(&mut catalog, "CREATE INDEX h_name ON h (name)");
        create_index(&mut catalog, "CREATE INDEX l_name ON l (name)");
        // Write the rows as files from before versions did: no headers, LSM
        // keys of their own, and no commit log.
        let encoded: Vec<Vec<u8>> = rows
            .iter()
            .map(|row| tuple::encode(&columns, row).unwrap())
            .collect();
        let manifest = catalog.get("l").unwrap().root_pages().0;
        let mut tree = LsmTree::open(pool.clone(), manifest, LsmConfig::default()).unwrap();
        for (row, bytes) in rows.iter().zip(&encoded) {
            tree.put(tuple::encode_key(&row[0]), bytes.clone()).unwrap();
        }
        let snapshot = Snapshot::frozen(catalog.transactions());
        catalog
            .get_mut("c")
            .unwrap()
            .insert(&snapshot, rows[..10].to_vec())
            .unwrap();
        catalog
            .get_mut("h")
            .unwrap()
            .insert(&snapshot, rows.clone())
            .unwrap();
        catalog.save().unwrap();
        // Strip the headers of the heap's rows and the column table's delta
        // store, and drop the commit log's row, its pages left behind.
        let strip = |first: PageId| {
            let mut heap = TableHeap::open(pool.clone(), first, first).unwrap();
            let all: Vec<_> = heap.scan().map(Result::unwrap).collect();
            for (rid, bytes) in all {
                heap.delete(rid).unwrap();
                heap.insert(&bytes[HEADER..]).unwrap();
            }
        };
        strip(catalog.get("h").unwrap().root_pages().0);
        let column_manifest = catalog.get("c").unwrap().root_pages().0;
        let delta = PageId(crate::storage::io::disk::read_u32(
            &pool.fetch_read(column_manifest).unwrap(),
            0,
        ));
        strip(delta);
        // The legacy manifest has the group count where the pending deletes' page is.
        {
            let mut page = pool.fetch_write(column_manifest).unwrap();
            crate::storage::io::disk::write_u32(&mut page, 4, 0);
        }
        let log_row = catalog
            .heap
            .scan()
            .map(Result::unwrap)
            .find(|(_, bytes)| decode_row(bytes).unwrap().2 == LOG_FORMAT)
            .unwrap()
            .0;
        catalog.heap.delete(log_row).unwrap();
        // The old indexes' entries pointed at headerless rows: they are rebuilt anyway.
        pool.flush_all().unwrap();

        let catalog = open(&pool);
        let latest = catalog.transactions().latest();
        for (table, count) in [("h", 1500), ("l", 1500), ("c", 10)] {
            let table = catalog.get(table).unwrap();
            let found: Vec<Vec<Value>> = table.scan(&latest).map(|item| item.unwrap().1).collect();
            assert_eq!(found.len(), count, "{}", table.name);
            for row in found {
                let Value::Int(i) = row[0] else { panic!() };
                assert_eq!(row, self::row_of(i), "{}", table.name);
            }
        }
        for name in ["h", "l"] {
            let table = catalog.get(name).unwrap();
            let index = table.indexes().last().unwrap();
            assert_eq!(
                index
                    .scan(&[Value::Text("n3".into())], None, None)
                    .unwrap()
                    .len(),
                150,
                "{name}"
            );
        }
        // Its rows are versions every transaction sees.
        let heap_table = catalog.get("h").unwrap();
        let (rid, _) = heap_table.scan(&latest).next().unwrap().unwrap();
        let table::RowId::Record(rid) = rid else {
            panic!()
        };
        let stored = TableHeap::get_in(&pool, rid).unwrap().unwrap();
        assert_eq!(
            Header::read(&stored).unwrap().0.xmin,
            crate::storage::version::FROZEN
        );
    }

    fn row_of(i: i64) -> Vec<Value> {
        vec![Value::Int(i), Value::Text(format!("n{}", i % 10))]
    }
}
