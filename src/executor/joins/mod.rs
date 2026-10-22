//! Joins: for each left row, the right rows that may match it, paired with
//! it where the join's condition holds.
//!
//! ```text
//! left row --> candidates on the right --> left ++ right, if the condition holds
//!                  hash:  the right rows with equal keys, from a hash table built once
//!                  index: the right rows an index lookup finds, per left row
//!
//! block of left rows --> every right row, read again per block --> left ++ right, if the condition holds
//!
//! left, sorted by keys  --+
//!                         +--> merge: each left row with the right rows of equal keys
//! right, sorted by keys --+
//! ```
//!
//! A LEFT JOIN also returns each left row that no right row matched, with a
//! NULL for every right column. The right side of a hash join is read into
//! a hash table when the first left row is, within a memory budget
//! ([`HASH_MEMORY`] by default) past which it is partitioned and partly
//! written out (see `hash.rs`): then the left rows of the partitions
//! written out come after the others, out of order. A hash join that must
//! keep its left side's order then sorts its rows back into it.
//!
//! With more than one join worker set, a hash join that need not keep its
//! left side's order runs in parallel instead (see `parallel.rs`): both
//! sides are partitioned by a hash of their keys, then worker threads join
//! the pairs of partitions side by side, each as a hash join of its own.
//!
//! A block nested loop join holds left rows instead: a block of as many as
//! fit in its memory budget ([`JOIN_MEMORY`] by default). For each block it
//! reads the right side again from the start and pairs each right row with
//! every row of the block, so it reads the right side once per block rather
//! than once per left row, and never holds more than one block. Its rows come
//! out a block at a time, by right row within a block; a LEFT JOIN's
//! unmatched rows of a block come after its pairs.
//!
//! A sort-merge join sorts both sides by their keys with the sort ORDER BY
//! uses, which writes runs to a temporary file past its budget, so besides
//! those budgets it holds only the right rows of one key at a time. Its rows come out in
//! the left side's sorted order, which is why the planner picks it: for an
//! ORDER BY that starts with its keys, the sort is already done.
//!
//! Each method has a file of its own: `probe.rs` runs the joins that find a
//! left row's right rows by its keys, through `hash.rs` or `index.rs`, and
//! `parallel.rs` the parallel grace hash join, through them in each worker;
//! `block.rs` is the block nested loop join, `merge.rs` the sort-merge join.
//! `naive.rs`, the naive nested loop join, is not one of them: the planner
//! never picks it on its own, but runs it when it is forced to, for the joins
//! benchmark to measure the others against it.

mod block;
mod hash;
mod index;
mod merge;
mod naive;
mod parallel;
mod probe;
mod scan_index_join;
mod scan_join;

pub use block::BlockJoin;
pub use index::Lookup;
pub use merge::{MergeJoin, MergeSort};
pub use naive::NaiveJoin;
pub use parallel::ParallelHashJoin;
pub use probe::{Join, Key, Right};
pub(crate) use scan_index_join::scan_index_join;
pub(crate) use scan_join::scan_join;

/// Default memory a block nested loop join may hold left rows in: 4 MB.
pub const JOIN_MEMORY: usize = 4 << 20;
/// Default memory a hash join may hold right rows in, and a hash
/// aggregation its groups, before they write partitions out: 16 MB.
pub const HASH_MEMORY: usize = 16 << 20;
