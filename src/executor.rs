//! Executes parsed statements against the catalog.

use crate::ast::{BinaryOp, Expr, OrderBy, Projection, Statement, UnaryOp};
use crate::error::{Error, Result, err};
use crate::result::QueryResult;
use crate::catalog::Catalog;
use crate::table::{Row, Table, column_index};
use crate::types::{Column, Value};

pub fn execute(catalog: &mut Catalog, statement: Statement) -> Result<QueryResult> {
    match statement {
        Statement::CreateTable { name, columns, storage } => {
            catalog.create_table(name.clone(), columns, storage)?;
            Ok(message(format!("table {name} created")))
        }
        Statement::DropTable { name } => {
            catalog.drop_table(&name)?;
            Ok(message(format!("table {name} dropped")))
        }
        Statement::Insert { table, columns, rows } => insert(catalog.get_mut(&table)?, columns, rows),
        Statement::Select { table, projection, filter, order_by, limit } => {
            select(catalog.get(&table)?, projection, filter, order_by, limit)
        }
        Statement::Update { table, assignments, filter } => {
            update(catalog.get_mut(&table)?, assignments, filter)
        }
        Statement::Delete { table, filter } => delete(catalog.get_mut(&table)?, filter),
    }
}

fn insert(table: &mut Table, columns: Option<Vec<String>>, rows: Vec<Vec<Expr>>) -> Result<QueryResult> {
    // Which column each supplied value goes to; omitted columns become NULL.
    let targets: Vec<usize> = match columns {
        Some(names) => names.iter().map(|n| table.column_index(n)).collect::<Result<_>>()?,
        None => (0..table.columns.len()).collect(),
    };

    let mut new_rows = Vec::new();
    for exprs in rows {
        if exprs.len() != targets.len() {
            return err(format!("expected {} values, got {}", targets.len(), exprs.len()));
        }
        let mut row = vec![Value::Null; table.columns.len()];
        for (&i, expr) in targets.iter().zip(&exprs) {
            row[i] = eval(expr, &[], &[])?;
        }
        new_rows.push(row);
    }

    let count = new_rows.len();
    table.insert(new_rows)?;
    Ok(message(format!("{count} row(s) inserted")))
}

fn select(
    table: &Table,
    projection: Projection,
    filter: Option<Expr>,
    order_by: Vec<OrderBy>,
    limit: Option<usize>,
) -> Result<QueryResult> {
    let columns = &table.columns;
    let needed = match &projection {
        Projection::All => vec![true; columns.len()],
        Projection::Exprs(exprs) => {
            let order_by = order_by.iter().map(|o| &o.expr);
            needed_columns(columns, exprs.iter().chain(&filter).chain(order_by))
        }
    };

    let mut rows: Vec<Row> = Vec::new();
    for item in table.scan_columns(&needed) {
        let (_, row) = item?;
        if matches(&filter, columns, &row)? {
            rows.push(row);
        }
    }

    if !order_by.is_empty() {
        // Evaluate sort keys up front so errors surface before sorting.
        let mut keyed = rows
            .into_iter()
            .map(|row| {
                let keys = order_by.iter().map(|o| eval(&o.expr, columns, &row));
                Ok((keys.collect::<Result<Vec<_>>>()?, row))
            })
            .collect::<Result<Vec<_>>>()?;
        keyed.sort_by(|(a, _), (b, _)| {
            order_by
                .iter()
                .zip(a.iter().zip(b))
                .map(|(o, (x, y))| if o.desc { y.sort_cmp(x) } else { x.sort_cmp(y) })
                .find(|ord| ord.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        rows = keyed.into_iter().map(|(_, row)| row).collect();
    }

    if let Some(n) = limit {
        rows.truncate(n);
    }

    let (names, rows) = match projection {
        Projection::All => (
            columns.iter().map(|c| c.name.clone()).collect(),
            rows,
        ),
        Projection::Exprs(exprs) => {
            let names = exprs
                .iter()
                .map(|e| match e {
                    Expr::Column(name) => name.clone(),
                    _ => "?column?".into(),
                })
                .collect();
            let rows = rows
                .into_iter()
                .map(|row| exprs.iter().map(|e| eval(e, columns, &row)).collect())
                .collect::<Result<_>>()?;
            (names, rows)
        }
    };
    Ok(QueryResult::Rows { columns: names, rows })
}

fn update(table: &mut Table, assignments: Vec<(String, Expr)>, filter: Option<Expr>) -> Result<QueryResult> {
    let assignments: Vec<(usize, Expr)> = assignments
        .into_iter()
        .map(|(name, expr)| Ok((table.column_index(&name)?, expr)))
        .collect::<Result<_>>()?;

    // Collect every change before writing, so the scan never sees its own updates.
    let mut changes = Vec::new();
    for item in table.scan() {
        let (rid, mut row) = item?;
        if !matches(&filter, &table.columns, &row)? {
            continue;
        }
        // All right-hand sides see the old row values.
        let values: Vec<Value> =
            assignments.iter().map(|(_, e)| eval(e, &table.columns, &row)).collect::<Result<_>>()?;
        for ((i, _), value) in assignments.iter().zip(values) {
            row[*i] = value;
        }
        changes.push((rid, row));
    }

    let count = changes.len();
    table.update(changes)?;
    Ok(message(format!("{count} row(s) updated")))
}

fn delete(table: &mut Table, filter: Option<Expr>) -> Result<QueryResult> {
    let mut rids = Vec::new();
    for item in table.scan_columns(&needed_columns(&table.columns, &filter)) {
        let (rid, row) = item?;
        if matches(&filter, &table.columns, &row)? {
            rids.push(rid);
        }
    }
    table.delete(&rids)?;
    Ok(message(format!("{} row(s) deleted", rids.len())))
}

/// Marks the columns the expressions use, so that a scan may skip the others.
fn needed_columns<'a>(columns: &[Column], exprs: impl IntoIterator<Item = &'a Expr>) -> Vec<bool> {
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

/// Evaluates a WHERE clause; NULL counts as false.
fn matches(filter: &Option<Expr>, columns: &[Column], row: &[Value]) -> Result<bool> {
    let Some(expr) = filter else { return Ok(true) };
    match eval(expr, columns, row)? {
        Value::Bool(b) => Ok(b),
        Value::Null => Ok(false),
        other => err(format!("WHERE clause must be boolean, got {}", other.to_sql())),
    }
}

/// Evaluates an expression against one row.
fn eval(expr: &Expr, columns: &[Column], row: &[Value]) -> Result<Value> {
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Column(name) => Ok(row[column_index(columns, name)?].clone()),
        Expr::IsNull(inner, negated) => Ok(Value::Bool(eval(inner, columns, row)?.is_null() != *negated)),
        Expr::Unary(op, inner) => match (op, eval(inner, columns, row)?) {
            (_, Value::Null) => Ok(Value::Null),
            (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
            (UnaryOp::Neg, Value::Int(n)) => n.checked_neg().map(Value::Int).ok_or_else(overflow),
            (op, value) => err(format!("cannot apply {op:?} to {}", value.to_sql())),
        },
        Expr::Binary(left, op, right) => binary(eval(left, columns, row)?, *op, eval(right, columns, row)?),
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

fn message(text: String) -> QueryResult {
    QueryResult::Message(text)
}
