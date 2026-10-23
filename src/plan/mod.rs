//! Query planning: `filter` works out what a statement needs from its table,
//! `access` picks how to read it, and `query` turns a SELECT into a tree of
//! steps for the executor to run, which `tree` draws for PLANTREE.
//! `selectivity` estimates what share of rows conditions keep, from the
//! statistics ANALYZE gathers if there are any, and `order` picks the order
//! in which a query joins its tables.
//! `rewrite` simplifies expressions once they are bound, before the rest.

pub mod access;
pub mod filter;
pub mod order;
pub mod query;
pub mod rewrite;
pub mod selectivity;
pub mod tree;
