//! Sequoia: a minimal relational database.
//!
//! Pipeline: SQL text -> `sql` (tokens, AST) -> `executor`, which asks `plan`
//! how to read each table -> `catalog` (tables and their indexes) -> `storage`.
//!
//! A `Database` is for one thread; a `SharedDatabase` gives each of many
//! threads a `Session` of its own (`shared`), and `server` serves one over
//! TCP, a thread for each connection.

pub mod catalog;
pub mod database;
pub mod datetime;
pub mod decimal;
pub mod error;
pub mod executor;
pub mod plan;
pub mod predicate;
pub mod result;
pub mod server;
pub mod shared;
pub mod sql;
pub mod storage;
pub mod trie;
pub mod types;

pub use database::Database;
pub use error::{Error, Result};
pub use executor::Query;
pub use result::QueryResult;
pub use shared::{Session, SharedDatabase};
pub use types::Value;
