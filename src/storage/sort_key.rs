//! Sort keys: an encoding of values whose bytes compare as ORDER BY orders
//! the values, used by sorts and by index entries.
//!
//! NULL comes first, then BOOL, the numbers (INT, REAL and DECIMAL together,
//! by value), TEXT, BLOB, DATE, TIME, TIMESTAMP, TIMESTAMPTZ and INTERVAL (as values of different
//! types are never equal, those of one type all sort before those of
//! another). Each value's encoding ends where it can be told to, so a key
//! of several values compares like its values one after another:
//!
//! ```text
//! NULL       01
//! BOOL       02 for FALSE, 03 for TRUE
//! INT        14 for zero; 14 + n, then its n big-endian bytes, for a positive
//!            one; 14 - n, then the bytes of the complement of its magnitude,
//!            for a negative one; then 00
//! REAL       as the INT it equals, if any; else, if an INT is below it, that
//!            INT as above but for 80 in place of 00, then its fraction's
//!            big-endian bits; else 0B (below every INT) or 1D (above), then
//!            its ordered bits; then 00
//! DECIMAL    as the INT it equals, if any; else as the REAL nearest it would
//!            be, but for its exact digits before that 00: after a fraction's
//!            bits, those of its fraction, each as 01 to 0A; after the
//!            ordered bits, 01, the largest whole number not above it in 16
//!            big-endian bytes with the sign bit flipped, then the digits
//!            of its fraction likewise
//! TEXT       20, its UTF-8 bytes with each 00 as 00 FF, then 00 00
//! BLOB       21, its bytes likewise
//! DATE       22, its day as 4 big-endian bytes with the sign bit flipped
//! TIME       23, its microseconds as 8 big-endian bytes with the sign bit flipped
//! TIMESTAMP  24, its microseconds likewise
//! TIMESTAMPTZ  25, its microseconds likewise
//! INTERVAL   26, its length (a month as 30 days) as 16 big-endian bytes
//!            with the sign bit flipped, then its months, days (4 bytes
//!            each) and microseconds (8) likewise
//! ```
//!
//! A REAL or DECIMAL equal to an INT is encoded as that INT, so equal
//! numbers have equal keys (a DECIMAL and the REAL nearest it aside), and
//! such a key decodes to an INT; [`Value::coerce`] makes it a REAL or a
//! DECIMAL again where the type is known. Of a REAL and a DECIMAL with the
//! same bits, which compare as equal, the REAL comes first; DECIMALs with
//! the same bits are told apart by their digits, which have no zeros at
//! their end, so a shorter fraction is the smaller one.
//!
//! A descending key has every byte of its encoding inverted.

use crate::decimal::Decimal;
use crate::error::{Result, err};
use crate::types::{Value, ordered_real, real_as_int, real_from_ordered};

const NULL: u8 = 0x01;
const FALSE: u8 = 0x02;
const TRUE: u8 = 0x03;
/// INT zero; a positive INT of n bytes is `INT + n`, a negative one `INT - n`.
const INT: u8 = 0x14;
const TEXT: u8 = 0x20;
const BLOB: u8 = 0x21;
const DATE: u8 = 0x22;
const TIME: u8 = 0x23;
const TIMESTAMP: u8 = 0x24;
const TIMESTAMPTZ: u8 = 0x25;
const INTERVAL: u8 = 0x26;
/// After an INT, or a number equal to one; and at the end of any other number.
const WHOLE: u8 = 0x00;
/// After the INT below a number with a fraction, before the fraction.
const FRACTION: u8 = 0x80;
/// After a DECIMAL's ordered bits, before its exact value.
const EXACT: u8 = 0x01;

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
        Value::Real(r) => encode_real(*r, out),
        Value::Decimal(d) => encode_decimal(*d, out),
        Value::Text(s) => escaped(TEXT, s.as_bytes(), out),
        Value::Blob(b) => escaped(BLOB, b, out),
        Value::Date(d) => {
            out.push(DATE);
            out.extend_from_slice(&((*d as u32) ^ (1 << 31)).to_be_bytes());
        }
        Value::Time(t) | Value::Timestamp(t) | Value::TimestampTz(t) => {
            out.push(match value {
                Value::Time(_) => TIME,
                Value::Timestamp(_) => TIMESTAMP,
                _ => TIMESTAMPTZ,
            });
            out.extend_from_slice(&((*t as u64) ^ (1 << 63)).to_be_bytes());
        }
        Value::Interval(i) => {
            out.push(INTERVAL);
            out.extend_from_slice(&((i.length() as u128) ^ (1 << 127)).to_be_bytes());
            out.extend_from_slice(&((i.months as u32) ^ (1 << 31)).to_be_bytes());
            out.extend_from_slice(&((i.days as u32) ^ (1 << 31)).to_be_bytes());
            out.extend_from_slice(&((i.micros as u64) ^ (1 << 63)).to_be_bytes());
        }
    }
    if desc {
        out[start..].iter_mut().for_each(|b| *b = !*b);
    }
}

/// Appends a tag, then bytes with each 00 as 00 FF, then 00 00.
fn escaped(tag: u8, bytes: &[u8], out: &mut Vec<u8>) {
    out.push(tag);
    for (i, part) in bytes.split(|&b| b == 0).enumerate() {
        if i > 0 {
            out.extend_from_slice(&[0, 0xFF]);
        }
        out.extend_from_slice(part);
    }
    out.extend_from_slice(&[0, 0]);
}

/// Appends a REAL's key, but for the 00 at its end if it has a fraction or
/// is out of the INTs' range, where a DECIMAL's exact digits go.
fn encode_real_bits(r: f64, out: &mut Vec<u8>) -> bool {
    match real_as_int(r) {
        Some(n) => {
            encode_int(n, out);
            out.push(WHOLE);
            false
        }
        // Within the INTs, and not one: so it has a fraction, which is exact.
        None if r.abs() < 9_223_372_036_854_775_808.0 => {
            let floor = r.floor();
            encode_int(floor as i64, out);
            out.push(FRACTION);
            out.extend_from_slice(&(r - floor).to_bits().to_be_bytes());
            true
        }
        None => {
            out.push(if r < 0.0 { INT - 9 } else { INT + 9 });
            out.extend_from_slice(&ordered_real(r).to_be_bytes());
            true
        }
    }
}

fn encode_real(r: f64, out: &mut Vec<u8>) {
    if encode_real_bits(r, out) {
        out.push(WHOLE);
    }
}

fn encode_decimal(d: Decimal, out: &mut Vec<u8>) {
    if let Some(n) = d.to_int() {
        encode_int(n, out);
        out.push(WHOLE);
        return;
    }
    let (floor, digits) = d.floor_and_fraction();
    let digit_bytes = digits.iter().map(|d| d + 1);
    match i64::try_from(floor) {
        Ok(floor) => {
            // As a REAL with this fraction would be: the floor, then the bits
            // of the REAL nearest the fraction.
            encode_int(floor, out);
            out.push(FRACTION);
            let fraction: f64 = format!(
                "0.{}",
                digits
                    .iter()
                    .map(|d| (b'0' + d) as char)
                    .collect::<String>()
            )
            .parse()
            .expect("digits");
            out.extend_from_slice(&fraction.to_bits().to_be_bytes());
            out.extend(digit_bytes);
        }
        Err(_) => {
            out.push(if floor < 0 { INT - 9 } else { INT + 9 });
            out.extend_from_slice(&ordered_real(d.to_real()).to_be_bytes());
            out.push(EXACT);
            out.extend_from_slice(&((floor as u128) ^ (1 << 127)).to_be_bytes());
            out.extend(digit_bytes);
        }
    }
    out.push(WHOLE);
}

/// Appends an INT's tag and bytes (see [`encode`]).
fn encode_int(n: i64, out: &mut Vec<u8>) {
    let magnitude = n.unsigned_abs();
    let len = 8 - magnitude.leading_zeros() as usize / 8;
    let (tag, bits) = if n < 0 {
        (INT - len as u8, !magnitude)
    } else {
        (INT + len as u8, magnitude)
    };
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
        TEXT => Value::Text(String::from_utf8(unescape(bytes, pos)?).map_err(|_| corrupted())?),
        BLOB => Value::Blob(unescape(bytes, pos)?),
        DATE => Value::Date((u32::from_be_bytes(array(take(4)?)) ^ (1 << 31)) as i32),
        TIME | TIMESTAMP | TIMESTAMPTZ => {
            let t = (u64::from_be_bytes(array(take(8)?)) ^ (1 << 63)) as i64;
            match tag {
                TIME => Value::Time(t),
                TIMESTAMP => Value::Timestamp(t),
                _ => Value::TimestampTz(t),
            }
        }
        INTERVAL => {
            take(16)?; // the length, which the parts give
            Value::Interval(crate::datetime::Interval {
                months: (u32::from_be_bytes(array(take(4)?)) ^ (1 << 31)) as i32,
                days: (u32::from_be_bytes(array(take(4)?)) ^ (1 << 31)) as i32,
                micros: (u64::from_be_bytes(array(take(8)?)) ^ (1 << 63)) as i64,
            })
        }
        _ if tag == INT - 9 || tag == INT + 9 => {
            let real = real_from_ordered(u64::from_be_bytes(array(take(8)?)));
            match take(1)?[0] {
                WHOLE => Value::Real(real),
                EXACT => {
                    let floor = (u128::from_be_bytes(array(take(16)?)) ^ (1 << 127)) as i128;
                    exact(floor, bytes, pos)?
                }
                _ => return err(corrupted().0),
            }
        }
        _ if (INT - 8..=INT + 8).contains(&tag) => {
            let len = tag.abs_diff(INT) as usize;
            let raw = take(len)?.iter().fold(0u64, |n, &b| n << 8 | b as u64);
            let n = match tag < INT {
                // The complement of the magnitude, whose leading bytes, all ones, were left out.
                true => (!(raw | u64::MAX.checked_shl(8 * len as u32).unwrap_or(0)) as i64)
                    .wrapping_neg(),
                false => raw as i64,
            };
            match take(1)?[0] {
                WHOLE => Value::Int(n),
                FRACTION => {
                    let fraction = f64::from_bits(u64::from_be_bytes(array(take(8)?)));
                    match bytes.get(*pos) {
                        Some(&WHOLE) => {
                            *pos += 1;
                            Value::Real(n as f64 + fraction)
                        }
                        Some(_) => exact(n as i128, bytes, pos)?,
                        None => return err(corrupted().0),
                    }
                }
                _ => return err(corrupted().0),
            }
        }
        _ => return err(corrupted().0),
    })
}

/// The DECIMAL `floor + 0.digits`, its digits read from `*pos` up to the 00
/// after them.
fn exact(floor: i128, bytes: &[u8], pos: &mut usize) -> Result<Value> {
    let mut digits = Vec::new();
    loop {
        match bytes.get(*pos) {
            Some(&WHOLE) => break,
            Some(&d @ 1..=10) => digits.push(d - 1),
            _ => return err(corrupted().0),
        }
        *pos += 1;
    }
    *pos += 1;
    Decimal::from_floor_and_fraction(floor, &digits)
        .map(Value::Decimal)
        .map_err(|_| corrupted())
}

/// The bytes escaped from `*pos` on, up to the 00 00 after them.
fn unescape(bytes: &[u8], pos: &mut usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let byte = *bytes.get(*pos).ok_or_else(corrupted)?;
        *pos += 1;
        if byte != 0 {
            out.push(byte);
            continue;
        }
        let next = *bytes.get(*pos).ok_or_else(corrupted)?;
        *pos += 1;
        match next {
            0 => return Ok(out),
            0xFF => out.push(0),
            _ => return err(corrupted().0),
        }
    }
}

fn array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    bytes.try_into().expect("taken to length")
}

/// Where the ascending key that starts at `pos` ends.
pub fn skip(bytes: &[u8], pos: usize) -> Result<usize> {
    let tag = *bytes.get(pos).ok_or_else(corrupted)?;
    // The end of a number's exact digits, from `i`, after its 00.
    let digits_end = |mut i: usize| loop {
        match bytes.get(i) {
            Some(&WHOLE) => break Ok(i + 1),
            Some(1..=10) => i += 1,
            _ => break err(corrupted().0),
        }
    };
    let end = match tag {
        NULL | FALSE | TRUE => pos + 1,
        TEXT | BLOB => {
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
        DATE => pos + 5,
        TIME | TIMESTAMP | TIMESTAMPTZ => pos + 9,
        INTERVAL => pos + 33,
        _ if tag == INT - 9 || tag == INT + 9 => match bytes.get(pos + 9) {
            Some(&WHOLE) => pos + 10,
            Some(&EXACT) => digits_end(pos + 26)?,
            _ => return err(corrupted().0),
        },
        _ if (INT - 8..=INT + 8).contains(&tag) => {
            let after = pos + 1 + tag.abs_diff(INT) as usize;
            match bytes.get(after) {
                Some(&WHOLE) => after + 1,
                Some(&FRACTION) => digits_end(after + 9)?,
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

    fn decimal(text: &str) -> Value {
        Value::Decimal(Decimal::parse(text).unwrap())
    }

    fn values() -> Vec<Value> {
        let mut values = vec![
            Value::Null,
            Value::Bool(false),
            Value::Bool(true),
            Value::Real(-1e300),
            decimal(&format!("-{}.5", "9".repeat(30))),
            decimal("-1e20"),
        ];
        for n in [i64::MIN, -70_000, -256, -255, -1] {
            values.push(Value::Int(n));
        }
        values.extend([
            decimal("-0.75"),
            Value::Real(-0.5),
            decimal("-0.49999999999999999999"),
            Value::Int(0),
            decimal("0.1"),
            Value::Real(0.25),
            decimal("0.2500000000000000000001"),
            decimal("0.25000000000000000001"),
            Value::Int(1),
            Value::Real(1.5),
            decimal("1.75"),
        ]);
        for n in [255, 256, 70_000, i64::MAX] {
            values.push(Value::Int(n));
        }
        values.extend([
            decimal("9223372036854775807.5"),
            decimal("1e20"),
            decimal("100000000000000000000.5"),
            Value::Real(1e300),
            Value::Text("".into()),
            Value::Text("\0".into()),
            Value::Text("a".into()),
            Value::Text("a\0b".into()),
            Value::Blob(vec![]),
            Value::Blob(vec![0]),
            Value::Blob(vec![0, 0xFF]),
            Value::Blob(vec![1]),
            Value::Date(-719_162),
            Value::Date(-1),
            Value::Date(0),
            Value::Date(20_000),
            Value::Time(0),
            Value::Time(86_399_999_999),
            Value::Timestamp(-5),
            Value::Timestamp(0),
            Value::Timestamp(1_700_000_000_000_000),
        ]);
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
        for (pair, values) in keys.windows(2).zip(values().windows(2)) {
            assert!(pair[0] < pair[1], "{values:?}: {pair:?}");
        }
        for (value, key) in values().iter().zip(&keys) {
            // What follows a key is not part of it: another key, or bytes
            // that may start with anything.
            for after in [[7, 7], [0, 0], [1, 2]] {
                let mut bytes = key.clone();
                bytes.extend_from_slice(&after);
                let mut pos = 0;
                let decoded = decode(&bytes, &mut pos).unwrap();
                assert_eq!(decoded, *value);
                assert_eq!(
                    std::mem::discriminant(&decoded) == std::mem::discriminant(value),
                    !matches!(value, Value::Decimal(d) if d.to_int().is_some()),
                    "{value:?} decoded as {decoded:?}"
                );
                assert_eq!(pos, key.len(), "{value:?}");
                assert_eq!(skip(&bytes, 0).unwrap(), key.len(), "{value:?}");
            }
        }
    }

    #[test]
    fn equal_numbers_have_equal_keys() {
        let key = |v: Value| {
            let mut key = Vec::new();
            encode(&v, false, &mut key);
            key
        };
        assert_eq!(key(Value::Int(-3)), key(Value::Real(-3.0)));
        assert_eq!(key(Value::Int(-3)), key(decimal("-3.000")));
        assert!(matches!(
            decode(&key(Value::Real(-3.0)), &mut 0).unwrap(),
            Value::Int(-3)
        ));
        assert_eq!(key(decimal("1.50")), key(decimal("1.5")));
        assert_eq!(
            decode(&key(decimal("1.50")), &mut 0).unwrap().to_sql(),
            "DECIMAL '1.5'"
        );
    }
}
