//! Storage engine: page-oriented on-disk layout.
//!
//! Layers, bottom up:
//!
//! ```text
//! io        medium (bytes in a file) -> wal (writes made durable in groups) -> disk (pages, free list)
//! buffer    pool (pages cached in memory), evicted by a replacer policy
//! page      tuples in a slotted page;  tuple: a row <-> bytes
//! a table's rows, stored in one of three ways:
//!   heap    tuples in a linked list of pages, updated in place
//!   lsm     a log-structured merge tree of SSTables, never updated in place
//!   column  row groups stored column by column, compressed, plus a heap as the delta store
//! btree     a B+ tree of byte-string keys, which indexes are built on
//! ```

pub mod btree;
pub mod buffer;
pub mod column;
pub mod heap;
pub mod io;
pub mod lsm;
pub mod page;
pub mod tuple;

pub use btree::BPlusTree;
pub use buffer::pool::{BufferPool, PageReadGuard, PageWriteGuard, REPLACEMENT_POLICY};
pub use buffer::replacer::{ArcReplacer, ClockReplacer, LruKReplacer, Policy, Replacer};
pub use column::store::{ColumnConfig, ColumnStore, Position};
pub use heap::{RecordId, TableHeap};
pub use io::disk::{CHECKPOINT_BYTES, DiskManager, PAGE_SIZE, PAGE_USABLE, Page, PageId};
pub use lsm::tree::{LsmConfig, LsmTree};
pub use page::{MAX_TUPLE_SIZE, SlottedPage};
