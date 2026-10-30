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
//! version   the header of each stored row: which transactions wrote and deleted it
//! btree     a B+ tree of byte-string keys, which indexes are built on
//! hash      a hash file of byte-string entries: extendible or linear hashing, for indexes USING HASH
//! ```

pub mod btree;
pub mod buffer;
pub mod column;
pub mod hash;
pub mod heap;
pub mod io;
pub mod lsm;
pub mod page;
pub mod sort_key;
pub mod tuple;
pub mod version;

pub use btree::BPlusTree;
pub use buffer::pool::{BufferPool, PageReadGuard, PageWriteGuard, REPLACEMENT_POLICY, Stats};
pub use buffer::replacer::{ArcReplacer, ClockReplacer, LruKReplacer, Policy, Replacer};
pub use column::store::{ColumnChunk, ColumnConfig, ColumnStore, Position};
pub use hash::{HASH_SCHEME, HashFile, HashScheme};
pub use heap::{RecordId, SharedScan, TableHeap};
pub use io::disk::{CHECKPOINT_BYTES, DiskManager, PAGE_SIZE, PAGE_USABLE, Page, PageId};
pub use lsm::tree::{LsmConfig, LsmTree};
pub use page::{MAX_TUPLE_SIZE, SlottedPage};
