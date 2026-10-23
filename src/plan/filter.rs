//! What a statement needs from its table: the columns its expressions read,
//! and the simple conditions its WHERE clause implies.

use crate::catalog::table::Table;
use crate::predicate::{Compare, Predicate};
use crate::sql::ast::{BinaryOp, ColumnRef, Expr, Select};
use crate::sql::eval::eval;
use crate::storage::sort_key;
use crate::types::{DataType, Value};

/// Marks the columns of `table`, which goes by `qualifier`, that the
/// expressions use, so that a scan may skip the others.
pub fn needed_columns<'a>(
    table: &Table,
    qualifier: &str,
    exprs: impl IntoIterator<Item = &'a Expr>,
) -> Vec<bool> {
    let mut needed = vec![false; table.columns.len()];
    for expr in exprs {
        visit_columns(expr, &mut |column| {
            // An unknown name is reported when the expression is evaluated.
            if let Some(i) = own_column(table, qualifier, column) {
                needed[i] = true;
            }
        });
    }
    needed
}

/// Calls `f` on every column reference in the expression: those of its
/// subqueries count only if they are outer references, to this expression's row.
pub fn visit_columns(expr: &Expr, f: &mut impl FnMut(&ColumnRef)) {
    match expr {
        Expr::Literal(_) | Expr::Aggregate(_, None, _) => {}
        Expr::Column(column) | Expr::Outer(column) | Expr::Slot(_, column) => f(column),
        Expr::Unary(_, inner) | Expr::IsNull(inner, _) | Expr::Aggregate(_, Some(inner), _) => {
            visit_columns(inner, f)
        }
        Expr::Binary(left, _, right) => {
            visit_columns(left, f);
            visit_columns(right, f);
        }
        Expr::InList(left, items, _) => {
            visit_columns(left, f);
            items.iter().for_each(|e| visit_columns(e, f));
        }
        Expr::Subquery(query) | Expr::Exists(query) => visit_outer(query, f),
        Expr::InSubquery(left, query, _) => {
            visit_columns(left, f);
            visit_outer(query, f);
        }
        Expr::Case { .. } | Expr::Function(..) | Expr::Window(_) => {
            expr.parts().into_iter().for_each(|e| visit_columns(e, f))
        }
    }
}

/// Calls `f` on every outer reference in the query, its subqueries' included.
pub fn visit_outer(query: &Select, f: &mut impl FnMut(&ColumnRef)) {
    for e in query.exprs() {
        e.visit(&mut |e| {
            if let Expr::Outer(column) = e {
                f(column);
            }
        });
    }
}

/// The index of the column of `table`, which goes by `qualifier`, that a reference names, if it names one.
fn own_column(table: &Table, qualifier: &str, column: &ColumnRef) -> Option<usize> {
    match &column.table {
        Some(t) if t != qualifier => None,
        _ => table.column_index(&column.name).ok(),
    }
}

/// A conjunct `expr op constant` of a WHERE clause (or `constant op expr`,
/// turned around), whose expression reads columns of one table and nothing
/// else: written with those columns unqualified, as index keys are. The
/// constant is not NULL, and, compared with a column, is of its type.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub expr: Expr,
    pub op: Compare,
    pub literal: Value,
}

/// Splits a WHERE clause at its ANDs.
pub fn conjuncts(expr: &Expr) -> Vec<&Expr> {
    fn split<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
        match expr {
            Expr::Binary(left, BinaryOp::And, right) => {
                split(left, out);
                split(right, out);
            }
            expr => out.push(expr),
        }
    }
    let mut out = Vec::new();
    split(expr, &mut out);
    out
}

/// The conjuncts of a WHERE clause that read only `table`, which goes by
/// `qualifier`, with its columns unqualified, as a partial index's condition
/// is written.
pub fn own_conjuncts(table: &Table, qualifier: &str, filter: &Option<Expr>) -> Vec<Expr> {
    let Some(filter) = filter else {
        return Vec::new();
    };
    conjuncts(filter)
        .into_iter()
        .filter_map(|e| own_expr(table, qualifier, e))
        .collect()
}

/// The conditions among the conjuncts of a WHERE clause on `table`, which
/// goes by `qualifier`.
///
/// A row the clause matches satisfies every one, so a scan may skip rows
/// that fail one: through an index, or in a column store's compressed
/// chunks. The whole clause is still evaluated on the rows returned. A
/// comparison with NULL, or of a column with a value of another type, is
/// left to that evaluation, which matches nothing or reports the error. As
/// in any database that reorders predicates, the rest of the clause is then
/// never evaluated on the skipped rows, so an error it would raise only
/// there (`1 / 0`) is not raised; nor is the error of comparing an
/// expression an index has with a value of another type.
///
/// Besides the comparisons, `x IN (v)` of one value is the condition
/// `x = v` (see [`in_lists`] for more values), and a comparison of an INT
/// column plus or minus an INT constant gives one on the column itself:
/// `x + 1 > 5` gives `x > 4`, and `10 - x <= 3` gives `x >= 7`, which an
/// index on `x`, or its chunks, can serve. (The comparison itself stays: it
/// may still serve an index on `x + 1`.)
pub fn conditions(table: &Table, qualifier: &str, filter: &Option<Expr>) -> Vec<Condition> {
    let Some(filter) = filter else {
        return Vec::new();
    };
    let mut conditions = Vec::new();
    for part in conjuncts(filter) {
        if let Some(list) = in_list(table, qualifier, part)
            && let [value] = list.values.as_slice()
        {
            conditions.push(Condition {
                expr: list.expr,
                op: Compare::Eq,
                literal: value.clone(),
            });
            continue;
        }
        let Expr::Binary(left, op, right) = part else {
            continue;
        };
        let op = match op {
            BinaryOp::Eq => Compare::Eq,
            BinaryOp::NotEq => Compare::NotEq,
            BinaryOp::Lt => Compare::Lt,
            BinaryOp::LtEq => Compare::LtEq,
            BinaryOp::Gt => Compare::Gt,
            BinaryOp::GtEq => Compare::GtEq,
            _ => continue,
        };
        let sides = match (constant(left), constant(right)) {
            (None, Some(literal)) => (left, literal, op),
            (Some(literal), None) => (right, literal, op.flip()),
            _ => continue,
        };
        let (expr, literal, op) = sides;
        let Some(expr) = own_expr(table, qualifier, expr) else {
            continue;
        };
        conditions.extend(arithmetic(table, &expr, op, &literal));
        // As a value of the column's type, which an index or a chunk holds:
        // `price > 10` on a REAL column compares with 10.0.
        let literal = match &expr {
            Expr::Column(column) => table
                .column_index(&column.name)
                .ok()
                .and_then(|i| literal.coerce(table.columns[i].data_type)),
            _ => Some(literal),
        };
        if let Some(literal) = literal.filter(|l| !l.is_null()) {
            conditions.push(Condition { expr, op, literal });
        }
    }
    conditions
}

/// The condition on an INT column that `expr op literal` gives, when
/// `expr` is the column plus or minus an INT constant, or that constant
/// minus the column, and moving the constant over cannot overflow.
fn arithmetic(table: &Table, expr: &Expr, op: Compare, literal: &Value) -> Option<Condition> {
    let Expr::Binary(left, arith @ (BinaryOp::Add | BinaryOp::Sub), right) = expr else {
        return None;
    };
    let Value::Int(v) = literal.coerce(DataType::Int)? else {
        return None;
    };
    let (column, op, value) = match (&**left, arith, &**right) {
        // x + k op v, k + x op v: x op v - k
        (Expr::Column(c), BinaryOp::Add, Expr::Literal(Value::Int(k)))
        | (Expr::Literal(Value::Int(k)), BinaryOp::Add, Expr::Column(c)) => {
            (c, op, v.checked_sub(*k)?)
        }
        // x - k op v: x op v + k
        (Expr::Column(c), BinaryOp::Sub, Expr::Literal(Value::Int(k))) => {
            (c, op, v.checked_add(*k)?)
        }
        // k - x op v: x (op turned around) k - v
        (Expr::Literal(Value::Int(k)), BinaryOp::Sub, Expr::Column(c)) => {
            (c, op.flip(), k.checked_sub(v)?)
        }
        _ => return None,
    };
    let at = table.column_index(&column.name).ok()?;
    (table.columns[at].data_type == DataType::Int).then(|| Condition {
        expr: Expr::Column(column.clone()),
        op,
        literal: Value::Int(value),
    })
}

/// A conjunct `expr IN (constants)` of a WHERE clause on `table`, whose
/// expression reads only `table`'s columns, written as [`Condition`]s are.
/// Its values are those of the list but NULL, which matches nothing; as
/// values of the column's type if `expr` is a column; each once, in the
/// order of their keys.
#[derive(Debug, Clone, PartialEq)]
pub struct InList {
    pub expr: Expr,
    pub values: Vec<Value>,
}

/// The conjuncts `expr IN (constants)` of a WHERE clause on `table`, which
/// goes by `qualifier`, of more than one value, for an index to find each
/// value's rows (see [`conditions`] for one value). A list with a value
/// that a column's type has no value equal to is left out, as a comparison
/// would be: the whole clause, evaluated, reports the error.
pub fn in_lists(table: &Table, qualifier: &str, filter: &Option<Expr>) -> Vec<InList> {
    let Some(filter) = filter else {
        return Vec::new();
    };
    conjuncts(filter)
        .into_iter()
        .filter_map(|part| in_list(table, qualifier, part))
        .filter(|list| list.values.len() > 1)
        .collect()
}

fn in_list(table: &Table, qualifier: &str, part: &Expr) -> Option<InList> {
    let Expr::InList(left, items, false) = part else {
        return None;
    };
    let expr = own_expr(table, qualifier, left)?;
    let data_type = match &expr {
        Expr::Column(column) => {
            Some(table.columns[table.column_index(&column.name).ok()?].data_type)
        }
        _ => None,
    };
    let mut keyed: Vec<(Vec<u8>, Value)> = Vec::new();
    for item in items {
        let value = constant(item)?;
        if value.is_null() {
            continue;
        }
        let value = match data_type {
            Some(data_type) => value.coerce(data_type)?,
            None => value,
        };
        let mut key = Vec::new();
        sort_key::encode(&value, false, &mut key);
        keyed.push((key, value));
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.dedup_by(|a, b| a.0 == b.0);
    (!keyed.is_empty()).then(|| InList {
        expr,
        values: keyed.into_iter().map(|(_, v)| v).collect(),
    })
}

/// The conditions `column op literal` among a WHERE clause's (see
/// [`conditions`]), for a column store to evaluate on its compressed chunks.
pub fn predicates(table: &Table, conditions: &[Condition]) -> Vec<Predicate> {
    conditions
        .iter()
        .filter_map(|Condition { expr, op, literal }| {
            let Expr::Column(column) = expr else {
                return None;
            };
            Some(Predicate {
                column: table.column_index(&column.name).ok()?,
                op: *op,
                literal: literal.clone(),
            })
        })
        .collect()
}

/// The expression with its columns unqualified, if it reads columns of
/// `table`, which goes by `qualifier`, and nothing else: no other table's
/// columns, subquery or aggregate.
fn own_expr(table: &Table, qualifier: &str, expr: &Expr) -> Option<Expr> {
    if let Expr::Column(column) = expr {
        return own_column(table, qualifier, column)
            .map(|_| Expr::Column(ColumnRef::bare(&column.name)));
    }
    let mut own = true;
    expr.visit(&mut |e| match e {
        Expr::Column(column) => own &= own_column(table, qualifier, column).is_some(),
        Expr::Outer(_)
        | Expr::Aggregate(..)
        | Expr::Subquery(_)
        | Expr::Exists(_)
        | Expr::InSubquery(..) => own = false,
        _ => {}
    });
    let expr = expr.replace(&mut |e| match e {
        Expr::Column(column) => Some(Ok(Expr::Column(ColumnRef::bare(&column.name)))),
        _ => None,
    });
    own.then_some(expr.expect("replacing columns cannot fail"))
}

/// The value of an expression that reads no column, if it has one.
pub(crate) fn constant(expr: &Expr) -> Option<Value> {
    fn reads_columns(expr: &Expr) -> bool {
        match expr {
            Expr::Literal(_) => false,
            Expr::Column(_) | Expr::Outer(_) | Expr::Slot(..) => true,
            Expr::Unary(_, inner) | Expr::IsNull(inner, _) => reads_columns(inner),
            Expr::InList(left, items, _) => reads_columns(left) || items.iter().any(reads_columns),
            Expr::Aggregate(..)
            | Expr::Window(_)
            | Expr::Subquery(_)
            | Expr::Exists(_)
            | Expr::InSubquery(..) => true,
            Expr::Binary(left, _, right) => reads_columns(left) || reads_columns(right),
            Expr::Case { .. } | Expr::Function(..) => expr.parts().into_iter().any(reads_columns),
        }
    }
    if reads_columns(expr) {
        None
    } else {
        eval(expr, &[], &[]).ok()
    }
}
