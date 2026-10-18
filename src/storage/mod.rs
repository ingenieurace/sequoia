//! Storage engine: page-oriented on-disk layout.
//!
//! Layers, bottom up: `disk` (pages in a file) -> `page` (tuples in a page)
//! -> `heap` (tuples in a table). `tuple` converts rows to and from bytes.

pub mod disk;
pub mod heap;
pub mod page;
pub mod tuple;

pub use disk::{DiskManager, PAGE_SIZE, PAGE_USABLE, Page, PageId};
pub use heap::{RecordId, TableHeap};
pub use page::{MAX_TUPLE_SIZE, SlottedPage};
