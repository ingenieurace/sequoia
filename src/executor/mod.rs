//! Executes parsed statements against the catalog. A SELECT is planned (see
//! `plan/query.rs`) and run by `operator`s: `sort` orders rows, `aggregate`
//! groups them.

pub mod aggregate;
pub mod join;
pub mod operator;
pub mod sort;

use crate::catalog::Catalog;
use crate::catalog::table::{Row, RowId, Table};
use crate::error::{Result, err};
use crate::plan::query::{self, forbid};
use crate::plan::{access, filter};
use crate::result::QueryResult;
use crate::sql::ast::{Expr, Statement};
use crate::sql::eval::{Field, eval, matches};
use crate::types::Value;
use operator::Rows;
use sort::SORT_MEMORY;

/// Knobs that change how statements run, not what they return.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Bytes a sort may buffer rows in before it writes sorted runs to a
    /// temporary file. Like PostgreSQL's `work_mem`.
    pub sort_memory: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { sort_memory: SORT_MEMORY }
    }
}

pub fn execute(catalog: &mut Catalog, statement: Statement, settings: &Settings) -> Result<QueryResult> {
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
        statement @ Statement::Select { .. } => {
            let query = select(catalog, statement, settings)?;
            let columns = query.columns.clone();
            Ok(QueryResult::Rows { columns, rows: query.collect::<Result<_>>()? })
        }
        Statement::Checkpoint => {
            catalog.checkpoint()?;
            Ok(message("checkpoint done".into()))
        }
        Statement::Insert { table, columns, rows } => insert(catalog.get_mut(&table)?, columns, rows),
        Statement::Update { table, assignments, filter } => {
            update(catalog.get_mut(&table)?, assignments, filter)
        }
        Statement::Delete { table, filter } => delete(catalog.get_mut(&table)?, filter),
    }
}

/// A running SELECT: the names of its columns, and its rows, read one at a time.
pub struct Query<'a> {
    pub columns: Vec<String>,
    rows: Rows<'a>,
}

impl Iterator for Query<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        self.rows.next()
    }
}

/// Plans a SELECT and starts running it; no row is read until asked for.
pub fn select<'a>(catalog: &'a Catalog, statement: Statement, settings: &Settings) -> Result<Query<'a>> {
    let plan = plan_select(catalog, statement)?;
    Ok(Query { columns: plan.columns, rows: operator::open(plan.root, settings) })
}

fn plan_select(catalog: &Catalog, statement: Statement) -> Result<query::Plan<'_>> {
    let Statement::Select { from, projection, filter, group_by, having, order_by, limit } = statement else {
        return err("expected a SELECT");
    };
    let names = std::iter::once(&from.table).chain(from.joins.iter().map(|j| &j.table));
    let tables = names.map(|t| catalog.get(&t.name)).collect::<Result<Vec<_>>>()?;
    query::select(tables, from, projection, filter, group_by, having, order_by, limit)
}

/// Describes how a statement would run: its plan, one step per line, the
/// scan (and the index it uses, if any) last.
fn explain(catalog: &Catalog, statement: Statement) -> Result<QueryResult> {
    let plan = match statement {
        statement @ Statement::Select { .. } => plan_select(catalog, statement)?.root,
        Statement::Update { table, filter, .. } | Statement::Delete { table, filter } => {
            let table = catalog.get(&table)?;
            query::scan(table, &table.name, &filter, vec![true; table.columns.len()])
        }
        _ => return err("EXPLAIN supports only SELECT, UPDATE and DELETE"),
    };
    Ok(message(plan.explain()))
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
        forbid(&exprs, "VALUES")?;
        for (&i, expr) in targets.iter().zip(&exprs) {
            row[i] = eval(expr, &[], &[])?;
        }
        new_rows.push(row);
    }

    let count = new_rows.len();
    table.insert(new_rows)?;
    Ok(message(format!("{count} row(s) inserted")))
}

fn update(table: &mut Table, assignments: Vec<(String, Expr)>, filter: Option<Expr>) -> Result<QueryResult> {
    forbid(&filter, "WHERE")?;
    forbid(assignments.iter().map(|(_, e)| e), "UPDATE")?;
    let assignments: Vec<(usize, Expr)> = assignments
        .into_iter()
        .map(|(name, expr)| Ok((table.column_index(&name)?, expr)))
        .collect::<Result<_>>()?;

    // Collect every change before writing, so the scan never sees its own updates.
    let fields = Field::of_table(&table.name, &table.columns);
    let mut changes = Vec::new();
    for item in scan(table, &vec![true; table.columns.len()], &filter) {
        let (rid, mut row) = item?;
        if !matches(&filter, &fields, &row)? {
            continue;
        }
        // All right-hand sides see the old row values.
        let values: Vec<Value> = assignments.iter().map(|(_, e)| eval(e, &fields, &row)).collect::<Result<_>>()?;
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
    forbid(&filter, "WHERE")?;
    let fields = Field::of_table(&table.name, &table.columns);
    let mut rids = Vec::new();
    for item in scan(table, &filter::needed_columns(table, &table.name, &filter), &filter) {
        let (rid, row) = item?;
        if matches(&filter, &fields, &row)? {
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
    let predicates = filter::predicates(table, &table.name, filter);
    let access = access::choose(table, &predicates);
    table.scan_filtered(needed, &predicates, &access)
}

fn message(text: String) -> QueryResult {
    QueryResult::Message(text)
}
