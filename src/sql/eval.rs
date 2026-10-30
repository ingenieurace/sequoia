//! What an expression means: evaluates it against one row, with SQL's
//! three-valued logic for NULL.
//!
//! `AND` and `OR` evaluate their right side only if their left one does not
//! decide: `x <> 0 AND 10 / x > 1` fails on no row. `CASE` and `COALESCE`
//! evaluate only the parts they need. Columns are found by name, unless
//! [`locate`] has found them already, as plan steps have it do when they start.
//!
//! Subqueries are run by whoever evaluates the expression, through
//! [`Subqueries`]: this layer cannot read tables. `x IN (list)` means
//! `x = a OR x = b ...`; `x IN (SELECT ...)` looks the value up among the
//! subquery's, so a value of another type matches nothing instead of being
//! an error. Either is NULL, not false, when no value matches but one is NULL.

use std::collections::HashSet;
use std::rc::Rc;

use crate::datetime;
use crate::decimal::Decimal;
use crate::error::{Error, Result, err};
use crate::sql::ast::{
    BinaryOp, ColumnRef, Expr, Function, InsertSource, Projection, SelectItem, Statement, UnaryOp,
};
use crate::types::{Column, DataType, Value};

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
        columns
            .iter()
            .map(|c| Field {
                table: Some(qualifier.into()),
                name: c.name.clone(),
            })
            .collect()
    }

    /// A field that only its name refers to.
    pub fn named(name: impl Into<String>) -> Field {
        Field {
            table: None,
            name: name.into(),
        }
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
    let mut found = fields
        .iter()
        .enumerate()
        .filter(|(_, f)| f.is_named_by(column));
    match (found.next(), found.next()) {
        (Some((i, _)), None) => Ok(i),
        (None, _) => err(format!("unknown column: {column}")),
        (Some(_), Some(_)) => err(format!("column reference {column} is ambiguous")),
    }
}

/// The expression with each column replaced by its position in `fields`
/// (an [`Expr::Slot`]), so that evaluating it on many rows finds each
/// column once rather than by name for every row. A column `fields` does
/// not have, or has more than once, stays as it is, to fail as it would
/// have when evaluated. Subqueries stay as they are: their own columns are
/// their tables', and their outer ones are found when they run.
pub fn locate(expr: &Expr, fields: &[Field]) -> Expr {
    let located = expr.replace(&mut |e| match e {
        Expr::Column(column) => Some(Ok(match resolve(fields, column) {
            Ok(i) => Expr::Slot(i, column.clone()),
            Err(_) => e.clone(),
        })),
        Expr::Subquery(_) | Expr::Exists(_) => Some(Ok(e.clone())),
        Expr::InSubquery(left, query, negated) => Some(Ok(Expr::InSubquery(
            Box::new(locate(left, fields)),
            query.clone(),
            *negated,
        ))),
        _ => None,
    });
    located.expect("locating columns cannot fail")
}

/// Each of the expressions as [`locate`] makes it.
pub fn locate_all(exprs: &[Expr], fields: &[Field]) -> Vec<Expr> {
    exprs.iter().map(|e| locate(e, fields)).collect()
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
        other => err(format!(
            "WHERE clause must be boolean, got {}",
            other.to_sql()
        )),
    }
}

/// Evaluates an expression against one row, whose columns are `fields`.
pub fn eval(expr: &Expr, fields: &[Field], row: &[Value]) -> Result<Value> {
    eval_with(expr, fields, row, &mut NoSubqueries)
}

/// Evaluates an expression against one row, running its subqueries with `subqueries`.
pub fn eval_with(
    expr: &Expr,
    fields: &[Field],
    row: &[Value],
    subqueries: &mut dyn Subqueries,
) -> Result<Value> {
    let mut eval = |e: &Expr| eval_with(e, fields, row, subqueries);
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Slot(i, _) => Ok(row[*i].clone()),
        Expr::Column(column) => Ok(row[resolve(fields, column)?].clone()),
        Expr::IsNull(inner, negated) => Ok(Value::Bool(eval(inner)?.is_null() != *negated)),
        // Grouped queries replace aggregates by their results before evaluating.
        Expr::Aggregate(function, ..) => err(format!(
            "{} is not allowed here",
            function.name().to_uppercase()
        )),
        // Queries compute window functions in steps of their own, which the
        // expressions above read as columns.
        Expr::Window(window) => err(format!(
            "window functions are not allowed here: {}",
            window.function.name().to_uppercase()
        )),
        Expr::Unary(op, inner) => unary(*op, eval(inner)?),
        // FALSE AND x is FALSE, and TRUE OR x TRUE, whatever x: x is not
        // evaluated, so it fails on no row the left side decides.
        Expr::Binary(left, op @ (BinaryOp::And | BinaryOp::Or), right) => {
            let left = eval(left)?;
            if left == Value::Bool(*op == BinaryOp::Or) {
                return Ok(left);
            }
            binary(left, *op, eval(right)?)
        }
        Expr::Binary(left, op, right) => binary(eval(left)?, *op, eval(right)?),
        Expr::InList(left, items, negated) => {
            let value = eval(left)?;
            let mut found = Value::Bool(false);
            for item in items {
                found = binary(
                    found,
                    BinaryOp::Or,
                    binary(value.clone(), BinaryOp::Eq, eval(item)?)?,
                )?;
            }
            if *negated {
                unary(UnaryOp::Not, found)
            } else {
                Ok(found)
            }
        }
        Expr::Outer(column) => err(format!(
            "column {column} of an outer query cannot be used here"
        )),
        Expr::Subquery(_) | Expr::Exists(_) => match &*subqueries.answer(expr, fields, row)? {
            Answer::Value(value) => Ok(value.clone()),
            Answer::Exists(found) => Ok(Value::Bool(*found)),
            Answer::Values { .. } => unreachable!("only IN asks for values"),
        },
        Expr::InSubquery(left, _, negated) => {
            let value = eval(left)?;
            let answer = subqueries.answer(expr, fields, row)?;
            let Answer::Values { values, null } = &*answer else {
                unreachable!("IN asks for values")
            };
            let found = match value {
                _ if values.is_empty() && !null => Value::Bool(false),
                Value::Null => Value::Null,
                value if values.contains(&value) => Value::Bool(true),
                _ if *null => Value::Null,
                _ => Value::Bool(false),
            };
            if *negated {
                unary(UnaryOp::Not, found)
            } else {
                Ok(found)
            }
        }
        Expr::Case {
            operand,
            branches,
            otherwise,
        } => {
            let operand = operand.as_deref().map(&mut eval).transpose()?;
            for (when, then) in branches {
                let when = eval(when)?;
                let taken = match &operand {
                    Some(operand) => {
                        binary(operand.clone(), BinaryOp::Eq, when)? == Value::Bool(true)
                    }
                    None => match when {
                        Value::Bool(b) => b,
                        Value::Null => false,
                        other => {
                            return err(format!(
                                "CASE WHEN must be boolean, got {}",
                                other.to_sql()
                            ));
                        }
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
fn call(
    function: Function,
    args: &[Expr],
    eval: &mut dyn FnMut(&Expr) -> Result<Value>,
) -> Result<Value> {
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
        return Ok(values
            .iter()
            .find(|v| !v.is_null())
            .cloned()
            .unwrap_or(Null));
    }
    if let Some(result) = apply_more(function, values) {
        return result;
    }
    let wrong = |value: &Value, expected: &str| {
        err(format!(
            "{} expects {expected}, got {}",
            function.name().to_uppercase(),
            value.to_sql()
        ))
    };
    match (function, values) {
        (Function::Nullif, [a, b]) => Ok(
            if binary(a.clone(), BinaryOp::Eq, b.clone())? == Value::Bool(true) {
                Null
            } else {
                a.clone()
            },
        ),
        (_, values) if values.iter().any(Value::is_null) => Ok(Null),
        (Function::Cast(data_type), [value]) => cast(value, data_type),
        (Function::Extract(field), [value]) => extract(field, value),
        (Function::Length, [Value::Blob(b)]) => Ok(Int(b.len() as i64)),
        (Function::Abs, [Value::Decimal(d)]) => Ok(Value::Decimal(d.abs())),
        (Function::Round, [Value::Decimal(d)]) => Ok(Value::Decimal(d.round(0)?)),
        (Function::Round, [Value::Decimal(d), Int(places)]) => {
            Ok(Value::Decimal(d.round(*places)?))
        }
        (Function::Length, [Text(s)]) => Ok(Int(s.chars().count() as i64)),
        (Function::Upper, [Text(s)]) => Ok(Text(s.to_ascii_uppercase())),
        (Function::Lower, [Text(s)]) => Ok(Text(s.to_ascii_lowercase())),
        (Function::Substr, [Text(s), Int(start)]) => Ok(Text(substr(s, *start, None))),
        (Function::Substr, [Text(s), Int(start), Int(len)]) => {
            Ok(Text(substr(s, *start, Some(*len))))
        }
        (Function::Abs, [Int(n)]) => n.checked_abs().map(Int).ok_or_else(overflow),
        (Function::Abs, [Real(r)]) => Ok(Real(r.abs())),
        (Function::Round, [x]) if x.as_real().is_some() => Ok(Real(round(x.as_real().unwrap(), 0))),
        (Function::Round, [x, Int(digits)]) if x.as_real().is_some() => {
            Ok(Real(round(x.as_real().unwrap(), *digits)))
        }
        (Function::Length, [value]) => wrong(value, "TEXT or BLOB"),
        (Function::Upper | Function::Lower | Function::Substr, [value, ..])
            if !matches!(value, Text(_)) =>
        {
            wrong(value, "TEXT")
        }
        (Function::Substr, [_, rest @ ..]) => wrong(
            rest.iter()
                .find(|v| !matches!(v, Int(_)))
                .expect("one is not INT"),
            "INT",
        ),
        (Function::Abs, [value]) => wrong(value, "a number"),
        (Function::Round, [x, ..]) if x.as_real().is_none() => wrong(x, "a number"),
        (Function::Round, [_, digits]) => wrong(digits, "INT"),
        _ => unreachable!("the parser checks how many arguments a function has"),
    }
}

/// The most characters a function makes a TEXT of, as REPEAT and LPAD can
/// make very long ones: 2^28, a quarter of PostgreSQL's limit.
const LONGEST_TEXT: usize = 1 << 28;

/// Applies the functions on texts, numbers and times that `apply` does
/// not, if `function` is one of them.
fn apply_more(function: Function, values: &[Value]) -> Option<Result<Value>> {
    use Value::{Int, Null, Real, Text};
    let older = matches!(
        function,
        Function::Coalesce
            | Function::Nullif
            | Function::Length
            | Function::Upper
            | Function::Lower
            | Function::Substr
            | Function::Abs
            | Function::Round
            | Function::Cast(_)
            | Function::Extract(_)
    );
    if older {
        return None;
    }
    let name = || function.name().to_uppercase();
    let wrong = |value: &Value, expected: &str| {
        err(format!(
            "{} expects {expected}, got {}",
            name(),
            value.to_sql()
        ))
    };
    let text = |s: String| match s.chars().count() > LONGEST_TEXT {
        true => err(format!("{} would make a TEXT too long", name())),
        false => Ok(Text(s)),
    };
    // A number as a REAL, for the functions that compute with REALs.
    let real_of = |value: &Value| {
        value.to_f64().ok_or_else(|| {
            crate::error::Error(format!(
                "{} expects a number, got {}",
                name(),
                value.to_sql()
            ))
        })
    };
    Some((|| match (function, values) {
        // These take NULLs as their other arguments do not.
        (Function::Concat, values) => Ok(Text(
            values
                .iter()
                .filter(|v| !v.is_null())
                .map(|v| cast(v, DataType::Text).map(|t| t.to_string()))
                .collect::<Result<String>>()?,
        )),
        (Function::Greatest | Function::Least, values) => {
            let mut best: Option<&Value> = None;
            for value in values.iter().filter(|v| !v.is_null()) {
                let Some(ord) = best.map(|b| value.compare(b)) else {
                    best = Some(value);
                    continue;
                };
                let Some(ord) = ord else {
                    return err(format!(
                        "{} cannot compare {} and {}",
                        name(),
                        value.to_sql(),
                        best.expect("compared").to_sql()
                    ));
                };
                let better = match function {
                    Function::Greatest => ord.is_gt(),
                    _ => ord.is_lt(),
                };
                if better {
                    best = Some(value);
                }
            }
            Ok(best.cloned().unwrap_or(Null))
        }
        (Function::Pi, []) => Ok(Real(std::f64::consts::PI)),
        // When the statement started, if it was not put in already.
        (Function::Now, []) => Ok(Value::TimestampTz(datetime::now())),
        (Function::CurrentDate, []) => Ok(Value::Date(datetime::split_timestamp(datetime::now()).0)),
        (Function::CurrentTime, []) => Ok(Value::Time(datetime::split_timestamp(datetime::now()).1)),
        (Function::LocalTimestamp, []) => Ok(Value::Timestamp(datetime::now())),
        (_, values) if values.iter().any(Value::is_null) => Ok(Null),
        (Function::Trim | Function::Ltrim | Function::Rtrim, [Text(s), rest @ ..]) => {
            let set: Vec<char> = match rest {
                [] => vec![' '],
                [Text(chars)] => chars.chars().collect(),
                [other] => return wrong(other, "TEXT"),
                _ => unreachable!("at most two arguments"),
            };
            let strip = |c: char| set.contains(&c);
            Ok(Text(match function {
                Function::Ltrim => s.trim_start_matches(strip),
                Function::Rtrim => s.trim_end_matches(strip),
                _ => s.trim_matches(strip),
            }
            .to_string()))
        }
        (Function::Replace, [Text(s), Text(from), Text(to)]) => match from.is_empty() {
            true => Ok(Text(s.clone())),
            false => text(s.replace(from.as_str(), to)),
        },
        (Function::Instr, [Text(s), Text(part)]) => Ok(Int(match s.find(part.as_str()) {
            Some(at) => s[..at].chars().count() as i64 + 1,
            None => 0,
        })),
        (Function::Instr, [Value::Blob(b), Value::Blob(part)]) => Ok(Int(
            match part.is_empty() {
                true => 1,
                false => b
                    .windows(part.len())
                    .position(|w| w == part.as_slice())
                    .map_or(0, |at| at as i64 + 1),
            },
        )),
        (Function::Left | Function::Right, [Text(s), Int(n)]) => {
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            // How many to keep: n, or all but -n.
            let keep = match *n >= 0 {
                true => (*n).min(len),
                false => (len + n).max(0),
            } as usize;
            Ok(Text(match function {
                Function::Left => chars[..keep].iter().collect(),
                _ => chars[chars.len() - keep..].iter().collect(),
            }))
        }
        (Function::Reverse, [Text(s)]) => Ok(Text(s.chars().rev().collect())),
        (Function::Repeat, [Text(s), Int(n)]) => {
            let n = (*n).max(0) as usize;
            match s.chars().count().saturating_mul(n) > LONGEST_TEXT {
                true => err(format!("{} would make a TEXT too long", name())),
                false => Ok(Text(s.repeat(n))),
            }
        }
        (Function::Lpad | Function::Rpad, [Text(s), Int(length), rest @ ..]) => {
            let fill: Vec<char> = match rest {
                [] => vec![' '],
                [Text(fill)] => fill.chars().collect(),
                [other] => return wrong(other, "TEXT"),
                _ => unreachable!("at most three arguments"),
            };
            let length = (*length).max(0) as usize;
            if length > LONGEST_TEXT {
                return err(format!("{} would make a TEXT too long", name()));
            }
            let chars: Vec<char> = s.chars().collect();
            if chars.len() >= length || fill.is_empty() {
                return Ok(Text(chars[..length.min(chars.len())].iter().collect()));
            }
            let padding: String = fill.iter().cycle().take(length - chars.len()).collect();
            Ok(Text(match function {
                Function::Lpad => format!("{padding}{s}"),
                _ => format!("{s}{padding}"),
            }))
        }
        (
            Function::Trim
            | Function::Ltrim
            | Function::Rtrim
            | Function::Replace
            | Function::Reverse
            | Function::Left
            | Function::Right
            | Function::Repeat
            | Function::Lpad
            | Function::Rpad,
            [first, ..],
        ) if !matches!(first, Text(_)) => wrong(first, "TEXT"),
        (Function::Instr, [a, b]) => wrong(if matches!(a, Text(_)) { b } else { a }, "two TEXTs or two BLOBs"),
        (Function::Left | Function::Right | Function::Repeat | Function::Lpad | Function::Rpad, [_, n, ..]) => {
            wrong(n, "INT")
        }
        (Function::Replace, [_, a, b]) => wrong(if matches!(a, Text(_)) { b } else { a }, "TEXT"),
        (Function::Ceil | Function::Floor, [value]) => match value {
            Int(n) => Ok(Int(*n)),
            Real(r) => Ok(Real(if function == Function::Ceil { r.ceil() } else { r.floor() })),
            Value::Decimal(d) => Ok(Value::Decimal(d.floor(function == Function::Ceil)?)),
            other => wrong(other, "a number"),
        },
        (Function::Trunc, [value, rest @ ..]) => {
            let places = match rest {
                [] => 0,
                [Int(places)] => *places,
                [other] => return wrong(other, "INT"),
                _ => unreachable!("at most two arguments"),
            };
            match value {
                Int(n) => Ok(Int(*n)),
                Real(r) => {
                    let factor = 10f64.powi(places.clamp(-30, 30) as i32);
                    real((r * factor).trunc() / factor)
                }
                Value::Decimal(d) => Ok(Value::Decimal(d.trunc(places))),
                other => wrong(other, "a number"),
            }
        }
        (Function::Sign, [value]) => match value {
            Int(n) => Ok(Int(n.signum())),
            Real(r) => Ok(Real(if *r == 0.0 { 0.0 } else { r.signum() })),
            Value::Decimal(d) => Ok(Int(d.signum())),
            other => wrong(other, "a number"),
        },
        (Function::Sqrt, [value]) => match real_of(value)? {
            r if r < 0.0 => err("cannot take the square root of a negative number"),
            r => real(r.sqrt()),
        },
        (Function::Exp, [value]) => real(real_of(value)?.exp()),
        (Function::Ln | Function::Log10, [value]) | (Function::Log, [value]) => {
            match real_of(value)? {
                r if r <= 0.0 => err("cannot take the logarithm of zero or a negative number"),
                r if function == Function::Ln => real(r.ln()),
                r => real(r.log10()),
            }
        }
        (Function::Log, [base, value]) => match (real_of(base)?, real_of(value)?) {
            (b, r) if b <= 0.0 || r <= 0.0 => {
                err("cannot take the logarithm of zero or a negative number")
            }
            (1.0, _) => err("division by zero"),
            (b, r) => real(r.ln() / b.ln()),
        },
        (Function::Power, [base, exponent]) => match (real_of(base)?, real_of(exponent)?) {
            (0.0, e) if e < 0.0 => err("zero raised to a negative power is undefined"),
            (b, e) if b < 0.0 && e.fract() != 0.0 => {
                err("a negative number raised to a power that is not whole is not a REAL")
            }
            (b, e) => real(b.powf(e)),
        },
        (Function::Mod, [a, b]) => binary(a.clone(), BinaryOp::Mod, b.clone()),
        (Function::DateTrunc, [Text(unit), value]) => match value {
            Value::Timestamp(t) => Ok(Value::Timestamp(datetime::truncate(*t, unit)?)),
            Value::TimestampTz(t) => Ok(Value::TimestampTz(datetime::truncate(*t, unit)?)),
            Value::Date(d) => Ok(Value::Timestamp(datetime::truncate(
                *d as i64 * datetime::MICROS_PER_DAY,
                unit,
            )?)),
            other => wrong(other, "a DATE or TIMESTAMP"),
        },
        (Function::DateTrunc, [unit, _]) => wrong(unit, "a TEXT unit"),
        (Function::AtTimeZone, [value, zone]) => {
            let offset = match zone {
                Text(zone) => datetime::parse_zone(zone)?,
                Value::Interval(i) if i.months == 0 && i.days == 0 => i.micros,
                other => return wrong(other, "a TEXT or an INTERVAL of hours and minutes"),
            };
            match value {
                Value::TimestampTz(t) => Ok(Value::Timestamp(datetime::timestamp(t + offset)?)),
                Value::Timestamp(t) => Ok(Value::TimestampTz(datetime::timestamp(t - offset)?)),
                other => err(format!(
                    "AT TIME ZONE expects a TIMESTAMP, with or without time zone, got {}",
                    other.to_sql()
                )),
            }
        }
        _ => unreachable!("the parser checks how many arguments a function has"),
    })())
}

/// The expression with every function of the current time replaced by its
/// value at `now` (see [`at_time`]): for a column's default, which a
/// statement takes at its own time.
pub fn expr_at_time(expr: &Expr, now: i64) -> Result<Expr> {
    expr.replace(&mut |e| match e {
        Expr::Function(function, _) if function.reads_clock() => {
            let (day, time) = datetime::split_timestamp(now);
            Some(Ok(Expr::Literal(match function {
                Function::Now => Value::TimestampTz(now),
                Function::CurrentDate => Value::Date(day),
                Function::CurrentTime => Value::Time(time),
                _ => Value::Timestamp(now),
            })))
        }
        _ => None,
    })
}

/// The statement with every function of the current time (`NOW()`,
/// `CURRENT_DATE` and the like) replaced by its value at `now`, so that a
/// statement sees one time throughout, as SQLite has it (PostgreSQL's is
/// the transaction's).
pub fn at_time(statement: Statement, now: i64) -> Result<Statement> {
    let fix = |e: &Expr| expr_at_time(e, now);
    let value = |e: &Option<Expr>| e.as_ref().map(fix).transpose();
    let returning = |p: Option<Projection>| -> Result<Option<Projection>> {
        Ok(match p {
            Some(Projection::Exprs(items)) => Some(Projection::Exprs(
                items
                    .into_iter()
                    .map(|item| {
                        Ok(SelectItem {
                            expr: fix(&item.expr)?,
                            alias: item.alias,
                        })
                    })
                    .collect::<Result<_>>()?,
            )),
            other => other,
        })
    };
    Ok(match statement {
        Statement::Select(query) => Statement::Select(Box::new(query.try_map(&mut |e| fix(e))?)),
        Statement::Explain { statement, analyze } => Statement::Explain {
            statement: Box::new(at_time(*statement, now)?),
            analyze,
        },
        Statement::Insert {
            table,
            columns,
            source,
            returning: r,
        } => Statement::Insert {
            table,
            columns,
            source: match source {
                InsertSource::Values(rows) => InsertSource::Values(
                    rows.iter()
                        .map(|row| row.iter().map(value).collect::<Result<_>>())
                        .collect::<Result<_>>()?,
                ),
                InsertSource::Select(query) => InsertSource::Select(Box::new(query.try_map(&mut |e| fix(e))?)),
                InsertSource::DefaultValues => InsertSource::DefaultValues,
            },
            returning: returning(r)?,
        },
        Statement::Update {
            table,
            assignments,
            filter,
            returning: r,
        } => Statement::Update {
            table,
            assignments: assignments
                .into_iter()
                .map(|(c, e)| Ok((c, value(&e)?)))
                .collect::<Result<_>>()?,
            filter: filter.as_ref().map(fix).transpose()?,
            returning: returning(r)?,
        },
        Statement::Delete {
            table,
            filter,
            returning: r,
        } => Statement::Delete {
            table,
            filter: filter.as_ref().map(fix).transpose()?,
            returning: returning(r)?,
        },
        other => other,
    })
}

/// `CAST(value AS data_type)`. A value of the type stays as it is (a
/// DECIMAL rounded to the type's places); otherwise:
///
/// ```text
/// to INT        a REAL or DECIMAL rounded, halves away from zero; a BOOL as
///               0 or 1; a TEXT's digits
/// to REAL       an INT or DECIMAL, the REAL nearest it; a TEXT's number
/// to DECIMAL    an INT, a REAL (by its shortest digits) or a TEXT's digits,
///               rounded to the type's places
/// to TEXT       how a result shows it; a BOOL as 'true' or 'false'; a
///               BLOB's bytes, if they are UTF-8
/// to BOOL       an INT, true unless 0; a TEXT 'true', 't', 'yes', 'y',
///               'on', '1' or their opposites, in any case
/// to DATE       a TEXT 'YYYY-MM-DD'; a TIMESTAMP's date
/// to TIME       a TEXT 'HH:MM:SS'; a TIMESTAMP's time
/// to TIMESTAMP  a TEXT 'YYYY-MM-DD HH:MM:SS'; a DATE at midnight
/// to BLOB       a TEXT's UTF-8 bytes
/// ```
///
/// Anything else, or a TEXT that is not one, is an error.
pub(crate) fn cast(value: &Value, to: DataType) -> Result<Value> {
    use Value::{Blob, Bool, Date, Int, Null, Real, Text, Time, Timestamp};
    let fail = || err(format!("cannot cast {} to {to}", value.to_sql()));
    let invalid = |text: &str| err(format!("invalid {to}: '{text}'"));
    Ok(match (value, to) {
        (Null, _) => Null,
        (value, to) if value.fits(to) => value.clone(),
        (Value::Decimal(_) | Int(_) | Real(_), DataType::Decimal(_)) => value.assign(to)?,
        (Text(s), DataType::Decimal(_)) => match Decimal::parse(s) {
            Ok(d) => Value::Decimal(d).assign(to)?,
            Err(_) => return invalid(s),
        },
        (Real(r), DataType::Int) => {
            let rounded = r.round();
            match rounded >= -9_223_372_036_854_775_808.0 && rounded < 9_223_372_036_854_775_808.0 {
                true => Int(rounded as i64),
                false => return Err(overflow()),
            }
        }
        (Value::Decimal(d), DataType::Int) => Int(d.round_to_int()?),
        (Bool(b), DataType::Int) => Int(*b as i64),
        (Text(s), DataType::Int) => match s.trim().parse() {
            Ok(n) => Int(n),
            Err(_) => return invalid(s),
        },
        (Int(n), DataType::Real) => Real(*n as f64),
        (Value::Decimal(d), DataType::Real) => Real(d.to_real()),
        (Text(s), DataType::Real) => match s.trim().parse::<f64>() {
            Ok(r) if r.is_finite() => Real(r),
            _ => return invalid(s),
        },
        (Bool(b), DataType::Text) => Text(if *b { "true" } else { "false" }.into()),
        (Blob(b), DataType::Text) => match String::from_utf8(b.clone()) {
            Ok(s) => Text(s),
            Err(_) => return err(format!("cannot cast {} to TEXT: not UTF-8", value.to_sql())),
        },
        (value, DataType::Text) => Text(value.to_string()),
        (Int(n), DataType::Bool) => Bool(*n != 0),
        (Text(s), DataType::Bool) => match s.trim().to_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => Bool(true),
            "false" | "f" | "no" | "n" | "off" | "0" => Bool(false),
            _ => return invalid(s),
        },
        (Text(s), DataType::Date) => Date(datetime::parse_date(s)?),
        (Timestamp(t), DataType::Date) => Date(datetime::split_timestamp(*t).0),
        (Text(s), DataType::Time) => Time(datetime::parse_time(s)?),
        (Timestamp(t), DataType::Time) => Time(datetime::split_timestamp(*t).1),
        (Text(s), DataType::Timestamp) => Timestamp(datetime::parse_timestamp(s)?),
        (Date(d), DataType::Timestamp) => Timestamp(*d as i64 * datetime::MICROS_PER_DAY),
        // The session's time zone is UTC.
        (Value::TimestampTz(t), DataType::Timestamp) => Timestamp(*t),
        (Text(s), DataType::TimestampTz) => Value::TimestampTz(datetime::parse_timestamptz(s)?),
        (Timestamp(t), DataType::TimestampTz) => Value::TimestampTz(*t),
        (Date(d), DataType::TimestampTz) => {
            Value::TimestampTz(*d as i64 * datetime::MICROS_PER_DAY)
        }
        (Value::TimestampTz(t), DataType::Date) => Date(datetime::split_timestamp(*t).0),
        (Value::TimestampTz(t), DataType::Time) => Time(datetime::split_timestamp(*t).1),
        (Text(s), DataType::Interval) => Value::Interval(datetime::Interval::parse(s)?),
        (Time(t), DataType::Interval) => Value::Interval(datetime::Interval {
            micros: *t,
            ..Default::default()
        }),
        (Value::Interval(i), DataType::Time) => Time(datetime::add_to_time(0, *i)),
        (Text(s), DataType::Blob) => Blob(s.as_bytes().to_vec()),
        _ => return fail(),
    })
}

/// `EXTRACT(field FROM value)`: a part of a DATE, a TIME, a TIMESTAMP (with
/// time zone too, in UTC) or an INTERVAL, as an INT; EPOCH, as a REAL.
fn extract(field: datetime::Field, value: &Value) -> Result<Value> {
    use datetime::{Field, MICROS_PER_SECOND};
    let seconds = |micros: i64| micros as f64 / MICROS_PER_SECOND as f64;
    if field == Field::Epoch {
        return Ok(Value::Real(match value {
            Value::Date(d) => *d as f64 * 86_400.0,
            Value::Time(t) | Value::Timestamp(t) | Value::TimestampTz(t) => seconds(*t),
            // PostgreSQL's year of 365.25 days, and month of 30.
            Value::Interval(i) => {
                let (years, months) = (i.months / 12, i.months % 12);
                years as f64 * 365.25 * 86_400.0
                    + (months as f64 * 30.0 + i.days as f64) * 86_400.0
                    + seconds(i.micros)
            }
            _ => return err(format!("cannot extract EPOCH from {}", value.to_sql())),
        }));
    }
    let part = match value {
        Value::Date(d) if field.of_date() => datetime::calendar_field(*d, field),
        Value::Time(t) if !field.of_date() => datetime::time_field(*t, field),
        Value::Timestamp(t) | Value::TimestampTz(t) => {
            let (day, time) = datetime::split_timestamp(*t);
            match field.of_date() {
                true => datetime::calendar_field(day, field),
                false => datetime::time_field(time, field),
            }
        }
        Value::Interval(i) if !matches!(field, Field::Dow | Field::Doy) => {
            let time = i.micros;
            match field {
                Field::Year => (i.months / 12) as i64,
                Field::Quarter => (i.months % 12 / 3 + 1) as i64,
                Field::Month => (i.months % 12) as i64,
                Field::Day => i.days as i64,
                Field::Hour => i.micros / (3600 * MICROS_PER_SECOND),
                Field::Minute => time / (60 * MICROS_PER_SECOND) % 60,
                _ => time / MICROS_PER_SECOND % 60,
            }
        }
        _ => {
            return err(format!(
                "cannot extract {} from {}",
                field.name(),
                value.to_sql()
            ));
        }
    };
    Ok(Value::Int(part))
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
    if from >= end {
        String::new()
    } else {
        chars[from as usize..end as usize].iter().collect()
    }
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
    let rounded: f64 = format!(
        "{}.{}",
        String::from_utf8_lossy(whole),
        String::from_utf8_lossy(fraction)
    )
    .parse()
    .unwrap();
    rounded.copysign(x)
}

/// Whether `text` matches a LIKE `pattern`: `%` matches any characters, `_`
/// any one, and ASCII letters match in either case, as in SQLite.
pub(crate) fn like(text: &str, pattern: &str) -> bool {
    let (text, pattern): (Vec<char>, Vec<char>) =
        (text.chars().collect(), pattern.chars().collect());
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
        (UnaryOp::Neg, Value::Decimal(d)) => Ok(Value::Decimal(d.neg())),
        (UnaryOp::Neg, Value::Interval(i)) => Ok(Value::Interval(i.checked_neg()?)),
        (op, value) => err(format!("cannot apply {op:?} to {}", value.to_sql())),
    }
}

pub(crate) fn binary(left: Value, op: BinaryOp, right: Value) -> Result<Value> {
    use BinaryOp::*;
    use Value::{Bool, Int, Null, Text};

    let type_error = |l: &Value, r: &Value| {
        err(format!(
            "cannot apply {op:?} to {} and {}",
            l.to_sql(),
            r.to_sql()
        ))
    };

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
            let Some(ord) = left.compare(&right) else {
                return type_error(&left, &right);
            };
            Ok(Bool(match op {
                Eq => ord.is_eq(),
                NotEq => ord.is_ne(),
                Lt => ord.is_lt(),
                LtEq => ord.is_le(),
                Gt => ord.is_gt(),
                _ => ord.is_ge(),
            }))
        }
        Mod => match (&left, &right) {
            (Null, _) | (_, Null) => Ok(Null),
            (Int(_), Int(0)) => err("division by zero"),
            // The remainder has the sign of the dividend, as in SQLite and PostgreSQL.
            (Int(a), Int(b)) => Ok(Int(a.checked_rem(*b).unwrap_or(0))),
            (Value::Decimal(_), Int(_) | Value::Decimal(_)) | (Int(_), Value::Decimal(_)) => {
                let as_decimal = |v: &Value| match v {
                    Value::Decimal(d) => *d,
                    Int(n) => Decimal::from_int(*n),
                    _ => unreachable!("matched above"),
                };
                Ok(Value::Decimal(as_decimal(&left).remainder(as_decimal(&right))?))
            }
            _ => err(format!(
                "% takes INTs or DECIMALs, got {} and {}",
                left.to_sql(),
                right.to_sql()
            )),
        },
        Add | Sub | Mul | Div if let Some(result) = time_arithmetic(&left, op, &right) => result,
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
            // With a DECIMAL and no REAL, exactly.
            (Value::Decimal(_), Int(_) | Value::Decimal(_)) | (Int(_), Value::Decimal(_)) => {
                let as_decimal = |v: &Value| match v {
                    Value::Decimal(d) => *d,
                    Int(n) => Decimal::from_int(*n),
                    _ => unreachable!("matched above"),
                };
                let (a, b) = (as_decimal(&left), as_decimal(&right));
                Ok(Value::Decimal(match op {
                    Add => a.add(b)?,
                    Sub => a.sub(b)?,
                    Mul => a.mul(b)?,
                    _ => a.div(b)?,
                }))
            }
            // Days added to or taken from a date, or between two.
            (Value::Date(d), Int(n)) | (Int(n), Value::Date(d))
                if op == Add || (op == Sub && matches!(left, Value::Date(_))) =>
            {
                let days = match op {
                    Add => *d as i64 + n,
                    _ => *d as i64 - n,
                };
                Ok(Value::Date(datetime::date(days)?))
            }
            (Value::Date(a), Value::Date(b)) if op == Sub => Ok(Int(*a as i64 - *b as i64)),
            // A date at a time of day.
            (Value::Date(d), Value::Time(t)) | (Value::Time(t), Value::Date(d)) if op == Add => {
                let micros = *d as i64 * datetime::MICROS_PER_DAY + t;
                Ok(Value::Timestamp(datetime::timestamp(micros)?))
            }
            // With a REAL, both as REALs.
            (a, b) => match (a.to_f64(), b.to_f64()) {
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
            (Value::Blob(a), Value::Blob(b)) => Ok(Value::Blob([a.as_slice(), b].concat())),
            _ => type_error(&left, &right),
        },
    }
}

/// `+`, `-`, `*` or `/` with an INTERVAL or a TIMESTAMP WITH TIME ZONE, or
/// of two timestamps or times, if that is what the operands are:
///
/// ```text
/// interval ± interval          an interval, part by part
/// interval * number            each part times it, its fractions passed down
/// interval / number            likewise, divided
/// timestamp ± interval         the timestamp moved: months on the calendar, then the rest
/// timestamptz ± interval       likewise, in UTC
/// date ± interval              the date's midnight moved, a timestamp
/// time ± interval              the time moved by the interval's time, around midnight
/// timestamp - timestamp        an interval of days and the rest (timestamptz alike)
/// time - time                  an interval of the time between
/// ```
fn time_arithmetic(left: &Value, op: BinaryOp, right: &Value) -> Option<Result<Value>> {
    use BinaryOp::{Add, Div, Mul, Sub};
    use Value::{Date, Interval, Time, Timestamp, TimestampTz};
    let negated = |i: &datetime::Interval| match op {
        Sub => i.checked_neg(),
        _ => Ok(*i),
    };
    let factor = |v: &Value| match v {
        Value::Decimal(d) => Some(d.to_real()),
        v => v.as_real(),
    };
    let result = (|| -> Result<Option<Value>> {
        Ok(Some(match (left, op, right) {
            (Value::Null, ..) | (_, _, Value::Null)
                if matches!(left, Interval(_) | TimestampTz(_))
                    || matches!(right, Interval(_) | TimestampTz(_)) =>
            {
                Value::Null
            }
            (Interval(a), Add | Sub, Interval(b)) => Interval(a.checked_add(negated(b)?)?),
            (Interval(i), Mul, n) | (n, Mul, Interval(i)) if factor(n).is_some() => {
                Interval(i.scale(factor(n).expect("checked"))?)
            }
            (Interval(i), Div, n) if factor(n).is_some() => match factor(n).expect("checked") {
                0.0 => return err("division by zero"),
                n => Interval(i.scale(1.0 / n)?),
            },
            (Timestamp(t), Add | Sub, Interval(i)) => {
                Timestamp(datetime::add_interval(*t, negated(i)?)?)
            }
            (Interval(i), Add, Timestamp(t)) => Timestamp(datetime::add_interval(*t, *i)?),
            (TimestampTz(t), Add | Sub, Interval(i)) => {
                TimestampTz(datetime::add_interval(*t, negated(i)?)?)
            }
            (Interval(i), Add, TimestampTz(t)) => TimestampTz(datetime::add_interval(*t, *i)?),
            (Date(d), Add | Sub, Interval(i)) => Timestamp(datetime::add_interval(
                *d as i64 * datetime::MICROS_PER_DAY,
                negated(i)?,
            )?),
            (Interval(i), Add, Date(d)) => Timestamp(datetime::add_interval(
                *d as i64 * datetime::MICROS_PER_DAY,
                *i,
            )?),
            (Time(t), Add | Sub, Interval(i)) => Time(datetime::add_to_time(*t, negated(i)?)),
            (Interval(i), Add, Time(t)) => Time(datetime::add_to_time(*t, *i)),
            (Timestamp(a), Sub, Timestamp(b)) | (TimestampTz(a), Sub, TimestampTz(b)) => {
                Interval(datetime::between(*a, *b))
            }
            (Time(a), Sub, Time(b)) => Interval(datetime::Interval {
                micros: a - b,
                ..Default::default()
            }),
            _ => return Ok(None),
        }))
    })();
    result.transpose()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::{Projection, SelectItem, Statement};
    use crate::sql::parse;

    /// The expression of `SELECT <sql> FROM t`.
    fn expr(sql: &str) -> Expr {
        let Statement::Select(select) = parse(&format!("SELECT {sql} FROM t"))
            .unwrap()
            .pop()
            .unwrap()
        else {
            panic!()
        };
        let Projection::Exprs(mut items) = select.projection else {
            panic!()
        };
        let SelectItem { expr, .. } = items.remove(0);
        expr
    }

    #[test]
    fn located_columns_evaluate_and_print_as_named_ones() {
        let fields = vec![
            Field {
                table: Some("t".into()),
                name: "a".into(),
            },
            Field {
                table: Some("t".into()),
                name: "b".into(),
            },
        ];
        let row = [Value::Int(6), Value::Int(-2)];
        for sql in [
            "a + b",
            "t.b * 2 > a",
            "CASE WHEN a > 0 THEN b END",
            "COALESCE(NULL, t.a)",
            "a IN (b, 6)",
            "-a",
        ] {
            let named = expr(sql);
            let located = locate(&named, &fields);
            assert_ne!(located, named, "{sql}");
            assert_eq!(located.to_string(), named.to_string(), "{sql}");
            assert_eq!(
                eval(&located, &fields, &row).unwrap(),
                eval(&named, &fields, &row).unwrap(),
                "{sql}"
            );
        }
        assert!(matches!(locate(&expr("t.b"), &fields), Expr::Slot(1, _)));
        // A subquery's columns are its own tables', found when it runs.
        let sub = expr("(SELECT MAX(a) FROM u WHERE EXISTS (SELECT b FROM v))");
        assert_eq!(locate(&sub, &fields), sub);
        // Of `x IN (SELECT ...)`, x is located, not the subquery.
        let (Expr::InSubquery(left, query, _), Expr::InSubquery(_, original, _)) = (
            locate(&expr("b IN (SELECT a FROM u)"), &fields),
            expr("b IN (SELECT a FROM u)"),
        ) else {
            panic!()
        };
        assert!(matches!(*left, Expr::Slot(1, _)));
        assert_eq!(query, original);
        // A column not found once is left to fail when evaluated, as before.
        let twice = vec![Field::named("a"), Field::named("a")];
        assert_eq!(locate(&expr("a + 1"), &twice), expr("a + 1"));
        assert_eq!(locate(&expr("c"), &fields), expr("c"));
        assert!(eval(&locate(&expr("c"), &fields), &fields, &row).is_err());
    }

    #[test]
    fn and_or_skip_their_right_side_where_the_left_decides() {
        use Value::{Bool, Int, Null};
        let fields = vec![Field::named("x")];
        let at = |sql: &str, x: Value| eval(&expr(sql), &fields, &[x]);
        assert_eq!(at("x <> 0 AND 10 / x > 1", Int(0)).unwrap(), Bool(false));
        assert_eq!(at("x <> 0 AND 10 / x > 1", Int(5)).unwrap(), Bool(true));
        assert_eq!(at("x = 0 OR 10 / x > 1", Int(0)).unwrap(), Bool(true));
        assert_eq!(at("x = 0 OR 10 / x > 1", Int(20)).unwrap(), Bool(false));
        // NULL decides neither, nor does TRUE for AND or FALSE for OR: the right side runs.
        assert!(at("NULLIF(x, x) = 1 AND 1 / 0 = 1", Int(1)).is_err());
        assert!(at("x = 1 AND 1 / 0 = 1", Int(1)).is_err());
        assert!(at("x = 0 OR 1 / 0 = 1", Int(1)).is_err());
        assert_eq!(
            at("NULLIF(x, x) = 1 AND x = 2", Int(1)).unwrap(),
            Bool(false)
        );
        assert_eq!(at("NULLIF(x, x) = 1 OR x = 1", Int(1)).unwrap(), Bool(true));
        assert_eq!(at("NULLIF(x, x) = 1 OR x = 2", Int(1)).unwrap(), Null);
        // A right side of the wrong type fails only where it is evaluated.
        assert_eq!(at("FALSE AND x", Int(1)).unwrap(), Bool(false));
        assert_eq!(at("TRUE OR x", Int(1)).unwrap(), Bool(true));
        assert!(at("TRUE AND x", Int(1)).is_err());
        // A left side that is no BOOL fails, whatever the right side.
        assert!(at("x AND FALSE", Int(1)).is_err());
        assert!(at("x OR TRUE", Int(1)).is_err());
    }
}
