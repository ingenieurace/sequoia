//! What an expression means: evaluates it against one row, with SQL's
//! three-valued logic for NULL.

use crate::error::{Error, Result, err};
use crate::sql::ast::{BinaryOp, ColumnRef, Expr, UnaryOp};
use crate::types::{Column, Value};

/// A column of the rows an expression is evaluated on: its name, and the
/// table it comes from, if any, which `table.name` refers to it by.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub table: Option<String>,
    pub name: String,
}

impl Field {
    /// The fields of a table's rows, qualified by `qualifier` (its name or alias).
    pub fn of_table(qualifier: &str, columns: &[Column]) -> Vec<Field> {
        columns.iter().map(|c| Field { table: Some(qualifier.into()), name: c.name.clone() }).collect()
    }

    /// A field that only its name refers to.
    pub fn named(name: impl Into<String>) -> Field {
        Field { table: None, name: name.into() }
    }
}

/// Finds the field a column reference names: the one with its name, and
/// from its table if it names one. Fails if there is none, or more than one.
pub fn resolve(fields: &[Field], column: &ColumnRef) -> Result<usize> {
    let mut found = fields
        .iter()
        .enumerate()
        .filter(|(_, f)| f.name == column.name && (column.table.is_none() || f.table == column.table));
    match (found.next(), found.next()) {
        (Some((i, _)), None) => Ok(i),
        (None, _) => err(format!("unknown column: {column}")),
        (Some(_), Some(_)) => err(format!("column reference {column} is ambiguous")),
    }
}

/// Evaluates a WHERE clause; NULL counts as false.
pub fn matches(filter: &Option<Expr>, fields: &[Field], row: &[Value]) -> Result<bool> {
    let Some(expr) = filter else { return Ok(true) };
    match eval(expr, fields, row)? {
        Value::Bool(b) => Ok(b),
        Value::Null => Ok(false),
        other => err(format!("WHERE clause must be boolean, got {}", other.to_sql())),
    }
}

/// Evaluates an expression against one row, whose columns are `fields`.
pub fn eval(expr: &Expr, fields: &[Field], row: &[Value]) -> Result<Value> {
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Column(column) => Ok(row[resolve(fields, column)?].clone()),
        Expr::IsNull(inner, negated) => Ok(Value::Bool(eval(inner, fields, row)?.is_null() != *negated)),
        // Grouped queries replace aggregates by their results before evaluating.
        Expr::Aggregate(function, ..) => err(format!("{} is not allowed here", function.name().to_uppercase())),
        Expr::Unary(op, inner) => match (op, eval(inner, fields, row)?) {
            (_, Value::Null) => Ok(Value::Null),
            (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
            (UnaryOp::Neg, Value::Int(n)) => n.checked_neg().map(Value::Int).ok_or_else(overflow),
            (op, value) => err(format!("cannot apply {op:?} to {}", value.to_sql())),
        },
        Expr::Binary(left, op, right) => binary(eval(left, fields, row)?, *op, eval(right, fields, row)?),
    }
}

fn binary(left: Value, op: BinaryOp, right: Value) -> Result<Value> {
    use BinaryOp::*;
    use Value::{Bool, Int, Null};

    let type_error = |l: &Value, r: &Value| err(format!("cannot apply {op:?} to {} and {}", l.to_sql(), r.to_sql()));

    match op {
        // Three-valued logic: NULL means "unknown".
        And => match (&left, &right) {
            (Bool(false), Bool(_) | Null) | (Null, Bool(false)) => Ok(Bool(false)),
            (Bool(true), Bool(true)) => Ok(Bool(true)),
            (Bool(_) | Null, Bool(_) | Null) => Ok(Null),
            _ => type_error(&left, &right),
        },
        Or => match (&left, &right) {
            (Bool(true), Bool(_) | Null) | (Null, Bool(true)) => Ok(Bool(true)),
            (Bool(false), Bool(false)) => Ok(Bool(false)),
            (Bool(_) | Null, Bool(_) | Null) => Ok(Null),
            _ => type_error(&left, &right),
        },
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            if left.is_null() || right.is_null() {
                return Ok(Null);
            }
            let Some(ord) = left.compare(&right) else { return type_error(&left, &right) };
            Ok(Bool(match op {
                Eq => ord.is_eq(),
                NotEq => ord.is_ne(),
                Lt => ord.is_lt(),
                LtEq => ord.is_le(),
                Gt => ord.is_gt(),
                _ => ord.is_ge(),
            }))
        }
        Add | Sub | Mul | Div => match (&left, &right) {
            (Null, _) | (_, Null) => Ok(Null),
            (Int(_), Int(0)) if op == Div => err("division by zero"),
            (Int(a), Int(b)) => {
                let result = match op {
                    Add => a.checked_add(*b),
                    Sub => a.checked_sub(*b),
                    Mul => a.checked_mul(*b),
                    _ => a.checked_div(*b),
                };
                result.map(Int).ok_or_else(overflow)
            }
            _ => type_error(&left, &right),
        },
    }
}

fn overflow() -> Error {
    Error("integer overflow".into())
}
