//! Sort keys: an encoding of values whose bytes compare as ORDER BY orders
//! the values, used by sorts and by index entries.
//!
//! NULL comes first, then BOOL, the numbers (INT and REAL together, by
//! value) and TEXT (as values of different types are never equal, those of
//! one type all sort before those of another). Each value's encoding ends
//! where it can be told to, so a key of several values compares like its
//! values one after another:
//!
//! ```text
//! NULL   01
//! BOOL   02 for FALSE, 03 for TRUE
//! INT    14 for zero; 14 + n, then its n big-endian bytes, for a positive one;
//!        14 - n, then the bytes of the complement of its magnitude, for a
//!        negative one; then 00
//! REAL   as the INT it equals, if any; else, if an INT is below it, that INT
//!        as above but for 80 in place of 00, then its fraction's big-endian
//!        bits; else 0B (below every INT) or 1D (above), then its ordered bits
//! TEXT   20, its UTF-8 bytes with each 00 as 00 FF, then 00 00
//! ```
//!
//! A REAL equal to an INT is encoded as that INT, so equal numbers have
//! equal keys, and a key decodes to an INT; [`Value::coerce`] makes it a
//! REAL again where the type is known.
//!
//! A descending key has every byte of its encoding inverted.

use crate::error::{Result, err};
use crate::types::{Value, ordered_real, real_as_int, real_from_ordered};

const NULL: u8 = 0x01;
const FALSE: u8 = 0x02;
const TRUE: u8 = 0x03;
/// INT zero; a positive INT of n bytes is `INT + n`, a negative one `INT - n`.
const INT: u8 = 0x14;
const TEXT: u8 = 0x20;
/// After an INT, or a REAL equal to one.
const WHOLE: u8 = 0x00;
/// After the INT below a REAL with a fraction, before the fraction.
const FRACTION: u8 = 0x80;

/// Below the encoding of every value but NULL: where a range of keys that
/// leaves NULL out starts.
pub const ABOVE_NULL: u8 = FALSE;

/// Appends a value's sort key.
pub fn encode(value: &Value, desc: bool, out: &mut Vec<u8>) {
    let start = out.len();
    match value {
        Value::Null => out.push(NULL),
        Value::Bool(b) => out.push(if *b { TRUE } else { FALSE }),
        Value::Int(n) => {
            encode_int(*n, out);
            out.push(WHOLE);
        }
        Value::Real(r) => match real_as_int(*r) {
            Some(n) => {
                encode_int(n, out);
                out.push(WHOLE);
            }
            // Within the INTs, and not one: so it has a fraction, which is exact.
            None if r.abs() < 9_223_372_036_854_775_808.0 => {
                let floor = r.floor();
                encode_int(floor as i64, out);
                out.push(FRACTION);
                out.extend_from_slice(&(r - floor).to_bits().to_be_bytes());
            }
            None => {
                out.push(if *r < 0.0 { INT - 9 } else { INT + 9 });
                out.extend_from_slice(&ordered_real(*r).to_be_bytes());
            }
        },
        Value::Text(s) => {
            out.push(TEXT);
            for (i, part) in s.as_bytes().split(|&b| b == 0).enumerate() {
                if i > 0 {
                    out.extend_from_slice(&[0, 0xFF]);
                }
                out.extend_from_slice(part);
            }
            out.extend_from_slice(&[0, 0]);
        }
    }
    if desc {
        out[start..].iter_mut().for_each(|b| *b = !*b);
    }
}

/// Appends an INT's tag and bytes (see [`encode`]).
fn encode_int(n: i64, out: &mut Vec<u8>) {
    let magnitude = n.unsigned_abs();
    let len = 8 - magnitude.leading_zeros() as usize / 8;
    let (tag, bits) = if n < 0 { (INT - len as u8, !magnitude) } else { (INT + len as u8, magnitude) };
    out.push(tag);
    out.extend_from_slice(&bits.to_be_bytes()[8 - len..]);
}

/// Decodes the ascending key that starts at `*pos`, and moves `*pos` past it.
pub fn decode(bytes: &[u8], pos: &mut usize) -> Result<Value> {
    let mut take = |n: usize| -> Result<&[u8]> {
        let slice = bytes.get(*pos..*pos + n).ok_or_else(corrupted)?;
        *pos += n;
        Ok(slice)
    };
    let tag = take(1)?[0];
    Ok(match tag {
        NULL => Value::Null,
        FALSE => Value::Bool(false),
        TRUE => Value::Bool(true),
        TEXT => {
            let mut text = Vec::new();
            loop {
                match take(1)?[0] {
                    0 => match take(1)?[0] {
                        0 => break,
                        0xFF => text.push(0),
                        _ => return err(corrupted().0),
                    },
                    byte => text.push(byte),
                }
            }
            Value::Text(String::from_utf8(text).map_err(|_| corrupted())?)
        }
        _ if tag == INT - 9 || tag == INT + 9 => {
            Value::Real(real_from_ordered(u64::from_be_bytes(take(8)?.try_into().expect("8 bytes"))))
        }
        _ if (INT - 8..=INT + 8).contains(&tag) => {
            let len = tag.abs_diff(INT) as usize;
            let raw = take(len)?.iter().fold(0u64, |n, &b| n << 8 | b as u64);
            let n = match tag < INT {
                // The complement of the magnitude, whose leading bytes, all ones, were left out.
                true => (!(raw | u64::MAX.checked_shl(8 * len as u32).unwrap_or(0)) as i64).wrapping_neg(),
                false => raw as i64,
            };
            match take(1)?[0] {
                WHOLE => Value::Int(n),
                FRACTION => {
                    let fraction = f64::from_bits(u64::from_be_bytes(take(8)?.try_into().expect("8 bytes")));
                    Value::Real(n as f64 + fraction)
                }
                _ => return err(corrupted().0),
            }
        }
        _ => return err(corrupted().0),
    })
}

/// Where the ascending key that starts at `pos` ends.
pub fn skip(bytes: &[u8], pos: usize) -> Result<usize> {
    let tag = *bytes.get(pos).ok_or_else(corrupted)?;
    let end = match tag {
        NULL | FALSE | TRUE => pos + 1,
        TEXT => {
            let mut i = pos + 1;
            loop {
                match bytes.get(i..i + 2) {
                    Some([0, 0]) => break i + 2,
                    Some([0, _]) => i += 2,
                    Some(_) => i += 1,
                    None => return err(corrupted().0),
                }
            }
        }
        _ if tag == INT - 9 || tag == INT + 9 => pos + 9,
        _ if (INT - 8..=INT + 8).contains(&tag) => {
            let after = pos + 1 + tag.abs_diff(INT) as usize;
            match bytes.get(after) {
                Some(&WHOLE) => after + 1,
                Some(&FRACTION) => after + 9,
                _ => return err(corrupted().0),
            }
        }
        _ => return err(corrupted().0),
    };
    if end > bytes.len() {
        return err(corrupted().0);
    }
    Ok(end)
}

fn corrupted() -> crate::error::Error {
    crate::error::Error("corrupted sort key".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Vec<Value> {
        let mut values = vec![Value::Null, Value::Bool(false), Value::Bool(true), Value::Real(-1e300)];
        for n in [i64::MIN, -70_000, -256, -255, -1] {
            values.push(Value::Int(n));
        }
        values.extend([Value::Real(-0.5), Value::Int(0), Value::Real(0.25), Value::Int(1), Value::Real(1.5)]);
        for n in [255, 256, 70_000, i64::MAX] {
            values.push(Value::Int(n));
        }
        values.extend([Value::Real(1e300), Value::Text("".into()), Value::Text("\0".into()), Value::Text("a".into())]);
        values.push(Value::Text("a\0b".into()));
        values
    }

    #[test]
    fn keys_sort_like_values_and_decode_back() {
        let keys: Vec<Vec<u8>> = values()
            .iter()
            .map(|v| {
                let mut key = Vec::new();
                encode(v, false, &mut key);
                key
            })
            .collect();
        for pair in keys.windows(2) {
            assert!(pair[0] < pair[1], "{pair:?}");
        }
        for (value, key) in values().iter().zip(&keys) {
            let mut bytes = key.clone();
            bytes.extend_from_slice(&[7, 7]); // what follows a key is not part of it
            let mut pos = 0;
            let decoded = decode(&bytes, &mut pos).unwrap();
            assert_eq!(decoded, *value);
            assert_eq!(pos, key.len());
            assert_eq!(skip(&bytes, 0).unwrap(), key.len());
        }
    }

    #[test]
    fn equal_numbers_have_equal_keys() {
        let (mut int, mut real) = (Vec::new(), Vec::new());
        encode(&Value::Int(-3), false, &mut int);
        encode(&Value::Real(-3.0), false, &mut real);
        assert_eq!(int, real);
        assert!(matches!(decode(&real, &mut 0).unwrap(), Value::Int(-3)));
    }
}
