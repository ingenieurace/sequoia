use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::datetime::{self, Interval};
use crate::decimal::Decimal;
use crate::error::{Error, Result, err};

/// Column data types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Int,
    Text,
    Bool,
    /// A 64-bit floating-point number.
    Real,
    /// A day of the calendar (see `datetime.rs`).
    Date,
    /// A time of day, to the microsecond.
    Time,
    /// A date and a time of day, with no time zone.
    Timestamp,
    /// An instant: `TIMESTAMP WITH TIME ZONE`, or `TIMESTAMPTZ`.
    TimestampTz,
    /// A span of time: months, days and microseconds.
    Interval,
    /// Bytes.
    Blob,
    /// An exact decimal number (see `decimal.rs`): `DECIMAL(p, s)` holds
    /// those of at most `p` digits, `s` of them after the point (values are
    /// rounded to `s` places when stored). With neither, any DECIMAL, kept
    /// without the zeros at the end of its fraction.
    Decimal(Option<(u8, u8)>),
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
/// by value, so INT 1 equals REAL 1.0 and DECIMAL 1.00, and all three hash
/// alike, as GROUP BY, DISTINCT and hash joins need. A DECIMAL compares
/// with an INT exactly, and with a REAL as the REAL nearest it.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Int(i64),
    Text(String),
    Bool(bool),
    Real(f64),
    /// Days since 1970-01-01.
    Date(i32),
    /// Microseconds since midnight.
    Time(i64),
    /// Microseconds since 1970-01-01 00:00:00.
    Timestamp(i64),
    /// Microseconds since 1970-01-01 00:00:00 UTC.
    TimestampTz(i64),
    Interval(Interval),
    Blob(Vec<u8>),
    Decimal(Decimal),
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
            Value::Date(d) => (5u8, d).hash(state),
            Value::Time(t) => (6u8, t).hash(state),
            Value::Timestamp(t) => (7u8, t).hash(state),
            Value::Blob(b) => (8u8, b).hash(state),
            Value::TimestampTz(t) => (9u8, t).hash(state),
            Value::Interval(i) => (10u8, i).hash(state),
            // As the INT it equals, or else the REAL nearest it, which it equals.
            Value::Decimal(d) => match d.to_int() {
                Some(n) => (1u8, n).hash(state),
                None => Value::Real(d.to_real()).hash(state),
            },
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

    /// The value of an INT, a REAL or a DECIMAL as a REAL, the nearest one.
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            Value::Decimal(d) => Some(d.to_real()),
            other => other.as_real(),
        }
    }

    /// Whether this value can be stored in a column of the given type as
    /// it is.
    pub fn fits(&self, data_type: DataType) -> bool {
        match (self, data_type) {
            (Value::Decimal(d), DataType::Decimal(None)) => d.scale() == d.normalize().scale(),
            (Value::Decimal(d), DataType::Decimal(Some((p, s)))) => {
                d.scale() == s && d.whole_digits() <= p - s
            }
            _ => matches!(
                (self, data_type),
                (Value::Null, _)
                    | (Value::Int(_), DataType::Int)
                    | (Value::Text(_), DataType::Text)
                    | (Value::Bool(_), DataType::Bool)
                    | (Value::Real(_), DataType::Real)
                    | (Value::Date(_), DataType::Date)
                    | (Value::Time(_), DataType::Time)
                    | (Value::Timestamp(_), DataType::Timestamp)
                    | (Value::TimestampTz(_), DataType::TimestampTz)
                    | (Value::Interval(_), DataType::Interval)
                    | (Value::Blob(_), DataType::Blob)
            ),
        }
    }

    /// This value as one of the given type that it equals, if there is one:
    /// itself if it fits, an INT as the REAL it equals, a REAL with no
    /// fraction as an INT, a number as the DECIMAL with the column's places
    /// it equals (a REAL by its shortest digits). How a value is compared
    /// with a column through an index.
    pub fn coerce(&self, data_type: DataType) -> Option<Value> {
        let decimal = |d: Decimal| fit_decimal(d, data_type).map(Value::Decimal);
        match (self, data_type) {
            (Value::Int(n), DataType::Real) => {
                let r = *n as f64;
                (real_as_int(r) == Some(*n)).then_some(Value::Real(r))
            }
            (Value::Real(r), DataType::Int) => real_as_int(*r).map(Value::Int),
            (Value::Int(n), DataType::Decimal(_)) => decimal(Decimal::from_int(*n)),
            (Value::Real(r), DataType::Decimal(_)) => decimal(Decimal::from_real(*r).ok()?),
            (Value::Decimal(d), DataType::Decimal(_)) => decimal(*d),
            (Value::Decimal(d), DataType::Int) => d.to_int().map(Value::Int),
            (Value::Decimal(d), DataType::Real) => {
                let r = d.to_real();
                let back = Decimal::from_real(r).ok()?;
                (back.compare(*d) == Ordering::Equal).then_some(Value::Real(r))
            }
            (value, data_type) => value.fits(data_type).then(|| value.clone()),
        }
    }

    /// This value as it is stored in a column of the given type: as
    /// [`Value::coerce`] makes it, or a number as a DECIMAL rounded to the
    /// column's places (an error if it has too many digits before the
    /// point), or a DECIMAL as the REAL nearest it. Another value stays as
    /// it is, for the column to refuse.
    pub fn assign(&self, data_type: DataType) -> Result<Value> {
        if self.fits(data_type) {
            return Ok(self.clone());
        }
        if let Some(value) = self.coerce(data_type) {
            return Ok(value);
        }
        let decimal = match self {
            Value::Int(n) => Decimal::from_int(*n),
            Value::Real(r) => Decimal::from_real(*r)?,
            Value::Decimal(d) => *d,
            _ => return Ok(self.clone()),
        };
        match data_type {
            DataType::Decimal(None) => Ok(Value::Decimal(decimal.normalize())),
            DataType::Decimal(Some((p, s))) => match decimal.rescale(s) {
                Ok(d) if d.whole_digits() <= p - s => Ok(Value::Decimal(d)),
                _ => err(format!("{} does not fit in {data_type}", self.to_sql())),
            },
            DataType::Real => Ok(Value::Real(decimal.to_real())),
            _ => Ok(self.clone()),
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
            (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
            (Value::Time(a), Value::Time(b)) => Some(a.cmp(b)),
            (Value::Timestamp(a), Value::Timestamp(b)) => Some(a.cmp(b)),
            (Value::TimestampTz(a), Value::TimestampTz(b)) => Some(a.cmp(b)),
            (Value::Interval(a), Value::Interval(b)) => Some(a.compare(*b)),
            (Value::Blob(a), Value::Blob(b)) => Some(a.cmp(b)),
            (Value::Decimal(a), Value::Decimal(b)) => Some(a.compare(*b)),
            (Value::Decimal(a), Value::Int(b)) => Some(a.compare(Decimal::from_int(*b))),
            (Value::Int(a), Value::Decimal(b)) => Some(Decimal::from_int(*a).compare(*b)),
            (Value::Decimal(a), Value::Real(b)) => a.to_real().partial_cmp(b),
            (Value::Real(a), Value::Decimal(b)) => a.partial_cmp(&b.to_real()),
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
            Value::Date(d) => format!("DATE '{}'", datetime::format_date(*d)),
            Value::Time(t) => format!("TIME '{}'", datetime::format_time(*t)),
            Value::Timestamp(t) => format!("TIMESTAMP '{}'", datetime::format_timestamp(*t)),
            Value::TimestampTz(t) => {
                format!("TIMESTAMPTZ '{}'", datetime::format_timestamptz(*t))
            }
            Value::Interval(i) => format!("INTERVAL '{i}'"),
            Value::Blob(b) => format!("X'{}'", hex(b)),
            Value::Decimal(d) => format!("DECIMAL '{d}'"),
        }
    }
}

/// Bytes as hexadecimal digits, two for each, in upper case.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// A DECIMAL as one of a DECIMAL column of type `data_type`, if it equals one.
fn fit_decimal(d: Decimal, data_type: DataType) -> Option<Decimal> {
    match data_type {
        DataType::Decimal(None) => Some(d.normalize()),
        DataType::Decimal(Some((p, s))) => {
            d.rescale_exactly(s).filter(|d| d.whole_digits() <= p - s)
        }
        _ => None,
    }
}

impl fmt::Display for Value {
    /// The value as a result shows it: a TEXT, a date or time, or a DECIMAL
    /// without quotes, and anything else as its literal.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Value::Text(s) => f.write_str(s),
            Value::Date(d) => f.write_str(&datetime::format_date(*d)),
            Value::Time(t) => f.write_str(&datetime::format_time(*t)),
            Value::Timestamp(t) => f.write_str(&datetime::format_timestamp(*t)),
            Value::TimestampTz(t) => f.write_str(&datetime::format_timestamptz(*t)),
            Value::Interval(i) => write!(f, "{i}"),
            Value::Decimal(d) => write!(f, "{d}"),
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
            DataType::Date => "DATE",
            DataType::Time => "TIME",
            DataType::Timestamp => "TIMESTAMP",
            DataType::TimestampTz => "TIMESTAMPTZ",
            DataType::Interval => "INTERVAL",
            DataType::Blob => "BLOB",
            DataType::Decimal(None) => "DECIMAL",
            DataType::Decimal(Some((p, s))) => return write!(f, "DECIMAL({p}, {s})"),
        })
    }
}
