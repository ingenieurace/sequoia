//! System catalog: which tables exist, their schemas, and where their rows live.
//!
//! The catalog is itself a table heap whose first page is the DiskManager's
//! root page. Each table is one catalog row:
//!
//! ```text
//! (sql TEXT, first_page INT, last_page INT)
//! ```
//!
//! where `sql` is the table's CREATE TABLE statement, parsed again on open
//! (the same idea as SQLite's `sqlite_schema` table). Its `USING` clause
//! says how the rows are stored: for a heap the two pages are its first and
//! last page, for an LSM tree both are its manifest page.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::ast::Statement;
use crate::error::{Error, Result, err};
use crate::parser::parse;
use crate::storage::{BufferPool, PageId, RecordId, TableHeap, tuple};
use crate::table::{Table, validate_schema};
use crate::types::{Column, DataType, StorageKind, Value};

pub struct Catalog {
    pool: Arc<BufferPool>,
    /// Holds one row per table.
    heap: TableHeap,
    tables: BTreeMap<String, Entry>,
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
        for item in heap.scan() {
            let (rid, bytes) = item?;
            let (table, saved_last_page) = load_entry(&pool, &bytes)?;
            tables.insert(table.name.clone(), Entry { table, rid, saved_last_page });
        }
        Ok(Catalog { pool, heap, tables })
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

        let table = Table::create(self.pool.clone(), name.clone(), columns, storage)?;
        let rid = match encode_entry(&table).and_then(|bytes| self.heap.insert(&bytes)) {
            Ok(rid) => rid,
            Err(e) => {
                table.destroy()?;
                return Err(e);
            }
        };
        let saved_last_page = table.root_pages().1;
        self.tables.insert(name, Entry { table, rid, saved_last_page });
        Ok(())
    }

    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        let entry = self.tables.remove(name).ok_or_else(|| unknown_table(name))?;
        // Remove the catalog row before freeing pages: a crash in between leaks
        // pages instead of leaving the catalog pointing at freed ones.
        self.heap.delete(entry.rid)?;
        self.pool.flush_page(entry.rid.page_id)?;
        entry.table.destroy()
    }

    /// Records heaps that grew since the last save, then writes every dirty
    /// page back and syncs the file.
    pub fn save(&mut self) -> Result<()> {
        for entry in self.tables.values_mut() {
            let last_page = entry.table.root_pages().1;
            if last_page != entry.saved_last_page {
                // Same length as before, so this rewrites one page in place.
                if !self.heap.update(entry.rid, &encode_entry(&entry.table)?)? {
                    return err("cannot update catalog row");
                }
                entry.saved_last_page = last_page;
            }
        }
        self.pool.flush_all()?;
        self.pool.disk().sync()
    }
}

fn catalog_columns() -> Vec<Column> {
    [("sql", DataType::Text), ("first_page", DataType::Int), ("last_page", DataType::Int)]
        .map(|(name, data_type)| Column { name: name.into(), data_type, primary_key: false })
        .into()
}

fn encode_entry(table: &Table) -> Result<Vec<u8>> {
    let (first, last) = table.root_pages();
    let row = [
        Value::Text(create_table_sql(&table.name, &table.columns, table.storage_kind())),
        Value::Int(first.0.into()),
        Value::Int(last.0.into()),
    ];
    tuple::encode(&catalog_columns(), &row)
}

/// Decodes a catalog row into a table and the last page it records.
fn load_entry(pool: &Arc<BufferPool>, bytes: &[u8]) -> Result<(Table, PageId)> {
    let row = tuple::decode(&catalog_columns(), bytes)?;
    let [Value::Text(sql), Value::Int(first), Value::Int(last)] = row.as_slice() else {
        return err("corrupted catalog row");
    };
    let Some(Statement::CreateTable { name, columns, storage }) = parse(sql)?.pop() else {
        return err(format!("corrupted catalog row: {sql}"));
    };
    let (first, last) = (page_id(*first)?, page_id(*last)?);
    Ok((Table::open(pool.clone(), name, columns, storage, (first, last))?, last))
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
