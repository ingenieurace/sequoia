//! Row encoding: converts a row to bytes for storage in a page, and back.
//!
//! ```text
//! [null bitmap][column 1][column 2]...
//!
//! null bitmap  ceil(n / 8) bytes; bit i set = column i is NULL and stores no bytes
//! INT          8 bytes, little-endian
//! BOOL         1 byte, 0 or 1
//! TEXT         u16 length (little-endian) + UTF-8 bytes
//! ```
//!
//! The schema is not stored with the row: decoding needs the columns used to encode it.
//! Values are copied out when decoding, so no alignment padding is needed.

use crate::error::{Result, err};
use crate::types::{Column, DataType, Value};

pub fn encode(columns: &[Column], row: &[Value]) -> Result<Vec<u8>> {
    if row.len() != columns.len() {
        return err(format!("expected {} values, got {}", columns.len(), row.len()));
    }

    let mut bytes = vec![0u8; columns.len().div_ceil(8)];
    for (i, (column, value)) in columns.iter().zip(row).enumerate() {
        match (value, column.data_type) {
            (Value::Null, _) => bytes[i / 8] |= 1 << (i % 8),
            (Value::Int(n), DataType::Int) => bytes.extend_from_slice(&n.to_le_bytes()),
            (Value::Bool(b), DataType::Bool) => bytes.push(*b as u8),
            (Value::Text(s), DataType::Text) => {
                let Ok(len) = u16::try_from(s.len()) else {
                    return err(format!("text in column {} is longer than {} bytes", column.name, u16::MAX));
                };
                bytes.extend_from_slice(&len.to_le_bytes());
                bytes.extend_from_slice(s.as_bytes());
            }
            (value, data_type) => {
                return err(format!("column {} expects {data_type}, got {}", column.name, value.to_sql()));
            }
        }
    }
    Ok(bytes)
}

pub fn decode(columns: &[Column], bytes: &[u8]) -> Result<Vec<Value>> {
    let mut reader = Reader { bytes, pos: 0 };
    let bitmap = reader.take(columns.len().div_ceil(8))?;

    let mut row = Vec::with_capacity(columns.len());
    for (i, column) in columns.iter().enumerate() {
        if bitmap[i / 8] & (1 << (i % 8)) != 0 {
            row.push(Value::Null);
            continue;
        }
        let value = match column.data_type {
            DataType::Int => Value::Int(i64::from_le_bytes(reader.array()?)),
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

    if reader.pos != bytes.len() {
        return err("corrupted row: trailing bytes");
    }
    Ok(row)
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
        Column { name: name.into(), data_type, primary_key: false }
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
        let columns: Vec<Column> = (0..10).map(|i| column(&format!("c{i}"), types[i % 3])).collect();
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
        assert_eq!(bytes[1], 0b0000_0010, "column 9 is bit 1 of the second byte");
        assert_eq!(decode(&columns, &bytes).unwrap(), row);
    }

    #[test]
    fn nulls_store_only_the_bitmap() {
        let columns = [column("a", DataType::Int), column("b", DataType::Text)];
        assert_eq!(encode(&columns, &[Value::Null, Value::Null]).unwrap(), vec![0b11]);
    }

    #[test]
    fn encode_rejects_bad_rows() {
        let columns = [column("a", DataType::Int), column("b", DataType::Text)];
        assert!(encode(&columns, &[Value::Int(1)]).is_err(), "wrong arity");
        assert!(encode(&columns, &[text("x"), text("y")]).is_err(), "wrong type");
        let long = text(&"x".repeat(u16::MAX as usize + 1));
        assert!(encode(&columns, &[Value::Int(1), long]).is_err(), "text too long");
    }

    #[test]
    fn decode_rejects_corrupted_bytes() {
        let columns = [column("a", DataType::Bool), column("b", DataType::Text)];
        let good = encode(&columns, &[Value::Bool(true), text("ok")]).unwrap();
        assert!(decode(&columns, &good).is_ok());

        assert!(decode(&columns, &good[..good.len() - 1]).is_err(), "truncated");
        assert!(decode(&columns, &[good.as_slice(), &[0]].concat()).is_err(), "trailing bytes");

        let mut bad_bool = good.clone();
        bad_bool[1] = 7;
        assert!(decode(&columns, &bad_bool).is_err());

        let mut bad_utf8 = good.clone();
        bad_utf8[4] = 0xFF;
        assert!(decode(&columns, &bad_utf8).is_err());
    }
}
