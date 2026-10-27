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
//! [`INDEX_FORMAT`]. In files from before indexes had several keys, included
//! columns and NULL keys, the second was the root page again: such an index
//! is built again, in the format of today, when the catalog is opened.
//!
//! A heap table with a primary key gets a unique index on it, `<table>_pkey`,
//! when it is created; it is dropped with the table and not on its own.
//!
//! An analyzed table has one more row, its statistics (see `stats.rs`):
//! `sql` is the table's name and the statistics as text, the first page is
//! 0 and the second is [`STATS_FORMAT`]. ANALYZE replaces it.

use std::collections::BTreeMap;
use std::sync::Arc;

pub mod index;
pub mod stats;
pub mod table;

use crate::error::{Error, Result, err};
use crate::sql::ast::{ColumnRef, CreateIndex, Expr, IndexKind, Statement};
use crate::sql::parse;
use crate::storage::{BPlusTree, BufferPool, MAX_TUPLE_SIZE, PageId, RecordId, TableHeap, tuple};
use crate::types::{Column, DataType, StorageKind, Value};
use stats::TableStats;
use table::{Table, primary_key, validate_schema};

/// The second page of an index's catalog row: not a page, but a mark of the
/// format of its entries.
const INDEX_FORMAT: PageId = PageId(u32::MAX);

/// The second page of a table's statistics row: not a page either.
const STATS_FORMAT: PageId = PageId(u32::MAX - 1);

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
    /// Location of the table's statistics row, once it is analyzed.
    stats_rid: Option<RecordId>,
}

impl Catalog {
    /// Loads the catalog, creating an empty one in a new database.
    pub fn open(pool: Arc<BufferPool>) -> Result<Self> {
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

        let mut tables = BTreeMap::new();
        let mut index_rows = Vec::new();
        let mut stats_rows = Vec::new();
        for item in heap.scan() {
            let (rid, bytes) = item?;
            let (sql, first, last) = decode_row(&bytes)?;
            if last == STATS_FORMAT {
                stats_rows.push((rid, sql));
                continue;
            }
            match decode_entry(&sql, first, last)? {
                (
                    Statement::CreateTable {
                        name,
                        columns,
                        storage,
                    },
                    first,
                    last,
                ) => {
                    let table =
                        Table::open(pool.clone(), name.clone(), columns, storage, (first, last))?;
                    tables.insert(
                        name,
                        Entry {
                            table,
                            rid,
                            saved_last_page: last,
                            stats_rid: None,
                        },
                    );
                }
                (Statement::CreateIndex(definition), root, format) => {
                    index_rows.push((rid, definition, root, format))
                }
                (statement, ..) => return err(format!("corrupted catalog row: {statement:?}")),
            }
        }

        // Indexes are opened once every table is, as their rows may come first.
        let mut indexes = BTreeMap::new();
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
                // From before entries held NULL keys: built again.
                BPlusTree::open(pool.clone(), root).destroy()?;
                entry.table.create_index(&definition)?;
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
            heap,
            tables,
            indexes,
        })
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
        storage: StorageKind,
    ) -> Result<()> {
        if self.tables.contains_key(&name) {
            return err(format!("table already exists: {name}"));
        }
        validate_schema(&columns)?;
        let pk_index = match (storage, primary_key(&columns)) {
            (StorageKind::Heap, Some(pk)) => Some(CreateIndex {
                name: pk_index_name(&name),
                table: name.clone(),
                using: IndexKind::BTree,
                keys: vec![Expr::Column(ColumnRef::bare(&columns[pk].name))],
                include: Vec::new(),
                predicate: None,
                unique: true,
            }),
            _ => None,
        };
        if let Some(index) = &pk_index
            && self.indexes.contains_key(&index.name)
        {
            return err(format!("index already exists: {}", index.name));
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
        self.tables.insert(
            name.clone(),
            Entry {
                table,
                rid,
                saved_last_page,
                stats_rid: None,
            },
        );

        if let Some(index) = pk_index
            && let Err(e) = self.create_index(index)
        {
            self.drop_table(&name)?;
            return Err(e);
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

    /// Computes the statistics of a table (see `stats.rs`) and records
    /// them in place of those it had, halving their histograms and lists
    /// of common values until they fit in a catalog row.
    pub fn analyze(&mut self, name: &str) -> Result<()> {
        let entry = self
            .tables
            .get_mut(name)
            .ok_or_else(|| unknown_table(name))?;
        let mut stats = TableStats::collect(&entry.table)?;
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

    /// Builds an index on a table from its current rows.
    pub fn create_index(&mut self, definition: CreateIndex) -> Result<()> {
        let (name, table) = (definition.name.clone(), definition.table.clone());
        if self.indexes.contains_key(&name) {
            return err(format!("index already exists: {name}"));
        }
        let entry = self
            .tables
            .get_mut(&table)
            .ok_or_else(|| unknown_table(&table))?;
        entry.table.create_index(&definition)?;

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
        let index = self.indexes.remove(name).expect("found above");
        self.heap.delete(index.rid)?;
        self.tables
            .get_mut(&index.table)
            .expect("an index's table exists")
            .table
            .drop_index(name)
    }

    /// Copies the committed pages from the write-ahead log into the database file.
    pub fn checkpoint(&self) -> Result<()> {
        self.pool.disk().checkpoint()
    }

    /// Records heaps that grew since the last save, then writes every dirty
    /// page back to the disk, for it to commit or roll back.
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
        self.pool.flush_all()
    }
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
        create_table_sql(&table.name, &table.columns, table.storage_kind()),
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
    use crate::storage::DiskManager;

    fn create_index(catalog: &mut Catalog, sql: &str) {
        let Some(Statement::CreateIndex(definition)) = parse(sql).unwrap().pop() else {
            unreachable!()
        };
        catalog.create_index(definition).unwrap();
    }

    #[test]
    fn indexes_from_before_null_keys_are_built_again_when_opened() {
        let pool = Arc::new(BufferPool::new(Arc::new(DiskManager::in_memory()), 64));
        let mut catalog = Catalog::open(pool.clone()).unwrap();
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
        catalog
            .create_table("t".into(), columns, StorageKind::Heap)
            .unwrap();
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
        catalog
            .get_mut("t")
            .unwrap()
            .insert(rows.collect())
            .unwrap();
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

        let catalog = Catalog::open(pool.clone()).unwrap();
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
        let reopened = Catalog::open(pool.clone()).unwrap();
        assert_eq!(
            reopened.get("t").unwrap().indexes()[1].root_page(),
            index.root_page()
        );
    }
}
