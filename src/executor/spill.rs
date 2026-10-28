//! Rows spilled to temporary files past a memory budget: the sort's runs
//! are written with the encoding here, and hash joins and aggregation write
//! the rows of the partitions they cannot hold to [`RowFile`]s.
//!
//! A hash join or aggregation that outgrows its budget splits its rows into
//! [`FANOUT`] partitions by a hash of their keys, so rows with equal keys
//! share a partition, keeps what it can of them in memory and writes the
//! rest out, to deal with one partition at a time later. A partition still
//! too large is split again, by the next bits of the same hash
//! ([`partition`] at the next level), up to [`MAX_LEVEL`], past which its
//! rows are all held, whatever the budget: they can be told apart no more.
//!
//! A temporary file is removed as soon as it is created, so it disappears
//! once closed, even if the process does not close it. It does not go
//! through the write-ahead log: nothing in it needs to survive a crash.

use std::fs::OpenOptions;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::catalog::table::Row;
use crate::error::{Error, Result, err};
use crate::storage::hash;
use crate::storage::io::medium::{FileMedium, Medium};
use crate::storage::sort_key;
use crate::types::Value;

/// Bytes read or written to a temporary file at a time.
pub const IO_CHUNK: usize = 64 << 10;

/// Partitions rows are split into at each level.
pub const FANOUT: usize = 16;
/// Bits of a hash that pick a partition at each level.
const BITS: u32 = FANOUT.trailing_zeros();
/// The last level that partitions: 32-bit hashes have no bits left past it.
pub const MAX_LEVEL: u32 = 32 / BITS - 1;

/// The partition of rows with these keys at `level`. Keys that are equal
/// values are in the same one: they are hashed by their sort keys, which
/// are equal for equal values (an INT and the REAL it equals too).
pub fn partition(keys: &[Value], level: u32, scratch: &mut Vec<u8>) -> usize {
    scratch.clear();
    for key in keys {
        sort_key::encode(key, false, scratch);
    }
    (hash::hash(scratch) >> (level * BITS)) as usize & (FANOUT - 1)
}

/// Opens a temporary file that is removed at once, so it lives only as long as it is open.
pub fn temp_file() -> Result<Arc<dyn Medium>> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "sequoia-spill-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = std::env::temp_dir().join(name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(Arc::new(FileMedium(file)))
}

/// Appends a value, little-endian:
///
/// ```text
/// 0 = NULL | 1, i64 | 2, bool byte | 3, u32 length, UTF-8 bytes | 4, f64
///   | 5, i32 days | 6, i64 time | 7, i64 timestamp | 8, u32 length, bytes
///   | 9, i128 mantissa, scale byte | 10, i64 timestamp with time zone
///   | 11, i32 months, i32 days, i64 microseconds
/// ```
pub fn encode_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Date(d) => {
            out.push(5);
            out.extend_from_slice(&d.to_le_bytes());
        }
        Value::Time(t) | Value::Timestamp(t) | Value::TimestampTz(t) => {
            out.push(match value {
                Value::Time(_) => 6,
                Value::Timestamp(_) => 7,
                _ => 10,
            });
            out.extend_from_slice(&t.to_le_bytes());
        }
        Value::Interval(i) => {
            out.push(11);
            out.extend_from_slice(&i.months.to_le_bytes());
            out.extend_from_slice(&i.days.to_le_bytes());
            out.extend_from_slice(&i.micros.to_le_bytes());
        }
        Value::Blob(b) => {
            out.push(8);
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        Value::Decimal(d) => {
            out.push(9);
            out.extend_from_slice(&d.mantissa().to_le_bytes());
            out.push(d.scale());
        }
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

/// Decodes the value [`encode_value`] wrote at `*pos`, and moves `*pos` past it.
pub fn decode_value(bytes: &[u8], pos: &mut usize) -> Result<Value> {
    let mut take = |n: usize| -> Result<&[u8]> {
        let slice = bytes
            .get(*pos..*pos + n)
            .ok_or_else(|| Error("corrupted temporary file".into()))?;
        *pos += n;
        Ok(slice)
    };
    Ok(match take(1)?[0] {
        0 => Value::Null,
        1 => Value::Int(i64::from_le_bytes(take(8)?.try_into().unwrap())),
        2 => Value::Bool(take(1)?[0] != 0),
        4 => Value::Real(f64::from_le_bytes(take(8)?.try_into().unwrap())),
        5 => Value::Date(i32::from_le_bytes(take(4)?.try_into().unwrap())),
        6 => Value::Time(i64::from_le_bytes(take(8)?.try_into().unwrap())),
        7 => Value::Timestamp(i64::from_le_bytes(take(8)?.try_into().unwrap())),
        10 => Value::TimestampTz(i64::from_le_bytes(take(8)?.try_into().unwrap())),
        11 => Value::Interval(crate::datetime::Interval {
            months: i32::from_le_bytes(take(4)?.try_into().unwrap()),
            days: i32::from_le_bytes(take(4)?.try_into().unwrap()),
            micros: i64::from_le_bytes(take(8)?.try_into().unwrap()),
        }),
        8 => {
            let len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
            Value::Blob(take(len)?.to_vec())
        }
        9 => {
            let mantissa = i128::from_le_bytes(take(16)?.try_into().unwrap());
            let scale = take(1)?[0];
            match crate::decimal::Decimal::new(mantissa, scale) {
                Ok(d) => Value::Decimal(d),
                Err(_) => return err("corrupted temporary file: invalid DECIMAL"),
            }
        }
        3 => {
            let len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
            match String::from_utf8(take(len)?.to_vec()) {
                Ok(s) => Value::Text(s),
                Err(_) => return err("corrupted temporary file: invalid UTF-8"),
            }
        }
        tag => return err(format!("corrupted temporary file: value tag {tag}")),
    })
}

/// Rows written one after another to a temporary file of their own, made
/// when the first chunk of them is written out, to read back in order.
///
/// ```text
/// row: u32 length of the rest | its values, as encode_value writes them
/// ```
#[derive(Default)]
pub struct RowFile {
    file: Option<Arc<dyn Medium>>,
    /// Bytes written to the file.
    end: u64,
    chunk: Vec<u8>,
    rows: u64,
}

impl RowFile {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub fn push(&mut self, row: &[Value]) -> Result<()> {
        let at = self.chunk.len();
        self.chunk.extend_from_slice(&[0; 4]);
        for value in row {
            encode_value(value, &mut self.chunk);
        }
        let len = (self.chunk.len() - at - 4) as u32;
        self.chunk[at..at + 4].copy_from_slice(&len.to_le_bytes());
        self.rows += 1;
        if self.chunk.len() >= IO_CHUNK {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        let file = match &self.file {
            Some(file) => file,
            None => self.file.insert(temp_file()?),
        };
        file.write_at(self.end, &self.chunk)?;
        self.end += self.chunk.len() as u64;
        self.chunk.clear();
        Ok(())
    }

    /// The rows written, in order: those in the file, then those still in memory.
    pub fn read(self) -> RowReader {
        let RowFile {
            file,
            end,
            chunk,
            rows,
        } = self;
        let (chunk, tail) = match file {
            Some(_) => (Vec::new(), Some(chunk)),
            None => (chunk, None),
        };
        RowReader {
            file,
            pos: 0,
            end,
            chunk,
            at: 0,
            tail,
            left: rows,
        }
    }
}

/// Reads back the rows of a [`RowFile`], a chunk of the file at a time.
pub struct RowReader {
    file: Option<Arc<dyn Medium>>,
    /// The next byte of the file to read, and the end of what was written.
    pos: u64,
    end: u64,
    chunk: Vec<u8>,
    at: usize,
    /// Rows still in memory, after those in the file.
    tail: Option<Vec<u8>>,
    left: u64,
}

impl RowReader {
    /// Makes sure `n` unread bytes are in the chunk from `at`.
    fn fill(&mut self, n: usize) -> Result<()> {
        while self.chunk.len() - self.at < n {
            self.chunk.drain(..self.at);
            self.at = 0;
            if self.pos < self.end {
                let read = IO_CHUNK.max(n).min((self.end - self.pos) as usize);
                let old = self.chunk.len();
                self.chunk.resize(old + read, 0);
                self.file
                    .as_ref()
                    .expect("bytes were written")
                    .read_at(self.pos, &mut self.chunk[old..])?;
                self.pos += read as u64;
            } else if let Some(tail) = self.tail.take() {
                self.chunk.extend_from_slice(&tail);
            } else {
                return err("corrupted temporary file: a row is cut short");
            }
        }
        Ok(())
    }

    fn read_row(&mut self) -> Result<Row> {
        self.fill(4)?;
        let len = u32::from_le_bytes(self.chunk[self.at..self.at + 4].try_into().unwrap()) as usize;
        self.fill(4 + len)?;
        let (start, end) = (self.at + 4, self.at + 4 + len);
        let mut pos = start;
        let mut row = Vec::new();
        while pos < end {
            row.push(decode_value(&self.chunk[..end], &mut pos)?);
        }
        self.at = end;
        Ok(row)
    }
}

impl Iterator for RowReader {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let row = self.read_row();
        if row.is_err() {
            self.left = 0;
        }
        Some(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip() {
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
        let decoded: Vec<Value> = values
            .iter()
            .map(|_| decode_value(&bytes, &mut pos).unwrap())
            .collect();
        assert_eq!((decoded, pos), (values, bytes.len()));
        assert!(decode_value(&[9], &mut 0).is_err());
        assert!(
            decode_value(&[3, 5, 0, 0, 0, b'a'], &mut 0).is_err(),
            "cut short"
        );
    }

    #[test]
    fn rows_come_back_in_order_from_memory_and_file() {
        for count in [0, 3, 20_000] {
            let mut file = RowFile::new();
            let rows: Vec<Row> = (0..count)
                .map(|i| {
                    vec![
                        Value::Int(i),
                        Value::Text("x".repeat(i as usize % 50)),
                        Value::Null,
                    ]
                })
                .collect();
            for row in &rows {
                file.push(row).unwrap();
            }
            assert_eq!(file.rows(), count as u64);
            let back: Vec<Row> = file.read().collect::<Result<_>>().unwrap();
            assert_eq!(back, rows);
        }
    }

    #[test]
    fn partitions_split_keys_by_level_and_keep_equal_values_together() {
        let mut scratch = Vec::new();
        assert_eq!(MAX_LEVEL, 7);
        let p = |keys: &[Value], level, scratch: &mut Vec<u8>| partition(keys, level, scratch);
        assert_eq!(
            p(&[Value::Int(3)], 0, &mut scratch),
            p(&[Value::Real(3.0)], 0, &mut scratch)
        );
        // Keys of one partition at a level spread over every partition at the next.
        let keys: Vec<Value> = (0..20_000)
            .map(Value::Int)
            .filter(|k| p(std::slice::from_ref(k), 0, &mut scratch) == 5)
            .collect();
        let mut counts = [0; FANOUT];
        for key in &keys {
            counts[p(std::slice::from_ref(key), 1, &mut scratch)] += 1;
        }
        assert!(
            counts.iter().all(|&n| n > keys.len() / FANOUT / 2),
            "{counts:?}"
        );
    }
}
