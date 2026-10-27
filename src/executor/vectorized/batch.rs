//! What vectorized steps hand each other: batches of rows stored a column
//! at a time, each column a [`Vector`] of one type where it can be.
//!
//! ```text
//! Batch { len: 6, selection: Some([0, 2, 5]) }
//!   column 0: Int  [ 7,  3,  9,  1,  4,  8]
//!   column 1: Text ["a", "b", "c", "d", "e", "f"]   nulls: [_, _, _, _, x, _]
//!   rows of the batch: 0, 2 and 5 (a filter dropped the others)
//! ```
//!
//! A filter does not copy the rows it keeps: it only narrows the batch's
//! selection, the positions of the rows still in it. Columns are shared
//! (`Rc`), so a step that passes a column on unchanged copies nothing.

use std::rc::Rc;

use crate::catalog::table::Row;
use crate::types::Value;

/// Rows a scan puts in a batch, and the steps that make batches of rows.
pub const BATCH_SIZE: usize = 1024;

/// Which values of a typed vector are NULL, if any are: `true` for NULL.
/// The value at a NULL is a placeholder.
pub type Nulls = Option<Vec<bool>>;

/// One column of a batch.
#[derive(Debug, Clone)]
pub enum Vector {
    Int(Vec<i64>, Nulls),
    Real(Vec<f64>, Nulls),
    Bool(Vec<bool>, Nulls),
    Text(Vec<String>, Nulls),
    /// The same value in every one of this many rows: a literal, or a
    /// column the scan did not read.
    Const(Value, usize),
    /// Values of more than one type, as an expression may give.
    Mixed(Vec<Value>),
}

impl Vector {
    /// A vector of these values: of their type if they share one.
    pub fn from_values(values: Vec<Value>) -> Vector {
        let n = values.len();
        let first = values.iter().find(|v| !v.is_null());
        let Some(first) = first else {
            return Vector::Const(Value::Null, n);
        };
        let same = std::mem::discriminant(first);
        if values
            .iter()
            .any(|v| !v.is_null() && std::mem::discriminant(v) != same)
        {
            return Vector::Mixed(values);
        }
        let nulls = values
            .iter()
            .any(Value::is_null)
            .then(|| values.iter().map(Value::is_null).collect());
        macro_rules! typed {
            ($variant:ident, $default:expr) => {
                Vector::$variant(
                    values
                        .into_iter()
                        .map(|v| match v {
                            Value::$variant(x) => x,
                            _ => $default,
                        })
                        .collect(),
                    nulls,
                )
            };
        }
        match first {
            Value::Int(_) => typed!(Int, 0),
            Value::Real(_) => typed!(Real, 0.0),
            Value::Bool(_) => typed!(Bool, false),
            Value::Text(_) => typed!(Text, String::new()),
            // The other types go through the loops a value at a time.
            Value::Date(_)
            | Value::Time(_)
            | Value::Timestamp(_)
            | Value::Blob(_)
            | Value::Decimal(_) => Vector::Mixed(values),
            Value::Null => unreachable!("found a value that is not NULL"),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Vector::Int(v, _) => v.len(),
            Vector::Real(v, _) => v.len(),
            Vector::Bool(v, _) => v.len(),
            Vector::Text(v, _) => v.len(),
            Vector::Const(_, n) => *n,
            Vector::Mixed(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The nulls of a typed vector.
    fn nulls(&self) -> Option<&[bool]> {
        match self {
            Vector::Int(_, nulls)
            | Vector::Real(_, nulls)
            | Vector::Bool(_, nulls)
            | Vector::Text(_, nulls) => nulls.as_deref(),
            _ => None,
        }
    }

    pub fn is_null(&self, i: usize) -> bool {
        match self {
            Vector::Const(value, _) => value.is_null(),
            Vector::Mixed(values) => values[i].is_null(),
            typed => typed.nulls().is_some_and(|nulls| nulls[i]),
        }
    }

    /// The value in row `i`.
    pub fn get(&self, i: usize) -> Value {
        if self.is_null(i) {
            return Value::Null;
        }
        match self {
            Vector::Int(v, _) => Value::Int(v[i]),
            Vector::Real(v, _) => Value::Real(v[i]),
            Vector::Bool(v, _) => Value::Bool(v[i]),
            Vector::Text(v, _) => Value::Text(v[i].clone()),
            Vector::Const(value, _) => value.clone(),
            Vector::Mixed(values) => values[i].clone(),
        }
    }

    /// The values at these positions, in their order.
    pub fn gather(&self, rows: &[u32]) -> Vector {
        fn pick<T: Clone>(values: &[T], rows: &[u32]) -> Vec<T> {
            rows.iter().map(|&i| values[i as usize].clone()).collect()
        }
        let nulls = self
            .nulls()
            .map(|nulls| pick(nulls, rows))
            .filter(|nulls| nulls.contains(&true));
        match self {
            Vector::Int(v, _) => Vector::Int(pick(v, rows), nulls),
            Vector::Real(v, _) => Vector::Real(pick(v, rows), nulls),
            Vector::Bool(v, _) => Vector::Bool(pick(v, rows), nulls),
            Vector::Text(v, _) => Vector::Text(pick(v, rows), nulls),
            Vector::Const(value, _) => Vector::Const(value.clone(), rows.len()),
            Vector::Mixed(values) => Vector::Mixed(pick(values, rows)),
        }
    }
}

/// Rows stored a column at a time.
#[derive(Debug, Clone)]
pub struct Batch {
    /// Each `len` long.
    pub columns: Vec<Rc<Vector>>,
    pub len: usize,
    /// The positions of the rows in the batch, in order, if not all of
    /// `0..len` are.
    pub selection: Option<Vec<u32>>,
}

impl Batch {
    /// A batch of all the rows of these columns.
    pub fn new(columns: Vec<Rc<Vector>>, len: usize) -> Self {
        debug_assert!(columns.iter().all(|c| c.len() == len));
        Batch {
            columns,
            len,
            selection: None,
        }
    }

    /// A batch of these rows, each `width` values long.
    pub fn from_rows(rows: Vec<Row>, width: usize) -> Self {
        let len = rows.len();
        let mut columns: Vec<Vec<Value>> = (0..width).map(|_| Vec::with_capacity(len)).collect();
        for row in rows {
            for (column, value) in columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        Batch::new(
            columns
                .into_iter()
                .map(|values| Rc::new(Vector::from_values(values)))
                .collect(),
            len,
        )
    }

    /// How many rows are in the batch.
    pub fn rows(&self) -> usize {
        self.selection.as_ref().map_or(self.len, Vec::len)
    }

    /// The position of the `i`th row in the columns.
    pub fn position(&self, i: usize) -> usize {
        self.selection
            .as_ref()
            .map_or(i, |selection| selection[i] as usize)
    }

    /// The `i`th row.
    pub fn row(&self, i: usize) -> Row {
        let at = self.position(i);
        self.columns.iter().map(|column| column.get(at)).collect()
    }

    /// Column `c`'s values of the batch's rows, in order.
    pub fn column(&self, c: usize) -> Rc<Vector> {
        match &self.selection {
            None => self.columns[c].clone(),
            Some(selection) => Rc::new(self.columns[c].gather(selection)),
        }
    }

    /// Keeps only the `i`th rows for each `i` in `keep`, which goes up.
    pub fn keep(mut self, keep: &[usize]) -> Self {
        if keep.len() == self.rows() {
            return self;
        }
        self.selection = Some(keep.iter().map(|&i| self.position(i) as u32).collect());
        self
    }

    pub fn into_rows(self) -> Vec<Row> {
        (0..self.rows()).map(|i| self.row(i)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_are_typed_when_their_values_share_a_type() {
        let ints = Vector::from_values(vec![Value::Int(1), Value::Null, Value::Int(3)]);
        assert!(
            matches!(&ints, Vector::Int(v, Some(nulls)) if v == &[1, 0, 3] && nulls == &[false, true, false])
        );
        assert_eq!((ints.get(1), ints.get(2)), (Value::Null, Value::Int(3)));
        assert!(matches!(
            Vector::from_values(vec![Value::Null, Value::Null]),
            Vector::Const(Value::Null, 2)
        ));
        // An INT and a REAL are values of two types, though they may be equal.
        assert!(matches!(
            Vector::from_values(vec![Value::Int(1), Value::Real(1.0)]),
            Vector::Mixed(_)
        ));
    }

    #[test]
    fn a_selection_keeps_rows_without_copying_them() {
        let rows = (0..5)
            .map(|i| vec![Value::Int(i), Value::Text(format!("r{i}"))])
            .collect();
        let batch = Batch::from_rows(rows, 2).keep(&[1, 3, 4]).keep(&[0, 2]);
        assert_eq!(batch.selection, Some(vec![1, 4]));
        assert_eq!(batch.rows(), 2);
        assert_eq!(batch.row(1), vec![Value::Int(4), Value::Text("r4".into())]);
        assert!(matches!(&*batch.column(0), Vector::Int(v, None) if v == &[1, 4]));
        assert_eq!(batch.into_rows().len(), 2);
    }
}
