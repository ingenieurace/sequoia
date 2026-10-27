//! Selectivity: what share of rows a condition keeps, and of pairs of rows a
//! join's equalities keep, for the planner to estimate how many rows each
//! step of a plan returns (see `estimate` in `query.rs`).
//!
//! With statistics (see `catalog/stats.rs`), a condition on a column is
//! estimated from what ANALYZE learned of its values:
//!
//! ```text
//! x = 5, x IN (1, 2)          the share of rows that hold the value(s)
//! x < 5, x BETWEEN 1 AND 9    the share of rows in the range: the bounds on
//!                             one column in an AND are one range, not two
//!                             independent conditions
//! x IS NULL                   the share of NULLs
//! a.x = b.y                   1 / the larger count of distinct values
//! p AND q, p OR q, NOT p      s(p) s(q), s(p) + s(q) - s(p) s(q), 1 - s(p):
//!                             as if the conditions were independent
//! ```
//!
//! Without statistics, it is guessed the way PostgreSQL guesses without
//! them: an equality keeps 1 in 200 rows ([`EQUAL_KEEPS`]), anything else a
//! third ([`OTHER_KEEPS`]). The planner keeps its old guesses for a whole
//! condition that reads no column with statistics, so that plans of tables
//! never analyzed stay as they were; [`keeps`] says so by returning None.
//!
//! A join of `l` and `r` rows on equalities `a = b` returns `l r / max(d(a),
//! d(b))` rows for `d` distinct values (System R's formula, which holds if
//! every value of the side with fewer is among those of the other), times
//! the share of each side that is not NULL; with several equalities, the
//! product of their shares.

use std::cmp::Ordering;

use super::filter::constant;
use super::query::Node;
use crate::catalog::stats::{ColumnStats, TableStats};
use crate::catalog::table::Table;
use crate::predicate::{Compare, Predicate};
use crate::sql::ast::{BinaryOp, ColumnRef, Expr, UnaryOp};
use crate::types::Value;

/// How many of its rows an equality keeps, guessed with no statistics as
/// PostgreSQL guesses: 1 in 200 (but at most one row on a primary key or
/// unique index, which `estimate` knows), `<>` all but that, and a
/// comparison or anything else 1 in 3.
pub const EQUAL_KEEPS: f64 = 0.005;
pub const OTHER_KEEPS: f64 = 1.0 / 3.0;

/// The rows a table is taken to have when they cannot be counted.
const UNKNOWN_ROWS: f64 = 1000.0;

/// How many rows a table has, for estimates.
pub fn table_rows(table: &Table) -> f64 {
    table.row_count().map_or(UNKNOWN_ROWS, |rows| rows as f64)
}

/// The tables whose columns a condition may read, by the names it calls
/// them, with the statistics of those that have any.
#[derive(Clone, Default)]
pub struct Scope<'a> {
    tables: Vec<(String, &'a Table)>,
}

/// A column of a table in a scope, with its statistics and the table's rows.
struct Known<'s> {
    /// Tells the columns apart, to put the bounds on one together.
    id: (usize, usize),
    stats: &'s ColumnStats,
    rows: f64,
}

impl<'a> Scope<'a> {
    pub fn new(tables: impl IntoIterator<Item = (String, &'a Table)>) -> Self {
        Scope {
            tables: tables.into_iter().collect(),
        }
    }

    /// The tables a plan step reads rows of, as its expressions name them:
    /// those of its scans and lookups, through filters, joins and the steps
    /// that keep their input's columns, but not through a projection, an
    /// aggregate or a derived table, which name their columns anew.
    pub fn of(node: &Node<'a>) -> Self {
        fn collect<'a>(node: &Node<'a>, out: &mut Vec<(String, &'a Table)>) {
            match node {
                Node::Scan {
                    table, qualifier, ..
                }
                | Node::Lookup {
                    table, qualifier, ..
                } => out.push((qualifier.clone(), *table)),
                Node::Filter { input, .. }
                | Node::Sort { input, .. }
                | Node::Limit { input, .. }
                | Node::Distinct { input }
                | Node::Gather { input, .. } => collect(input, out),
                Node::Join { left, right, .. } => {
                    collect(left, out);
                    collect(right, out);
                }
                Node::Aggregate { .. }
                | Node::Project { .. }
                | Node::Derived { .. }
                | Node::Union { .. } => {}
            }
        }
        let mut tables = Vec::new();
        collect(node, &mut tables);
        Scope { tables }
    }

    /// The tables, each with the name it goes by.
    pub fn tables(&self) -> &[(String, &'a Table)] {
        &self.tables
    }

    /// The tables of both scopes.
    pub fn with(&self, other: &Scope<'a>) -> Self {
        Scope {
            tables: [self.tables.clone(), other.tables.clone()].concat(),
        }
    }

    /// The statistics of the column an expression is, if it is a column of
    /// a table that has them.
    fn known(&self, expr: &Expr) -> Option<Known<'a>> {
        let Expr::Column(column) = expr else {
            return None;
        };
        let (t, table, c) = self.column(column)?;
        let stats: &'a TableStats = table.stats()?;
        Some(Known {
            id: (t, c),
            stats: &stats.columns[c],
            rows: table_rows(table),
        })
    }

    /// The table (by position) and column a reference names, if exactly one
    /// table of the scope has it.
    fn column(&self, column: &ColumnRef) -> Option<(usize, &'a Table, usize)> {
        let mut found = self
            .tables
            .iter()
            .enumerate()
            .filter_map(|(t, (name, table))| {
                if column.table.as_ref().is_some_and(|q| q != name) {
                    return None;
                }
                let c = table.column_index(&column.name).ok()?;
                Some((t, *table, c))
            });
        match (found.next(), found.next()) {
            (Some(one), None) => Some(one),
            _ => None,
        }
    }
}

/// The share of rows `condition` keeps, if it reads a column with
/// statistics; None if it reads none, for the caller to guess as it did
/// before statistics.
pub fn keeps(condition: &Expr, scope: &Scope) -> Option<f64> {
    let (share, informed) = share(condition, scope);
    informed.then_some(share.clamp(0.0, 1.0))
}

/// The share of a table's rows its scan's predicates keep, from its
/// statistics: each column's bounds as one range, and the rest as if
/// independent.
pub fn predicates(stats: &TableStats, predicates: &[Predicate], rows: f64) -> f64 {
    let known = predicates.iter().map(|p| {
        let column = Known {
            id: (0, p.column),
            stats: &stats.columns[p.column],
            rows,
        };
        (column, p.op, p.literal.clone())
    });
    combine(known.collect(), 1.0)
}

/// The share of pairs of rows, `l` left ones and `r` right ones, that
/// equalities `left = right` between the two sides keep, from the
/// statistics of the columns they compare; None if none of them compares a
/// column with statistics.
pub fn join(keys: &[(&Expr, &Expr)], left: &Scope, right: &Scope, l: f64, r: f64) -> Option<f64> {
    let mut product = None;
    for (a, b) in keys {
        if let Some(share) = equal_columns(left.known(a), right.known(b), l, r) {
            product = Some(product.unwrap_or(1.0) * share);
        }
    }
    product
}

/// The share of pairs of rows whose columns are equal, when at least one of
/// them has statistics: their non-NULL shares over the larger count of
/// distinct values, each no more than its side's rows.
fn equal_columns(a: Option<Known>, b: Option<Known>, a_rows: f64, b_rows: f64) -> Option<f64> {
    let side = |k: &Known, rows: f64| {
        let distinct = k.stats.distinct(k.rows).min(rows.max(1.0));
        (1.0 - k.stats.nulls, distinct)
    };
    let (non_null, distinct) = match (&a, &b) {
        (None, None) => return None,
        (Some(k), None) => side(k, a_rows),
        (None, Some(k)) => side(k, b_rows),
        (Some(x), Some(y)) => {
            let (x, y) = (side(x, a_rows), side(y, b_rows));
            (x.0 * y.0, x.1.max(y.1))
        }
    };
    Some(non_null / distinct.max(1.0))
}

/// The share of rows an expression keeps, and whether statistics told any of it.
fn share(expr: &Expr, scope: &Scope) -> (f64, bool) {
    match expr {
        Expr::Binary(_, BinaryOp::And, _) => {
            let mut parts = Vec::new();
            conjuncts(expr, &mut parts);
            let mut ranges = Vec::new();
            let (mut rest, mut informed) = (1.0, false);
            for part in parts {
                match comparison(part, scope) {
                    Some(known) => {
                        ranges.push(known);
                        informed = true;
                    }
                    None => {
                        let (s, i) = share(part, scope);
                        rest *= s;
                        informed |= i;
                    }
                }
            }
            (combine(ranges, rest), informed)
        }
        Expr::Binary(a, BinaryOp::Or, b) => {
            let ((x, i), (y, j)) = (share(a, scope), share(b, scope));
            (x + y - x * y, i || j)
        }
        Expr::Unary(UnaryOp::Not, inner) => {
            let (s, i) = share(inner, scope);
            (1.0 - s, i)
        }
        Expr::Binary(a, op, b) => {
            if let Some((known, op, value)) = comparison(expr, scope) {
                return (combine(vec![(known, op, value)], 1.0), true);
            }
            let op = compare_op(*op);
            if op == Some(Compare::Eq)
                && let Some(s) = equal_columns(scope.known(a), scope.known(b), f64::MAX, f64::MAX)
            {
                return (s, true);
            }
            (guess(op), false)
        }
        Expr::IsNull(inner, negated) => match scope.known(inner) {
            Some(k) if *negated => (1.0 - k.stats.nulls, true),
            Some(k) => (k.stats.nulls, true),
            None if *negated => (1.0 - EQUAL_KEEPS, false),
            None => (EQUAL_KEEPS, false),
        },
        Expr::InList(left, items, negated) => {
            let values: Option<Vec<Value>> = items.iter().map(constant).collect();
            match (scope.known(left), values) {
                (Some(k), Some(values)) => {
                    let mut distinct: Vec<&Value> = Vec::new();
                    for v in &values {
                        if !distinct
                            .iter()
                            .any(|d| d.compare(v) == Some(Ordering::Equal))
                        {
                            distinct.push(v);
                        }
                    }
                    let equal: f64 = distinct.iter().map(|v| k.stats.equal(v, k.rows)).sum();
                    let equal = equal.min(1.0 - k.stats.nulls);
                    match negated {
                        // NOT IN with a NULL in the list keeps no row.
                        true if values.iter().any(Value::is_null) => (0.0, true),
                        true => (1.0 - k.stats.nulls - equal, true),
                        false => (equal, true),
                    }
                }
                _ => {
                    let equal = (EQUAL_KEEPS * items.len() as f64).min(1.0);
                    (if *negated { 1.0 - equal } else { equal }, false)
                }
            }
        }
        Expr::Literal(Value::Bool(true)) => (1.0, false),
        Expr::Literal(_) => (0.0, false),
        _ => (OTHER_KEEPS, false),
    }
}

/// What a guess without statistics is for a comparison with this operator.
fn guess(op: Option<Compare>) -> f64 {
    match op {
        Some(Compare::Eq) => EQUAL_KEEPS,
        Some(Compare::NotEq) => 1.0 - EQUAL_KEEPS,
        _ => OTHER_KEEPS,
    }
}

fn compare_op(op: BinaryOp) -> Option<Compare> {
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

/// A comparison of a column that has statistics with a constant: the
/// column, the operator (as if the column were on the left) and the value.
fn comparison<'a>(expr: &Expr, scope: &Scope<'a>) -> Option<(Known<'a>, Compare, Value)> {
    let Expr::Binary(a, op, b) = expr else {
        return None;
    };
    let op = compare_op(*op)?;
    match (scope.known(a), scope.known(b)) {
        (Some(k), None) => Some((k, op, constant(b)?)),
        (None, Some(k)) => Some((k, op.flip(), constant(a)?)),
        _ => None,
    }
}

/// The share of rows that comparisons of columns with values keep together,
/// times `rest`: the bounds on each column as one range, and the columns
/// and other comparisons as if independent.
fn combine(comparisons: Vec<(Known, Compare, Value)>, rest: f64) -> f64 {
    // Each column's tightest bounds: (value, inclusive).
    let mut ranges: Vec<(Known, Option<(Value, bool)>, Option<(Value, bool)>)> = Vec::new();
    let mut share = rest;
    for (known, op, value) in comparisons {
        if value.is_null() {
            share = 0.0; // a comparison with NULL holds for no row
            continue;
        }
        let (lower, upper) = match op {
            Compare::Gt => (Some((value, false)), None),
            Compare::GtEq => (Some((value, true)), None),
            Compare::Lt => (None, Some((value, false))),
            Compare::LtEq => (None, Some((value, true))),
            Compare::Eq | Compare::NotEq => {
                share *= known.stats.compare(op, &value, known.rows);
                continue;
            }
        };
        match ranges.iter_mut().find(|(k, ..)| k.id == known.id) {
            Some((_, l, u)) => {
                *l = tighter(l.take(), lower, Ordering::Greater);
                *u = tighter(u.take(), upper, Ordering::Less);
            }
            None => ranges.push((known, lower, upper)),
        }
    }
    for (known, lower, upper) in ranges {
        let lower = lower.as_ref().map(|(v, i)| (v, *i));
        let upper = upper.as_ref().map(|(v, i)| (v, *i));
        share *= known.stats.range(lower, upper, known.rows);
    }
    share.clamp(0.0, 1.0)
}

/// The tighter of two bounds: the one further `toward` the middle of the
/// range (Greater for lower bounds), or the exclusive one of equal values.
fn tighter(
    a: Option<(Value, bool)>,
    b: Option<(Value, bool)>,
    toward: Ordering,
) -> Option<(Value, bool)> {
    match (a, b) {
        (None, b) => b,
        (a, None) => a,
        (Some(a), Some(b)) => match a.0.compare(&b.0) {
            Some(Ordering::Equal) => Some((a.0, a.1 && b.1)),
            Some(ord) if ord == toward => Some(a),
            Some(_) => Some(b),
            None => Some(a),
        },
    }
}

/// Splits an expression at its ANDs.
fn conjuncts<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
    match expr {
        Expr::Binary(left, BinaryOp::And, right) => {
            conjuncts(left, out);
            conjuncts(right, out);
        }
        expr => out.push(expr),
    }
}
