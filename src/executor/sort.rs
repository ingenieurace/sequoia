//! Sorting for ORDER BY within a memory budget: an external merge sort, and
//! a top-N heap when there is a LIMIT.
//!
//! ```text
//! rows --> buffer (in memory) --full--> sorted run --+
//!          buffer             --full--> sorted run --+--> merge 16 at a time --> ... --> one sorted stream
//!          buffer             --end---> sorted run --+
//! ```
//!
//! Rows are buffered until they take more than the budget, then sorted and
//! written out as a run to a temporary file. At the end, the runs are merged:
//! a heap holds the next row of each run and hands out the smallest. If there
//! are more than [`FAN_IN`] runs, groups of them are first merged into longer
//! runs, pass after pass, so that each merge reads only a few runs at once.
//! When nothing was written out, the buffer is just sorted in memory.
//!
//! With `LIMIT n`, only the first `n` rows can be in the result, so the
//! buffer is a heap of at most `n` rows whose top is the last of them: a new
//! row either replaces it or is dropped at once. Sorting `m` rows then takes
//! O(m log n) time and memory for `n` rows. A run, too, keeps only its first
//! `n` rows, and the merge stops after `n`.
//!
//! The sort is stable: each row is numbered as it arrives, and rows with
//! equal keys come out in that order.
//!
//! The temporary file is removed as soon as it is created, so it disappears
//! when the sort ends, even if the process does not. It does not go through
//! the write-ahead log: nothing in it needs to survive a crash.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fs::OpenOptions;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::catalog::table::Row;
use crate::error::{Result, err};
use crate::storage::io::medium::{FileMedium, Medium};
use crate::types::Value;

/// Default memory a sort may buffer rows in before writing runs out: 4 MB.
pub const SORT_MEMORY: usize = 4 << 20;
/// Runs a merge reads at once.
pub const FAN_IN: usize = 16;
/// Bytes read or written to the temporary file at a time.
const IO_CHUNK: usize = 64 << 10;

pub struct Sorter {
    /// For each sort key, whether it is descending.
    desc: Arc<[bool]>,
    limit: Option<usize>,
    budget: usize,
    fan_in: usize,
    buffer: Buffer,
    /// Estimated bytes held by `buffer`.
    bytes: usize,
    /// Number of the next row pushed.
    seq: u64,
    /// Created when the first run is written out.
    spill: Option<Spill>,
}

enum Buffer {
    /// Every row, in arrival order.
    All(Vec<Keyed>),
    /// The best rows so far, at most `limit` of them; the top is the worst.
    Top(BinaryHeap<Keyed>),
}

/// A row with its sort keys and arrival number.
struct Keyed {
    keys: Vec<Value>,
    seq: u64,
    row: Row,
    desc: Arc<[bool]>,
}

impl Sorter {
    /// A sorter for rows ordered by keys with these directions, keeping the
    /// first `limit` rows if given, and buffering about `budget` bytes.
    pub fn new(desc: Vec<bool>, limit: Option<usize>, budget: usize) -> Self {
        Self::with_fan_in(desc, limit, budget, FAN_IN)
    }

    pub fn with_fan_in(desc: Vec<bool>, limit: Option<usize>, budget: usize, fan_in: usize) -> Self {
        assert!(fan_in >= 2, "a merge needs at least two runs");
        let buffer = match limit {
            Some(_) => Buffer::Top(BinaryHeap::new()),
            None => Buffer::All(Vec::new()),
        };
        Sorter { desc: desc.into(), limit, budget, fan_in, buffer, bytes: 0, seq: 0, spill: None }
    }

    /// Adds a row with its sort keys, one per direction given to `new`.
    pub fn push(&mut self, keys: Vec<Value>, row: Row) -> Result<()> {
        assert_eq!(keys.len(), self.desc.len());
        let item = Keyed { keys, seq: self.seq, row, desc: self.desc.clone() };
        self.seq += 1;
        match &mut self.buffer {
            Buffer::All(rows) => {
                self.bytes += item.size();
                rows.push(item);
            }
            Buffer::Top(heap) => {
                let limit = self.limit.expect("a heap has a limit");
                if limit == 0 {
                    return Ok(());
                }
                if heap.len() == limit {
                    // Arrives later than every row kept, so it loses ties.
                    if item >= *heap.peek().expect("not empty") {
                        return Ok(());
                    }
                    self.bytes -= heap.pop().expect("not empty").size();
                }
                self.bytes += item.size();
                heap.push(item);
            }
        }
        if self.bytes > self.budget {
            self.write_run()?;
        }
        Ok(())
    }

    /// Whether any run was written out.
    pub fn spilled(&self) -> bool {
        self.spill.is_some()
    }

    /// Returns the rows in order.
    pub fn finish(mut self) -> Result<Sorted> {
        if self.spill.is_none() {
            return Ok(Sorted { inner: Inner::Memory(self.take_sorted().into_iter()) });
        }
        self.write_run()?;
        let mut spill = self.spill.take().expect("spilled");
        while spill.runs.len() > self.fan_in {
            let runs = std::mem::take(&mut spill.runs);
            for group in runs.chunks(self.fan_in) {
                let merged = Merge::new(&spill.file, group, &self.desc, self.limit)?;
                let run = spill.write(merged)?;
                spill.runs.push(run);
            }
        }
        let merge = Merge::new(&spill.file, &spill.runs, &self.desc, self.limit)?;
        Ok(Sorted { inner: Inner::Merge(merge) })
    }

    /// Empties the buffer into a sorted vector, keeping only the first `limit` rows.
    fn take_sorted(&mut self) -> Vec<Keyed> {
        self.bytes = 0;
        let mut rows = match &mut self.buffer {
            Buffer::All(rows) => {
                let mut rows = std::mem::take(rows);
                rows.sort_unstable(); // no two rows are equal: their numbers differ
                rows
            }
            Buffer::Top(heap) => std::mem::take(heap).into_sorted_vec(),
        };
        if let Some(limit) = self.limit {
            rows.truncate(limit);
        }
        rows
    }

    /// Sorts the buffer and writes it out as a run.
    fn write_run(&mut self) -> Result<()> {
        let rows = self.take_sorted();
        if rows.is_empty() {
            return Ok(());
        }
        let spill = match &mut self.spill {
            Some(spill) => spill,
            None => self.spill.insert(Spill { file: temp_file()?, end: 0, runs: Vec::new() }),
        };
        let run = spill.write(rows.into_iter().map(Ok))?;
        spill.runs.push(run);
        Ok(())
    }
}

/// The rows of a finished sort, in order.
pub struct Sorted {
    inner: Inner,
}

enum Inner {
    Memory(std::vec::IntoIter<Keyed>),
    Merge(Merge),
}

impl Iterator for Sorted {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = match &mut self.inner {
            Inner::Memory(rows) => Ok(rows.next()?),
            Inner::Merge(merge) => merge.next()?,
        };
        Some(item.map(|keyed| keyed.row))
    }
}

impl Keyed {
    /// A rough count of the bytes the row takes in memory.
    fn size(&self) -> usize {
        let value = |v: &Value| match v {
            Value::Text(s) => 24 + s.len(),
            _ => 16,
        };
        64 + self.keys.iter().chain(&self.row).map(value).sum::<usize>()
    }
}

impl Ord for Keyed {
    fn cmp(&self, other: &Self) -> Ordering {
        self.desc
            .iter()
            .zip(self.keys.iter().zip(&other.keys))
            .map(|(&desc, (a, b))| if desc { b.sort_cmp(a) } else { a.sort_cmp(b) })
            .find(|ord| ord.is_ne())
            .unwrap_or(Ordering::Equal)
            .then(self.seq.cmp(&other.seq))
    }
}

impl PartialOrd for Keyed {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Keyed {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Keyed {}

// ---- runs ----

/// The temporary file and the runs written to it, one after another.
struct Spill {
    file: Arc<dyn Medium>,
    /// End of the last run.
    end: u64,
    runs: Vec<Run>,
}

/// Where a run is in the temporary file: rows in order, each one a record
///
/// ```text
/// u32 length of the rest | u64 number | u16 key count | keys, then the row's values
/// value: 0 = NULL | 1, i64 | 2, bool byte | 3, u32 length, UTF-8 bytes   (little-endian)
/// ```
#[derive(Debug, Clone, Copy)]
struct Run {
    start: u64,
    end: u64,
}

impl Spill {
    /// Appends rows, which must be in order, as a new run.
    fn write(&mut self, rows: impl Iterator<Item = Result<Keyed>>) -> Result<Run> {
        let start = self.end;
        let mut chunk = Vec::with_capacity(IO_CHUNK);
        for row in rows {
            let row = row?;
            let len_at = chunk.len();
            chunk.extend_from_slice(&[0; 4]);
            chunk.extend_from_slice(&row.seq.to_le_bytes());
            chunk.extend_from_slice(&(row.keys.len() as u16).to_le_bytes());
            for value in row.keys.iter().chain(&row.row) {
                encode_value(value, &mut chunk);
            }
            let len = (chunk.len() - len_at - 4) as u32;
            chunk[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
            if chunk.len() >= IO_CHUNK {
                self.file.write_at(self.end, &chunk)?;
                self.end += chunk.len() as u64;
                chunk.clear();
            }
        }
        self.file.write_at(self.end, &chunk)?;
        self.end += chunk.len() as u64;
        Ok(Run { start, end: self.end })
    }
}

/// Opens a temporary file that is removed at once, so it lives only as long as it is open.
fn temp_file() -> Result<Arc<dyn Medium>> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!("sequoia-sort-{}-{}", std::process::id(), NEXT.fetch_add(1, AtomicOrdering::Relaxed));
    let path = std::env::temp_dir().join(name);
    let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(Arc::new(FileMedium(file)))
}

fn encode_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0),
        Value::Int(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Bool(b) => out.extend_from_slice(&[2, *b as u8]),
        Value::Text(s) => {
            out.push(3);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
    }
}

/// Reads a run's records in order, a chunk of the file at a time.
struct RunReader {
    file: Arc<dyn Medium>,
    /// Next byte of the run to read from the file.
    pos: u64,
    end: u64,
    chunk: Vec<u8>,
    /// Next unread byte of `chunk`.
    at: usize,
    desc: Arc<[bool]>,
}

impl RunReader {
    fn new(file: &Arc<dyn Medium>, run: Run, desc: &Arc<[bool]>) -> Self {
        RunReader { file: file.clone(), pos: run.start, end: run.end, chunk: Vec::new(), at: 0, desc: desc.clone() }
    }

    fn next(&mut self) -> Result<Option<Keyed>> {
        if !self.fill(4)? {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.take(4).try_into().unwrap()) as usize;
        if !self.fill(len)? {
            return err("corrupted sort run: a record is cut short");
        }
        let record = self.take(len);
        let seq = u64::from_le_bytes(record[0..8].try_into().unwrap());
        let key_count = u16::from_le_bytes(record[8..10].try_into().unwrap()) as usize;
        let mut values = Vec::new();
        let mut pos = 10;
        while pos < record.len() {
            values.push(decode_value(record, &mut pos)?);
        }
        let row = values.split_off(key_count);
        Ok(Some(Keyed { keys: values, seq, row, desc: self.desc.clone() }))
    }

    /// Makes sure `n` unread bytes are in the chunk. Returns false if the run
    /// has no bytes left at all.
    fn fill(&mut self, n: usize) -> Result<bool> {
        let unread = self.chunk.len() - self.at;
        if unread >= n {
            return Ok(true);
        }
        if unread == 0 && self.pos == self.end {
            return Ok(false);
        }
        let read = (n - unread).max(IO_CHUNK).min((self.end - self.pos) as usize);
        if unread + read < n {
            return err("corrupted sort run: a record is cut short");
        }
        self.chunk.drain(..self.at);
        self.at = 0;
        let old = self.chunk.len();
        self.chunk.resize(old + read, 0);
        self.file.read_at(self.pos, &mut self.chunk[old..])?;
        self.pos += read as u64;
        Ok(true)
    }

    fn take(&mut self, n: usize) -> &[u8] {
        self.at += n;
        &self.chunk[self.at - n..self.at]
    }
}

fn decode_value(bytes: &[u8], pos: &mut usize) -> Result<Value> {
    let mut take = |n: usize| -> Result<&[u8]> {
        let slice = bytes.get(*pos..*pos + n).ok_or_else(|| crate::error::Error("corrupted sort run".into()))?;
        *pos += n;
        Ok(slice)
    };
    Ok(match take(1)?[0] {
        0 => Value::Null,
        1 => Value::Int(i64::from_le_bytes(take(8)?.try_into().unwrap())),
        2 => Value::Bool(take(1)?[0] != 0),
        3 => {
            let len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
            match String::from_utf8(take(len)?.to_vec()) {
                Ok(s) => Value::Text(s),
                Err(_) => return err("corrupted sort run: invalid UTF-8"),
            }
        }
        tag => return err(format!("corrupted sort run: value tag {tag}")),
    })
}

/// Merges sorted runs: a heap holds the next row of each and hands out the smallest.
struct Merge {
    readers: Vec<RunReader>,
    heap: BinaryHeap<Reverse<(Keyed, usize)>>,
    /// Rows left to return, if there is a limit.
    remaining: Option<usize>,
}

impl Merge {
    fn new(file: &Arc<dyn Medium>, runs: &[Run], desc: &Arc<[bool]>, limit: Option<usize>) -> Result<Self> {
        let mut readers: Vec<RunReader> = runs.iter().map(|&run| RunReader::new(file, run, desc)).collect();
        let mut heap = BinaryHeap::new();
        for (i, reader) in readers.iter_mut().enumerate() {
            if let Some(row) = reader.next()? {
                heap.push(Reverse((row, i)));
            }
        }
        Ok(Merge { readers, heap, remaining: limit })
    }
}

impl Iterator for Merge {
    type Item = Result<Keyed>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == Some(0) {
            return None;
        }
        let Reverse((row, i)) = self.heap.pop()?;
        match self.readers[i].next() {
            Ok(Some(next)) => self.heap.push(Reverse((next, i))),
            Ok(None) => {}
            Err(e) => return Some(Err(e)),
        }
        if let Some(n) = &mut self.remaining {
            *n -= 1;
        }
        Some(Ok(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pseudo-random generator, so the tests need no dependencies.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    /// Rows `(i, key1, key2)` with few distinct keys, so many rows tie.
    fn rows(n: usize) -> Vec<(Vec<Value>, Row)> {
        let mut rng = Rng(1);
        (0..n)
            .map(|i| {
                let a = match rng.below(12) {
                    0 => Value::Null,
                    k => Value::Int(k as i64 - 5),
                };
                let b = Value::Text(["x", "y", "zz", ""][rng.below(4) as usize].repeat(1 + rng.below(30) as usize));
                (vec![a.clone(), b.clone()], vec![Value::Int(i as i64), a, b])
            })
            .collect()
    }

    /// The expected result: a stable in-memory sort, then the limit.
    fn expected(rows: &[(Vec<Value>, Row)], desc: &[bool], limit: Option<usize>) -> Vec<Row> {
        let mut sorted: Vec<&(Vec<Value>, Row)> = rows.iter().collect();
        sorted.sort_by(|(a, _), (b, _)| {
            desc.iter()
                .zip(a.iter().zip(b))
                .map(|(&d, (x, y))| if d { y.sort_cmp(x) } else { x.sort_cmp(y) })
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
        });
        sorted.into_iter().take(limit.unwrap_or(usize::MAX)).map(|(_, row)| row.clone()).collect()
    }

    fn sort(
        rows: &[(Vec<Value>, Row)],
        desc: &[bool],
        limit: Option<usize>,
        budget: usize,
        fan_in: usize,
    ) -> (Vec<Row>, bool) {
        let mut sorter = Sorter::with_fan_in(desc.to_vec(), limit, budget, fan_in);
        for (keys, row) in rows {
            sorter.push(keys.clone(), row.clone()).unwrap();
        }
        let spilled = sorter.spilled();
        (sorter.finish().unwrap().collect::<Result<_>>().unwrap(), spilled)
    }

    #[test]
    fn sorts_in_memory_within_the_budget() {
        let rows = rows(500);
        let (sorted, spilled) = sort(&rows, &[false, true], None, SORT_MEMORY, FAN_IN);
        assert!(!spilled);
        assert_eq!(sorted, expected(&rows, &[false, true], None));
    }

    #[test]
    fn spills_runs_and_merges_them_in_several_passes() {
        let rows = rows(3000);
        for desc in [[false, false], [true, false], [false, true]] {
            // Runs of a few dozen rows, merged 3 at a time over several passes.
            let (sorted, spilled) = sort(&rows, &desc, None, 2000, 3);
            assert!(spilled);
            assert_eq!(sorted, expected(&rows, &desc, None), "{desc:?}");
        }
        // A budget smaller than any row writes a run per row.
        let (sorted, _) = sort(&rows[..200], &[true, true], None, 1, 2);
        assert_eq!(sorted, expected(&rows[..200], &[true, true], None));
    }

    #[test]
    fn top_n_keeps_the_first_rows_stably() {
        let rows = rows(3000);
        for limit in [0, 1, 7, 100, 2999, 3000, 5000] {
            for desc in [[false, false], [true, true]] {
                let want = expected(&rows, &desc, Some(limit));
                let (sorted, spilled) = sort(&rows, &desc, Some(limit), SORT_MEMORY, FAN_IN);
                assert!(!spilled);
                assert_eq!(sorted, want, "limit {limit}, {desc:?}");
                // The top rows themselves overflow a small budget: runs of at most `limit` rows.
                let (sorted, _) = sort(&rows, &desc, Some(limit), 3000, 4);
                assert_eq!(sorted, want, "limit {limit}, {desc:?}, spilled");
            }
        }
    }

    #[test]
    fn values_round_trip_through_a_run() {
        let values = vec![
            Value::Null,
            Value::Int(i64::MIN),
            Value::Int(-1),
            Value::Bool(true),
            Value::Bool(false),
            Value::Text(String::new()),
            Value::Text("héllo, 世界".into()),
        ];
        let mut bytes = Vec::new();
        for value in &values {
            encode_value(value, &mut bytes);
        }
        let mut pos = 0;
        let decoded: Vec<Value> = values.iter().map(|_| decode_value(&bytes, &mut pos).unwrap()).collect();
        assert_eq!((decoded, pos), (values, bytes.len()));
        assert!(decode_value(&[9], &mut 0).is_err());
        assert!(decode_value(&[3, 5, 0, 0, 0, b'a'], &mut 0).is_err(), "cut short");
    }
}
