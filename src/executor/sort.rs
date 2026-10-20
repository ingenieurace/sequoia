//! Sorting for ORDER BY within a memory budget: an external merge sort, and
//! a top-N heap when there is a LIMIT.
//!
//! ```text
//! rows --> buffer (in memory) --full--> sorted run --+
//!          buffer             --full--> sorted run --+--> merge --> ... --> one sorted stream
//!          buffer             --end---> sorted, kept in memory --+
//! ```
//!
//! Each row's sort keys are encoded as one byte string that compares, byte
//! by byte, as the keys do (see [`sort_key`](crate::storage::sort_key)), and rows with equal keys
//! by the order they came in: the sort is stable. Its first eight bytes are
//! kept as a number, beside the rest and the row's arrival number, so a key
//! of up to eight bytes, as most INT keys are, takes no memory of its own,
//! and comparing two such rows compares only numbers.
//!
//! Rows are buffered until they take more than the budget, then sorted and
//! written out as a run to a temporary file. At the end, the last buffer is
//! sorted and kept in memory, and merged with the runs: a tournament tree
//! holds the next row of each and hands out the smallest. A merge reads at
//! most a fan-in of runs at once, more with a larger budget. If there are
//! more, the smallest runs are first merged into longer ones, as few at the
//! start as it takes for every later merge to be a full one, so that rows
//! are copied as few times as they can be. A merge before the last copies
//! records whole, without decoding them. When nothing was written out, the
//! buffer is just sorted in memory.
//!
//! With `LIMIT n`, only the first `n` rows can be in the result, so the
//! buffer is a heap of at most `n` rows whose top is the last of them: a new
//! row either replaces it or is dropped at once. Sorting `m` rows then takes
//! O(m log n) time and memory for `n` rows. A run, too, keeps only its first
//! `n` rows, and the merge stops after `n`. Once a run of `n` rows is
//! written, a row that sorts after its last can never be in the result
//! either, and is dropped as it arrives.
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
use crate::storage::sort_key::encode as encode_key;
use crate::types::Value;

/// Default memory a sort may buffer rows in before writing runs out: 4 MB.
pub const SORT_MEMORY: usize = 4 << 20;
/// Fewest runs a merge reads at once. With a larger budget, it reads one per
/// [`IO_CHUNK`] of the budget, up to [`MAX_FAN_IN`].
pub const FAN_IN: usize = 16;
/// Most runs a merge reads at once.
const MAX_FAN_IN: usize = 256;
/// Bytes read or written to the temporary file at a time.
const IO_CHUNK: usize = 64 << 10;

pub struct Sorter {
    /// For each sort key, whether it is descending.
    desc: Box<[bool]>,
    limit: Option<usize>,
    budget: usize,
    fan_in: usize,
    buffer: Buffer,
    /// Estimated bytes held by `buffer`.
    bytes: usize,
    /// Number of the next row pushed.
    seq: u64,
    /// The key being encoded, kept to reuse its memory.
    scratch: Vec<u8>,
    /// With a limit, the smallest last key of a run of `limit` rows: rows
    /// with keys from it on can be dropped.
    cutoff: Option<Key>,
    /// Created when the first run is written out.
    spill: Option<Spill>,
}

enum Buffer {
    /// Every row, in arrival order.
    All(Vec<Keyed>),
    /// The best rows so far, at most `limit` of them; the top is the worst.
    Top(BinaryHeap<Keyed>),
}

/// A row with its sort key.
struct Keyed {
    key: Key,
    row: Row,
}

/// A row's encoded sort key, split so that most comparisons need only look
/// at numbers: its first eight bytes (padded with zeros), the rest, and the
/// row's arrival number. The fields compare in that order, as the bytes of
/// the key would: as no key's encoding begins with another's, two keys
/// whose first eight bytes match are both longer than eight bytes, or equal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    prefix: u64,
    rest: Box<[u8]>,
    seq: u64,
}

/// A [`Key`] whose bytes are borrowed: from a key being encoded, or a run's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct KeyRef<'a> {
    prefix: u64,
    rest: &'a [u8],
    seq: u64,
}

impl Sorter {
    /// A sorter for rows ordered by keys with these directions, keeping the
    /// first `limit` rows if given, and buffering about `budget` bytes.
    pub fn new(desc: Vec<bool>, limit: Option<usize>, budget: usize) -> Self {
        Self::with_fan_in(desc, limit, budget, (budget / IO_CHUNK).clamp(FAN_IN, MAX_FAN_IN))
    }

    pub fn with_fan_in(desc: Vec<bool>, limit: Option<usize>, budget: usize, fan_in: usize) -> Self {
        assert!(fan_in >= 2, "a merge needs at least two runs");
        let buffer = match limit {
            Some(_) => Buffer::Top(BinaryHeap::new()),
            None => Buffer::All(Vec::new()),
        };
        Sorter {
            desc: desc.into(),
            limit,
            budget,
            fan_in,
            buffer,
            bytes: 0,
            seq: 0,
            scratch: Vec::new(),
            cutoff: None,
            spill: None,
        }
    }

    /// Adds a row with its sort keys, one per direction given to `new`.
    pub fn push(&mut self, keys: &[Value], row: Row) -> Result<()> {
        assert_eq!(keys.len(), self.desc.len());
        self.scratch.clear();
        for (value, &desc) in keys.iter().zip(&self.desc) {
            encode_key(value, desc, &mut self.scratch);
        }
        let key = KeyRef::new(&self.scratch, self.seq);
        self.seq += 1;
        if let Some(cutoff) = &self.cutoff
            && key >= cutoff.as_ref()
        {
            return Ok(());
        }
        match &mut self.buffer {
            Buffer::All(rows) => {
                let item = Keyed { key: key.to_owned(), row };
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
                    if key >= heap.peek().expect("not empty").key.as_ref() {
                        return Ok(());
                    }
                    self.bytes -= heap.pop().expect("not empty").size();
                }
                let item = Keyed { key: key.to_owned(), row };
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
        let rows = self.take_sorted();
        let Some(mut spill) = self.spill.take() else {
            return Ok(Sorted { inner: Inner::Memory(rows.into_iter()) });
        };
        // The last rows stay in memory, as one more input of the last merge.
        let memory = (!rows.is_empty()).then(|| Source::Memory(Memory::new(rows)));
        let inputs = |spill: &Spill| spill.runs.len() + memory.is_some() as usize;
        while inputs(&spill) > self.fan_in {
            // Merging k runs leaves k - 1 fewer. Merge as few as it takes for
            // every merge after this one to merge fan_in, the smallest first.
            let take = (inputs(&spill) - 2) % (self.fan_in - 1) + 2;
            spill.runs.sort_unstable_by_key(|run| Reverse(run.end - run.start));
            let group = spill.runs.split_off(spill.runs.len() - take);
            let sources = group.iter().map(|&run| Source::run(&spill.file, run)).collect::<Result<_>>()?;
            let mut merge = Merge::new(sources, self.limit);
            let run = spill.write_merged(&mut merge)?;
            spill.runs.push(run);
        }
        let mut sources: Vec<Source> =
            spill.runs.iter().map(|&run| Source::run(&spill.file, run)).collect::<Result<_>>()?;
        sources.extend(memory);
        Ok(Sorted { inner: Inner::Merge(Merge::new(sources, self.limit)) })
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
        let Some(last) = rows.last() else {
            return Ok(());
        };
        if Some(rows.len()) == self.limit && self.cutoff.as_ref().is_none_or(|cutoff| last.key < *cutoff) {
            self.cutoff = Some(last.key.clone());
        }
        let spill = match &mut self.spill {
            Some(spill) => spill,
            None => self.spill.insert(Spill { file: temp_file()?, end: 0, runs: Vec::new() }),
        };
        let mut writer = RunWriter::new(spill);
        for row in &rows {
            writer.keyed(row)?;
        }
        let run = writer.finish()?;
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
        match &mut self.inner {
            Inner::Memory(rows) => Some(Ok(rows.next()?.row)),
            Inner::Merge(merge) => merge.next(),
        }
    }
}

impl Keyed {
    /// A rough count of the bytes the row takes in memory.
    fn size(&self) -> usize {
        56 + self.key.rest.len() + self.row.iter().map(value_size).sum::<usize>()
    }
}

impl Key {
    fn as_ref(&self) -> KeyRef<'_> {
        KeyRef { prefix: self.prefix, rest: &self.rest, seq: self.seq }
    }
}

impl<'a> KeyRef<'a> {
    /// The key of a row with this encoding and number.
    fn new(encoded: &'a [u8], seq: u64) -> Self {
        let n = encoded.len().min(8);
        let mut prefix = [0; 8];
        prefix[..n].copy_from_slice(&encoded[..n]);
        KeyRef { prefix: u64::from_be_bytes(prefix), rest: &encoded[n..], seq }
    }

    fn to_owned(self) -> Key {
        // An empty box takes no memory.
        Key { prefix: self.prefix, rest: self.rest.into(), seq: self.seq }
    }
}

/// A rough count of the bytes a value takes in memory.
pub fn value_size(value: &Value) -> usize {
    match value {
        Value::Text(s) => 24 + s.len(),
        _ => 16,
    }
}

impl Ord for Keyed {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
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
/// u32 length of the rest | key: u64 prefix | u64 number | u32 length, rest of the key | the row's values
/// value: 0 = NULL | 1, i64 | 2, bool byte | 3, u32 length, UTF-8 bytes   (little-endian)
/// ```
#[derive(Debug, Clone, Copy)]
struct Run {
    start: u64,
    end: u64,
}

/// Where the rest of the key starts in a record: after the record's length,
/// the key's prefix and number, and the rest's length.
const KEY_REST: usize = 4 + 8 + 8 + 4;

impl Spill {
    /// Appends the rows a merge hands out as a new run.
    fn write_merged(&mut self, merge: &mut Merge) -> Result<Run> {
        let mut writer = RunWriter::new(self);
        while let Some(i) = merge.winner() {
            match &merge.sources[i] {
                Source::Run(reader) => writer.record(reader.record())?,
                Source::Memory(memory) => writer.keyed(memory.current.as_ref().expect("has a key"))?,
            }
            merge.advance()?;
        }
        writer.finish()
    }
}

/// Appends records to the temporary file as a new run, a chunk at a time.
struct RunWriter<'a> {
    spill: &'a mut Spill,
    start: u64,
    chunk: Vec<u8>,
}

impl<'a> RunWriter<'a> {
    fn new(spill: &'a mut Spill) -> Self {
        let start = spill.end;
        RunWriter { spill, start, chunk: Vec::with_capacity(IO_CHUNK) }
    }

    fn keyed(&mut self, row: &Keyed) -> Result<()> {
        let at = self.chunk.len();
        self.chunk.extend_from_slice(&[0; 4]);
        self.chunk.extend_from_slice(&row.key.prefix.to_le_bytes());
        self.chunk.extend_from_slice(&row.key.seq.to_le_bytes());
        self.chunk.extend_from_slice(&(row.key.rest.len() as u32).to_le_bytes());
        self.chunk.extend_from_slice(&row.key.rest);
        for value in &row.row {
            encode_value(value, &mut self.chunk);
        }
        let len = (self.chunk.len() - at - 4) as u32;
        self.chunk[at..at + 4].copy_from_slice(&len.to_le_bytes());
        self.flush_if_full()
    }

    /// Appends a whole record as a run reader read it.
    fn record(&mut self, record: &[u8]) -> Result<()> {
        self.chunk.extend_from_slice(record);
        self.flush_if_full()
    }

    fn flush_if_full(&mut self) -> Result<()> {
        if self.chunk.len() >= IO_CHUNK {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.spill.file.write_at(self.spill.end, &self.chunk)?;
        self.spill.end += self.chunk.len() as u64;
        self.chunk.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<Run> {
        self.flush()?;
        Ok(Run { start: self.start, end: self.spill.end })
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
        Value::Real(r) => {
            out.push(4);
            out.extend_from_slice(&r.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(3);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
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
        4 => Value::Real(f64::from_le_bytes(take(8)?.try_into().unwrap())),
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

/// Reads a run's records in order, a chunk of the file at a time. The
/// current record stays in the chunk until the reader moves past it.
struct RunReader {
    file: Arc<dyn Medium>,
    /// Next byte of the run to read from the file.
    pos: u64,
    end: u64,
    chunk: Vec<u8>,
    /// Where the current record starts in `chunk`.
    at: usize,
    /// Its length, with the length before it; 0 once the run is read.
    len: usize,
}

impl RunReader {
    fn new(file: &Arc<dyn Medium>, run: Run) -> Result<Self> {
        let mut reader = RunReader { file: file.clone(), pos: run.start, end: run.end, chunk: Vec::new(), at: 0, len: 0 };
        reader.advance()?;
        Ok(reader)
    }

    /// Moves to the next record, if there is one.
    fn advance(&mut self) -> Result<()> {
        self.at += self.len;
        self.len = 0;
        if !self.fill(4)? {
            return Ok(());
        }
        let len = 4 + u32::from_le_bytes(self.chunk[self.at..self.at + 4].try_into().unwrap()) as usize;
        if len < KEY_REST || !self.fill(len)? {
            return err("corrupted sort run: a record is cut short");
        }
        self.len = len;
        if self.row_at() > len {
            self.len = 0;
            return err("corrupted sort run: a key runs past its record");
        }
        Ok(())
    }

    /// The current record, whole; empty once the run is read.
    fn record(&self) -> &[u8] {
        &self.chunk[self.at..self.at + self.len]
    }

    /// Where the current record's row starts in it, after the key.
    fn row_at(&self) -> usize {
        let record = self.record();
        KEY_REST + u32::from_le_bytes(record[KEY_REST - 4..KEY_REST].try_into().unwrap()) as usize
    }

    /// The current record's key, if there is a current record.
    fn key(&self) -> Option<KeyRef<'_>> {
        let record = self.record();
        if record.is_empty() {
            return None;
        }
        let number = |at: usize| u64::from_le_bytes(record[at..at + 8].try_into().unwrap());
        Some(KeyRef { prefix: number(4), rest: &record[KEY_REST..self.row_at()], seq: number(12) })
    }

    /// Decodes the current record's row.
    fn row(&self) -> Result<Row> {
        let record = self.record();
        let mut pos = self.row_at();
        let mut row = Vec::new();
        while pos < record.len() {
            row.push(decode_value(record, &mut pos)?);
        }
        Ok(row)
    }

    /// Makes sure `n` unread bytes are in the chunk from `at`. Returns false
    /// if the run has no bytes left at all.
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
}

/// The sorted rows kept in memory at the end.
struct Memory {
    rows: std::vec::IntoIter<Keyed>,
    current: Option<Keyed>,
}

impl Memory {
    fn new(rows: Vec<Keyed>) -> Self {
        let mut rows = rows.into_iter();
        Memory { current: rows.next(), rows }
    }
}

/// An input of a merge.
enum Source {
    Run(RunReader),
    Memory(Memory),
}

impl Source {
    fn run(file: &Arc<dyn Medium>, run: Run) -> Result<Source> {
        Ok(Source::Run(RunReader::new(file, run)?))
    }

    /// The key of the next row, if any rows are left.
    fn key(&self) -> Option<KeyRef<'_>> {
        match self {
            Source::Run(reader) => reader.key(),
            Source::Memory(memory) => memory.current.as_ref().map(|row| row.key.as_ref()),
        }
    }

    fn advance(&mut self) -> Result<()> {
        match self {
            Source::Run(reader) => reader.advance(),
            Source::Memory(memory) => {
                memory.current = memory.rows.next();
                Ok(())
            }
        }
    }
}

/// Merges sorted inputs with a tournament tree of losers: each inner node
/// holds the input that lost the match there, and `tree[0]` the winner, the
/// input with the smallest next row. Once the winner moves on, it plays only
/// the matches on its way up again: log2(inputs) comparisons per row.
struct Merge {
    sources: Vec<Source>,
    /// Node `i` has children `2i` and `2i + 1`; input `j` is leaf `len + j`.
    tree: Vec<usize>,
    /// Rows left to return, if there is a limit.
    remaining: Option<usize>,
}

impl Merge {
    fn new(sources: Vec<Source>, limit: Option<usize>) -> Self {
        let n = sources.len();
        let mut tree = vec![0; n.max(1)];
        let mut winners = vec![0; 2 * n];
        for (i, winner) in winners[n..].iter_mut().enumerate() {
            *winner = i;
        }
        for node in (1..n).rev() {
            let (a, b) = (winners[2 * node], winners[2 * node + 1]);
            let (winner, loser) = if before(&sources, b, a) { (b, a) } else { (a, b) };
            winners[node] = winner;
            tree[node] = loser;
        }
        if n > 1 {
            tree[0] = winners[1];
        }
        Merge { sources, tree, remaining: limit }
    }

    /// The input with the next row, if the merge has rows left.
    fn winner(&self) -> Option<usize> {
        if self.remaining == Some(0) {
            return None;
        }
        let winner = self.tree[0];
        self.sources.get(winner)?.key().map(|_| winner)
    }

    /// Moves the winner past its row and finds the next one.
    fn advance(&mut self) -> Result<()> {
        let mut winner = self.tree[0];
        self.sources[winner].advance()?;
        let mut node = (self.sources.len() + winner) / 2;
        while node > 0 {
            if before(&self.sources, self.tree[node], winner) {
                std::mem::swap(&mut self.tree[node], &mut winner);
            }
            node /= 2;
        }
        self.tree[0] = winner;
        if let Some(n) = &mut self.remaining {
            *n -= 1;
        }
        Ok(())
    }
}

/// Whether input `a`'s next row comes before input `b`'s; an input with no
/// rows left comes after every other.
fn before(sources: &[Source], a: usize, b: usize) -> bool {
    match (sources[a].key(), sources[b].key()) {
        (Some(a), Some(b)) => a < b,
        (a, _) => a.is_some(),
    }
}

impl Iterator for Merge {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        let i = self.winner()?;
        let row = match &mut self.sources[i] {
            Source::Run(reader) => reader.row(),
            Source::Memory(memory) => Ok(memory.current.take().expect("has a key").row),
        };
        if let Err(e) = self.advance() {
            return Some(Err(e));
        }
        Some(row)
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

    /// The order of [`encode_key`]: by type, NULL first, then by value.
    fn compare(a: &Value, b: &Value) -> Ordering {
        let rank = |v: &Value| match v {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) | Value::Real(_) => 2,
            Value::Text(_) => 3,
        };
        rank(a).cmp(&rank(b)).then_with(|| a.sort_cmp(b))
    }

    /// The expected result: a stable in-memory sort, then the limit.
    fn expected(rows: &[(Vec<Value>, Row)], desc: &[bool], limit: Option<usize>) -> Vec<Row> {
        let mut sorted: Vec<&(Vec<Value>, Row)> = rows.iter().collect();
        sorted.sort_by(|(a, _), (b, _)| {
            desc.iter()
                .zip(a.iter().zip(b))
                .map(|(&d, (x, y))| if d { compare(y, x) } else { compare(x, y) })
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
            sorter.push(keys, row.clone()).unwrap();
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
        for fan_in in [2, 3, 7, 16] {
            let (sorted, _) = sort(&rows[..200], &[true, true], None, 1, fan_in);
            assert_eq!(sorted, expected(&rows[..200], &[true, true], None), "fan-in {fan_in}");
        }
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
                for fan_in in [2, 4] {
                    let (sorted, _) = sort(&rows, &desc, Some(limit), 3000, fan_in);
                    assert_eq!(sorted, want, "limit {limit}, {desc:?}, spilled, fan-in {fan_in}");
                }
            }
        }
    }

    #[test]
    fn a_full_run_cuts_off_later_rows() {
        let rows: Vec<(Vec<Value>, Row)> = (0..1000).map(|i| (vec![Value::Int(i % 100)], vec![Value::Int(i)])).collect();
        let mut sorter = Sorter::with_fan_in(vec![false], Some(10), 700, 4);
        for (keys, row) in &rows {
            sorter.push(keys, row.clone()).unwrap();
        }
        // The first run holds keys 0 to 9: only rows with smaller keys, or
        // equal ones that came earlier, which none did, are kept after it.
        assert!(sorter.spilled());
        let runs = sorter.spill.as_ref().unwrap().runs.len();
        assert!(runs < 15, "{runs} runs");
        let sorted: Vec<Row> = sorter.finish().unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(sorted, expected(&rows, &[false], Some(10)));
    }

    #[test]
    fn keys_compare_as_values_do() {
        let mut values = vec![Value::Null, Value::Bool(false), Value::Bool(true)];
        for n in [i64::MIN, i64::MIN + 1, -(1 << 40), -65536, -65535, -256, -255, -1, 0, 1, 255, 256, 1 << 40, i64::MAX] {
            values.push(Value::Int(n));
        }
        for s in ["", "\0", "\0\0", "\0a", "a", "a\0", "a\0b", "ab", "b", "é", "世界"] {
            values.push(Value::Text(s.into()));
        }
        // REALs among the INTs, some equal to one.
        for r in [-1e300, -9.3e18, -65535.5, -256.0, -1.0, -0.75, -0.0, 0.25, 0.5, 1.0, 255.99, 1e15 + 0.5, 9.3e18, 1e300] {
            values.push(Value::Real(r));
        }
        let encode = |values: &[&Value], desc: bool| {
            let mut out = Vec::new();
            for value in values {
                encode_key(value, desc, &mut out);
            }
            out
        };
        for a in &values {
            for b in &values {
                for desc in [false, true] {
                    let want = if desc { compare(b, a) } else { compare(a, b) };
                    assert_eq!(encode(&[a], desc).cmp(&encode(&[b], desc)), want, "{a:?} {b:?} {desc}");
                    // A key of two values: the first decides unless equal.
                    for c in [&values[0], &values[12], &values[20]] {
                        let (x, y) = (encode(&[a, c], desc), encode(&[b, &values[5]], desc));
                        // Split into prefix and rest, keys of every length around
                        // eight bytes compare as their bytes do.
                        assert_eq!(KeyRef::new(&x, 0).cmp(&KeyRef::new(&y, 0)), x.cmp(&y), "{x:?} {y:?}");
                        let got = x.cmp(&y);
                        let want = want.then_with(|| if desc { compare(&values[5], c) } else { compare(c, &values[5]) });
                        assert_eq!(got, want, "{a:?},{c:?} {b:?} {desc}");
                    }
                }
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
            Value::Real(-2.5),
            Value::Real(1e300),
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
