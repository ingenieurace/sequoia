use std::cmp::Ordering;
use std::fmt;

/// Column data types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Int,
    Text,
    Bool,
}

/// A column in a table schema.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub primary_key: bool,
}

/// How a table stores its rows: `CREATE TABLE ... USING HEAP | LSM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageKind {
    /// Slotted pages, updated in place. The default.
    #[default]
    Heap,
    /// A log-structured merge tree.
    Lsm,
}

/// A single SQL value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    Null,
    Int(i64),
    Text(String),
    Bool(bool),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Whether this value can be stored in a column of the given type.
    pub fn fits(&self, data_type: DataType) -> bool {
        matches!(
            (self, data_type),
            (Value::Null, _)
                | (Value::Int(_), DataType::Int)
                | (Value::Text(_), DataType::Text)
                | (Value::Bool(_), DataType::Bool)
        )
    }

    /// Compares two non-null values of the same type.
    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
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
        })
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            DataType::Int => "INT",
            DataType::Text => "TEXT",
            DataType::Bool => "BOOL",
        })
    }
}
