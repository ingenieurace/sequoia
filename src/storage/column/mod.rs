//! Column store: the `store` (PAX row groups and a delta store) and the
//! compressed `encoding`s of its column chunks.

pub mod encoding;
pub mod store;
