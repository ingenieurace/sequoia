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
use crate::sql::ast::{BinaryOp, ColumnRef, Expr, Function, UnaryOp};
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
        Expr::Case { operand, branches, otherwise } => {
            let operand = operand.as_deref().map(&mut eval).transpose()?;
            for (when, then) in branches {
                let when = eval(when)?;
                let taken = match &operand {
                    Some(operand) => binary(operand.clone(), BinaryOp::Eq, when)? == Value::Bool(true),
                    None => match when {
                        Value::Bool(b) => b,
                        Value::Null => false,
                        other => return err(format!("CASE WHEN must be boolean, got {}", other.to_sql())),
                    },
                };
                if taken {
                    return eval(then);
                }
            }
            otherwise.as_deref().map_or(Ok(Value::Null), eval)
        }
        Expr::Function(function, args) => call(*function, args, &mut eval),
    }
}

/// Calls a scalar function on its arguments, which `eval` evaluates.
fn call(function: Function, args: &[Expr], eval: &mut dyn FnMut(&Expr) -> Result<Value>) -> Result<Value> {
    if function == Function::Coalesce {
        for arg in args {
            let value = eval(arg)?;
            if !value.is_null() {
                return Ok(value);
            }
        }
        return Ok(Value::Null);
    }
    let values: Vec<Value> = args.iter().map(&mut *eval).collect::<Result<_>>()?;
    apply(function, &values)
}

/// Applies a scalar function to the values of its arguments. `COALESCE`,
/// given them all, takes the first that is not NULL.
pub(crate) fn apply(function: Function, values: &[Value]) -> Result<Value> {
    use Value::{Int, Null, Real, Text};

    if function == Function::Coalesce {
        return Ok(values.iter().find(|v| !v.is_null()).cloned().unwrap_or(Null));
    }
    let wrong = |value: &Value, expected: &str| {
        err(format!("{} expects {expected}, got {}", function.name().to_uppercase(), value.to_sql()))
    };
    match (function, values) {
        (Function::Nullif, [a, b]) => {
            Ok(if binary(a.clone(), BinaryOp::Eq, b.clone())? == Value::Bool(true) { Null } else { a.clone() })
        }
        (_, values) if values.iter().any(Value::is_null) => Ok(Null),
        (Function::Length, [Text(s)]) => Ok(Int(s.chars().count() as i64)),
        (Function::Upper, [Text(s)]) => Ok(Text(s.to_ascii_uppercase())),
        (Function::Lower, [Text(s)]) => Ok(Text(s.to_ascii_lowercase())),
        (Function::Substr, [Text(s), Int(start)]) => Ok(Text(substr(s, *start, None))),
        (Function::Substr, [Text(s), Int(start), Int(len)]) => Ok(Text(substr(s, *start, Some(*len)))),
        (Function::Abs, [Int(n)]) => n.checked_abs().map(Int).ok_or_else(overflow),
        (Function::Abs, [Real(r)]) => Ok(Real(r.abs())),
        (Function::Round, [x]) if x.as_real().is_some() => Ok(Real(round(x.as_real().unwrap(), 0))),
        (Function::Round, [x, Int(digits)]) if x.as_real().is_some() => Ok(Real(round(x.as_real().unwrap(), *digits))),
        (Function::Length | Function::Upper | Function::Lower | Function::Substr, [value, ..])
            if !matches!(value, Text(_)) =>
        {
            wrong(value, "TEXT")
        }
        (Function::Substr, [_, rest @ ..]) => {
            wrong(rest.iter().find(|v| !matches!(v, Int(_))).expect("one is not INT"), "INT")
        }
        (Function::Abs, [value]) => wrong(value, "a number"),
        (Function::Round, [x, ..]) if x.as_real().is_none() => wrong(x, "a number"),
        (Function::Round, [_, digits]) => wrong(digits, "INT"),
        _ => unreachable!("the parser checks how many arguments a function has"),
    }
}

/// `SUBSTR`, as SQLite computes it: `start` counts characters from 1, or
/// from the end if negative, and 0 is just before the first; a negative
/// `len` takes the characters before `start` instead of from it.
fn substr(s: &str, start: i64, len: Option<i64>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let total = chars.len() as i64;
    let backwards = len.is_some_and(|len| len < 0);
    let (mut from, mut count) = (start, len.map_or(i64::MAX, i64::saturating_abs));
    if from < 0 {
        from += total;
        if from < 0 {
            count = count.saturating_add(from).max(0);
            from = 0;
        }
    } else if from > 0 {
        from -= 1;
    } else if count > 0 {
        count -= 1;
    }
    if backwards {
        from -= count;
        if from < 0 {
            count += from;
            from = 0;
        }
    }
    let end = from.saturating_add(count).min(total);
    if from >= end { String::new() } else { chars[from as usize..end as usize].iter().collect() }
}

/// `ROUND`, as SQLite computes it: to `digits` decimal places (none if
/// negative, at most 30), halves away from zero, going by the REAL's exact
/// decimal value, so 2.675, which is just below, rounds to 2.67.
fn round(x: f64, digits: i64) -> f64 {
    let digits = digits.clamp(0, 30) as usize;
    if digits == 0 {
        return x.round();
    }
    // 25 more places decide which way it goes, as the exact value's next digits would.
    let exact = format!("{:.*}", digits + 25, x.abs());
    let (whole, fraction) = exact.split_once('.').expect("formatted with places");
    let mut kept: Vec<u8> = [whole.as_bytes(), &fraction.as_bytes()[..digits]].concat();
    if fraction.as_bytes()[digits] >= b'5' {
        // Add one in the last place kept, carrying.
        let mut i = kept.len();
        loop {
            if i == 0 {
                kept.insert(0, b'1');
                break;
            }
            i -= 1;
            if kept[i] == b'9' {
                kept[i] = b'0';
            } else {
                kept[i] += 1;
                break;
            }
        }
    }
    let (whole, fraction) = kept.split_at(kept.len() - digits);
    let rounded: f64 = format!("{}.{}", String::from_utf8_lossy(whole), String::from_utf8_lossy(fraction)).parse().unwrap();
    rounded.copysign(x)
}

/// Whether `text` matches a LIKE `pattern`: `%` matches any characters, `_`
/// any one, and ASCII letters match in either case, as in SQLite.
pub(crate) fn like(text: &str, pattern: &str) -> bool {
    let (text, pattern): (Vec<char>, Vec<char>) = (text.chars().collect(), pattern.chars().collect());
    let (mut t, mut p) = (0, 0);
    // After the last `%` passed: where the pattern goes on, and the text
    // position it was tried from.
    let mut retry: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('%') => {
                p += 1;
                retry = Some((p, t));
            }
            Some(&c) if c == '_' || c.eq_ignore_ascii_case(&text[t]) => {
                p += 1;
                t += 1;
            }
            _ => match retry {
                // Let the `%` take one more character, and try again from there.
                Some((after, from)) => {
                    retry = Some((after, from + 1));
                    (p, t) = (after, from + 1);
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '%')
}

pub(crate) fn unary(op: UnaryOp, value: Value) -> Result<Value> {
    match (op, value) {
        (_, Value::Null) => Ok(Value::Null),
        (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (UnaryOp::Neg, Value::Int(n)) => n.checked_neg().map(Value::Int).ok_or_else(overflow),
        (UnaryOp::Neg, Value::Real(r)) => Ok(Value::Real(-r)),
        (op, value) => err(format!("cannot apply {op:?} to {}", value.to_sql())),
    }
}

pub(crate) fn binary(left: Value, op: BinaryOp, right: Value) -> Result<Value> {
    use BinaryOp::*;
    use Value::{Bool, Int, Null, Text};

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
            // With a REAL, both as REALs.
            (a, b) => match (a.as_real(), b.as_real()) {
                (Some(_), Some(0.0)) if op == Div => err("division by zero"),
                (Some(a), Some(b)) => real(match op {
                    Add => a + b,
                    Sub => a - b,
                    Mul => a * b,
                    _ => a / b,
                }),
                _ => type_error(&left, &right),
            },
        },
        Like => match (&left, &right) {
            (Null, _) | (_, Null) => Ok(Null),
            (Text(text), Text(pattern)) => Ok(Bool(like(text, pattern))),
            _ => type_error(&left, &right),
        },
        Concat => match (&left, &right) {
            (Null, _) | (_, Null) => Ok(Null),
            (Text(a), Text(b)) => Ok(Text(format!("{a}{b}"))),
            _ => type_error(&left, &right),
        },
    }
}

/// A REAL result, or an error if it is too big to be one.
pub(crate) fn real(r: f64) -> Result<Value> {
    match r.is_finite() {
        true => Ok(Value::Real(r)),
        false => err("REAL overflow"),
    }
}

pub(crate) fn overflow() -> Error {
    Error("integer overflow".into())
}
