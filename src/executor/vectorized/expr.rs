//! Expressions evaluated a column at a time: an expression over a batch
//! gives a vector of one value per row of the batch.
//!
//! Operators on columns of one type, or a column and a literal, run as one
//! loop over plain arrays (`i64 + i64`, `TEXT < TEXT`, `BOOL AND BOOL`), with
//! no [`Value`] to make or match per row. Anything else is evaluated per
//! value with the functions of `sql::eval`, so it means exactly what it
//! does there: an INT compared with a REAL, values of mixed types, `CASE`
//! and `COALESCE` (which evaluate only some of their parts), and subqueries,
//! which run once per row. `AND` and `OR` evaluate their right side only on
//! the rows their left one does not decide, as `sql::eval` does row by row.
//!
//! The loops of the usual cases are written for the compiler to turn into
//! SIMD instructions, a few values at a time: a comparison, an `AND` or an
//! `OR` of two columns with no error to check per row, and arithmetic that
//! first works the whole column out, then checks once whether any value
//! overflowed (or, for REALs, is not finite). If one did, the column is
//! worked out again a value at a time, which fails at the row that does,
//! with the error it always has, or at none, if that row is NULL. Built for
//! a CPU with wider registers (`-C target-cpu`), they do more at a time.
//!
//! The results are those of `sql::eval` row by row, but errors may differ:
//! a column of an expression is evaluated for every row before the next
//! column, so the first error found is not always the first a row-by-row
//! evaluation would meet. The steps that evaluate expressions start the
//! batch over a row at a time when one fails (see `vectorized/mod.rs`).

use std::cmp::Ordering;
use std::rc::Rc;

use super::batch::{Batch, Nulls, Vector};
use crate::error::{Error, Result, err};
use crate::sql::ast::{BinaryOp, Expr, Function, UnaryOp};
use crate::sql::eval::{self, Field, Subqueries, eval_with, resolve};
use crate::types::Value;

/// Evaluates `expr` on every row of `batch`, whose columns are `fields`.
pub fn eval(
    expr: &Expr,
    fields: &[Field],
    batch: &Batch,
    subqueries: &mut dyn Subqueries,
) -> Result<Rc<Vector>> {
    let n = batch.rows();
    if n == 0 {
        // Evaluated on no row, it fails on none.
        return Ok(Rc::new(Vector::Const(Value::Null, 0)));
    }
    Ok(match expr {
        Expr::Literal(value) => Rc::new(Vector::Const(value.clone(), n)),
        Expr::Slot(i, _) => batch.column(*i),
        Expr::Column(column) => batch.column(resolve(fields, column)?),
        Expr::IsNull(inner, negated) => {
            let inner = eval(inner, fields, batch, subqueries)?;
            Rc::new(match &*inner {
                Vector::Const(value, _) => {
                    Vector::Const(Value::Bool(value.is_null() != *negated), n)
                }
                inner => Vector::Bool((0..n).map(|i| inner.is_null(i) != *negated).collect(), None),
            })
        }
        Expr::Unary(op, inner) => {
            let inner = eval(inner, fields, batch, subqueries)?;
            Rc::new(unary(*op, &inner)?)
        }
        Expr::Binary(left, op @ (BinaryOp::And | BinaryOp::Or), right) => {
            let left = eval(left, fields, batch, subqueries)?;
            logic_lazily(&left, *op, right, fields, batch, subqueries)?
        }
        Expr::Binary(left, op, right) => {
            let left = eval(left, fields, batch, subqueries)?;
            let right = eval(right, fields, batch, subqueries)?;
            Rc::new(binary(&left, *op, &right)?)
        }
        // `x IN (a, b)` means `FALSE OR x = a OR x = b`.
        Expr::InList(left, items, negated) => {
            let value = eval(left, fields, batch, subqueries)?;
            let mut found = Vector::Const(Value::Bool(false), n);
            for item in items {
                let item = eval(item, fields, batch, subqueries)?;
                let equal = binary(&value, BinaryOp::Eq, &item)?;
                found = binary(&found, BinaryOp::Or, &equal)?;
            }
            Rc::new(if *negated {
                unary(UnaryOp::Not, &found)?
            } else {
                found
            })
        }
        // Every argument is evaluated, so a column at a time.
        Expr::Function(function, args) if *function != Function::Coalesce => {
            let args: Vec<Rc<Vector>> = args
                .iter()
                .map(|arg| eval(arg, fields, batch, subqueries))
                .collect::<Result<_>>()?;
            let constant = args.iter().all(|arg| matches!(**arg, Vector::Const(..)));
            let rows = if constant { 1 } else { n };
            let values: Vec<Value> = (0..rows)
                .map(|i| {
                    eval::apply(
                        *function,
                        &args.iter().map(|arg| arg.get(i)).collect::<Vec<_>>(),
                    )
                })
                .collect::<Result<_>>()?;
            Rc::new(match constant {
                true => Vector::Const(values.into_iter().next().expect("one row"), n),
                false => Vector::from_values(values),
            })
        }
        // The rest a row at a time.
        _ => {
            let values = (0..n).map(|i| eval_with(expr, fields, &batch.row(i), subqueries));
            Rc::new(Vector::from_values(values.collect::<Result<_>>()?))
        }
    })
}

/// `left AND right` or `left OR right` on the rows of `batch`, `left`
/// evaluated already, `right` only on the rows `left` does not decide (FALSE
/// for AND, TRUE for OR), as `sql::eval` evaluates it row by row: so it
/// fails on no row that does not, and does no work for them. Those rows
/// are picked by narrowing the batch's selection, which copies no row.
fn logic_lazily(
    left: &Rc<Vector>,
    op: BinaryOp,
    right: &Expr,
    fields: &[Field],
    batch: &Batch,
    subqueries: &mut dyn Subqueries,
) -> Result<Rc<Vector>> {
    let n = batch.rows();
    let decides = op == BinaryOp::Or;
    let Some(lefts) = bools(left) else {
        // Not BOOLs, which is an error, unless all are NULL: as row by row.
        let right = eval(right, fields, batch, subqueries)?;
        return Ok(Rc::new(binary(left, op, &right)?));
    };
    let open: Vec<usize> = (0..n).filter(|&i| lefts.get(i) != Some(&decides)).collect();
    if open.is_empty() {
        return Ok(left.clone());
    }
    let right = match open.len() == n {
        true => eval(right, fields, batch, subqueries)?,
        false => {
            let part = eval(right, fields, &batch.clone().keep(&open), subqueries)?;
            Rc::new(spread(&part, &open, n, decides))
        }
    };
    Ok(Rc::new(binary(left, op, &right)?))
}

/// The values of `part`, one for each of the rows `at` of `n`, in a vector
/// of `n` with `fill` in the other rows.
fn spread(part: &Vector, at: &[usize], n: usize, fill: bool) -> Vector {
    if let Some(bools) = bools(part) {
        let mut values = vec![fill; n];
        let mut nulls: Nulls = None;
        for (j, &i) in at.iter().enumerate() {
            match bools.get(j) {
                Some(&value) => values[i] = value,
                None => nulls.get_or_insert_with(|| vec![false; n])[i] = true,
            }
        }
        return Vector::Bool(values, nulls);
    }
    let mut values = vec![Value::Bool(fill); n];
    for (j, &i) in at.iter().enumerate() {
        values[i] = part.get(j);
    }
    Vector::from_values(values)
}

/// The rows of `batch` a WHERE clause is true for: their numbers, in order.
pub fn select(
    predicate: &Expr,
    fields: &[Field],
    batch: &Batch,
    subqueries: &mut dyn Subqueries,
) -> Result<Vec<usize>> {
    let n = batch.rows();
    let matches = eval(predicate, fields, batch, subqueries)?;
    Ok(match &*matches {
        Vector::Bool(matches, nulls) => match nulls {
            None => positions(matches.iter().copied()),
            Some(nulls) => positions(matches.iter().zip(nulls).map(|(&m, &null)| m & !null)),
        },
        Vector::Const(Value::Bool(true), _) => (0..n).collect(),
        Vector::Const(Value::Bool(false) | Value::Null, _) => Vec::new(),
        other => {
            let mut keep = Vec::new();
            for i in 0..n {
                match other.get(i) {
                    Value::Bool(true) => keep.push(i),
                    Value::Bool(false) | Value::Null => {}
                    value => {
                        return err(format!(
                            "WHERE clause must be boolean, got {}",
                            value.to_sql()
                        ));
                    }
                }
            }
            keep
        }
    })
}

fn unary(op: UnaryOp, vector: &Vector) -> Result<Vector> {
    let n = vector.len();
    Ok(match (op, vector) {
        (_, Vector::Const(value, _)) => Vector::Const(eval::unary(op, value.clone())?, n),
        (UnaryOp::Not, Vector::Bool(values, nulls)) => {
            Vector::Bool(values.iter().map(|b| !b).collect(), nulls.clone())
        }
        (UnaryOp::Neg, Vector::Real(values, nulls)) => {
            Vector::Real(values.iter().map(|r| -r).collect(), nulls.clone())
        }
        (UnaryOp::Neg, Vector::Int(values, nulls)) => {
            let (values, nulls) = map(n, &Typed::Many(values, nulls.as_deref()), |n| {
                n.checked_neg().ok_or_else(eval::overflow)
            })?;
            Vector::Int(values, nulls)
        }
        _ => Vector::from_values(
            (0..n)
                .map(|i| eval::unary(op, vector.get(i)))
                .collect::<Result<_>>()?,
        ),
    })
}

/// Applies `op` to the values of two vectors of the same length, row by row.
pub fn binary(left: &Vector, op: BinaryOp, right: &Vector) -> Result<Vector> {
    let n = left.len();
    if let (Vector::Const(a, _), Vector::Const(b, _)) = (left, right) {
        return Ok(Vector::Const(eval::binary(a.clone(), op, b.clone())?, n));
    }
    if let Some(result) = typed(left, op, right) {
        return result;
    }
    let values = (0..n).map(|i| eval::binary(left.get(i), op, right.get(i)));
    Ok(Vector::from_values(values.collect::<Result<_>>()?))
}

/// `op` on two vectors whose types it has a loop for, or None.
fn typed(left: &Vector, op: BinaryOp, right: &Vector) -> Option<Result<Vector>> {
    use BinaryOp::*;

    let n = left.len();
    match op {
        And | Or => Some(Ok(logic(op, n, &bools(left)?, &bools(right)?))),
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            let test = move |ord: Ordering| match op {
                Eq => ord.is_eq(),
                NotEq => ord.is_ne(),
                Lt => ord.is_lt(),
                LtEq => ord.is_le(),
                Gt => ord.is_gt(),
                _ => ord.is_ge(),
            };
            if let (Some(a), Some(b)) = (ints(left), ints(right)) {
                return Some(Ok(compare(n, &a, &b, op)));
            }
            if let (Some(a), Some(b)) = (reals(left), reals(right))
                && !has_nan(&a)
                && !has_nan(&b)
            {
                return Some(Ok(compare(n, &a, &b, op)));
            }
            let compared = if let (Some(a), Some(b)) = (reals(left), reals(right)) {
                map2(n, &a, &b, |a, b| {
                    a.partial_cmp(b)
                        .map(test)
                        .ok_or_else(|| Error("cannot compare".into()))
                })
            } else if let (Some(a), Some(b)) = (texts(left), texts(right)) {
                map2(n, &a, &b, |a, b| Ok(test(a.cmp(b))))
            } else if let (Some(a), Some(b)) = (bools(left), bools(right)) {
                map2(n, &a, &b, |a, b| Ok(test(a.cmp(b))))
            } else {
                return None;
            };
            Some(compared.map(|(values, nulls)| Vector::Bool(values, nulls)))
        }
        Add | Sub | Mul | Div => {
            if let (Some(a), Some(b)) = (ints(left), ints(right)) {
                if let Some(result) = int_arithmetic(n, &a, op, &b) {
                    return Some(Ok(result));
                }
                let result = map2(n, &a, &b, |&a, &b| {
                    if op == Div && b == 0 {
                        return err("division by zero");
                    }
                    let result = match op {
                        Add => a.checked_add(b),
                        Sub => a.checked_sub(b),
                        Mul => a.checked_mul(b),
                        _ => a.checked_div(b),
                    };
                    result.ok_or_else(eval::overflow)
                });
                return Some(result.map(|(values, nulls)| Vector::Int(values, nulls)));
            }
            // With a REAL, both as REALs.
            // An INT column's values as REALs, for that.
            let (a_ints, b_ints): (Vec<f64>, Vec<f64>);
            let a = match (reals(left), ints(left)) {
                (Some(a), _) => Some(a),
                (None, Some(Typed::Many(values, nulls))) => {
                    a_ints = values.iter().map(|&v| v as f64).collect();
                    Some(Typed::Many(&a_ints[..], nulls))
                }
                (None, Some(Typed::One(value))) => Some(Typed::One(value.map(|v| v as f64))),
                _ => None,
            };
            let b = match (reals(right), ints(right)) {
                (Some(b), _) => Some(b),
                (None, Some(Typed::Many(values, nulls))) => {
                    b_ints = values.iter().map(|&v| v as f64).collect();
                    Some(Typed::Many(&b_ints[..], nulls))
                }
                (None, Some(Typed::One(value))) => Some(Typed::One(value.map(|v| v as f64))),
                _ => None,
            };
            if let (Some(a), Some(b)) = (&a, &b)
                && let Some(result) = real_arithmetic(n, a, op, b)
            {
                return Some(Ok(result));
            }
            let arithmetic = move |a: f64, b: f64| {
                if op == Div && b == 0.0 {
                    return err("division by zero");
                }
                let r = match op {
                    Add => a + b,
                    Sub => a - b,
                    Mul => a * b,
                    _ => a / b,
                };
                if r.is_finite() {
                    Ok(r)
                } else {
                    err("REAL overflow")
                }
            };
            let result = match (ints(left), reals(left), ints(right), reals(right)) {
                (_, Some(a), _, Some(b)) => map2(n, &a, &b, |&a, &b| arithmetic(a, b)),
                (Some(a), _, _, Some(b)) => map2(n, &a, &b, |&a, &b| arithmetic(a as f64, b)),
                (_, Some(a), Some(b), _) => map2(n, &a, &b, |&a, &b| arithmetic(a, b as f64)),
                _ => return None,
            };
            Some(result.map(|(values, nulls)| Vector::Real(values, nulls)))
        }
        // A row at a time, as `sql::eval` computes it.
        Mod => None,
        Like | Concat => {
            let (a, b) = (texts(left)?, texts(right)?);
            Some(match op {
                Like => map2(n, &a, &b, |text, pattern| Ok(eval::like(text, pattern)))
                    .map(|(v, nulls)| Vector::Bool(v, nulls)),
                _ => map2(n, &a, &b, |a, b| Ok(format!("{a}{b}")))
                    .map(|(v, nulls)| Vector::Text(v, nulls)),
            })
        }
    }
}

/// `AND` or `OR` with SQL's three-valued logic: NULL means unknown.
fn logic(op: BinaryOp, n: usize, a: &Typed<bool>, b: &Typed<bool>) -> Vector {
    // Without a NULL, the plain operation on two arrays.
    if null_flags(a).is_none() && null_flags(b).is_none() && !null_literal(a) && !null_literal(b) {
        let (values, nulls) = match op {
            BinaryOp::And => zip_with(n, a, b, |x, y| x & y),
            _ => zip_with(n, a, b, |x, y| x | y),
        };
        return Vector::Bool(values, nulls);
    }
    let (mut values, mut nulls) = (Vec::with_capacity(n), None);
    for i in 0..n {
        let value = match (op, a.get(i), b.get(i)) {
            (BinaryOp::And, Some(false), _) | (BinaryOp::And, _, Some(false)) => Some(false),
            (BinaryOp::And, Some(true), Some(true)) => Some(true),
            (BinaryOp::Or, Some(true), _) | (BinaryOp::Or, _, Some(true)) => Some(true),
            (BinaryOp::Or, Some(false), Some(false)) => Some(false),
            _ => None,
        };
        values.push(value.unwrap_or(false));
        if value.is_none() {
            nulls.get_or_insert_with(|| vec![false; n])[i] = true;
        }
    }
    Vector::Bool(values, nulls)
}

/// A vector's values of one type: a column of them, or one for every row
/// (`None` for a NULL literal, which is of any type).
enum Typed<'a, T> {
    Many(&'a [T], Option<&'a [bool]>),
    One(Option<T>),
}

impl<T> Typed<'_, T> {
    fn get(&self, i: usize) -> Option<&T> {
        match self {
            Typed::Many(values, nulls) => {
                (!nulls.is_some_and(|nulls| nulls[i])).then(|| &values[i])
            }
            Typed::One(value) => value.as_ref(),
        }
    }
}

macro_rules! typed_as {
    ($name:ident, $variant:ident, $t:ty) => {
        fn $name(vector: &Vector) -> Option<Typed<'_, $t>> {
            match vector {
                Vector::$variant(values, nulls) => Some(Typed::Many(values, nulls.as_deref())),
                Vector::Const(Value::$variant(value), _) => Some(Typed::One(Some(value.clone()))),
                Vector::Const(Value::Null, _) => Some(Typed::One(None)),
                _ => None,
            }
        }
    };
}

typed_as!(ints, Int, i64);
typed_as!(reals, Real, f64);
typed_as!(bools, Bool, bool);
typed_as!(texts, Text, String);

/// `f` of each value that is not NULL; NULL where it is.
fn map<A, R: Default>(
    n: usize,
    a: &Typed<A>,
    f: impl Fn(&A) -> Result<R>,
) -> Result<(Vec<R>, Nulls)> {
    map2(n, a, &Typed::<()>::One(Some(())), |a, _| f(a))
}

/// `f` of each pair of values that are not NULL; NULL where either is.
fn map2<A, B, R: Default>(
    n: usize,
    a: &Typed<A>,
    b: &Typed<B>,
    f: impl Fn(&A, &B) -> Result<R>,
) -> Result<(Vec<R>, Nulls)> {
    // The usual cases, without NULLs, as plain loops.
    match (a, b) {
        (Typed::Many(a, None), Typed::Many(b, None)) => {
            return Ok((
                a.iter()
                    .zip(b.iter())
                    .map(|(a, b)| f(a, b))
                    .collect::<Result<_>>()?,
                None,
            ));
        }
        (Typed::Many(a, None), Typed::One(Some(b))) => {
            return Ok((a.iter().map(|a| f(a, b)).collect::<Result<_>>()?, None));
        }
        (Typed::One(Some(a)), Typed::Many(b, None)) => {
            return Ok((b.iter().map(|b| f(a, b)).collect::<Result<_>>()?, None));
        }
        _ => {}
    }
    let (mut values, mut nulls) = (Vec::with_capacity(n), None);
    for i in 0..n {
        match (a.get(i), b.get(i)) {
            (Some(a), Some(b)) => values.push(f(a, b)?),
            _ => {
                values.push(R::default());
                nulls.get_or_insert_with(|| vec![false; n])[i] = true;
            }
        }
    }
    Ok((values, nulls))
}

/// The positions of the `true`s, in order, found without a branch per
/// value: each position is written, and kept by counting it only if true.
fn positions(flags: impl ExactSizeIterator<Item = bool>) -> Vec<usize> {
    let mut kept = vec![0; flags.len()];
    let mut count = 0;
    for (i, flag) in flags.enumerate() {
        kept[count] = i;
        count += flag as usize;
    }
    kept.truncate(count);
    kept
}

/// The null flags of a column, if it has any.
fn null_flags<'a, T>(values: &Typed<'a, T>) -> Option<&'a [bool]> {
    match values {
        Typed::Many(_, nulls) => *nulls,
        Typed::One(_) => None,
    }
}

/// Whether it is a NULL literal, which makes every row NULL.
fn null_literal<T>(values: &Typed<T>) -> bool {
    matches!(values, Typed::One(None))
}

/// `f` of each pair of values, NULL where either is (with `R::default()`
/// there), as one loop over plain arrays with no error to check and no
/// early exit, which a compiler turns into SIMD instructions where `f`
/// allows. `f` is worked out on the NULL rows' placeholders too: it must
/// not fail or panic on any values.
fn zip_with<T: Copy, R: Copy + Default>(
    n: usize,
    a: &Typed<T>,
    b: &Typed<T>,
    f: impl Fn(T, T) -> R,
) -> (Vec<R>, Nulls) {
    if null_literal(a) || null_literal(b) {
        return (vec![R::default(); n], Some(vec![true; n]));
    }
    let mut values: Vec<R> = match (a, b) {
        (Typed::Many(a, _), Typed::Many(b, _)) => {
            a.iter().zip(b.iter()).map(|(&x, &y)| f(x, y)).collect()
        }
        (Typed::Many(a, _), Typed::One(Some(y))) => a.iter().map(|&x| f(x, *y)).collect(),
        (Typed::One(Some(x)), Typed::Many(b, _)) => b.iter().map(|&y| f(*x, y)).collect(),
        (Typed::One(Some(x)), Typed::One(Some(y))) => vec![f(*x, *y); n],
        _ => unreachable!("NULL literals returned above"),
    };
    let nulls: Nulls = match (null_flags(a), null_flags(b)) {
        (None, None) => None,
        (Some(nulls), None) | (None, Some(nulls)) => Some(nulls.to_vec()),
        (Some(a), Some(b)) => Some(a.iter().zip(b).map(|(&x, &y)| x | y).collect()),
    };
    if let Some(nulls) = &nulls {
        for (value, &null) in values.iter_mut().zip(nulls) {
            *value = if null { R::default() } else { *value };
        }
    }
    (values, nulls)
}

/// Whether `f` is true of any pair of values, NULL rows' placeholders
/// included, as one loop that folds with no early exit.
fn any_pair<T: Copy>(a: &Typed<T>, b: &Typed<T>, f: impl Fn(T, T) -> bool) -> bool {
    match (a, b) {
        (Typed::Many(a, _), Typed::Many(b, _)) => a
            .iter()
            .zip(b.iter())
            .fold(false, |any, (&x, &y)| any | f(x, y)),
        (Typed::Many(a, _), Typed::One(Some(y))) => a.iter().fold(false, |any, &x| any | f(x, *y)),
        (Typed::One(Some(x)), Typed::Many(b, _)) => b.iter().fold(false, |any, &y| any | f(*x, y)),
        (Typed::One(Some(x)), Typed::One(Some(y))) => f(*x, *y),
        _ => false, // a NULL literal: every row NULL
    }
}

/// A comparison of two columns of INTs, or of REALs with no NaN, which
/// cannot fail.
fn compare<T: Copy + PartialOrd>(n: usize, a: &Typed<T>, b: &Typed<T>, op: BinaryOp) -> Vector {
    let (values, nulls) = match op {
        BinaryOp::Eq => zip_with(n, a, b, |x, y| x == y),
        BinaryOp::NotEq => zip_with(n, a, b, |x, y| x != y),
        BinaryOp::Lt => zip_with(n, a, b, |x, y| x < y),
        BinaryOp::LtEq => zip_with(n, a, b, |x, y| x <= y),
        BinaryOp::Gt => zip_with(n, a, b, |x, y| x > y),
        _ => zip_with(n, a, b, |x, y| x >= y),
    };
    Vector::Bool(values, nulls)
}

/// Whether a column of REALs has a NaN, which compares with nothing.
fn has_nan(values: &Typed<f64>) -> bool {
    match values {
        Typed::Many(values, _) => values.iter().fold(false, |any, v| any | v.is_nan()),
        Typed::One(value) => value.is_some_and(f64::is_nan),
    }
}

/// `+`, `-` or `*` of two columns of INTs, if no value overflows, NULL
/// rows' placeholders included; None to work it out a row at a time.
fn int_arithmetic(n: usize, a: &Typed<i64>, op: BinaryOp, b: &Typed<i64>) -> Option<Vector> {
    let (overflows, (values, nulls)) = match op {
        BinaryOp::Add => (
            any_pair(a, b, |x, y| x.overflowing_add(y).1),
            zip_with(n, a, b, i64::wrapping_add),
        ),
        BinaryOp::Sub => (
            any_pair(a, b, |x, y| x.overflowing_sub(y).1),
            zip_with(n, a, b, i64::wrapping_sub),
        ),
        BinaryOp::Mul => (
            any_pair(a, b, |x, y| x.overflowing_mul(y).1),
            zip_with(n, a, b, i64::wrapping_mul),
        ),
        _ => return None,
    };
    (!overflows).then_some(Vector::Int(values, nulls))
}

/// `+`, `-`, `*` or `/` of two columns of REALs, if every value is finite
/// (one divided by zero is not); None to work it out a row at a time.
fn real_arithmetic(n: usize, a: &Typed<f64>, op: BinaryOp, b: &Typed<f64>) -> Option<Vector> {
    let (values, nulls) = match op {
        BinaryOp::Add => zip_with(n, a, b, |x, y| x + y),
        BinaryOp::Sub => zip_with(n, a, b, |x, y| x - y),
        BinaryOp::Mul => zip_with(n, a, b, |x, y| x * y),
        _ => zip_with(n, a, b, |x, y| x / y),
    };
    // A NULL row's value is 0 by now, whatever its placeholders made.
    let finite = values.iter().fold(true, |all, v| all & v.is_finite());
    finite.then_some(Vector::Real(values, nulls))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::{Projection, SelectItem, Statement};
    use crate::sql::eval::NoSubqueries;
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

    /// Evaluates `sql` on every row, a column at a time and then a row at a
    /// time, and checks that they agree.
    fn both(sql: &str, rows: &[Vec<Value>]) -> Result<Vec<Value>> {
        let fields = vec![Field::named("a"), Field::named("b")];
        let batch = Batch::from_rows(rows.to_vec(), 2).keep(&(0..rows.len()).collect::<Vec<_>>());
        let expr = expr(sql);
        let by_row: Result<Vec<Value>> = rows
            .iter()
            .map(|row| eval_with(&expr, &fields, row, &mut NoSubqueries))
            .collect();
        let by_column = eval(&expr, &fields, &batch, &mut NoSubqueries)
            .map(|v| (0..rows.len()).map(|i| v.get(i)).collect());
        match (&by_row, &by_column) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "{sql}"),
            (Err(_), Err(_)) => {}
            _ => panic!("{sql}: by row {by_row:?}, by column {by_column:?}"),
        }
        by_column
    }

    #[test]
    fn columns_evaluate_as_rows_do() {
        use Value::{Bool, Int, Null, Real, Text};
        let rows = vec![
            vec![Int(1), Int(10)],
            vec![Int(-4), Null],
            vec![Null, Int(3)],
            vec![Int(7), Int(-2)],
        ];
        for sql in [
            "a + b",
            "a - b * 2",
            "a / b",
            "a * 1.5",
            "-a",
            "a < b",
            "a = 7",
            "a <> b",
            "a IS NULL",
            "b IS NOT NULL",
            "a > 0 AND b > 0",
            "a > 0 OR b > 0",
            "NOT (a > 0)",
            "a IN (1, 7, NULL)",
            "a NOT IN (2, 3)",
            "ABS(a)",
            "COALESCE(a, b, 0)",
            "CASE WHEN a > b THEN 'big' ELSE 'small' END",
            "a + 0.5 > b",
            "NULLIF(a, 7)",
            "1 + 2",
            "a || NULL",
        ] {
            both(sql, &rows).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        assert_eq!(both("a / b", &rows).unwrap(), [Int(0), Null, Null, Int(-3)]);
        assert_eq!(
            both("a < b", &rows).unwrap(),
            [Bool(true), Null, Null, Bool(false)]
        );
        assert_eq!(both("a * 1.5", &rows).unwrap()[3], Real(10.5));

        let texts = vec![
            vec![Text("ab".into()), Text("a%".into())],
            vec![Text("b".into()), Null],
        ];
        assert_eq!(both("a LIKE b", &texts).unwrap(), [Bool(true), Null]);
        assert_eq!(both("a || b", &texts).unwrap(), [Text("aba%".into()), Null]);
        assert_eq!(
            both("UPPER(a) < 'B'", &texts).unwrap(),
            [Bool(true), Bool(false)]
        );
        // INTs and REALs compare exactly, as values do.
        let numbers = vec![vec![
            Int(9_007_199_254_740_993),
            Real(9_007_199_254_740_992.0),
        ]];
        assert_eq!(both("a > b", &numbers).unwrap(), [Bool(true)]);
    }

    #[test]
    fn and_or_evaluate_their_right_side_only_where_the_left_does_not_decide() {
        use Value::{Bool, Int, Null};
        let (t, f) = (Bool(true), Bool(false));
        let rows = vec![
            vec![Int(0), Int(1)],
            vec![Int(2), Int(0)],
            vec![Null, Int(5)],
            vec![Int(5), Int(3)],
        ];
        // Each divides by zero on a row whose left side decides: a column at
        // a time, as a row at a time, it fails on none.
        for (sql, want) in [
            (
                "a <> 0 AND 10 / a > 1",
                [f.clone(), t.clone(), Null, t.clone()],
            ),
            (
                "b = 0 OR a / b > 0",
                [f.clone(), t.clone(), Null, t.clone()],
            ),
            (
                "a <> 0 AND (b = 0 OR 10 / a / b > 0)",
                [f.clone(), t.clone(), Null, f.clone()],
            ),
            (
                "NOT (a = 0 OR 10 / a < 3)",
                [f.clone(), t.clone(), Null, f.clone()],
            ),
            // Decided on every row: the right side runs on none.
            (
                "COALESCE(a, 0) >= 0 OR 1 / 0 = 1",
                [t.clone(), t.clone(), t.clone(), t.clone()],
            ),
            (
                "COALESCE(a, 0) < -1 AND 1 / 0 = 1",
                [f.clone(), f.clone(), f.clone(), f.clone()],
            ),
            // CASE on the right side, row by row, only on the rows left open.
            (
                "a <> 0 AND CASE WHEN 10 / a > 1 THEN TRUE END",
                [f.clone(), t.clone(), Null, t.clone()],
            ),
        ] {
            assert_eq!(
                both(sql, &rows).unwrap_or_else(|e| panic!("{sql}: {e}")),
                want,
                "{sql}"
            );
        }
        // Where the left side leaves a row open, NULL included, its error is an error.
        assert!(both("a <> 0 AND 10 / b > 1", &rows).is_err());
        assert!(both("a < -1 AND 1 / 0 = 1", &rows).is_err());
        assert!(both("a < -1 AND 1 / 0 = 1", &[vec![Int(-5), Int(0)]]).is_err());
        // A right side of the wrong type, only where it is evaluated.
        assert!(
            both("a = 0 AND b", &rows[..1])
                .unwrap_err()
                .to_string()
                .contains("cannot apply")
        );
        let no_nulls = [rows[0].clone(), rows[1].clone(), rows[3].clone()];
        assert_eq!(
            both("a = 7 AND b", &no_nulls).unwrap(),
            [f.clone(), f.clone(), f.clone()]
        );
        assert!(
            both("a = 7 AND b", &rows).is_err(),
            "NULL = 7 leaves the row open"
        );
    }

    #[test]
    fn errors_are_errors_a_column_at_a_time_too() {
        let rows = vec![
            vec![Value::Int(1), Value::Int(0)],
            vec![Value::Int(i64::MAX), Value::Int(1)],
        ];
        for sql in [
            "a / b",
            "a + a",
            "a + 'x'",
            "a AND b",
            "-(a - a - 9223372036854775807 - 1)",
        ] {
            assert!(both(sql, &rows).is_err(), "{sql}");
        }
        // On rows whose values make no error, none.
        assert!(both("a / b", &rows[1..]).is_ok());
    }

    /// The loops that work a column out first and check it after: a value
    /// that overflows, or is not finite, fails as it does row by row, and
    /// one on a NULL row (whose placeholder may overflow) fails not at all.
    #[test]
    fn whole_column_loops_fail_as_rows_do() {
        let (int, real, null) = (Value::Int, Value::Real, Value::Null);
        let ints = vec![
            vec![int(3), int(-7)],
            vec![int(i64::MAX), int(1)],
            vec![int(i64::MIN), null.clone()],
            vec![null.clone(), int(i64::MIN)],
            vec![int(0), int(0)],
        ];
        // Whether each fails on these rows: `both` checks it fails as it
        // does row by row, and returns what it returns.
        for (sql, fails) in [
            ("a + b", true), // MAX + 1
            ("a - b", false),
            ("b - a", false),
            ("a * b", false),
            ("a * 2", true),
            ("2 * a", true),
            ("a + NULL", false),
            ("-1 - b", false), // -1 - MIN is MAX
            ("0 - b", true),   // 0 - MIN
            ("a - 1", true),   // MIN - 1
        ] {
            assert_eq!(both(sql, &ints).is_err(), fails, "{sql}");
        }
        // NULL - MIN on the fourth row: its placeholder overflows, it does not.
        assert!(both("a - b", &ints[3..]).is_ok());
        assert_eq!(
            both("a - b", &ints[2..4]).unwrap(),
            vec![null.clone(), null.clone()]
        );

        let reals = vec![
            vec![real(1.5), real(2.0)],
            vec![real(-0.5), real(0.0)],
            vec![real(1e308), real(1e308)],
            vec![null.clone(), real(0.0)],
            vec![real(4.0), null.clone()],
        ];
        for (sql, rows) in [
            ("a / b", &reals[..2]),  // by zero
            ("a + b", &reals[2..3]), // overflow
            ("a * b", &reals[2..3]), // overflow
            ("a - b", &reals[..]),   // overflow on the third row only
        ] {
            let found = both(sql, rows);
            assert!(found.is_err() || sql == "a - b", "{sql}: {found:?}");
        }
        // NULL / 0.0 and 4.0 / NULL: no error, NULL.
        assert_eq!(
            both("a / b", &reals[3..]).unwrap(),
            vec![null.clone(), null.clone()]
        );
        assert_eq!(both("a / b + 1.0", &reals[..1]).unwrap(), vec![real(1.75)]);
        // INT with REAL, either way round, and a literal.
        let mixed = vec![vec![int(3), real(0.5)], vec![int(-2), null.clone()]];
        for sql in ["a + b", "b * a", "a / 2.0", "1.5 - a", "a - b * 2.0"] {
            both(sql, &mixed).unwrap();
        }

        // Comparisons and logic, with and without NULLs and literals.
        for sql in [
            "a < b",
            "a = b",
            "a <> b",
            "a >= 3",
            "2 > a",
            "a <= NULL",
            "a < b AND b < 3",
            "a < b OR a = 3",
            "NOT (a < b)",
        ] {
            both(sql, &ints).unwrap();
            both(sql, &ints[..2]).unwrap();
            both(sql, &reals).unwrap();
        }
        let bools = vec![
            vec![Value::Bool(true), Value::Bool(false)],
            vec![Value::Bool(true), Value::Bool(true)],
            vec![Value::Bool(false), Value::Bool(false)],
        ];
        for sql in ["a AND b", "a OR b", "a AND TRUE", "FALSE OR b"] {
            both(sql, &bools).unwrap();
        }
    }
}
