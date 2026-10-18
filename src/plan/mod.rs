//! Query planning: `filter` works out what a statement needs from its table,
//! and `access` picks how to read it.

pub mod access;
pub mod filter;
