//! Storage engine: page-oriented on-disk layout.

pub mod disk;

pub use disk::{DiskManager, PAGE_SIZE, PAGE_USABLE, Page, PageId};
