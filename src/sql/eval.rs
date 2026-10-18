//! What an expression means: evaluates it against one row, with SQL's
//! three-valued logic for NULL.
//!
//! Subqueries are run by whoever evaluates the expression, through
//! [`Subqueries`]: this layer cannot read tables. `x IN (list)` means
//! `x = a OR x = b ...`; `x IN (SELECT ...)` looks the value up among the
//! subquery's, so a value of another type matches nothing instead of being
//! an error. Either is NULL, not false, when no value matches but one is NULL.

use std::collections::HashSet;
use std::rc::Rc;

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

    /// Whether the column reference may name this field.
    pub fn is_named_by(&self, column: &ColumnRef) -> bool {
        self.name == column.name && (column.table.is_none() || self.table == column.table)
    }
}

/// What a subquery returned, as much as its expression needs.
pub enum Answer {
    /// A scalar subquery's value: NULL for no row.
    Value(Value),
    /// Whether an `EXISTS` subquery returned a row.
    Exists(bool),
    /// An `IN` subquery's values other than NULL, and whether it returned a NULL.
    Values { values: HashSet<Value>, null: bool },
}

/// Runs the subqueries of the expressions being evaluated.
pub trait Subqueries {
    /// Runs `subquery` (an `Expr::Subquery`, `Exists` or `InSubquery`) for the
    /// row being evaluated, whose columns are `fields`: its outer references
    /// take their values from `row`.
    fn answer(&mut self, subquery: &Expr, fields: &[Field], row: &[Value]) -> Result<Rc<Answer>>;
}

/// For expressions evaluated where subqueries cannot run.
pub struct NoSubqueries;

impl Subqueries for NoSubqueries {
    fn answer(&mut self, _: &Expr, _: &[Field], _: &[Value]) -> Result<Rc<Answer>> {
        err("subqueries are not allowed here")
    }
}

/// Finds the field a column reference names: the one with its name, and
/// from its table if it names one. Fails if there is none, or more than one.
pub fn resolve(fields: &[Field], column: &ColumnRef) -> Result<usize> {
    let mut found = fields.iter().enumerate().filter(|(_, f)| f.is_named_by(column));
    match (found.next(), found.next()) {
        (Some((i, _)), None) => Ok(i),
        (None, _) => err(format!("unknown column: {column}")),
        (Some(_), Some(_)) => err(format!("column reference {column} is ambiguous")),
    }
}

/// Evaluates a WHERE clause; NULL counts as false.
pub fn matches(filter: &Option<Expr>, fields: &[Field], row: &[Value]) -> Result<bool> {
    matches_with(filter, fields, row, &mut NoSubqueries)
}

/// Evaluates a WHERE clause, running its subqueries with `subqueries`.
pub fn matches_with(
    filter: &Option<Expr>,
    fields: &[Field],
    row: &[Value],
    subqueries: &mut dyn Subqueries,
) -> Result<bool> {
    let Some(expr) = filter else { return Ok(true) };
    match eval_with(expr, fields, row, subqueries)? {
        Value::Bool(b) => Ok(b),
        Value::Null => Ok(false),
        other => err(format!("WHERE clause must be boolean, got {}", other.to_sql())),
    }
}

/// Evaluates an expression against one row, whose columns are `fields`.
pub fn eval(expr: &Expr, fields: &[Field], row: &[Value]) -> Result<Value> {
    eval_with(expr, fields, row, &mut NoSubqueries)
}

/// Evaluates an expression against one row, running its subqueries with `subqueries`.
pub fn eval_with(expr: &Expr, fields: &[Field], row: &[Value], subqueries: &mut dyn Subqueries) -> Result<Value> {
    let mut eval = |e: &Expr| eval_with(e, fields, row, subqueries);
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Column(column) => Ok(row[resolve(fields, column)?].clone()),
        Expr::IsNull(inner, negated) => Ok(Value::Bool(eval(inner)?.is_null() != *negated)),
        // Grouped queries replace aggregates by their results before evaluating.
        Expr::Aggregate(function, ..) => err(format!("{} is not allowed here", function.name().to_uppercase())),
        Expr::Unary(op, inner) => unary(*op, eval(inner)?),
        Expr::Binary(left, op, right) => binary(eval(left)?, *op, eval(right)?),
        Expr::InList(left, items, negated) => {
            let value = eval(left)?;
            let mut found = Value::Bool(false);
            for item in items {
                found = binary(found, BinaryOp::Or, binary(value.clone(), BinaryOp::Eq, eval(item)?)?)?;
            }
            if *negated { unary(UnaryOp::Not, found) } else { Ok(found) }
        }
        Expr::Outer(column) => err(format!("column {column} of an outer query cannot be used here")),
        Expr::Subquery(_) | Expr::Exists(_) => match &*subqueries.answer(expr, fields, row)? {
            Answer::Value(value) => Ok(value.clone()),
            Answer::Exists(found) => Ok(Value::Bool(*found)),
            Answer::Values { .. } => unreachable!("only IN asks for values"),
        },
        Expr::InSubquery(left, _, negated) => {
            let value = eval(left)?;
            let answer = subqueries.answer(expr, fields, row)?;
            let Answer::Values { values, null } = &*answer else { unreachable!("IN asks for values") };
            let found = match value {
                _ if values.is_empty() && !null => Value::Bool(false),
                Value::Null => Value::Null,
                value if values.contains(&value) => Value::Bool(true),
                _ if *null => Value::Null,
                _ => Value::Bool(false),
            };
            if *negated { unary(UnaryOp::Not, found) } else { Ok(found) }
        }
    }
}

fn unary(op: UnaryOp, value: Value) -> Result<Value> {
    match (op, value) {
        (_, Value::Null) => Ok(Value::Null),
        (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (UnaryOp::Neg, Value::Int(n)) => n.checked_neg().map(Value::Int).ok_or_else(overflow),
        (op, value) => err(format!("cannot apply {op:?} to {}", value.to_sql())),
    }
}

fn binary(left: Value, op: BinaryOp, right: Value) -> Result<Value> {
    use BinaryOp::*;
    use Value::{Bool, Int, Null};

    let type_error = |l: &Value, r: &Value| err(format!("cannot apply {op:?} to {} and {}", l.to_sql(), r.to_sql()));

    match op {
        // Three-valued logic: NULL means "unknown".
        And => match (&left, &right) {
            (Bool(false), Bool(_) | Null) | (Bool(_) | Null, Bool(false)) => Ok(Bool(false)),
            (Bool(true), Bool(true)) => Ok(Bool(true)),
            (Bool(_) | Null, Bool(_) | Null) => Ok(Null),
            _ => type_error(&left, &right),
        },
        Or => match (&left, &right) {
            (Bool(true), Bool(_) | Null) | (Bool(_) | Null, Bool(true)) => Ok(Bool(true)),
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
