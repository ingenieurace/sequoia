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
//! tree or a column store both are its manifest page. For an index both are
//! the root page of its B+ tree, which never moves.
//!
//! A heap table with a primary key gets a unique index on it, `<table>_pkey`,
//! when it is created; it is dropped with the table and not on its own.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::ast::Statement;
use crate::error::{Error, Result, err};
use crate::parser::parse;
use crate::storage::{BufferPool, PageId, RecordId, TableHeap, tuple};
use crate::table::{Table, primary_key, validate_schema};
use crate::types::{Column, DataType, StorageKind, Value};

pub struct Catalog {
    pool: Arc<BufferPool>,
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
}

impl Catalog {
    /// Loads the catalog, creating an empty one in a new database.
    pub fn open(pool: Arc<BufferPool>) -> Result<Self> {
        let heap = match pool.disk().root_page() {
            Some(root) => TableHeap::open(pool.clone(), root, root)?,
            None => {
                let heap = TableHeap::create(pool.clone())?;
                // The root must not point at a page that was never written.
                pool.flush_page(heap.first_page())?;
                pool.disk().set_root_page(Some(heap.first_page()))?;
                heap
            }
        };

        let mut tables = BTreeMap::new();
        let mut index_rows = Vec::new();
        for item in heap.scan() {
            let (rid, bytes) = item?;
            match decode_entry(&bytes)? {
                (Statement::CreateTable { name, columns, storage }, first, last) => {
                    let table = Table::open(pool.clone(), name.clone(), columns, storage, (first, last))?;
                    tables.insert(name, Entry { table, rid, saved_last_page: last });
                }
                (Statement::CreateIndex { name, table, column, unique }, root, _) => {
                    index_rows.push((rid, name, table, column, unique, root));
                }
                (statement, ..) => return err(format!("corrupted catalog row: {statement:?}")),
            }
        }

        // Indexes are opened once every table is, as their rows may come first.
        let mut indexes = BTreeMap::new();
        for (rid, name, table, column, unique, root) in index_rows {
            let Some(entry) = tables.get_mut(&table) else {
                return err(format!("corrupted catalog: index {name} on unknown table {table}"));
            };
            let column = entry.table.column_index(&column)?;
            entry.table.open_index(name.clone(), column, unique, root);
            indexes.insert(name, IndexEntry { table, rid });
        }
        Ok(Catalog { pool, heap, tables, indexes })
    }

    pub fn get(&self, name: &str) -> Result<&Table> {
        self.tables.get(name).map(|e| &e.table).ok_or_else(|| unknown_table(name))
    }

    pub fn get_mut(&mut self, name: &str) -> Result<&mut Table> {
        self.tables.get_mut(name).map(|e| &mut e.table).ok_or_else(|| unknown_table(name))
    }

    pub fn table_names(&self) -> Vec<&str> {
        self.tables.keys().map(String::as_str).collect()
    }

    pub fn create_table(&mut self, name: String, columns: Vec<Column>, storage: StorageKind) -> Result<()> {
        if self.tables.contains_key(&name) {
            return err(format!("table already exists: {name}"));
        }
        validate_schema(&columns)?;
        let pk_index = match (storage, primary_key(&columns)) {
            (StorageKind::Heap, Some(pk)) => Some((pk_index_name(&name), columns[pk].name.clone())),
            _ => None,
        };
        if let Some((index, _)) = &pk_index
            && self.indexes.contains_key(index)
        {
            return err(format!("index already exists: {index}"));
        }

        let table = Table::create(self.pool.clone(), name.clone(), columns, storage)?;
        let rid = match encode_table_entry(&table).and_then(|bytes| self.heap.insert(&bytes)) {
            Ok(rid) => rid,
            Err(e) => {
                table.destroy()?;
                return Err(e);
            }
        };
        let saved_last_page = table.root_pages().1;
        self.tables.insert(name.clone(), Entry { table, rid, saved_last_page });

        if let Some((index, column)) = pk_index
            && let Err(e) = self.create_index(index, name.clone(), &column, true)
        {
            self.drop_table(&name)?;
            return Err(e);
        }
        Ok(())
    }

    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        let entry = self.tables.remove(name).ok_or_else(|| unknown_table(name))?;
        let indexes: Vec<String> =
            self.indexes.iter().filter(|(_, index)| index.table == name).map(|(n, _)| n.clone()).collect();
        for index in indexes {
            let rid = self.indexes.remove(&index).expect("listed above").rid;
            self.heap.delete(rid)?;
        }
        self.heap.delete(entry.rid)?;
        entry.table.destroy()
    }

    /// Builds an index on one column of a table from its current rows.
    pub fn create_index(&mut self, name: String, table: String, column: &str, unique: bool) -> Result<()> {
        if self.indexes.contains_key(&name) {
            return err(format!("index already exists: {name}"));
        }
        let entry = self.tables.get_mut(&table).ok_or_else(|| unknown_table(&table))?;
        let column_index = entry.table.column_index(column)?;
        entry.table.create_index(name.clone(), column_index, unique)?;

        let root = entry.table.indexes().last().expect("just created").root_page();
        let sql = create_index_sql(&name, &table, column, unique);
        match encode_entry(sql, (root, root)).and_then(|bytes| self.heap.insert(&bytes)) {
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
        let index = self.indexes.get(name).ok_or_else(|| Error(format!("unknown index: {name}")))?;
        if name == pk_index_name(&index.table) {
            return err(format!("cannot drop index {name}: the primary key of {} needs it", index.table));
        }
        let index = self.indexes.remove(name).expect("found above");
        self.heap.delete(index.rid)?;
        self.tables.get_mut(&index.table).expect("an index's table exists").table.drop_index(name)
    }

    /// Copies the committed pages from the write-ahead log into the database file.
    pub fn checkpoint(&self) -> Result<()> {
        self.pool.disk().checkpoint()
    }

    /// Records heaps that grew since the last save, then writes every dirty
    /// page back and commits, making everything since the last save durable.
    pub fn save(&mut self) -> Result<()> {
        for entry in self.tables.values_mut() {
            let last_page = entry.table.root_pages().1;
            if last_page != entry.saved_last_page {
                // Same length as before, so this rewrites one page in place.
                if !self.heap.update(entry.rid, &encode_table_entry(&entry.table)?)? {
                    return err("cannot update catalog row");
                }
                entry.saved_last_page = last_page;
            }
        }
        self.pool.flush_all()?;
        self.pool.disk().commit()
    }
}

fn catalog_columns() -> Vec<Column> {
    [("sql", DataType::Text), ("first_page", DataType::Int), ("last_page", DataType::Int)]
        .map(|(name, data_type)| Column { name: name.into(), data_type, primary_key: false })
        .into()
}

fn encode_table_entry(table: &Table) -> Result<Vec<u8>> {
    encode_entry(create_table_sql(&table.name, &table.columns, table.storage_kind()), table.root_pages())
}

fn encode_entry(sql: String, (first, last): (PageId, PageId)) -> Result<Vec<u8>> {
    let row = [Value::Text(sql), Value::Int(first.0.into()), Value::Int(last.0.into())];
    tuple::encode(&catalog_columns(), &row)
}

/// Decodes a catalog row into its statement and the two pages it records.
fn decode_entry(bytes: &[u8]) -> Result<(Statement, PageId, PageId)> {
    let row = tuple::decode(&catalog_columns(), bytes)?;
    let [Value::Text(sql), Value::Int(first), Value::Int(last)] = row.as_slice() else {
        return err("corrupted catalog row");
    };
    let Some(statement) = parse(sql)?.pop() else {
        return err(format!("corrupted catalog row: {sql}"));
    };
    Ok((statement, page_id(*first)?, page_id(*last)?))
}

fn create_index_sql(name: &str, table: &str, column: &str, unique: bool) -> String {
    let unique = if unique { "UNIQUE " } else { "" };
    format!("CREATE {unique}INDEX {name} ON {table} ({column})")
}

fn pk_index_name(table: &str) -> String {
    format!("{table}_pkey")
}

fn create_table_sql(name: &str, columns: &[Column], storage: StorageKind) -> String {
    let columns: Vec<String> = columns
        .iter()
        .map(|c| {
            let pk = if c.primary_key { " PRIMARY KEY" } else { "" };
            format!("{} {}{pk}", c.name, c.data_type)
        })
        .collect();
    let using = match storage {
        StorageKind::Heap => String::new(), // the default, as in files written before LSM existed
        other => format!(" USING {other}"),
    };
    format!("CREATE TABLE {name} ({}){using}", columns.join(", "))
}

fn page_id(raw: i64) -> Result<PageId> {
    u32::try_from(raw).map(PageId).map_err(|_| Error(format!("corrupted catalog: bad page id {raw}")))
}

fn unknown_table(name: &str) -> Error {
    Error(format!("unknown table: {name}"))
}
