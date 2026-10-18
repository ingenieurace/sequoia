//! Query planning: `filter` works out what a statement needs from its table,
//! `access` picks how to read it, and `query` turns a SELECT into a tree of
//! steps for the executor to run.

pub mod access;
pub mod filter;
pub mod query;
