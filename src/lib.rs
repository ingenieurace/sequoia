//! Sequoia: a minimal relational database.
//!
//! Pipeline: SQL text -> `sql` (tokens, AST) -> `executor`, which asks `plan`
//! how to read each table -> `catalog` (tables and their indexes) -> `storage`.

pub mod catalog;
pub mod database;
pub mod error;
pub mod executor;
pub mod plan;
pub mod predicate;
pub mod result;
pub mod sql;
pub mod storage;
pub mod types;

pub use database::Database;
pub use executor::Query;
pub use error::{Error, Result};
pub use result::QueryResult;
pub use types::Value;
