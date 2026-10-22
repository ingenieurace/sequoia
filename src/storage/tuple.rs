//! Row encoding: converts a row to bytes for storage in a page, and back.
//!
//! ```text
//! [null bitmap][column 1][column 2]...
//!
//! null bitmap  ceil(n / 8) bytes; bit i set = column i is NULL and stores no bytes
//! INT          8 bytes, little-endian
//! BOOL         1 byte, 0 or 1
//! TEXT         u16 length (little-endian) + UTF-8 bytes
//! REAL         8 bytes, little-endian
//! ```
//!
//! The schema is not stored with the row: decoding needs the columns used to encode it.
//! Values are copied out when decoding, so no alignment padding is needed.

use crate::error::{Result, err};
use crate::types::{Column, DataType, Value, ordered_real};

pub fn encode(columns: &[Column], row: &[Value]) -> Result<Vec<u8>> {
    if row.len() != columns.len() {
        return err(format!(
            "expected {} values, got {}",
            columns.len(),
            row.len()
        ));
    }

    let mut bytes = vec![0u8; columns.len().div_ceil(8)];
    for (i, (column, value)) in columns.iter().zip(row).enumerate() {
        match (value, column.data_type) {
            (Value::Null, _) => bytes[i / 8] |= 1 << (i % 8),
            (Value::Int(n), DataType::Int) => bytes.extend_from_slice(&n.to_le_bytes()),
            (Value::Bool(b), DataType::Bool) => bytes.push(*b as u8),
            (Value::Real(r), DataType::Real) => bytes.extend_from_slice(&r.to_le_bytes()),
            (Value::Text(s), DataType::Text) => {
                let Ok(len) = u16::try_from(s.len()) else {
                    return err(format!(
                        "text in column {} is longer than {} bytes",
                        column.name,
                        u16::MAX
                    ));
                };
                bytes.extend_from_slice(&len.to_le_bytes());
                bytes.extend_from_slice(s.as_bytes());
            }
            (value, data_type) => {
                return err(format!(
                    "column {} expects {data_type}, got {}",
                    column.name,
                    value.to_sql()
                ));
            }
        }
    }
    Ok(bytes)
}

pub fn decode(columns: &[Column], bytes: &[u8]) -> Result<Vec<Value>> {
    decode_with(columns, bytes, |_| true)
}

/// Decodes only the columns marked in `needed`, skipping the bytes of the
/// others, which come out NULL: a TEXT value skipped is never copied out.
pub fn decode_columns(columns: &[Column], bytes: &[u8], needed: &[bool]) -> Result<Vec<Value>> {
    decode_with(columns, bytes, |i| needed[i])
}

/// Decodes the row at the start of `bytes`, and returns it with where it
/// ends: what follows it is not part of it.
pub fn decode_prefix(columns: &[Column], bytes: &[u8]) -> Result<(Vec<Value>, usize)> {
    decode_from(columns, bytes, |_| true)
}

fn decode_with(
    columns: &[Column],
    bytes: &[u8],
    needed: impl Fn(usize) -> bool,
) -> Result<Vec<Value>> {
    let (row, end) = decode_from(columns, bytes, needed)?;
    if end != bytes.len() {
        return err("corrupted row: trailing bytes");
    }
    Ok(row)
}

fn decode_from(
    columns: &[Column],
    bytes: &[u8],
    needed: impl Fn(usize) -> bool,
) -> Result<(Vec<Value>, usize)> {
    let mut reader = Reader { bytes, pos: 0 };
    let bitmap = reader.take(columns.len().div_ceil(8))?;

    let mut row = Vec::with_capacity(columns.len());
    for (i, column) in columns.iter().enumerate() {
        if bitmap[i / 8] & (1 << (i % 8)) != 0 {
            row.push(Value::Null);
            continue;
        }
        if !needed(i) {
            match column.data_type {
                DataType::Int | DataType::Real => reader.take(8)?,
                DataType::Bool => reader.take(1)?,
                DataType::Text => {
                    let len = u16::from_le_bytes(reader.array()?) as usize;
                    reader.take(len)?
                }
            };
            row.push(Value::Null);
            continue;
        }
        let value = match column.data_type {
            DataType::Int => Value::Int(i64::from_le_bytes(reader.array()?)),
            DataType::Real => Value::Real(f64::from_le_bytes(reader.array()?)),
            DataType::Bool => match reader.array::<1>()? {
                [0] => Value::Bool(false),
                [1] => Value::Bool(true),
                [b] => return err(format!("corrupted row: invalid BOOL byte {b}")),
            },
            DataType::Text => {
                let len = u16::from_le_bytes(reader.array()?) as usize;
                match std::str::from_utf8(reader.take(len)?) {
                    Ok(s) => Value::Text(s.to_owned()),
                    Err(_) => return err("corrupted row: invalid UTF-8"),
                }
            }
        };
        row.push(value);
    }
    Ok((row, reader.pos))
}

/// Encodes a key value so that comparing the bytes orders keys like the values.
///
/// INT is 8 bytes big-endian with the sign bit flipped, TEXT its UTF-8 bytes,
/// BOOL one byte, REAL its [`ordered_real`] bits big-endian. All keys of a
/// table have the same type, so no tag is needed.
pub fn encode_key(value: &Value) -> Vec<u8> {
    match value {
        Value::Int(n) => ((*n as u64) ^ (1 << 63)).to_be_bytes().to_vec(),
        Value::Text(s) => s.as_bytes().to_vec(),
        Value::Bool(b) => vec![*b as u8],
        Value::Real(r) => ordered_real(*r).to_be_bytes().to_vec(),
        Value::Null => panic!("NULL cannot be a key"),
    }
}

/// Decodes an INT key made by [`encode_key`].
pub fn decode_int_key(bytes: &[u8]) -> Result<i64> {
    match <[u8; 8]>::try_from(bytes) {
        Ok(bytes) => Ok((u64::from_be_bytes(bytes) ^ (1 << 63)) as i64),
        Err(_) => err("corrupted INT key"),
    }
}

/// Reads bytes sequentially, failing instead of panicking on truncated data.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let Some(slice) = self.bytes.get(self.pos..self.pos + n) else {
            return err("corrupted row: unexpected end of data");
        };
        self.pos += n;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, data_type: DataType) -> Column {
        Column {
            name: name.into(),
            data_type,
            primary_key: false,
        }
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn byte_layout_matches_the_spec() {
        let columns = [
            column("a", DataType::Int),
            column("b", DataType::Text),
            column("c", DataType::Bool),
            column("d", DataType::Text),
        ];
        let row = [Value::Int(1), Value::Null, Value::Bool(true), text("hi")];

        let mut expected = vec![0b0000_0010]; // column b is NULL
        expected.extend_from_slice(&1i64.to_le_bytes());
        expected.push(1);
        expected.extend_from_slice(&[2, 0, b'h', b'i']);
        assert_eq!(encode(&columns, &row).unwrap(), expected);
    }

    #[test]
    fn round_trips_every_type_across_a_multi_byte_bitmap() {
        let types = [DataType::Int, DataType::Text, DataType::Bool];
        let columns: Vec<Column> = (0..10)
            .map(|i| column(&format!("c{i}"), types[i % 3]))
            .collect();
        let row = vec![
            Value::Int(i64::MIN),
            text(""),
            Value::Bool(false),
            Value::Int(i64::MAX),
            text("héllo, 世界"),
            Value::Null,
            Value::Null,
            text("it's"),
            Value::Bool(true),
            Value::Null, // lands in the second bitmap byte
        ];

        let bytes = encode(&columns, &row).unwrap();
        assert_eq!(
            bytes[1], 0b0000_0010,
            "column 9 is bit 1 of the second byte"
        );
        assert_eq!(decode(&columns, &bytes).unwrap(), row);
    }

    #[test]
    fn nulls_store_only_the_bitmap() {
        let columns = [column("a", DataType::Int), column("b", DataType::Text)];
        assert_eq!(
            encode(&columns, &[Value::Null, Value::Null]).unwrap(),
            vec![0b11]
        );
    }

    #[test]
    fn encode_rejects_bad_rows() {
        let columns = [column("a", DataType::Int), column("b", DataType::Text)];
        assert!(encode(&columns, &[Value::Int(1)]).is_err(), "wrong arity");
        assert!(
            encode(&columns, &[text("x"), text("y")]).is_err(),
            "wrong type"
        );
        let long = text(&"x".repeat(u16::MAX as usize + 1));
        assert!(
            encode(&columns, &[Value::Int(1), long]).is_err(),
            "text too long"
        );
    }

    #[test]
    fn keys_sort_like_their_values() {
        let ints = [i64::MIN, -300, -1, 0, 1, 255, 256, i64::MAX].map(Value::Int);
        let texts = ["", "a", "ab", "b", "é"].map(text);
        let bools = [false, true].map(Value::Bool);
        for values in [&ints[..], &texts, &bools] {
            for pair in values.windows(2) {
                assert!(encode_key(&pair[0]) < encode_key(&pair[1]), "{pair:?}");
            }
        }
        for n in [i64::MIN, -1, 0, 42, i64::MAX] {
            assert_eq!(decode_int_key(&encode_key(&Value::Int(n))).unwrap(), n);
        }
        assert!(decode_int_key(b"short").is_err());
    }

    #[test]
    fn decode_rejects_corrupted_bytes() {
        let columns = [column("a", DataType::Bool), column("b", DataType::Text)];
        let good = encode(&columns, &[Value::Bool(true), text("ok")]).unwrap();
        assert!(decode(&columns, &good).is_ok());

        assert!(
            decode(&columns, &good[..good.len() - 1]).is_err(),
            "truncated"
        );
        assert!(
            decode(&columns, &[good.as_slice(), &[0]].concat()).is_err(),
            "trailing bytes"
        );

        let mut bad_bool = good.clone();
        bad_bool[1] = 7;
        assert!(decode(&columns, &bad_bool).is_err());

        let mut bad_utf8 = good.clone();
        bad_utf8[4] = 0xFF;
        assert!(decode(&columns, &bad_utf8).is_err());
    }
}
