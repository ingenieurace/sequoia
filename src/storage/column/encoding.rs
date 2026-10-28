//! Columnar compression: encodes the values of one column of a row group.
//!
//! A column chunk is:
//!
//! ```text
//! 1 byte       1 if a null bitmap follows, 0 if no value is NULL
//! null bitmap  ceil(n / 8) bytes; bit i set = row i is NULL
//! values       one encoding of all n values, a NULL stored as 0, FALSE or ''
//! ```
//!
//! Keeping a placeholder for each NULL keeps every row at the same position in
//! every column, so a row is put back together from its position alone. BOOL
//! is stored as the integers 0 and 1.
//!
//! Every encoding is lossless. Writing a chunk tries each one that applies
//! and keeps the smallest; the first byte says which was chosen:
//!
//! ```text
//! integers
//!   0 Plain       n x i64
//!   1 BitPack     base i64, width u8, n x (value - base) in `width` bits
//!   2 Patched     base i64, width u8, n packed values, exception count u32,
//!                 exceptions (i64 each); a packed value of all ones (the
//!                 marker) means "the next exception"
//!   3 Rle         run count u32, runs (value i64, start u32, length u32)
//!   4 Delta       first value i64, then the n - 1 differences between
//!                 neighbours, themselves encoded (Plain, BitPack, Patched or Rle)
//!   5 Dictionary  value count u32, sorted distinct values (i64 each), then
//!                 each row's code (its value's position) encoded like Delta's
//!   6 Bitmap      value count u32, then per distinct value: the value (i64)
//!                 and a bitmap with bit i set where row i holds it
//! text
//!   0 Plain       n x (u32 length + UTF-8 bytes)
//!   5 Dictionary  value count u32, sorted distinct strings (u32 length +
//!                 bytes), then each row's code encoded like Delta's
//! ```
//!
//! The dictionaries are sorted, so codes compare like the values they stand
//! for (order preserving). Bitmap is only tried for at most
//! [`BITMAP_MAX_VALUES`] distinct values: its size grows with each one.
//!
//! [`filter`] compares a chunk's values with a literal without decoding them
//! into [`Value`]s, so a scan can find the matching rows first and decode the
//! other columns only for those (late materialization):
//!
//! ```text
//! Rle         one comparison per run
//! Bitmap      the bitmaps of the values that match, ORed together (for =,
//!             just the literal's bitmap)
//! Dictionary  binary search the sorted dictionary for the literal, then
//!             compare each row's code with that position: `x < 'oslo'`
//!             becomes `code < 1`, and the codes may be run-length encoded
//! others      decoded to integers or strings, then compared
//! ```

use std::collections::BTreeSet;

use crate::error::{Result, err};
use crate::predicate::Compare;
use crate::storage::sort_key;
use crate::types::{DataType, Value, ordered_real, real_from_ordered};

/// Most distinct values for which Bitmap encoding is tried.
pub const BITMAP_MAX_VALUES: usize = 8;

/// Fraction (in percent) of values that Patched packs without an exception.
const PATCHED_PERCENTILE: usize = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Plain,
    BitPack,
    Patched,
    Rle,
    Delta,
    Dictionary,
    Bitmap,
}

impl Encoding {
    fn tag(self) -> u8 {
        self as u8
    }

    fn from_tag(tag: u8) -> Result<Self> {
        use Encoding::*;
        [Plain, BitPack, Patched, Rle, Delta, Dictionary, Bitmap]
            .get(tag as usize)
            .copied()
            .ok_or_else(|| {
                crate::error::Error(format!("corrupted column chunk: unknown encoding {tag}"))
            })
    }
}

/// Whether bit `i` of a bitmap is set.
pub fn bit(bitmap: &[u8], i: usize) -> bool {
    bitmap[i / 8] & (1 << (i % 8)) != 0
}

/// Encodings for numbers inside Delta and Dictionary.
const INNER: [Encoding; 4] = [
    Encoding::Rle,
    Encoding::BitPack,
    Encoding::Patched,
    Encoding::Plain,
];
/// Every integer encoding, the cheapest to decode first so that it wins ties.
const ALL_INTS: [Encoding; 7] = [
    Encoding::Rle,
    Encoding::BitPack,
    Encoding::Delta,
    Encoding::Patched,
    Encoding::Dictionary,
    Encoding::Bitmap,
    Encoding::Plain,
];

/// Encodes one column's values, which must fit `data_type`.
pub fn encode(data_type: DataType, values: &[Value]) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8];
    if values.iter().any(Value::is_null) {
        bytes[0] = 1;
        bytes.resize(1 + values.len().div_ceil(8), 0);
        for (i, value) in values.iter().enumerate() {
            if value.is_null() {
                bytes[1 + i / 8] |= 1 << (i % 8);
            }
        }
    }
    let expected = |value: &Value| {
        crate::error::Error(format!("expected {data_type}, got {}", value.to_sql()))
    };
    match data_type {
        DataType::Text => {
            let texts = values
                .iter()
                .map(|value| match value {
                    Value::Null => Ok(""),
                    Value::Text(s) => Ok(s.as_str()),
                    value => Err(expected(value)),
                })
                .collect::<Result<Vec<&str>>>()?;
            bytes.extend(encode_texts(&texts));
        }
        DataType::Blob | DataType::Decimal(_) | DataType::Interval => {
            let texts = values
                .iter()
                .map(|value| match value {
                    Value::Null => Ok(String::new()),
                    value if value.fits(data_type) => Ok(keyed(value)),
                    value => Err(expected(value)),
                })
                .collect::<Result<Vec<String>>>()?;
            let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
            bytes.extend(encode_texts(&texts));
        }
        _ => {
            let ints = values
                .iter()
                .map(|value| match value {
                    Value::Null => Ok(0),
                    value => stored_int(value, data_type).ok_or_else(|| expected(value)),
                })
                .collect::<Result<Vec<i64>>>()?;
            bytes.extend(encode_ints(&ints, &ALL_INTS));
        }
    }
    Ok(bytes)
}

/// The integer a chunk stores a value of a type stored as integers as: a
/// BOOL as 0 or 1, a REAL as its ordered bits, a date or time as its count.
fn stored_int(value: &Value, data_type: DataType) -> Option<i64> {
    match (value, data_type) {
        (Value::Int(n), DataType::Int) => Some(*n),
        (Value::Bool(b), DataType::Bool) => Some(*b as i64),
        (Value::Real(r), DataType::Real) => Some(real_to_int(*r)),
        (Value::Date(d), DataType::Date) => Some(*d as i64),
        (Value::Time(t), DataType::Time)
        | (Value::Timestamp(t), DataType::Timestamp)
        | (Value::TimestampTz(t), DataType::TimestampTz) => Some(*t),
        _ => None,
    }
}

/// The value of a type stored as integers that a chunk stores as `n`.
fn from_stored_int(n: i64, data_type: DataType) -> Result<Value> {
    Ok(match data_type {
        DataType::Int => Value::Int(n),
        DataType::Real => Value::Real(int_to_real(n)),
        DataType::Bool => match n {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            v => return err(format!("corrupted column chunk: invalid BOOL {v}")),
        },
        DataType::Date => match i32::try_from(n) {
            Ok(d) => Value::Date(d),
            Err(_) => return err(format!("corrupted column chunk: invalid DATE {n}")),
        },
        DataType::Time => Value::Time(n),
        DataType::Timestamp => Value::Timestamp(n),
        DataType::TimestampTz => Value::TimestampTz(n),
        other => unreachable!("{other} is not stored as integers"),
    })
}

/// A BLOB, DECIMAL or INTERVAL as the text a chunk stores it as: its sort key's bytes
/// in hexadecimal, which compare as the values do, so that a chunk of them
/// is encoded and filtered as one of TEXTs.
fn keyed(value: &Value) -> String {
    let mut key = Vec::new();
    sort_key::encode(value, false, &mut key);
    crate::types::hex(&key)
}

/// The BLOB, DECIMAL or INTERVAL [`keyed`] made a text of.
fn from_keyed(text: &str, data_type: DataType) -> Result<Value> {
    let corrupted = || err(format!("corrupted column chunk: invalid {data_type}"));
    let bytes: Option<Vec<u8>> = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect();
    let Some(bytes) = bytes else {
        return corrupted();
    };
    let value = sort_key::decode(&bytes, &mut 0)?;
    value.coerce(data_type).map_or_else(corrupted, Ok)
}

/// Decodes a chunk of `n` values written by [`encode`].
pub fn decode(data_type: DataType, bytes: &[u8], n: usize) -> Result<Vec<Value>> {
    decode_selected(data_type, bytes, n, &vec![u8::MAX; n.div_ceil(8)])
}

/// Decodes only the rows whose bit is set in `selected`, in row order. The
/// others are never turned into [`Value`]s: text is copied out of the chunk
/// only for the selected rows.
pub fn decode_selected(
    data_type: DataType,
    bytes: &[u8],
    n: usize,
    selected: &[u8],
) -> Result<Vec<Value>> {
    let mut reader = Reader { bytes, pos: 0 };
    let nulls = read_nulls(&mut reader, n)?;
    let rows = (0..n).filter(|&i| bit(selected, i));
    let values = match data_type {
        DataType::Int => {
            let ints = decode_ints(&mut reader, n)?;
            rows.map(|i| {
                if nulls.is_some_and(|b| bit(b, i)) {
                    Value::Null
                } else {
                    Value::Int(ints[i])
                }
            })
            .collect()
        }
        DataType::Blob | DataType::Decimal(_) | DataType::Interval => {
            let texts = decode_texts(&mut reader, n)?;
            rows.map(|i| match nulls.is_some_and(|b| bit(b, i)) {
                true => Ok(Value::Null),
                false => from_keyed(texts[i], data_type),
            })
            .collect::<Result<_>>()?
        }
        DataType::Real
        | DataType::Bool
        | DataType::Date
        | DataType::Time
        | DataType::Timestamp
        | DataType::TimestampTz => {
            let ints = decode_ints(&mut reader, n)?;
            rows.map(|i| match nulls.is_some_and(|b| bit(b, i)) {
                true => Ok(Value::Null),
                false => from_stored_int(ints[i], data_type),
            })
            .collect::<Result<_>>()?
        }
        DataType::Text => {
            let texts = decode_texts(&mut reader, n)?;
            rows.map(|i| {
                if nulls.is_some_and(|b| bit(b, i)) {
                    Value::Null
                } else {
                    Value::Text(texts[i].to_owned())
                }
            })
            .collect()
        }
    };
    reader.finish()?;
    Ok(values)
}

/// Returns a bitmap of the rows of a chunk of `n` values whose value `v`
/// satisfies `v op literal`; a NULL row never does. The literal must be a
/// non-NULL value of the chunk's type. The values are compared while still
/// encoded where the encoding allows it (see the module docs).
pub fn filter(
    data_type: DataType,
    bytes: &[u8],
    n: usize,
    op: Compare,
    literal: &Value,
) -> Result<Vec<u8>> {
    let mut reader = Reader { bytes, pos: 0 };
    let nulls = read_nulls(&mut reader, n)?;
    let mut matching = match (data_type, literal) {
        (DataType::Text, Value::Text(s)) => filter_texts(&mut reader, n, op, s)?,
        (DataType::Blob | DataType::Decimal(_) | DataType::Interval, literal)
            if literal.fits(data_type) =>
        {
            filter_texts(&mut reader, n, op, &keyed(literal))?
        }
        (_, literal) if stored_int(literal, data_type).is_some() => {
            let v = stored_int(literal, data_type).expect("checked");
            filter_ints(&mut reader, n, op, v)?
        }
        (_, literal) => {
            return err(format!(
                "cannot compare a {data_type} column with {}",
                literal.to_sql()
            ));
        }
    };
    reader.finish()?;
    if let Some(nulls) = nulls {
        matching
            .iter_mut()
            .zip(nulls)
            .for_each(|(m, null)| *m &= !null);
    }
    Ok(matching)
}

/// A REAL as the INT a chunk stores it as: its [`ordered_real`] bits, which
/// compare as the REALs do, as a signed number, so that a chunk of REALs is
/// encoded and filtered as one of INTs.
fn real_to_int(r: f64) -> i64 {
    (ordered_real(r) ^ 1 << 63) as i64
}

fn int_to_real(n: i64) -> f64 {
    real_from_ordered(n as u64 ^ 1 << 63)
}

/// Reads the null flag and, if there is one, the null bitmap.
fn read_nulls<'a>(reader: &mut Reader<'a>, n: usize) -> Result<Option<&'a [u8]>> {
    match reader.u8()? {
        0 => Ok(None),
        1 => Ok(Some(reader.take(n.div_ceil(8))?)),
        flag => err(format!("corrupted column chunk: null flag {flag}")),
    }
}

/// Sets bits `start..start + len` of a bitmap.
fn set_bits(bitmap: &mut [u8], start: usize, len: usize) {
    (start..start + len).for_each(|i| bitmap[i / 8] |= 1 << (i % 8));
}

/// Translates `value op literal` into a test on dictionary codes, given the
/// sorted dictionary's values below the literal (`lo`) and up to it (`hi`):
/// the literal's code is `lo` if `hi > lo`, and absent otherwise. Codes are
/// never negative, so `code < 0` matches nothing and `code >= 0` everything.
fn code_test(op: Compare, lo: usize, hi: usize) -> (Compare, i64) {
    let (lo, hi, found) = (lo as i64, hi as i64, hi > lo);
    match op {
        Compare::Eq if found => (Compare::Eq, lo),
        Compare::Eq => (Compare::Lt, 0),
        Compare::NotEq if found => (Compare::NotEq, lo),
        Compare::NotEq => (Compare::GtEq, 0),
        Compare::Lt => (Compare::Lt, lo),
        Compare::LtEq => (Compare::Lt, hi),
        Compare::Gt => (Compare::GtEq, hi),
        Compare::GtEq => (Compare::GtEq, lo),
    }
}

/// Which encoding a chunk of `n` values uses.
pub fn encoding_of(bytes: &[u8], n: usize) -> Result<Encoding> {
    let at = match bytes.first() {
        Some(1) => 1 + n.div_ceil(8),
        _ => 1,
    };
    match bytes.get(at) {
        Some(&tag) => Encoding::from_tag(tag),
        None => err("corrupted column chunk: too short"),
    }
}

// ---- integers ----

/// Encodes with each of `candidates` that applies and returns the smallest result.
fn encode_ints(values: &[i64], candidates: &[Encoding]) -> Vec<u8> {
    candidates
        .iter()
        .filter_map(|&encoding| {
            let mut bytes = vec![encoding.tag()];
            encode_ints_with(encoding, values, &mut bytes).then_some(bytes)
        })
        .min_by_key(Vec::len)
        .expect("Plain always applies")
}

/// Appends `values` in `encoding`. Returns false, leaving `out` in any
/// state, if the encoding does not apply to them.
fn encode_ints_with(encoding: Encoding, values: &[i64], out: &mut Vec<u8>) -> bool {
    match encoding {
        Encoding::Plain => values
            .iter()
            .for_each(|v| out.extend_from_slice(&v.to_le_bytes())),
        Encoding::BitPack => {
            let base = values.iter().copied().min().unwrap_or(0);
            let offsets: Vec<u64> = values
                .iter()
                .map(|&v| v.wrapping_sub(base) as u64)
                .collect();
            let width = offsets.iter().map(|&o| bit_width(o)).max().unwrap_or(0);
            out.extend_from_slice(&base.to_le_bytes());
            out.push(width);
            pack(&offsets, width, out);
        }
        Encoding::Patched => {
            let base = values.iter().copied().min().unwrap_or(0);
            let offsets: Vec<u64> = values
                .iter()
                .map(|&v| v.wrapping_sub(base) as u64)
                .collect();
            let mut sorted = offsets.clone();
            sorted.sort_unstable();
            // Wide enough for most values, with the marker above all of them.
            let Some(&p) = sorted.get(values.len() * PATCHED_PERCENTILE / 100) else {
                return false;
            };
            let width = bit_width(p.saturating_add(1));
            if width >= 64 {
                return false;
            }
            let marker = (1u64 << width) - 1;
            let mut exceptions = Vec::new();
            let packed: Vec<u64> = offsets
                .iter()
                .zip(values)
                .map(|(&offset, &value)| {
                    if offset >= marker {
                        exceptions.push(value);
                        marker
                    } else {
                        offset
                    }
                })
                .collect();
            out.extend_from_slice(&base.to_le_bytes());
            out.push(width);
            pack(&packed, width, out);
            out.extend_from_slice(&(exceptions.len() as u32).to_le_bytes());
            exceptions
                .iter()
                .for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        }
        Encoding::Rle => {
            let mut runs: Vec<(i64, u32, u32)> = Vec::new();
            for (i, &value) in values.iter().enumerate() {
                match runs.last_mut() {
                    Some((v, _, len)) if *v == value => *len += 1,
                    _ => runs.push((value, i as u32, 1)),
                }
            }
            out.extend_from_slice(&(runs.len() as u32).to_le_bytes());
            for (value, start, len) in runs {
                out.extend_from_slice(&value.to_le_bytes());
                out.extend_from_slice(&start.to_le_bytes());
                out.extend_from_slice(&len.to_le_bytes());
            }
        }
        Encoding::Delta => {
            let Some(&first) = values.first() else {
                return false;
            };
            let deltas: Vec<i64> = values.windows(2).map(|w| w[1].wrapping_sub(w[0])).collect();
            out.extend_from_slice(&first.to_le_bytes());
            out.extend(encode_ints(&deltas, &INNER));
        }
        Encoding::Dictionary => {
            let dictionary: Vec<i64> = values
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let codes: Vec<i64> = values
                .iter()
                .map(|v| dictionary.binary_search(v).unwrap() as i64)
                .collect();
            out.extend_from_slice(&(dictionary.len() as u32).to_le_bytes());
            dictionary
                .iter()
                .for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
            out.extend(encode_ints(&codes, &INNER));
        }
        Encoding::Bitmap => {
            let distinct: BTreeSet<i64> = values.iter().copied().collect();
            if distinct.len() > BITMAP_MAX_VALUES {
                return false;
            }
            out.extend_from_slice(&(distinct.len() as u32).to_le_bytes());
            for value in distinct {
                out.extend_from_slice(&value.to_le_bytes());
                let mut bitmap = vec![0u8; values.len().div_ceil(8)];
                for (i, _) in values.iter().enumerate().filter(|(_, v)| **v == value) {
                    bitmap[i / 8] |= 1 << (i % 8);
                }
                out.extend(bitmap);
            }
        }
    }
    true
}

fn decode_ints(reader: &mut Reader, n: usize) -> Result<Vec<i64>> {
    match Encoding::from_tag(reader.u8()?)? {
        Encoding::Plain => (0..n).map(|_| reader.i64()).collect(),
        Encoding::BitPack => {
            let base = reader.i64()?;
            let width = reader.width()?;
            let offsets = unpack(reader, width, n)?;
            Ok(offsets
                .into_iter()
                .map(|o| base.wrapping_add(o as i64))
                .collect())
        }
        Encoding::Patched => {
            let base = reader.i64()?;
            let width = reader.width()?;
            if !(1..64).contains(&width) {
                return err(format!("corrupted column chunk: patched width {width}"));
            }
            let packed = unpack(reader, width, n)?;
            let count = reader.u32()? as usize;
            let mut exceptions = (0..count)
                .map(|_| reader.i64())
                .collect::<Result<Vec<_>>>()?
                .into_iter();
            let marker = (1u64 << width) - 1;
            let values = packed
                .into_iter()
                .map(|p| {
                    if p == marker {
                        exceptions
                            .next()
                            .ok_or_else(|| corrupted("missing exception"))
                    } else {
                        Ok(base.wrapping_add(p as i64))
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            if exceptions.next().is_some() {
                return err("corrupted column chunk: unused exception");
            }
            Ok(values)
        }
        Encoding::Rle => {
            let mut values = Vec::with_capacity(n);
            for _ in 0..reader.u32()? {
                let (value, start, len) = (
                    reader.i64()?,
                    reader.u32()? as usize,
                    reader.u32()? as usize,
                );
                if start != values.len() || start + len > n {
                    return err("corrupted column chunk: bad run");
                }
                values.resize(start + len, value);
            }
            Ok(values)
        }
        Encoding::Delta => {
            if n == 0 {
                return err("corrupted column chunk: delta of no values");
            }
            let first = reader.i64()?;
            let mut values = vec![first];
            for delta in decode_ints(reader, n - 1)? {
                values.push(values.last().unwrap().wrapping_add(delta));
            }
            Ok(values)
        }
        Encoding::Dictionary => {
            let dictionary = (0..reader.u32()?)
                .map(|_| reader.i64())
                .collect::<Result<Vec<_>>>()?;
            decode_codes(reader, n, &dictionary)
        }
        Encoding::Bitmap => {
            let mut values: Vec<Option<i64>> = vec![None; n];
            for _ in 0..reader.u32()? {
                let value = reader.i64()?;
                let bitmap = reader.take(n.div_ceil(8))?;
                for (i, slot) in values.iter_mut().enumerate() {
                    if bitmap[i / 8] & (1 << (i % 8)) != 0 && slot.replace(value).is_some() {
                        return err("corrupted column chunk: overlapping bitmaps");
                    }
                }
            }
            values
                .into_iter()
                .map(|v| v.ok_or_else(|| corrupted("row in no bitmap")))
                .collect()
        }
    }
}

/// Returns a bitmap of the `n` values for which `value op literal` holds.
fn filter_ints(reader: &mut Reader, n: usize, op: Compare, literal: i64) -> Result<Vec<u8>> {
    let mut matching = vec![0u8; n.div_ceil(8)];
    let start = reader.pos;
    match Encoding::from_tag(reader.u8()?)? {
        Encoding::Rle => {
            let mut next = 0;
            for _ in 0..reader.u32()? {
                let (value, start, len) = (
                    reader.i64()?,
                    reader.u32()? as usize,
                    reader.u32()? as usize,
                );
                if start != next || start + len > n {
                    return err("corrupted column chunk: bad run");
                }
                if op.holds(value.cmp(&literal)) {
                    set_bits(&mut matching, start, len);
                }
                next = start + len;
            }
            if next != n {
                return err("corrupted column chunk: bad run");
            }
        }
        Encoding::Bitmap => {
            for _ in 0..reader.u32()? {
                let value = reader.i64()?;
                let bitmap = reader.take(n.div_ceil(8))?;
                if op.holds(value.cmp(&literal)) {
                    matching.iter_mut().zip(bitmap).for_each(|(m, b)| *m |= b);
                }
            }
        }
        Encoding::Dictionary => {
            let dictionary = (0..reader.u32()?)
                .map(|_| reader.i64())
                .collect::<Result<Vec<_>>>()?;
            let lo = dictionary.partition_point(|&v| v < literal);
            let hi = dictionary.partition_point(|&v| v <= literal);
            let (op, code) = code_test(op, lo, hi);
            return filter_ints(reader, n, op, code);
        }
        Encoding::Plain | Encoding::BitPack | Encoding::Patched | Encoding::Delta => {
            reader.pos = start;
            for (i, value) in decode_ints(reader, n)?.into_iter().enumerate() {
                if op.holds(value.cmp(&literal)) {
                    set_bits(&mut matching, i, 1);
                }
            }
        }
    }
    Ok(matching)
}

/// Reads codes and looks each one up in the dictionary.
fn decode_codes<T: Clone>(reader: &mut Reader, n: usize, dictionary: &[T]) -> Result<Vec<T>> {
    decode_ints(reader, n)?
        .into_iter()
        .map(|code| {
            usize::try_from(code)
                .ok()
                .and_then(|i| dictionary.get(i))
                .cloned()
                .ok_or_else(|| corrupted("bad code"))
        })
        .collect()
}

// ---- text ----

fn encode_texts(values: &[&str]) -> Vec<u8> {
    let plain = {
        let mut bytes = vec![Encoding::Plain.tag()];
        values.iter().for_each(|s| put_str(&mut bytes, s));
        bytes
    };
    let dictionary = {
        let dictionary: Vec<&str> = values
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let codes: Vec<i64> = values
            .iter()
            .map(|v| dictionary.binary_search(v).unwrap() as i64)
            .collect();
        let mut bytes = vec![Encoding::Dictionary.tag()];
        bytes.extend_from_slice(&(dictionary.len() as u32).to_le_bytes());
        dictionary.iter().for_each(|s| put_str(&mut bytes, s));
        bytes.extend(encode_ints(&codes, &INNER));
        bytes
    };
    if dictionary.len() < plain.len() {
        dictionary
    } else {
        plain
    }
}

/// Returns each row's string, borrowed from the chunk.
fn decode_texts<'a>(reader: &mut Reader<'a>, n: usize) -> Result<Vec<&'a str>> {
    match Encoding::from_tag(reader.u8()?)? {
        Encoding::Plain => (0..n).map(|_| reader.str()).collect(),
        Encoding::Dictionary => {
            let dictionary = (0..reader.u32()?)
                .map(|_| reader.str())
                .collect::<Result<Vec<_>>>()?;
            decode_codes(reader, n, &dictionary)
        }
        encoding => err(format!(
            "corrupted column chunk: {encoding:?} is not a text encoding"
        )),
    }
}

/// Returns a bitmap of the `n` strings for which `string op literal` holds.
fn filter_texts(reader: &mut Reader, n: usize, op: Compare, literal: &str) -> Result<Vec<u8>> {
    match Encoding::from_tag(reader.u8()?)? {
        Encoding::Dictionary => {
            let dictionary = (0..reader.u32()?)
                .map(|_| reader.str())
                .collect::<Result<Vec<_>>>()?;
            let lo = dictionary.partition_point(|&s| s < literal);
            let hi = dictionary.partition_point(|&s| s <= literal);
            let (op, code) = code_test(op, lo, hi);
            filter_ints(reader, n, op, code)
        }
        _ => {
            reader.pos -= 1;
            let mut matching = vec![0u8; n.div_ceil(8)];
            for (i, s) in decode_texts(reader, n)?.into_iter().enumerate() {
                if op.holds(s.cmp(literal)) {
                    set_bits(&mut matching, i, 1);
                }
            }
            Ok(matching)
        }
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

// ---- bit packing ----

/// Bits needed to store `value`: 0 for 0.
fn bit_width(value: u64) -> u8 {
    (64 - value.leading_zeros()) as u8
}

/// Appends each value's low `width` bits, least significant bit first.
fn pack(values: &[u64], width: u8, out: &mut Vec<u8>) {
    let start = out.len();
    out.resize(start + (values.len() * width as usize).div_ceil(8), 0);
    for (i, &value) in values.iter().enumerate() {
        for bit in 0..width as usize {
            if value >> bit & 1 != 0 {
                let at = i * width as usize + bit;
                out[start + at / 8] |= 1 << (at % 8);
            }
        }
    }
}

fn unpack(reader: &mut Reader, width: u8, n: usize) -> Result<Vec<u64>> {
    let bytes = reader.take((n * width as usize).div_ceil(8))?;
    Ok((0..n)
        .map(|i| {
            (0..width as usize).fold(0u64, |value, bit| {
                let at = i * width as usize + bit;
                value | ((bytes[at / 8] >> (at % 8) & 1) as u64) << bit
            })
        })
        .collect())
}

// ---- reading ----

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let Some(slice) = self.bytes.get(self.pos..self.pos + n) else {
            return err("corrupted column chunk: too short");
        };
        self.pos += n;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn width(&mut self) -> Result<u8> {
        match self.u8()? {
            width @ 0..=64 => Ok(width),
            width => err(format!("corrupted column chunk: bit width {width}")),
        }
    }

    fn str(&mut self) -> Result<&'a str> {
        let len = self.u32()? as usize;
        match std::str::from_utf8(self.take(len)?) {
            Ok(s) => Ok(s),
            Err(_) => err("corrupted column chunk: invalid UTF-8"),
        }
    }

    fn finish(&self) -> Result<()> {
        if self.pos != self.bytes.len() {
            return err("corrupted column chunk: trailing bytes");
        }
        Ok(())
    }
}

fn corrupted(what: &str) -> crate::error::Error {
    crate::error::Error(format!("corrupted column chunk: {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A number in 0..1000 with no pattern in `i`.
    fn noise(i: i64) -> i64 {
        (i.wrapping_mul(0x9E37_79B9_7F4A_7C15_u64 as i64) as u64 >> 40) as i64 % 1000
    }

    fn ints(values: impl IntoIterator<Item = i64>) -> Vec<Value> {
        values.into_iter().map(Value::Int).collect()
    }

    /// Encodes, checks the round trip, and returns the chosen encoding and size.
    fn round_trip(data_type: DataType, values: &[Value]) -> (Encoding, usize) {
        let bytes = encode(data_type, values).unwrap();
        assert_eq!(decode(data_type, &bytes, values.len()).unwrap(), values);
        (encoding_of(&bytes, values.len()).unwrap(), bytes.len())
    }

    /// The chunk's size with one encoding forced.
    fn size_with(encoding: Encoding, values: &[i64]) -> usize {
        let mut bytes = vec![encoding.tag()];
        assert!(encode_ints_with(encoding, values, &mut bytes));
        let mut reader = Reader {
            bytes: &bytes,
            pos: 0,
        };
        assert_eq!(
            decode_ints(&mut reader, values.len()).unwrap(),
            values,
            "{encoding:?}"
        );
        reader.finish().unwrap();
        bytes.len()
    }

    #[test]
    fn every_encoding_round_trips() {
        let samples: Vec<Vec<i64>> = vec![
            vec![7],
            vec![0, 0, 0, 0],
            vec![i64::MIN, i64::MAX, 0, -1, 1],
            (0..1000).collect(),
            (0..1000).map(|i| i % 3).collect(),
            (0..1000)
                .map(|i| if i % 50 == 0 { 1 << 40 } else { i % 16 })
                .collect(),
            (0..1000).map(|i| -i * 1_000_003).collect(),
        ];
        for values in &samples {
            for encoding in ALL_INTS {
                let mut bytes = vec![encoding.tag()];
                if encode_ints_with(encoding, values, &mut bytes) {
                    size_with(encoding, values);
                }
            }
        }
    }

    #[test]
    fn the_smallest_encoding_is_chosen() {
        // A sorted column of few values: one run each.
        let runs = ints((0..1000).map(|i| i / 250));
        assert_eq!(round_trip(DataType::Int, &runs).0, Encoding::Rle);

        // Small numbers in no order: a few bits each.
        let small = ints((0..1000).map(noise));
        let (encoding, size) = round_trip(DataType::Int, &small);
        assert_eq!(encoding, Encoding::BitPack);
        assert!(size < 1000 * 2, "10 bits per value, not 64: {size}");

        // Increasing ids far from zero: differences of one.
        let ids = ints((0..1000).map(|i| 1_000_000_000 + i));
        assert_eq!(round_trip(DataType::Int, &ids).0, Encoding::Delta);

        // Mostly small and all different, with a few huge outliers.
        let outliers = ints((0..1000).map(|i| if i % 100 == 0 { 1 << 50 } else { noise(i) }));
        assert_eq!(round_trip(DataType::Int, &outliers).0, Encoding::Patched);

        // Few distinct values that are far apart and mixed.
        let sparse = ints((0..1000).map(|i| [i64::MIN, 0, i64::MAX][noise(i) as usize % 3]));
        let (encoding, _) = round_trip(DataType::Int, &sparse);
        assert!(
            matches!(encoding, Encoding::Dictionary | Encoding::Bitmap),
            "{encoding:?}"
        );
    }

    #[test]
    fn patched_beats_bit_packing_with_outliers() {
        let values: Vec<i64> = (0..1000)
            .map(|i| if i % 100 == 0 { 1 << 50 } else { i % 8 })
            .collect();
        assert!(size_with(Encoding::Patched, &values) * 5 < size_with(Encoding::BitPack, &values));
    }

    #[test]
    fn bitmap_is_only_tried_for_few_values() {
        let few: Vec<i64> = (0..100).map(|i| i % 4).collect();
        size_with(Encoding::Bitmap, &few);
        let many: Vec<i64> = (0..100).collect();
        assert!(!encode_ints_with(Encoding::Bitmap, &many, &mut Vec::new()));
    }

    #[test]
    fn text_uses_an_order_preserving_dictionary() {
        let cities = ["paris", "tokyo", "lima", "oslo"];
        let values: Vec<Value> = (0..500)
            .map(|i| Value::Text(cities[i % 4].into()))
            .collect();
        let (encoding, size) = round_trip(DataType::Text, &values);
        assert_eq!(encoding, Encoding::Dictionary);
        assert!(size < 300, "{size}");

        // Codes follow the sorted order of the strings.
        let bytes = encode(DataType::Text, &values).unwrap();
        let mut reader = Reader {
            bytes: &bytes[2..],
            pos: 0,
        };
        let dictionary: Vec<&str> = (0..reader.u32().unwrap())
            .map(|_| reader.str().unwrap())
            .collect();
        assert_eq!(dictionary, ["lima", "oslo", "paris", "tokyo"]);

        let unique: Vec<Value> = (0..100).map(|i| Value::Text(format!("row {i}"))).collect();
        assert_eq!(round_trip(DataType::Text, &unique).0, Encoding::Plain);
    }

    #[test]
    fn nulls_and_bools_round_trip() {
        let values = vec![Value::Int(5), Value::Null, Value::Int(-5), Value::Null];
        round_trip(DataType::Int, &values);
        let flags: Vec<Value> = (0..100)
            .map(|i| {
                if i % 7 == 0 {
                    Value::Null
                } else {
                    Value::Bool(i % 3 == 0)
                }
            })
            .collect();
        round_trip(DataType::Bool, &flags);
        let texts = vec![
            Value::Null,
            Value::Text("".into()),
            Value::Text("ünïcode".into()),
        ];
        round_trip(DataType::Text, &texts);
        round_trip(DataType::Int, &[]);
    }

    const OPS: [Compare; 6] = [
        Compare::Eq,
        Compare::NotEq,
        Compare::Lt,
        Compare::LtEq,
        Compare::Gt,
        Compare::GtEq,
    ];

    /// The rows of `values` for which `value op literal` holds, as a filter
    /// that decodes everything would find them.
    fn expected(values: &[Value], op: Compare, literal: &Value) -> Vec<usize> {
        (0..values.len())
            .filter(|&i| values[i].compare(literal).is_some_and(|ord| op.holds(ord)))
            .collect()
    }

    fn rows_of(bitmap: &[u8], n: usize) -> Vec<usize> {
        assert_eq!(bitmap.len(), n.div_ceil(8));
        (0..n).filter(|&i| bit(bitmap, i)).collect()
    }

    #[test]
    fn filters_match_decoding_in_every_encoding() {
        let samples: Vec<Vec<i64>> = vec![
            vec![7],
            vec![0, 0, 0, 0],
            vec![i64::MIN, i64::MAX, 0, -1, 1],
            (0..300).collect(),
            (0..300).map(|i| i / 100 * 10).collect(),
            (0..300)
                .map(|i| [-20, 0, 50][noise(i) as usize % 3])
                .collect(),
            (0..300)
                .map(|i| if i % 50 == 0 { 1 << 40 } else { i % 16 })
                .collect(),
        ];
        for values in &samples {
            let literals: BTreeSet<i64> = values
                .iter()
                .flat_map(|&v| [v.saturating_sub(1), v, v.saturating_add(1)])
                .chain([i64::MIN, i64::MAX])
                .collect();
            for encoding in ALL_INTS {
                let mut chunk = vec![0, encoding.tag()];
                if !encode_ints_with(encoding, values, &mut chunk) {
                    continue;
                }
                let decoded = decode(DataType::Int, &chunk, values.len()).unwrap();
                for &literal in &literals {
                    for op in OPS {
                        let literal = Value::Int(literal);
                        let matching =
                            filter(DataType::Int, &chunk, values.len(), op, &literal).unwrap();
                        assert_eq!(
                            rows_of(&matching, values.len()),
                            expected(&decoded, op, &literal),
                            "{encoding:?} {op:?} {literal:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn dictionary_codes_are_compared_in_place_of_strings() {
        let cities = ["lima", "oslo", "paris", "tokyo"];
        let dictionary: Vec<Value> = (0..500)
            .map(|i| {
                if i % 9 == 0 {
                    Value::Null
                } else {
                    Value::Text(cities[noise(i) as usize % 4].into())
                }
            })
            .collect();
        let runs: Vec<Value> = (0..500)
            .map(|i| Value::Text(cities[i / 125].into()))
            .collect();
        let plain: Vec<Value> = (0..100)
            .map(|i| Value::Text(format!("row {i:03}")))
            .collect();
        let cases = [
            (&dictionary, Encoding::Dictionary),
            (&runs, Encoding::Dictionary),
            (&plain, Encoding::Plain),
        ];
        for (values, encoding) in cases {
            let chunk = encode(DataType::Text, values).unwrap();
            assert_eq!(encoding_of(&chunk, values.len()).unwrap(), encoding);
            // Present, absent, before the first and after the last string.
            for literal in ["lima", "oslo", "m", "a", "zz", "row 050", "row 05"] {
                for op in OPS {
                    let literal = Value::Text(literal.into());
                    let matching =
                        filter(DataType::Text, &chunk, values.len(), op, &literal).unwrap();
                    assert_eq!(
                        rows_of(&matching, values.len()),
                        expected(values, op, &literal),
                        "{op:?} {literal:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn null_rows_never_match() {
        let values: Vec<Value> = (0..100)
            .map(|i| {
                if i % 3 == 0 {
                    Value::Null
                } else {
                    Value::Int(i % 2)
                }
            })
            .collect();
        let chunk = encode(DataType::Int, &values).unwrap();
        let matching = filter(DataType::Int, &chunk, 100, Compare::NotEq, &Value::Int(7)).unwrap();
        assert_eq!(
            rows_of(&matching, 100),
            (0..100).filter(|i| i % 3 != 0).collect::<Vec<_>>()
        );

        let flags: Vec<Value> = (0..100)
            .map(|i| {
                if i % 4 == 0 {
                    Value::Null
                } else {
                    Value::Bool(i % 3 == 0)
                }
            })
            .collect();
        let chunk = encode(DataType::Bool, &flags).unwrap();
        for op in OPS {
            for literal in [Value::Bool(false), Value::Bool(true)] {
                let matching = filter(DataType::Bool, &chunk, 100, op, &literal).unwrap();
                assert_eq!(rows_of(&matching, 100), expected(&flags, op, &literal));
            }
        }
        assert!(filter(DataType::Bool, &chunk, 100, Compare::Eq, &Value::Int(1)).is_err());
        assert!(
            filter(
                DataType::Int,
                &encode(DataType::Int, &[]).unwrap(),
                0,
                Compare::Eq,
                &Value::Null
            )
            .is_err()
        );
    }

    #[test]
    fn selected_rows_are_decoded_alone() {
        let values: Vec<Value> = (0..20)
            .map(|i| {
                if i % 5 == 0 {
                    Value::Null
                } else {
                    Value::Text(format!("v{}", i % 3))
                }
            })
            .collect();
        let chunk = encode(DataType::Text, &values).unwrap();
        let selected = [0b0010_0001, 0b0000_0100, 0b1000];
        let rows: Vec<Value> = [0, 5, 10, 19].iter().map(|&i| values[i].clone()).collect();
        assert_eq!(
            decode_selected(DataType::Text, &chunk, 20, &selected).unwrap(),
            rows
        );
    }

    #[test]
    fn rejects_wrong_types_and_corrupted_chunks() {
        assert!(encode(DataType::Int, &[Value::Text("x".into())]).is_err());
        let bytes = encode(DataType::Int, &ints(0..100)).unwrap();
        assert!(decode(DataType::Int, &bytes[..bytes.len() - 1], 100).is_err());
        assert!(decode(DataType::Int, &[bytes.clone(), vec![0]].concat(), 100).is_err());
        assert!(
            decode(
                DataType::Bool,
                &encode(DataType::Int, &ints([2])).unwrap(),
                1
            )
            .is_err()
        );
    }
}
