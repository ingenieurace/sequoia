use std::collections::{BTreeMap, HashSet};

use crate::error::{Error, Result, err};
use crate::types::{Column, Value};

pub type Row = Vec<Value>;

/// All tables in a database, keyed by name.
pub type Catalog = BTreeMap<String, Table>;

/// An in-memory table: a schema plus rows. Enforces column types and primary keys.
#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
}

impl Table {
    pub fn new(name: String, columns: Vec<Column>) -> Result<Table> {
        for (i, column) in columns.iter().enumerate() {
            if columns[..i].iter().any(|c| c.name == column.name) {
                return err(format!("duplicate column: {}", column.name));
            }
        }
        if columns.iter().filter(|c| c.primary_key).count() > 1 {
            return err("a table can have only one primary key");
        }
        Ok(Table { name, columns, rows: Vec::new() })
    }

    pub fn column_index(&self, name: &str) -> Result<usize> {
        column_index(&self.columns, name)
    }

    /// Appends rows; either all are inserted or none.
    pub fn insert(&mut self, rows: Vec<Row>) -> Result<()> {
        for row in &rows {
            self.check_types(row)?;
        }
        self.check_primary_key(self.rows.iter().chain(&rows))?;
        self.rows.extend(rows);
        Ok(())
    }

    /// Replaces all rows (used by UPDATE); either all are accepted or none.
    pub fn replace(&mut self, rows: Vec<Row>) -> Result<()> {
        for row in &rows {
            self.check_types(row)?;
        }
        self.check_primary_key(rows.iter())?;
        self.rows = rows;
        Ok(())
    }

    fn check_types(&self, row: &Row) -> Result<()> {
        for (column, value) in self.columns.iter().zip(row) {
            if !value.fits(column.data_type) {
                return err(format!(
                    "column {} expects {}, got {}",
                    column.name,
                    column.data_type,
                    value.to_sql()
                ));
            }
        }
        Ok(())
    }

    fn check_primary_key<'a>(&self, rows: impl Iterator<Item = &'a Row>) -> Result<()> {
        let Some(pk) = self.columns.iter().position(|c| c.primary_key) else {
            return Ok(());
        };
        let mut seen = HashSet::new();
        for row in rows {
            let key = &row[pk];
            if key.is_null() {
                return err(format!("primary key {} cannot be NULL", self.columns[pk].name));
            }
            if !seen.insert(key) {
                return err(format!("duplicate primary key: {}", key.to_sql()));
            }
        }
        Ok(())
    }
}

pub fn column_index(columns: &[Column], name: &str) -> Result<usize> {
    columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| Error(format!("unknown column: {name}")))
}
