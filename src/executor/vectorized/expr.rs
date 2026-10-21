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
pub fn eval(expr: &Expr, fields: &[Field], batch: &Batch, subqueries: &mut dyn Subqueries) -> Result<Rc<Vector>> {
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
                Vector::Const(value, _) => Vector::Const(Value::Bool(value.is_null() != *negated), n),
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
            Rc::new(if *negated { unary(UnaryOp::Not, &found)? } else { found })
        }
        // Every argument is evaluated, so a column at a time.
        Expr::Function(function, args) if *function != Function::Coalesce => {
            let args: Vec<Rc<Vector>> = args.iter().map(|arg| eval(arg, fields, batch, subqueries)).collect::<Result<_>>()?;
            let constant = args.iter().all(|arg| matches!(**arg, Vector::Const(..)));
            let rows = if constant { 1 } else { n };
            let values: Vec<Value> = (0..rows)
                .map(|i| eval::apply(*function, &args.iter().map(|arg| arg.get(i)).collect::<Vec<_>>()))
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
pub fn select(predicate: &Expr, fields: &[Field], batch: &Batch, subqueries: &mut dyn Subqueries) -> Result<Vec<usize>> {
    let n = batch.rows();
    let matches = eval(predicate, fields, batch, subqueries)?;
    Ok(match &*matches {
        Vector::Bool(matches, nulls) => match nulls {
            None => (0..n).filter(|&i| matches[i]).collect(),
            Some(nulls) => (0..n).filter(|&i| matches[i] && !nulls[i]).collect(),
        },
        Vector::Const(Value::Bool(true), _) => (0..n).collect(),
        Vector::Const(Value::Bool(false) | Value::Null, _) => Vec::new(),
        other => {
            let mut keep = Vec::new();
            for i in 0..n {
                match other.get(i) {
                    Value::Bool(true) => keep.push(i),
                    Value::Bool(false) | Value::Null => {}
                    value => return err(format!("WHERE clause must be boolean, got {}", value.to_sql())),
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
        (UnaryOp::Not, Vector::Bool(values, nulls)) => Vector::Bool(values.iter().map(|b| !b).collect(), nulls.clone()),
        (UnaryOp::Neg, Vector::Real(values, nulls)) => Vector::Real(values.iter().map(|r| -r).collect(), nulls.clone()),
        (UnaryOp::Neg, Vector::Int(values, nulls)) => {
            let (values, nulls) = map(n, &Typed::Many(values, nulls.as_deref()), |n| n.checked_neg().ok_or_else(eval::overflow))?;
            Vector::Int(values, nulls)
        }
        _ => Vector::from_values((0..n).map(|i| eval::unary(op, vector.get(i))).collect::<Result<_>>()?),
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
            let compared = if let (Some(a), Some(b)) = (ints(left), ints(right)) {
                map2(n, &a, &b, |a, b| Ok(test(a.cmp(b))))
            } else if let (Some(a), Some(b)) = (reals(left), reals(right)) {
                map2(n, &a, &b, |a, b| a.partial_cmp(b).map(test).ok_or_else(|| Error("cannot compare".into())))
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
                if r.is_finite() { Ok(r) } else { err("REAL overflow") }
            };
            let result = match (ints(left), reals(left), ints(right), reals(right)) {
                (_, Some(a), _, Some(b)) => map2(n, &a, &b, |&a, &b| arithmetic(a, b)),
                (Some(a), _, _, Some(b)) => map2(n, &a, &b, |&a, &b| arithmetic(a as f64, b)),
                (_, Some(a), Some(b), _) => map2(n, &a, &b, |&a, &b| arithmetic(a, b as f64)),
                _ => return None,
            };
            Some(result.map(|(values, nulls)| Vector::Real(values, nulls)))
        }
        Like | Concat => {
            let (a, b) = (texts(left)?, texts(right)?);
            Some(match op {
                Like => map2(n, &a, &b, |text, pattern| Ok(eval::like(text, pattern))).map(|(v, nulls)| Vector::Bool(v, nulls)),
                _ => map2(n, &a, &b, |a, b| Ok(format!("{a}{b}"))).map(|(v, nulls)| Vector::Text(v, nulls)),
            })
        }
    }
}

/// `AND` or `OR` with SQL's three-valued logic: NULL means unknown.
fn logic(op: BinaryOp, n: usize, a: &Typed<bool>, b: &Typed<bool>) -> Vector {
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
            Typed::Many(values, nulls) => (!nulls.is_some_and(|nulls| nulls[i])).then(|| &values[i]),
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
fn map<A, R: Default>(n: usize, a: &Typed<A>, f: impl Fn(&A) -> Result<R>) -> Result<(Vec<R>, Nulls)> {
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
            return Ok((a.iter().zip(b.iter()).map(|(a, b)| f(a, b)).collect::<Result<_>>()?, None));
        }
        (Typed::Many(a, None), Typed::One(Some(b))) => return Ok((a.iter().map(|a| f(a, b)).collect::<Result<_>>()?, None)),
        (Typed::One(Some(a)), Typed::Many(b, None)) => return Ok((b.iter().map(|b| f(a, b)).collect::<Result<_>>()?, None)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::eval::NoSubqueries;
    use crate::sql::parse;
    use crate::sql::ast::{Projection, SelectItem, Statement};

    /// The expression of `SELECT <sql> FROM t`.
    fn expr(sql: &str) -> Expr {
        let Statement::Select(select) = parse(&format!("SELECT {sql} FROM t")).unwrap().pop().unwrap() else { panic!() };
        let Projection::Exprs(mut items) = select.projection else { panic!() };
        let SelectItem { expr, .. } = items.remove(0);
        expr
    }

    /// Evaluates `sql` on every row, a column at a time and then a row at a
    /// time, and checks that they agree.
    fn both(sql: &str, rows: &[Vec<Value>]) -> Result<Vec<Value>> {
        let fields = vec![Field::named("a"), Field::named("b")];
        let batch = Batch::from_rows(rows.to_vec(), 2).keep(&(0..rows.len()).collect::<Vec<_>>());
        let expr = expr(sql);
        let by_row: Result<Vec<Value>> = rows.iter().map(|row| eval_with(&expr, &fields, row, &mut NoSubqueries)).collect();
        let by_column = eval(&expr, &fields, &batch, &mut NoSubqueries).map(|v| (0..rows.len()).map(|i| v.get(i)).collect());
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
            "a + b", "a - b * 2", "a / b", "a * 1.5", "-a", "a < b", "a = 7", "a <> b", "a IS NULL", "b IS NOT NULL",
            "a > 0 AND b > 0", "a > 0 OR b > 0", "NOT (a > 0)", "a IN (1, 7, NULL)", "a NOT IN (2, 3)",
            "ABS(a)", "COALESCE(a, b, 0)", "CASE WHEN a > b THEN 'big' ELSE 'small' END", "a + 0.5 > b",
            "NULLIF(a, 7)", "1 + 2", "a || NULL",
        ] {
            both(sql, &rows).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        assert_eq!(both("a / b", &rows).unwrap(), [Int(0), Null, Null, Int(-3)]);
        assert_eq!(both("a < b", &rows).unwrap(), [Bool(true), Null, Null, Bool(false)]);
        assert_eq!(both("a * 1.5", &rows).unwrap()[3], Real(10.5));

        let texts = vec![vec![Text("ab".into()), Text("a%".into())], vec![Text("b".into()), Null]];
        assert_eq!(both("a LIKE b", &texts).unwrap(), [Bool(true), Null]);
        assert_eq!(both("a || b", &texts).unwrap(), [Text("aba%".into()), Null]);
        assert_eq!(both("UPPER(a) < 'B'", &texts).unwrap(), [Bool(true), Bool(false)]);
        // INTs and REALs compare exactly, as values do.
        let numbers = vec![vec![Int(9_007_199_254_740_993), Real(9_007_199_254_740_992.0)]];
        assert_eq!(both("a > b", &numbers).unwrap(), [Bool(true)]);
    }

    #[test]
    fn and_or_evaluate_their_right_side_only_where_the_left_does_not_decide() {
        use Value::{Bool, Int, Null};
        let (t, f) = (Bool(true), Bool(false));
        let rows = vec![vec![Int(0), Int(1)], vec![Int(2), Int(0)], vec![Null, Int(5)], vec![Int(5), Int(3)]];
        // Each divides by zero on a row whose left side decides: a column at
        // a time, as a row at a time, it fails on none.
        for (sql, want) in [
            ("a <> 0 AND 10 / a > 1", [f.clone(), t.clone(), Null, t.clone()]),
            ("b = 0 OR a / b > 0", [f.clone(), t.clone(), Null, t.clone()]),
            ("a <> 0 AND (b = 0 OR 10 / a / b > 0)", [f.clone(), t.clone(), Null, f.clone()]),
            ("NOT (a = 0 OR 10 / a < 3)", [f.clone(), t.clone(), Null, f.clone()]),
            // Decided on every row: the right side runs on none.
            ("COALESCE(a, 0) >= 0 OR 1 / 0 = 1", [t.clone(), t.clone(), t.clone(), t.clone()]),
            ("COALESCE(a, 0) < -1 AND 1 / 0 = 1", [f.clone(), f.clone(), f.clone(), f.clone()]),
            // CASE on the right side, row by row, only on the rows left open.
            ("a <> 0 AND CASE WHEN 10 / a > 1 THEN TRUE END", [f.clone(), t.clone(), Null, t.clone()]),
        ] {
            assert_eq!(both(sql, &rows).unwrap_or_else(|e| panic!("{sql}: {e}")), want, "{sql}");
        }
        // Where the left side leaves a row open, NULL included, its error is an error.
        assert!(both("a <> 0 AND 10 / b > 1", &rows).is_err());
        assert!(both("a < -1 AND 1 / 0 = 1", &rows).is_err());
        assert!(both("a < -1 AND 1 / 0 = 1", &[vec![Int(-5), Int(0)]]).is_err());
        // A right side of the wrong type, only where it is evaluated.
        assert!(both("a = 0 AND b", &rows[..1]).unwrap_err().to_string().contains("cannot apply"));
        let no_nulls = [rows[0].clone(), rows[1].clone(), rows[3].clone()];
        assert_eq!(both("a = 7 AND b", &no_nulls).unwrap(), [f.clone(), f.clone(), f.clone()]);
        assert!(both("a = 7 AND b", &rows).is_err(), "NULL = 7 leaves the row open");
    }

    #[test]
    fn errors_are_errors_a_column_at_a_time_too() {
        let rows = vec![vec![Value::Int(1), Value::Int(0)], vec![Value::Int(i64::MAX), Value::Int(1)]];
        for sql in ["a / b", "a + a", "a + 'x'", "a AND b", "-(a - a - 9223372036854775807 - 1)"] {
            assert!(both(sql, &rows).is_err(), "{sql}");
        }
        // On rows whose values make no error, none.
        assert!(both("a / b", &rows[1..]).is_ok());
    }
}
