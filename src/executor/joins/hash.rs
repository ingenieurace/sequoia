//! The right side of a hash join: its rows in a hash table by their keys,
//! within a memory budget, and past it partitioned, partly on disk. This is
//! a hybrid hash join.
//!
//! The table is built from the right rows when the first left row comes.
//! While they fit in the budget, it is one table, as a hash join that never
//! spills has. When they outgrow it, the table splits into [`FANOUT`]
//! partitions by a hash of the keys (see `spill.rs`), and the largest are
//! written to temporary files, one by one, until the rest fit; a right row
//! of a partition written out is written after them.
//!
//! ```text
//! right rows --> partition 0   in memory     <-- left rows of partition 0 probe it at once
//!                partition 1   right rows on disk     left rows of partition 1 go to disk too
//!                ...
//!                partition 15  in memory
//! ```
//!
//! A left row then finds its matches at once if its partition is in
//! memory, and is written out otherwise, to be joined after the left side
//! runs out: then each partition written out is joined in turn, its right
//! rows read into a table (split again, by the next bits of the hash, if
//! they still do not fit) and its left rows read back to probe it. So the
//! right rows held never take much more than the budget, but left rows come
//! out of order: those of partitions in memory first.
//!
//! Rows with a NULL key match nothing, and are never written out.

use std::collections::HashMap;

use super::Key;
use crate::catalog::table::Row;
use crate::error::Result;
use crate::executor::operator::Rows;
use crate::executor::sort::value_size;
use crate::executor::spill::{FANOUT, MAX_LEVEL, RowFile, RowReader, partition};
use crate::sql::eval::Field;
use crate::types::Value;

pub struct HashSide<'a> {
    /// The right rows, until the table is built from them.
    source: Option<Rows<'a>>,
    left_keys: Vec<Key>,
    right_keys: Vec<Key>,
    left_fields: Vec<Field>,
    right_fields: Vec<Field>,
    budget: usize,
    table: Table,
    /// Partitions written out whose turn has not come.
    pending: Vec<Pending>,
    /// The left rows of the partition being joined, read back.
    deferred: Option<RowReader>,
    /// Whether any partition was written out.
    spilled: bool,
    /// The current left row's keys, and the bucket of right rows they found.
    probe: Vec<Value>,
    bucket: Option<(usize, usize)>,
    scratch: Vec<u8>,
}

/// A partition written out, to join later; split at `level` if it must be.
struct Pending {
    right: RowFile,
    left: RowFile,
    level: u32,
}

/// Right rows by their keys: in one part, or once they outgrew the budget,
/// in [`FANOUT`] partitions, by the hash bits of `level`.
#[derive(Default)]
struct Table {
    level: u32,
    parts: Vec<Part>,
    /// Bytes of the parts in memory.
    bytes: usize,
}

enum Part {
    Memory(Buckets),
    /// Written out: the right rows of the partition, then those of its
    /// left rows that came to probe it.
    Spilled { right: RowFile, left: RowFile },
}

/// Rows with equal keys, together.
#[derive(Default)]
struct Buckets {
    /// Each key's position in `rows`.
    index: HashMap<Vec<Value>, usize>,
    rows: Vec<Vec<Row>>,
    bytes: usize,
}

impl<'a> HashSide<'a> {
    pub fn new(
        source: Rows<'a>,
        left_keys: Vec<Key>,
        right_keys: Vec<Key>,
        left_fields: Vec<Field>,
        right_fields: Vec<Field>,
        budget: usize,
    ) -> Self {
        HashSide {
            source: Some(source),
            left_keys,
            right_keys,
            left_fields,
            right_fields,
            budget,
            table: Table::default(),
            pending: Vec::new(),
            deferred: None,
            spilled: false,
            probe: Vec::new(),
            bucket: None,
            scratch: Vec::new(),
        }
    }

    /// Whether some of the right rows were written out, so that some left
    /// rows will come back after the left side runs out.
    pub fn spilled(&self) -> bool {
        self.spilled
    }

    /// Finds the right rows that may match `left`, and returns how many
    /// there are, or `None` if `left` was written out with its partition, to
    /// come back from [`HashSide::deferred`].
    pub fn find(&mut self, left: &Row) -> Result<Option<usize>> {
        if let Some(source) = self.source.take() {
            self.build(source, 0)?;
        }
        Key::eval_all(&self.left_keys, &self.left_fields, left, &mut self.probe)?;
        self.bucket = None;
        if self.probe.iter().any(Value::is_null) {
            return Ok(Some(0));
        }
        let p = self.table.part_of(&self.probe, &mut self.scratch);
        match &mut self.table.parts[p] {
            Part::Memory(buckets) => {
                self.bucket = buckets.index.get(self.probe.as_slice()).map(|&b| (p, b));
                Ok(Some(self.bucket.map_or(0, |(_, b)| buckets.rows[b].len())))
            }
            Part::Spilled { left: rows, .. } => {
                rows.push(left)?;
                Ok(None)
            }
        }
    }

    /// The `i`th right row [`find`](Self::find) found.
    pub fn candidate(&self, i: usize) -> &Row {
        let (p, b) = self.bucket.expect("found a bucket");
        match &self.table.parts[p] {
            Part::Memory(buckets) => &buckets.rows[b][i],
            Part::Spilled { .. } => unreachable!("a bucket is in memory"),
        }
    }

    /// Once the left side has run out, the left rows written out, one
    /// partition after another, each partition's right rows read into the
    /// table before its first left row comes: to give to `find` again.
    pub fn deferred(&mut self) -> Result<Option<Row>> {
        loop {
            if let Some(rows) = &mut self.deferred {
                match rows.next() {
                    Some(row) => return row.map(Some),
                    None => self.deferred = None,
                }
            }
            // Every left row of the table has come: its partitions written
            // out are next, split by the next bits if need be.
            let level = self.table.level + 1;
            for part in std::mem::take(&mut self.table.parts) {
                if let Part::Spilled { right, left } = part
                    && left.rows() > 0
                {
                    self.pending.push(Pending { right, left, level });
                }
            }
            let Some(Pending { right, left, level }) = self.pending.pop() else { return Ok(None) };
            self.build(Box::new(right.read()), level)?;
            self.deferred = Some(left.read());
        }
    }

    /// Fills the table with `rows`, partitioning them at `level` if they outgrow the budget.
    fn build(&mut self, rows: Rows, level: u32) -> Result<()> {
        self.table = Table { level, parts: vec![Part::Memory(Buckets::default())], bytes: 0 };
        let mut keys = Vec::new();
        for row in rows {
            let row = row?;
            Key::eval_all(&self.right_keys, &self.right_fields, &row, &mut keys)?;
            if keys.iter().any(Value::is_null) {
                continue;
            }
            let p = self.table.part_of(&keys, &mut self.scratch);
            match &mut self.table.parts[p] {
                Part::Spilled { right, .. } => right.push(&row)?,
                Part::Memory(buckets) => {
                    self.table.bytes += buckets.add(&keys, row);
                    if self.table.bytes > self.budget {
                        self.spilled |= self.table.shrink(self.budget, &mut self.scratch)?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Table {
    /// The part that rows with these keys belong in.
    fn part_of(&self, keys: &[Value], scratch: &mut Vec<u8>) -> usize {
        match self.parts.len() {
            1 => 0,
            _ => partition(keys, self.level, scratch),
        }
    }

    /// Brings the bytes in memory under `budget`: splits the table into
    /// partitions if it is not yet (unless there are no bits left to split
    /// by, or all of its rows have one key), then writes out the largest
    /// partitions in memory until the rest fit. Returns whether it wrote
    /// any out.
    fn shrink(&mut self, budget: usize, scratch: &mut Vec<u8>) -> Result<bool> {
        if self.parts.len() == 1 {
            let Part::Memory(all) = &mut self.parts[0] else { unreachable!("one part is in memory") };
            if self.level > MAX_LEVEL || all.index.len() < 2 {
                return Ok(false);
            }
            let all = std::mem::take(all);
            let mut parts: Vec<Buckets> = (0..FANOUT).map(|_| Buckets::default()).collect();
            let mut rows: Vec<Option<Vec<Row>>> = all.rows.into_iter().map(Some).collect();
            for (key, b) in all.index {
                let part = &mut parts[partition(&key, self.level, scratch)];
                let rows = rows[b].take().expect("one key per bucket");
                part.bytes += key_size(&key) + rows.iter().map(|row| row_size(row)).sum::<usize>();
                part.index.insert(key, part.rows.len());
                part.rows.push(rows);
            }
            self.parts = parts.into_iter().map(Part::Memory).collect();
        }

        let mut spilled = false;
        while self.bytes > budget {
            let largest = self.parts.iter().enumerate().filter_map(|(i, part)| match part {
                Part::Memory(buckets) if buckets.bytes > 0 => Some((buckets.bytes, i)),
                _ => None,
            });
            let Some((bytes, i)) = largest.max() else { break };
            let Part::Memory(buckets) = std::mem::replace(&mut self.parts[i], Part::Memory(Buckets::default())) else {
                unreachable!("chosen in memory")
            };
            let mut right = RowFile::new();
            for row in buckets.rows.iter().flatten() {
                right.push(row)?;
            }
            self.parts[i] = Part::Spilled { right, left: RowFile::new() };
            self.bytes -= bytes;
            spilled = true;
        }
        Ok(spilled)
    }
}

impl Buckets {
    /// Adds a row with these keys, and returns the bytes it took.
    fn add(&mut self, keys: &[Value], row: Row) -> usize {
        let mut size = row_size(&row);
        match self.index.get(keys) {
            Some(&b) => self.rows[b].push(row),
            None => {
                size += key_size(keys);
                self.index.insert(keys.to_vec(), self.rows.len());
                self.rows.push(vec![row]);
            }
        }
        self.bytes += size;
        size
    }
}

/// A rough count of the bytes a row takes in a bucket.
fn row_size(row: &[Value]) -> usize {
    24 + row.iter().map(value_size).sum::<usize>()
}

/// A rough count of the bytes a key takes in the index, with its bucket.
fn key_size(keys: &[Value]) -> usize {
    80 + keys.iter().map(value_size).sum::<usize>()
}
