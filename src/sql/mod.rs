//! The SQL language: `lexer` (text -> tokens), `parser` (tokens -> `ast`),
//! and `eval`, what an expression means for a row.

pub mod ast;
pub mod eval;
pub mod lexer;
pub mod parser;

pub use parser::parse;
