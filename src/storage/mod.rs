//! Storage engine: page-oriented on-disk layout.

pub mod disk;
pub mod page;

pub use disk::{DiskManager, PAGE_SIZE, PAGE_USABLE, Page, PageId};
pub use page::{MAX_TUPLE_SIZE, SlottedPage};
