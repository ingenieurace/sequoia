//! Sequoia: a minimal relational database.
//!
//! Pipeline: SQL text -> `lexer` -> `parser` (AST) -> `executor` -> `table` storage.

pub mod ast;
pub mod database;
pub mod error;
pub mod executor;
pub mod lexer;
pub mod parser;
pub mod result;
pub mod table;
pub mod types;

pub use database::Database;
pub use error::{Error, Result};
pub use result::QueryResult;
pub use types::Value;
