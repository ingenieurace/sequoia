//! Public entry point: owns the catalog and the disk it lives on.
//!
//! After every statement that changes data, the catalog is saved, dirty pages
//! are written back from the buffer pool, and the file is synced, so a
//! statement that returns successfully is on disk.
//! Without a WAL, a crash in the middle of a statement may leave it half applied.

use std::path::Path;
use std::sync::Arc;

use crate::ast::Statement;
use crate::catalog::Catalog;
use crate::error::Result;
use crate::executor;
use crate::parser::parse;
use crate::result::QueryResult;
use crate::storage::{BufferPool, DiskManager};

/// Frames in the buffer pool: 256 x 4 KB = 1 MB of cached pages.
const BUFFER_POOL_FRAMES: usize = 256;

pub struct Database {
    catalog: Catalog,
}

impl Database {
    /// Creates a database that lives only in memory.
    pub fn in_memory() -> Self {
        Self::with_disk(DiskManager::in_memory()).expect("in-memory storage cannot fail")
    }

    /// Opens (or creates) a database stored in the given file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_disk(DiskManager::open(path)?)
    }

    fn with_disk(disk: DiskManager) -> Result<Self> {
        let pool = BufferPool::new(Arc::new(disk), BUFFER_POOL_FRAMES);
        Ok(Database { catalog: Catalog::open(Arc::new(pool))? })
    }

    /// Parses and runs every statement in `sql`, stopping at the first error.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        parse(sql)?.into_iter().map(|stmt| self.run(stmt)).collect()
    }

    /// Runs a single parsed statement.
    pub fn run(&mut self, statement: Statement) -> Result<QueryResult> {
        let modifies = !matches!(statement, Statement::Select { .. });
        let result = executor::execute(&mut self.catalog, statement);
        if modifies {
            self.catalog.save()?;
        }
        result
    }

    pub fn table_names(&self) -> Vec<&str> {
        self.catalog.table_names()
    }
}
