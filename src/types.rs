use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::error::{Error, Result};

/// Column data types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Int,
    Text,
    Bool,
    /// A 64-bit floating-point number.
    Real,
}

/// A column in a table schema.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub primary_key: bool,
}

/// The position of the column with this name.
pub fn column_index(columns: &[Column], name: &str) -> Result<usize> {
    columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| Error(format!("unknown column: {name}")))
}

/// How a table stores its rows: `CREATE TABLE ... USING HEAP | LSM | COLUMN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageKind {
    /// Slotted pages, updated in place. The default.
    #[default]
    Heap,
    /// A log-structured merge tree.
    Lsm,
    /// Compressed row groups stored column by column, plus a delta store.
    Column,
}

/// A single SQL value. A REAL is never NaN or infinite: arithmetic that
/// would make one fails instead.
///
/// Values are equal as SQL compares them, with NULL equal to NULL: numbers
/// by value, so INT 1 equals REAL 1.0 and both hash alike, as GROUP BY,
/// DISTINCT and hash joins need.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Int(i64),
    Text(String),
    Bool(bool),
    Real(f64),
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            _ => self.compare(other) == Some(Ordering::Equal),
        }
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Value::Null => 0u8.hash(state),
            Value::Int(n) => (1u8, n).hash(state),
            Value::Real(r) => match real_as_int(*r) {
                Some(n) => (1u8, n).hash(state),
                None => (4u8, r.to_bits()).hash(state),
            },
            Value::Text(s) => (2u8, s).hash(state),
            Value::Bool(b) => (3u8, b).hash(state),
        }
    }
}

/// The INT a REAL equals, if one does.
pub fn real_as_int(r: f64) -> Option<i64> {
    // -2^63 <= r < 2^63, the range of i64, with no fraction.
    (r.fract() == 0.0 && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&r))
        .then_some(r as i64)
}

/// A REAL's bits, changed so that they compare as unsigned numbers as the
/// REALs do (with -0.0 as 0.0): negative REALs have every bit flipped,
/// others only the sign bit.
pub fn ordered_real(r: f64) -> u64 {
    let bits = (if r == 0.0 { 0.0 } else { r }).to_bits();
    if bits >> 63 == 1 {
        !bits
    } else {
        bits | 1 << 63
    }
}

/// The REAL whose [`ordered_real`] bits these are.
pub fn real_from_ordered(bits: u64) -> f64 {
    f64::from_bits(if bits >> 63 == 1 {
        bits & !(1 << 63)
    } else {
        !bits
    })
}

/// Compares an INT with a REAL exactly, without rounding the INT to a REAL.
fn compare_int_real(a: i64, b: f64) -> Ordering {
    if b >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    if b < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    let floor = b.floor();
    match a.cmp(&(floor as i64)) {
        Ordering::Equal if b > floor => Ordering::Less,
        ord => ord,
    }
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The value of an INT or a REAL as a REAL.
    pub fn as_real(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Real(r) => Some(*r),
            _ => None,
        }
    }

    /// Whether this value can be stored in a column of the given type.
    pub fn fits(&self, data_type: DataType) -> bool {
        matches!(
            (self, data_type),
            (Value::Null, _)
                | (Value::Int(_), DataType::Int)
                | (Value::Text(_), DataType::Text)
                | (Value::Bool(_), DataType::Bool)
                | (Value::Real(_), DataType::Real)
        )
    }

    /// This value as one of the given type that it equals, if there is one:
    /// itself if it fits, an INT as the REAL it equals, a REAL with no
    /// fraction as an INT. How a value is stored in a typed column, and
    /// compared with one through an index.
    pub fn coerce(&self, data_type: DataType) -> Option<Value> {
        match (self, data_type) {
            (Value::Int(n), DataType::Real) => {
                let r = *n as f64;
                (real_as_int(r) == Some(*n)).then_some(Value::Real(r))
            }
            (Value::Real(r), DataType::Int) => real_as_int(*r).map(Value::Int),
            (value, data_type) => value.fits(data_type).then(|| value.clone()),
        }
    }

    /// Compares two non-null values of the same type, or two numbers.
    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Real(a), Value::Real(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Real(b)) => Some(compare_int_real(*a, *b)),
            (Value::Real(a), Value::Int(b)) => Some(compare_int_real(*b, *a).reverse()),
            (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    /// Total ordering used by ORDER BY: NULL sorts first.
    pub fn sort_cmp(&self, other: &Value) -> Ordering {
        match (self.is_null(), other.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => self.compare(other).unwrap_or(Ordering::Equal),
        }
    }

    /// Renders the value as a SQL literal that the parser can read back.
    pub fn to_sql(&self) -> String {
        match self {
            Value::Null => "NULL".into(),
            Value::Int(i64::MIN) => format!("({} - 1)", i64::MIN + 1),
            Value::Int(n) => n.to_string(),
            Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
            Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
            // The shortest digits that read back as the same REAL: 2.5, 1.0, 1e20.
            Value::Real(r) => format!("{r:?}"),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Value::Text(s) => f.write_str(s),
            other => f.write_str(&other.to_sql()),
        }
    }
}

impl fmt::Display for StorageKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            StorageKind::Heap => "HEAP",
            StorageKind::Lsm => "LSM",
            StorageKind::Column => "COLUMN",
        })
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            DataType::Int => "INT",
            DataType::Text => "TEXT",
            DataType::Bool => "BOOL",
            DataType::Real => "REAL",
        })
    }
}
