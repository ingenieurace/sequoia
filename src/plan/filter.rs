//! What a statement needs from its table: the columns its expressions read,
//! and the simple conditions its WHERE clause implies.

use crate::catalog::table::Table;
use crate::predicate::{Compare, Predicate};
use crate::sql::ast::{BinaryOp, Expr};
use crate::sql::eval::eval;
use crate::types::{Column, Value, column_index};

/// Marks the columns the expressions use, so that a scan may skip the others.
pub fn needed_columns<'a>(columns: &[Column], exprs: impl IntoIterator<Item = &'a Expr>) -> Vec<bool> {
    fn visit(expr: &Expr, columns: &[Column], needed: &mut [bool]) {
        match expr {
            Expr::Literal(_) => {}
            // An unknown name is reported when the expression is evaluated.
            Expr::Column(name) => column_index(columns, name).into_iter().for_each(|i| needed[i] = true),
            Expr::Unary(_, inner) | Expr::IsNull(inner, _) => visit(inner, columns, needed),
            Expr::Binary(left, _, right) => {
                visit(left, columns, needed);
                visit(right, columns, needed);
            }
        }
    }
    let mut needed = vec![false; columns.len()];
    exprs.into_iter().for_each(|expr| visit(expr, columns, &mut needed));
    needed
}

/// Pulls the conjuncts `column op literal` (or `literal op column`) out of a
/// WHERE clause, for a column store to evaluate on its compressed chunks, or
/// for a heap or LSM table to pick an index with. A row the clause matches
/// satisfies every conjunct, so the scan may skip rows that fail one; the
/// whole clause is still evaluated on the rows returned.
/// A literal that is NULL or of the wrong type is left to that evaluation,
/// which matches nothing or reports the error. As in any database that
/// reorders predicates, the rest of the clause is then never evaluated on the
/// skipped rows, so an error it would raise only there (`1 / 0`) is not raised.
pub fn predicates(table: &Table, filter: &Option<Expr>) -> Vec<Predicate> {
    fn conjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
        match expr {
            Expr::Binary(left, BinaryOp::And, right) => {
                conjuncts(left, out);
                conjuncts(right, out);
            }
            expr => out.push(expr),
        }
    }
    /// The value of an expression that reads no column, if it has one.
    fn constant(expr: &Expr) -> Option<Value> {
        fn reads_columns(expr: &Expr) -> bool {
            match expr {
                Expr::Literal(_) => false,
                Expr::Column(_) => true,
                Expr::Unary(_, inner) | Expr::IsNull(inner, _) => reads_columns(inner),
                Expr::Binary(left, _, right) => reads_columns(left) || reads_columns(right),
            }
        }
        if reads_columns(expr) { None } else { eval(expr, &[], &[]).ok() }
    }

    let Some(filter) = filter else { return Vec::new() };
    let mut exprs = Vec::new();
    conjuncts(filter, &mut exprs);
    exprs
        .into_iter()
        .filter_map(|expr| {
            let Expr::Binary(left, op, right) = expr else { return None };
            let op = match op {
                BinaryOp::Eq => Compare::Eq,
                BinaryOp::NotEq => Compare::NotEq,
                BinaryOp::Lt => Compare::Lt,
                BinaryOp::LtEq => Compare::LtEq,
                BinaryOp::Gt => Compare::Gt,
                BinaryOp::GtEq => Compare::GtEq,
                _ => return None,
            };
            let (name, literal, op) = match (&**left, &**right) {
                (Expr::Column(name), other) => (name, constant(other)?, op),
                (other, Expr::Column(name)) => (name, constant(other)?, op.flip()),
                _ => return None,
            };
            let column = table.column_index(name).ok()?;
            (!literal.is_null() && literal.fits(table.columns[column].data_type))
                .then_some(Predicate { column, op, literal })
        })
        .collect()
}
