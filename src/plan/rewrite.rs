//! Expression rewriting: what the planner does to a statement's expressions
//! once their columns are bound, before it pushes conditions down and picks
//! indexes, so that both work on the simpler form and `EXPLAIN` shows it.
//!
//! | rewrite                 | example                                            |
//! |-------------------------|----------------------------------------------------|
//! | constant folding        | `price > 2 * 50` is `price > 100`; `WHERE 1 = 1` is gone |
//! | boolean identities      | `TRUE AND x` is `x`, `FALSE AND x` is `FALSE`, `x OR FALSE` is `x` |
//! | NOT pushed down         | `NOT (a = 1 OR b < 2)` is `a != 1 AND b >= 2`; `NOT x IS NULL` is `x IS NOT NULL` |
//! | equalities to IN        | `x = 1 OR x = 2 OR x IN (3)` is `x IN (1, 2, 3)`   |
//! | redundant conjuncts     | `x > 5 AND x > 3` is `x > 5`; `p AND p` is `p`     |
//! | a clause never true     | `WHERE x > 1 AND FALSE` returns no row, reading nothing |
//! | common subexpressions   | `SELECT a * b, a * b + 1` computes `a * b` once (see [`common`]) |
//!
//! Other rewrites live where their facts are: equalities carried across a
//! join (`a.x = b.x AND b.x = 5` gives `a.x = 5`) in `query.rs`, where the
//! columns' types are known; conditions on a column found from arithmetic
//! on it (`x + 1 > 5` gives `x > 4`) in `filter.rs`; and `EXISTS` turned
//! into an uncorrelated `IN` in the binder (`query.rs`).
//!
//! **Meaning kept.** Each rewrite gives the same value for every row, NULL
//! included, as SQL's three-valued logic and Sequoia's evaluation define
//! it: `AND` and `OR` evaluate their right side only where the left one
//! does not decide, and a value of the wrong type is an error. So:
//!
//! - An expression is folded only if evaluating it succeeds: `1 / 0` stays,
//!   to fail on the rows it is evaluated on, as before, and on no others.
//! - `TRUE AND x` becomes `x` only where `x` is known to be a BOOL (or
//!   NULL, or an error): a comparison, a logical operator, `IS NULL`, `IN`,
//!   `LIKE`, `EXISTS`. Were `x` an INT, `TRUE AND x` is an error, and `x`
//!   alone would not be. Likewise for De Morgan's laws and `NOT NOT x`.
//! - Only what a rewrite leaves out evaluating may change which error a
//!   failing row reports, or whether it reports one: the conjuncts of a
//!   WHERE are a set, as in any database that reorders them, so one that is
//!   always false (or another that implies it) decides a row before the
//!   rest is evaluated on it.

use crate::plan::access::implies;
use crate::plan::filter::{Condition, conjuncts};
use crate::predicate::Compare;
use crate::sql::ast::{BinaryOp, ColumnRef, Expr, Function, UnaryOp};
use crate::sql::eval::eval;
use crate::types::Value;

/// The expression rewritten as the module docs describe, but for the
/// conjuncts of a clause, which [`simplify_clause`] tidies.
pub fn simplify(expr: &Expr) -> Expr {
    match expr {
        // Subqueries are simplified when they are planned, as queries; a
        // window function is left as written, to be matched with the
        // WINDOW step that computes it.
        Expr::Literal(_)
        | Expr::Column(_)
        | Expr::Outer(_)
        | Expr::Slot(..)
        | Expr::Subquery(_)
        | Expr::Exists(_)
        | Expr::Window(_) => expr.clone(),
        Expr::InSubquery(left, query, negated) => {
            Expr::InSubquery(Box::new(simplify(left)), query.clone(), *negated)
        }
        Expr::Aggregate(function, arg, distinct) => Expr::Aggregate(
            *function,
            arg.as_deref().map(|arg| Box::new(simplify(arg))),
            *distinct,
        ),
        Expr::Unary(UnaryOp::Not, inner) => not(simplify(inner)),
        Expr::Unary(op, inner) => fold(Expr::Unary(*op, Box::new(simplify(inner)))),
        Expr::Binary(left, op, right) => binary(simplify(left), *op, simplify(right)),
        Expr::IsNull(inner, negated) => fold(Expr::IsNull(Box::new(simplify(inner)), *negated)),
        Expr::InList(left, items, negated) => fold(Expr::InList(
            Box::new(simplify(left)),
            items.iter().map(simplify).collect(),
            *negated,
        )),
        Expr::Case { .. } | Expr::Function(..) => fold(
            expr.try_map_parts(&mut |part| Ok(simplify(part)))
                .expect("simplifying cannot fail"),
        ),
    }
}

/// What a WHERE, ON or HAVING clause comes to, simplified.
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    /// True of every row: no condition.
    Always,
    /// True of no row: a conjunct is FALSE or NULL whatever the row.
    Never,
    /// The conjuncts left, joined by AND.
    When(Expr),
}

impl Clause {
    /// The condition, if a row must meet one; `FALSE` for [`Clause::Never`].
    pub fn condition(self) -> Option<Expr> {
        match self {
            Clause::Always => None,
            Clause::Never => Some(Expr::Literal(Value::Bool(false))),
            Clause::When(expr) => Some(expr),
        }
    }
}

/// A clause simplified, then split at its ANDs into conjuncts: those always
/// true are dropped, as are those another implies (`x > 3` beside `x > 5`)
/// and repeated ones, and one that is never true makes the clause
/// [`Clause::Never`].
pub fn simplify_clause(expr: &Expr) -> Clause {
    let simplified = simplify(expr);
    let mut kept: Vec<Expr> = Vec::new();
    for part in conjuncts(&simplified) {
        match part {
            Expr::Literal(Value::Bool(true)) => continue,
            Expr::Literal(Value::Bool(false) | Value::Null) => return Clause::Never,
            _ => {}
        }
        if is_boolean(part) && kept.contains(part) {
            continue;
        }
        if let Some(condition) = comparison(part) {
            let implied = |other: &Expr| comparison(other).is_some_and(|o| implies(&o, &condition));
            if kept.iter().any(implied) {
                continue;
            }
            kept.retain(|other| comparison(other).is_none_or(|o| !implies(&condition, &o)));
        }
        kept.push(part.clone());
    }
    match and(kept) {
        Some(expr) => Clause::When(expr),
        None => Clause::Always,
    }
}

/// The conditions joined by AND, if there are any.
pub fn and(parts: Vec<Expr>) -> Option<Expr> {
    parts
        .into_iter()
        .reduce(|a, b| Expr::Binary(Box::new(a), BinaryOp::And, Box::new(b)))
}

/// A comparison of an expression with a constant other than NULL, as a
/// condition on the expression, if it is one.
fn comparison(expr: &Expr) -> Option<Condition> {
    let Expr::Binary(left, op, right) = expr else {
        return None;
    };
    let op = compare(*op)?;
    let (expr, literal, op) = match (&**left, &**right) {
        (Expr::Literal(_), Expr::Literal(_)) => return None,
        (expr, Expr::Literal(literal)) => (expr, literal, op),
        (Expr::Literal(literal), expr) => (expr, literal, op.flip()),
        _ => return None,
    };
    (!literal.is_null() && !expr.has_subquery()).then(|| Condition {
        expr: expr.clone(),
        op,
        literal: literal.clone(),
    })
}

fn compare(op: BinaryOp) -> Option<Compare> {
    Some(match op {
        BinaryOp::Eq => Compare::Eq,
        BinaryOp::NotEq => Compare::NotEq,
        BinaryOp::Lt => Compare::Lt,
        BinaryOp::LtEq => Compare::LtEq,
        BinaryOp::Gt => Compare::Gt,
        BinaryOp::GtEq => Compare::GtEq,
        _ => return None,
    })
}

/// Whether the expression is a BOOL wherever it does not fail: NULL, a
/// BOOL constant, or an operator that gives one.
pub fn is_boolean(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(value) => matches!(value, Value::Bool(_) | Value::Null),
        Expr::Binary(_, op, _) => !matches!(
            op,
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Concat
        ),
        Expr::Unary(UnaryOp::Not, _)
        | Expr::IsNull(..)
        | Expr::InList(..)
        | Expr::InSubquery(..)
        | Expr::Exists(_) => true,
        _ => false,
    }
}

/// Whether an expression reads nothing of a row: no column, no subquery,
/// no aggregate.
fn constant(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(_) => true,
        Expr::Column(_)
        | Expr::Outer(_)
        | Expr::Slot(..)
        | Expr::Aggregate(..)
        | Expr::Window(_)
        | Expr::Subquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery(..) => false,
        Expr::Unary(_, inner) | Expr::IsNull(inner, _) => constant(inner),
        Expr::Binary(left, _, right) => constant(left) && constant(right),
        Expr::InList(left, items, _) => constant(left) && items.iter().all(constant),
        Expr::Case { .. } | Expr::Function(..) => expr.parts().into_iter().all(constant),
    }
}

/// The expression's value, if it reads nothing of a row and evaluates
/// without an error; else the expression.
fn fold(expr: Expr) -> Expr {
    if matches!(expr, Expr::Literal(_)) || !constant(&expr) {
        return expr;
    }
    match eval(&expr, &[], &[]) {
        Ok(value) => Expr::Literal(value),
        Err(_) => expr,
    }
}

fn is(expr: &Expr, value: bool) -> bool {
    *expr == Expr::Literal(Value::Bool(value))
}

/// `left op right`, simplified, both sides simplified already.
fn binary(left: Expr, op: BinaryOp, right: Expr) -> Expr {
    match op {
        // `FALSE AND x` never evaluates `x`; `TRUE AND x` is `x`'s value.
        BinaryOp::And if is(&left, false) => return left,
        BinaryOp::And if is(&left, true) && is_boolean(&right) => return right,
        BinaryOp::And if is(&right, true) && is_boolean(&left) => return left,
        BinaryOp::Or if is(&left, true) => return left,
        BinaryOp::Or if is(&left, false) && is_boolean(&right) => return right,
        BinaryOp::Or if is(&right, false) && is_boolean(&left) => return left,
        BinaryOp::Or => {
            if let Some(list) = merge_in(&left, &right) {
                return list;
            }
        }
        _ => {}
    }
    fold(Expr::Binary(Box::new(left), op, Box::new(right)))
}

/// `left OR right` as one `x IN (...)`, if each is `x = constant` or
/// `x IN (constants)` for the same `x`, with constants that are not NULL
/// and all of one kind that compare with each other: all numbers, all
/// TEXT, all BOOL, all DATE and so on. The two give the same value
/// for every `x`, and fail alike for an `x` of another type: on its
/// comparison with the first constant.
fn merge_in(left: &Expr, right: &Expr) -> Option<Expr> {
    fn parts(expr: &Expr) -> Option<(&Expr, Vec<&Value>)> {
        match expr {
            Expr::Binary(a, BinaryOp::Eq, b) => match (&**a, &**b) {
                (Expr::Literal(_), Expr::Literal(_)) => None,
                (x, Expr::Literal(v)) | (Expr::Literal(v), x) => Some((x, vec![v])),
                _ => None,
            },
            Expr::InList(x, items, false) => {
                let values = items.iter().map(|item| match item {
                    Expr::Literal(v) => Some(v),
                    _ => None,
                });
                Some((x, values.collect::<Option<_>>()?))
            }
            _ => None,
        }
    }
    let (x, mut values) = parts(left)?;
    let (y, more) = parts(right)?;
    values.extend(more);
    // Values of one kind compare with each other (see `Value::compare`).
    let kind = |v: &Value| match v {
        Value::Int(_) | Value::Real(_) | Value::Decimal(_) => Some(0),
        Value::Text(_) => Some(1),
        Value::Bool(_) => Some(2),
        Value::Date(_) => Some(3),
        Value::Time(_) => Some(4),
        Value::Timestamp(_) => Some(5),
        Value::TimestampTz(_) => Some(6),
        Value::Interval(_) => Some(7),
        Value::Blob(_) => Some(8),
        Value::Null => None,
    };
    let first = kind(values[0])?;
    let same = values.iter().all(|v| kind(v) == Some(first));
    (x == y && same && !x.has_subquery()).then(|| {
        let items = values.into_iter().cloned().map(Expr::Literal).collect();
        Expr::InList(Box::new(x.clone()), items, false)
    })
}

/// `NOT inner`, simplified, `inner` simplified already: pushed into what
/// it negates, where it can be.
fn not(inner: Expr) -> Expr {
    let negated = |expr: Expr| Expr::Unary(UnaryOp::Not, Box::new(expr));
    match inner {
        Expr::Literal(_) => fold(negated(inner)),
        Expr::Unary(UnaryOp::Not, inner) if is_boolean(&inner) => *inner,
        // De Morgan: each side is evaluated where it was, as `x AND y`
        // evaluates `y` only where `x` is not FALSE, that is, where NOT x
        // is not TRUE.
        Expr::Binary(left, op @ (BinaryOp::And | BinaryOp::Or), right)
            if is_boolean(&left) && is_boolean(&right) =>
        {
            let flipped = match op {
                BinaryOp::And => BinaryOp::Or,
                _ => BinaryOp::And,
            };
            binary(not(*left), flipped, not(*right))
        }
        Expr::Binary(left, op, right) if compare(op).is_some() => {
            let op = match op {
                BinaryOp::Eq => BinaryOp::NotEq,
                BinaryOp::NotEq => BinaryOp::Eq,
                BinaryOp::Lt => BinaryOp::GtEq,
                BinaryOp::LtEq => BinaryOp::Gt,
                BinaryOp::Gt => BinaryOp::LtEq,
                _ => BinaryOp::Lt,
            };
            Expr::Binary(left, op, right)
        }
        Expr::IsNull(inner, negated) => Expr::IsNull(inner, !negated),
        Expr::InList(left, items, negated) => Expr::InList(left, items, !negated),
        Expr::InSubquery(left, query, negated) => Expr::InSubquery(left, query, !negated),
        inner => negated(inner),
    }
}

/// Common subexpression elimination for a select list: the expressions
/// that it computes more than once, each named `#1`, `#2`, ... and the list
/// with each of their places reading that name instead.
///
/// The step that computes them (`EXTEND`) runs before the select list, so
/// one is taken only if it is evaluated on every row anyway: somewhere
/// outside the right side of an `AND` or `OR`, the branches of a `CASE`
/// and the later arguments of a `COALESCE`, which may not be. The largest
/// is taken first, and then the next among what is left. Nothing with a
/// subquery is taken, nor a column or a constant, which cost nothing to
/// read again.
pub fn common(exprs: Vec<Expr>) -> (Vec<(String, Expr)>, Vec<Expr>) {
    let mut exprs = exprs;
    let mut shared: Vec<(String, Expr)> = Vec::new();
    loop {
        let mut seen: Vec<Seen> = Vec::new();
        for expr in &exprs {
            count(expr, true, &mut seen);
        }
        let best = seen.into_iter().filter(|s| s.times > 1 && s.always).fold(
            None,
            |best: Option<Seen>, s| match &best {
                Some(b) if size(&b.expr) >= size(&s.expr) => best,
                _ => Some(s),
            },
        );
        let Some(Seen { expr, .. }) = best else {
            return (shared, exprs);
        };
        let name = format!("#{}", shared.len() + 1);
        let column = Expr::Column(ColumnRef::bare(&name));
        exprs = exprs
            .iter()
            .map(|e| replace_outside(e, &expr, &column))
            .collect();
        shared.push((name, expr));
    }
}

/// A subexpression found, how often, and whether once where it is always evaluated.
struct Seen {
    expr: Expr,
    times: usize,
    always: bool,
}

/// Counts the subexpressions of `expr` that could be computed once, outside
/// its subqueries; `always` if `expr` is evaluated on every row.
fn count(expr: &Expr, always: bool, seen: &mut Vec<Seen>) {
    let worth = !matches!(
        expr,
        Expr::Literal(_) | Expr::Column(_) | Expr::Outer(_) | Expr::Slot(..)
    ) && !expr.has_subquery()
        && !has_aggregate(expr);
    if worth {
        match seen.iter_mut().find(|s| s.expr == *expr) {
            Some(s) => {
                s.times += 1;
                s.always |= always;
            }
            None => seen.push(Seen {
                expr: expr.clone(),
                times: 1,
                always,
            }),
        }
    }
    match expr {
        Expr::Binary(left, BinaryOp::And | BinaryOp::Or, right) => {
            count(left, always, seen);
            count(right, false, seen);
        }
        Expr::Binary(left, _, right) => {
            count(left, always, seen);
            count(right, always, seen);
        }
        Expr::Unary(_, inner) | Expr::IsNull(inner, _) => count(inner, always, seen),
        Expr::InList(left, items, _) => {
            count(left, always, seen);
            items.iter().for_each(|e| count(e, always, seen));
        }
        Expr::InSubquery(left, ..) => count(left, always, seen),
        // The operand and the first WHEN are always evaluated; the rest may not be.
        Expr::Case { .. } | Expr::Function(Function::Coalesce, _) => {
            for (i, part) in expr.parts().into_iter().enumerate() {
                count(part, always && i == 0, seen);
            }
        }
        Expr::Function(_, args) => args.iter().for_each(|e| count(e, always, seen)),
        _ => {}
    }
}

fn has_aggregate(expr: &Expr) -> bool {
    let mut found = false;
    expr.visit(&mut |e| found |= matches!(e, Expr::Aggregate(..)));
    found
}

/// How many nodes the expression has.
fn size(expr: &Expr) -> usize {
    let mut n = 0;
    expr.visit(&mut |_| n += 1);
    n
}

/// `expr` with each place that is `target` reading `with`, outside its
/// subqueries, whose expressions are of their own tables.
fn replace_outside(expr: &Expr, target: &Expr, with: &Expr) -> Expr {
    expr.replace(&mut |e| match e {
        e if e == target => Some(Ok(with.clone())),
        Expr::Subquery(_) | Expr::Exists(_) => Some(Ok(e.clone())),
        Expr::InSubquery(left, query, negated) => Some(Ok(Expr::InSubquery(
            Box::new(replace_outside(left, target, with)),
            query.clone(),
            *negated,
        ))),
        _ => None,
    })
    .expect("replacing cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::{Select, Statement};
    use crate::sql::parse;

    fn expr(sql: &str) -> Expr {
        match parse(&format!("SELECT * FROM t WHERE {sql}"))
            .unwrap()
            .pop()
        {
            Some(Statement::Select(select)) => match *select {
                Select {
                    filter: Some(filter),
                    ..
                } => filter,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// The simplified expression, as SQL.
    fn simplified(sql: &str) -> String {
        simplify(&expr(sql)).to_string()
    }

    fn clause(sql: &str) -> String {
        match simplify_clause(&expr(sql)) {
            Clause::Always => "always".into(),
            Clause::Never => "never".into(),
            Clause::When(e) => e.to_string(),
        }
    }

    #[test]
    fn constants_fold_unless_they_fail() {
        assert_eq!(simplified("price > 2 * 50"), "price > 100");
        assert_eq!(simplified("x = -(3 - 5)"), "x = 2");
        assert_eq!(simplified("name = UPPER('a' || 'b')"), "name = 'AB'");
        assert_eq!(simplified("1 + 1 = 2"), "TRUE");
        assert_eq!(simplified("NULL = 1"), "NULL");
        assert_eq!(simplified("x IN (1 + 1, 3)"), "x IN (2, 3)");
        // An error is left to happen where it would have.
        assert_eq!(simplified("x > 1 / 0"), "x > (1 / 0)");
        assert_eq!(
            simplified("CASE WHEN x > 0 THEN 1 / 0 ELSE 2 * 3 END = 6"),
            "CASE WHEN x > 0 THEN 1 / 0 ELSE 6 END = 6"
        );
        assert_eq!(simplified("COUNT(1 + 1) > 2 * 2"), "COUNT(2) > 4");
    }

    #[test]
    fn boolean_identities_hold_only_for_booleans() {
        assert_eq!(simplified("TRUE AND x > 1"), "x > 1");
        assert_eq!(simplified("x > 1 AND TRUE"), "x > 1");
        assert_eq!(simplified("FALSE AND x"), "FALSE");
        assert_eq!(simplified("TRUE OR x"), "TRUE");
        assert_eq!(simplified("x = 1 OR FALSE"), "x = 1");
        assert_eq!(simplified("1 = 2 OR x IS NULL"), "x IS NULL");
        // `x` may be no BOOL, and `TRUE AND x` would then fail where `x` would not.
        assert_eq!(simplified("TRUE AND x"), "TRUE AND x");
        assert_eq!(simplified("x OR FALSE"), "x OR FALSE");
        // `x AND FALSE` evaluates `x`, which may fail.
        assert_eq!(simplified("x > 1 AND FALSE"), "(x > 1) AND FALSE");
    }

    #[test]
    fn not_is_pushed_down() {
        assert_eq!(simplified("NOT (a = 1 OR b < 2)"), "(a != 1) AND (b >= 2)");
        assert_eq!(
            simplified("NOT (a > 1 AND b IS NULL)"),
            "(a <= 1) OR (b IS NOT NULL)"
        );
        assert_eq!(simplified("NOT NOT (a = 1)"), "a = 1");
        assert_eq!(simplified("NOT a NOT IN (1, 2)"), "a IN (1, 2)");
        assert_eq!(simplified("NOT (a BETWEEN 1 AND 5)"), "(a < 1) OR (a > 5)");
        assert_eq!(simplified("NOT a LIKE 'x%'"), "NOT (a LIKE 'x%')");
        // Not of something that may be no BOOL stays as it is.
        assert_eq!(simplified("NOT NOT a"), "NOT (NOT a)");
        assert_eq!(simplified("NOT (a AND b = 1)"), "NOT (a AND (b = 1))");
    }

    #[test]
    fn equalities_on_one_expression_become_in() {
        assert_eq!(simplified("x = 1 OR x = 2 OR 3 = x"), "x IN (1, 2, 3)");
        assert_eq!(simplified("x IN (1, 2) OR x = 2.5"), "x IN (1, 2, 2.5)");
        assert_eq!(
            simplified("LOWER(n) = 'a' OR LOWER(n) = 'b'"),
            "LOWER(n) IN ('a', 'b')"
        );
        // Different expressions, or constants of different types, stay apart.
        assert_eq!(simplified("x = 1 OR y = 2"), "(x = 1) OR (y = 2)");
        assert_eq!(simplified("x = 1 OR x = 'a'"), "(x = 1) OR (x = 'a')");
        // `x = NULL` is NULL, but evaluates `x`, which may fail.
        assert_eq!(simplified("x = 1 OR x = NULL"), "(x = 1) OR (x = NULL)");
    }

    #[test]
    fn clauses_drop_what_they_need_not_check() {
        assert_eq!(clause("x > 5 AND x > 3 AND y = 1"), "(x > 5) AND (y = 1)");
        assert_eq!(clause("x > 3 AND x >= 5"), "x >= 5");
        assert_eq!(clause("x = 3 AND x != 2 AND x < 10"), "x = 3");
        assert_eq!(clause("x > 1 AND x > 1"), "x > 1");
        assert_eq!(clause("1 = 1 AND x > 1"), "x > 1");
        assert_eq!(clause("1 = 1"), "always");
        assert_eq!(clause("x > 1 AND 1 = 0"), "never");
        assert_eq!(clause("x > 1 AND NULL"), "never");
        // Values of other types imply nothing of each other.
        assert_eq!(clause("x > 'a' AND x > 3"), "(x > 'a') AND (x > 3)");
    }

    #[test]
    fn repeated_subexpressions_are_computed_once() {
        let exprs = |sql: &str| match parse(&format!("SELECT {sql} FROM t")).unwrap().pop() {
            Some(Statement::Select(select)) => match *select {
                Select {
                    projection: crate::sql::ast::Projection::Exprs(items),
                    ..
                } => items.into_iter().map(|i| i.expr).collect::<Vec<_>>(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        let show = |(shared, exprs): (Vec<(String, Expr)>, Vec<Expr>)| {
            let shared: Vec<String> = shared.iter().map(|(n, e)| format!("{n} = {e}")).collect();
            let exprs: Vec<String> = exprs.iter().map(Expr::to_string).collect();
            format!("{}; {}", shared.join(", "), exprs.join(", "))
        };
        assert_eq!(
            show(common(exprs("a * b, a * b + 1, (a * b + 1) * 2"))),
            r##"#1 = (a * b) + 1; a * b, "#1", "#1" * 2"##
        );
        // Taken where it is always evaluated, then read in every place.
        assert_eq!(
            show(common(exprs("a + 1, CASE WHEN x THEN a + 1 END"))),
            r##"#1 = a + 1; "#1", CASE WHEN x THEN "#1" END"##
        );
        // Never computed ahead of a place that may not evaluate it.
        assert_eq!(
            show(common(exprs(
                "CASE WHEN x THEN a / b END, COALESCE(c, a / b)"
            ))),
            "; CASE WHEN x THEN a / b END, COALESCE(c, a / b)"
        );
        assert_eq!(show(common(exprs("a, a, 1, 1"))), "; a, a, 1, 1");
    }
}
