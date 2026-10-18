//! Storage engine: page-oriented on-disk layout.
//!
//! Layers, bottom up: `medium` (bytes in a file) -> `wal` (page writes made
//! durable in groups) -> `disk` (pages in a file) -> `buffer` (pages cached in
//! memory, evicted by a `replacer` policy) -> `page` (tuples in a page) ->
//! a table's rows, stored in one of two ways:
//! - `heap`: tuples in a linked list of pages, updated in place.
//! - `lsm`: a log-structured merge tree of `sstable`s, never updated in place.
//! - `column`: row groups stored column by column, compressed by `encoding`,
//!   with a heap as the delta store for new rows.
//!
//! `btree` is a B+ tree of byte-string keys, which indexes are built on.
//! `tuple` converts rows to and from bytes.

mod bloom;
pub mod btree;
pub mod buffer;
pub mod column;
pub mod disk;
pub mod encoding;
pub mod heap;
pub mod lsm;
pub mod medium;
pub mod page;
pub mod replacer;
pub mod sstable;
pub mod tuple;
pub mod wal;

pub use btree::BPlusTree;
pub use buffer::{BufferPool, PageReadGuard, PageWriteGuard, REPLACEMENT_POLICY};
pub use column::{ColumnConfig, ColumnStore, Position, Predicate};
pub use encoding::Compare;
pub use disk::{CHECKPOINT_BYTES, DiskManager, PAGE_SIZE, PAGE_USABLE, Page, PageId};
pub use heap::{RecordId, TableHeap};
pub use lsm::{LsmConfig, LsmTree};
pub use page::{MAX_TUPLE_SIZE, SlottedPage};
pub use replacer::{ArcReplacer, ClockReplacer, LruKReplacer, Policy, Replacer};
