//! Executes parsed statements against the catalog.

use crate::catalog::Catalog;
use crate::catalog::table::{Row, RowId, Table};
use crate::error::{Result, err};
use crate::plan::{access, filter};
use crate::result::QueryResult;
use crate::sql::ast::{Expr, OrderBy, Projection, Statement};
use crate::sql::eval::{eval, matches};
use crate::types::Value;

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
        Statement::CreateIndex { name, table, column, unique } => {
            catalog.create_index(name.clone(), table, &column, unique)?;
            Ok(message(format!("index {name} created")))
        }
        Statement::DropIndex { name } => {
            catalog.drop_index(&name)?;
            Ok(message(format!("index {name} dropped")))
        }
        Statement::Explain(statement) => explain(catalog, *statement),
        Statement::Checkpoint => {
            catalog.checkpoint()?;
            Ok(message("checkpoint done".into()))
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

/// Describes how a statement would read its table: through which index, if any.
fn explain(catalog: &Catalog, statement: Statement) -> Result<QueryResult> {
    let (table, filter) = match statement {
        Statement::Select { table, filter, .. }
        | Statement::Update { table, filter, .. }
        | Statement::Delete { table, filter } => (table, filter),
        _ => return err("EXPLAIN supports only SELECT, UPDATE and DELETE"),
    };
    let table = catalog.get(&table)?;
    Ok(message(access::explain(table, &filter::predicates(table, &filter))))
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
            filter::needed_columns(columns, exprs.iter().chain(&filter).chain(order_by))
        }
    };

    let mut rows: Vec<Row> = Vec::new();
    for item in scan(table, &needed, &filter) {
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
    for item in scan(table, &vec![true; table.columns.len()], &filter) {
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
    for item in scan(table, &filter::needed_columns(&table.columns, &filter), &filter) {
        let (rid, row) = item?;
        if matches(&filter, &table.columns, &row)? {
            rids.push(rid);
        }
    }
    table.delete(&rids)?;
    Ok(message(format!("{} row(s) deleted", rids.len())))
}

/// Reads the rows of `table` that `filter` may match, the way the planner chooses.
fn scan<'a>(
    table: &'a Table,
    needed: &[bool],
    filter: &Option<Expr>,
) -> Box<dyn Iterator<Item = Result<(RowId, Row)>> + 'a> {
    let predicates = filter::predicates(table, filter);
    let access = access::choose(table, &predicates);
    table.scan_filtered(needed, &predicates, &access)
}

fn message(text: String) -> QueryResult {
    QueryResult::Message(text)
}
