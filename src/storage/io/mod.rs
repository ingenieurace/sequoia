//! Durable pages: `medium` (bytes in a file) -> `wal` (page writes made
//! durable in groups) -> `disk` (numbered pages, allocation, the free list).

pub mod disk;
pub mod medium;
pub mod wal;
