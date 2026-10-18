//! Public entry point: owns the catalog and persists it to disk.
//!
//! Persistence is deliberately simple: after every change the whole database
//! is written out as a SQL script (CREATE TABLE + INSERT statements), and
//! opening a file replays that script.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use crate::ast::Statement;
use crate::error::Result;
use crate::executor;
use crate::parser::parse;
use crate::result::QueryResult;
use crate::table::Catalog;
use crate::types::Value;

#[derive(Default)]
pub struct Database {
    catalog: Catalog,
    /// File to persist to; `None` means in-memory only.
    path: Option<PathBuf>,
}

impl Database {
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Opens (or creates) a database stored in the given file.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut db = Self::in_memory();
        if path.exists() {
            db.execute(&fs::read_to_string(&path)?)?;
        }
        db.path = Some(path);
        Ok(db)
    }

    /// Parses and runs every statement in `sql`, stopping at the first error.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        parse(sql)?.into_iter().map(|stmt| self.run(stmt)).collect()
    }

    /// Runs a single parsed statement.
    pub fn run(&mut self, statement: Statement) -> Result<QueryResult> {
        let modifies = !matches!(statement, Statement::Select { .. });
        let result = executor::execute(&mut self.catalog, statement)?;
        if modifies {
            self.save()?;
        }
        Ok(result)
    }

    pub fn table_names(&self) -> Vec<&str> {
        self.catalog.keys().map(String::as_str).collect()
    }

    fn save(&self) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        // Write to a temp file first so a crash never leaves a half-written database.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, self.dump())?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Serializes the whole database as a SQL script.
    fn dump(&self) -> String {
        let mut out = String::new();
        for table in self.catalog.values() {
            let columns: Vec<String> = table
                .columns
                .iter()
                .map(|c| {
                    let pk = if c.primary_key { " PRIMARY KEY" } else { "" };
                    format!("{} {}{pk}", c.name, c.data_type)
                })
                .collect();
            let _ = writeln!(out, "CREATE TABLE {} ({});", table.name, columns.join(", "));
            for row in &table.rows {
                let values: Vec<String> = row.iter().map(Value::to_sql).collect();
                let _ = writeln!(out, "INSERT INTO {} VALUES ({});", table.name, values.join(", "));
            }
        }
        out
    }
}
