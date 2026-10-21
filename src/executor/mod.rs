//! Executes parsed statements against the catalog. A SELECT is planned (see
//! `plan/query.rs`) and run in one of three execution models (see
//! `model.rs`): `operator`s pass rows on one at a time, `materialize` runs
//! each step to completion, `vectorized` passes batches of columns. `sort`
//! orders rows, `aggregate` groups them, and `subquery` runs the subqueries
//! of their expressions.

pub mod aggregate;
pub mod hash_join;
pub mod join;
mod materialize;
pub mod model;
pub mod operator;
pub mod sort;
pub mod spill;
pub mod subquery;
pub mod vectorized;

use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::catalog::Catalog;
use crate::catalog::table::{Row, RowId, Table};
use crate::error::{Result, err};
use crate::plan::query::{self, forbid};
use crate::plan::{access, filter};
use crate::result::QueryResult;
use crate::sql::ast::{Expr, Select, Statement};
use crate::sql::eval::{Field, eval_with, matches_with};
use crate::types::Value;
use join::{HASH_MEMORY, JOIN_MEMORY};
use model::Model;
use operator::{Profile, Rows, StepCount};
use sort::SORT_MEMORY;
use subquery::Runner;

/// Knobs that change how statements run, not what they return.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Bytes a sort may buffer rows in before it writes sorted runs to a
    /// temporary file. Like PostgreSQL's `work_mem`.
    pub sort_memory: usize,
    /// Bytes of left rows a block nested loop join may hold at once; it reads
    /// its right side once for each block of them.
    pub join_memory: usize,
    /// Bytes of right rows a hash join, or of groups a hash aggregation, may
    /// hold before it writes partitions of them to temporary files.
    pub hash_memory: usize,
    /// The execution model every query runs in; by default, none: each
    /// query runs in the one that suits it (see [`model::choose`]).
    pub model: Option<Model>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { sort_memory: SORT_MEMORY, join_memory: JOIN_MEMORY, hash_memory: HASH_MEMORY, model: None }
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
        Statement::CreateIndex(definition) => {
            let name = definition.name.clone();
            catalog.create_index(definition)?;
            Ok(message(format!("index {name} created")))
        }
        Statement::DropIndex { name } => {
            catalog.drop_index(&name)?;
            Ok(message(format!("index {name} dropped")))
        }
        Statement::Explain { statement, analyze: false } => explain(catalog, *statement),
        Statement::Explain { statement, analyze: true } => match *statement {
            Statement::Select(query) => explain_analyze(catalog, query, settings),
            _ => err("EXPLAIN ANALYZE supports only SELECT"),
        },
        Statement::Select(query) => {
            let query = select(catalog, query, settings)?;
            let columns = query.columns.clone();
            Ok(QueryResult::Rows { columns, rows: query.collect::<Result<_>>()? })
        }
        Statement::Checkpoint => {
            catalog.checkpoint()?;
            Ok(message("checkpoint done".into()))
        }
        Statement::Begin
        | Statement::Commit
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::Release { .. } => err("transaction statements are run by the database"),
        Statement::Insert { table, columns, rows } => insert(catalog, &table, columns, rows, settings),
        Statement::Update { table, assignments, filter } => update(catalog, &table, assignments, filter, settings),
        Statement::Delete { table, filter } => delete(catalog, &table, filter, settings),
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
pub fn select<'a>(catalog: &'a Catalog, query: Select, settings: &Settings) -> Result<Query<'a>> {
    let plan = query::select(catalog, query)?;
    Ok(Query { columns: plan.columns, rows: model::open(plan.root, catalog, settings) })
}

/// Describes how a statement would run: its plan, one step per line, the
/// scan (and the index it uses, if any) last.
fn explain(catalog: &Catalog, statement: Statement) -> Result<QueryResult> {
    let plan = match statement {
        Statement::Select(query) => query::select(catalog, query)?.root,
        Statement::Update { table, filter, .. } | Statement::Delete { table, filter } => {
            let table = catalog.get(&table)?;
            let filter = filter.map(|f| query::bind(catalog, Some(table), &f)).transpose()?;
            query::scan(table, &table.name, &filter, vec![true; table.columns.len()])
        }
        _ => return err("EXPLAIN supports only SELECT, UPDATE and DELETE"),
    };
    Ok(message(plan.explain()))
}

/// Runs a SELECT, reading all of its rows, and describes its plan as
/// EXPLAIN does, each step followed by what it did: the rows it returned and
/// the time it took, the time of the steps below it included (and, if it
/// ran more than once, how often). A first line gives the execution model it
/// ran in and why, a last line the query's rows and time.
fn explain_analyze(catalog: &Catalog, query: Select, settings: &Settings) -> Result<QueryResult> {
    let root = query::select(catalog, query)?.root;
    let lines = root.explain_lines();
    let (model, why) = model::choose(&root, settings);
    let profile = Rc::new(Profile::new(lines.len()));
    let start = Instant::now();
    let mut returned = 0;
    for row in model::open_in(model, root, catalog, settings, Some((profile.clone(), 0))) {
        row?;
        returned += 1;
    }
    let time = start.elapsed();

    let mut out = vec![format!("MODEL {model}  ({why})")];
    out.extend(lines
        .into_iter()
        .zip(profile.counts())
        .map(|(line, count)| {
            let did = match count {
                StepCount { inside: true, .. } => "counted with the join above".to_string(),
                StepCount { runs: 0, .. } => "never run".to_string(),
                StepCount { runs: 1, rows, time, .. } => format!("{}, {}", rows_of(rows), ms(time)),
                StepCount { runs, rows, time, .. } => format!("{} in {runs} runs, {}", rows_of(rows), ms(time)),
            };
            format!("{line}  ({did})")
        }));
    out.push(format!("{} in {}", rows_of(returned), ms(time)));
    Ok(message(out.join("\n")))
}

fn rows_of(n: u64) -> String {
    format!("{n} row{}", if n == 1 { "" } else { "s" })
}

/// A time in milliseconds, with more decimals under one.
fn ms(time: Duration) -> String {
    match time.as_secs_f64() * 1000.0 {
        ms if ms < 1.0 => format!("{ms:.3} ms"),
        ms => format!("{ms:.1} ms"),
    }
}

fn insert(
    catalog: &mut Catalog,
    table: &str,
    columns: Option<Vec<String>>,
    rows: Vec<Vec<Expr>>,
    settings: &Settings,
) -> Result<QueryResult> {
    let new_rows = {
        let catalog: &Catalog = catalog;
        let table = catalog.get(table)?;
        // Which column each supplied value goes to; omitted columns become NULL.
        let targets: Vec<usize> = match columns {
            Some(names) => names.iter().map(|n| table.column_index(n)).collect::<Result<_>>()?,
            None => (0..table.columns.len()).collect(),
        };
        let mut subqueries = Runner::new(catalog, settings);
        let mut new_rows = Vec::new();
        for exprs in rows {
            if exprs.len() != targets.len() {
                return err(format!("expected {} values, got {}", targets.len(), exprs.len()));
            }
            let mut row = vec![Value::Null; table.columns.len()];
            forbid(&exprs, "VALUES")?;
            for (&i, expr) in targets.iter().zip(&exprs) {
                row[i] = eval_with(&query::bind(catalog, None, expr)?, &[], &[], &mut subqueries)?;
            }
            new_rows.push(row);
        }
        new_rows
    };

    let count = new_rows.len();
    catalog.get_mut(table)?.insert(new_rows)?;
    Ok(message(format!("{count} row(s) inserted")))
}

fn update(
    catalog: &mut Catalog,
    table: &str,
    assignments: Vec<(String, Expr)>,
    filter: Option<Expr>,
    settings: &Settings,
) -> Result<QueryResult> {
    forbid(&filter, "WHERE")?;
    forbid(assignments.iter().map(|(_, e)| e), "UPDATE")?;
    let changes = {
        let catalog: &Catalog = catalog;
        let table = catalog.get(table)?;
        let filter = filter.map(|f| query::bind(catalog, Some(table), &f)).transpose()?;
        let assignments: Vec<(usize, Expr)> = assignments
            .into_iter()
            .map(|(name, expr)| Ok((table.column_index(&name)?, query::bind(catalog, Some(table), &expr)?)))
            .collect::<Result<_>>()?;

        // Collect every change before writing, so the scan never sees its own
        // updates, and subqueries see the table as it was.
        let fields = Field::of_table(&table.name, &table.columns);
        let mut subqueries = Runner::new(catalog, settings);
        let mut changes = Vec::new();
        for item in scan(table, &vec![true; table.columns.len()], &filter) {
            let (rid, old) = item?;
            if !matches_with(&filter, &fields, &old, &mut subqueries)? {
                continue;
            }
            // All right-hand sides see the old row values.
            let values: Vec<Value> = assignments
                .iter()
                .map(|(_, e)| eval_with(e, &fields, &old, &mut subqueries))
                .collect::<Result<_>>()?;
            let mut row = old.clone();
            for ((i, _), value) in assignments.iter().zip(values) {
                row[*i] = value;
            }
            changes.push((rid, old, row));
        }
        changes
    };

    let count = changes.len();
    catalog.get_mut(table)?.update(changes)?;
    Ok(message(format!("{count} row(s) updated")))
}

fn delete(catalog: &mut Catalog, table: &str, filter: Option<Expr>, settings: &Settings) -> Result<QueryResult> {
    forbid(&filter, "WHERE")?;
    let rows = {
        let catalog: &Catalog = catalog;
        let table = catalog.get(table)?;
        let filter = filter.map(|f| query::bind(catalog, Some(table), &f)).transpose()?;
        let fields = Field::of_table(&table.name, &table.columns);
        let mut subqueries = Runner::new(catalog, settings);
        // The filter's columns, and the indexed ones, whose entries the delete removes.
        let mut needed = filter::needed_columns(table, &table.name, &filter);
        needed.iter_mut().zip(table.indexed_columns()).for_each(|(needed, indexed)| *needed |= indexed);
        let mut rows = Vec::new();
        for item in scan(table, &needed, &filter) {
            let (rid, row) = item?;
            if matches_with(&filter, &fields, &row, &mut subqueries)? {
                rows.push((rid, row));
            }
        }
        rows
    };
    catalog.get_mut(table)?.delete(&rows)?;
    Ok(message(format!("{} row(s) deleted", rows.len())))
}

/// Reads the rows of `table` that `filter` may match, the way the planner chooses.
fn scan<'a>(
    table: &'a Table,
    needed: &[bool],
    filter: &Option<Expr>,
) -> Box<dyn Iterator<Item = Result<(RowId, Row)>> + 'a> {
    let conditions = filter::conditions(table, &table.name, filter);
    let predicates = filter::predicates(table, &conditions);
    let access = access::choose(table, &table.name, filter, &conditions, needed);
    table.scan_filtered(needed, &predicates, &access)
}

fn message(text: String) -> QueryResult {
    QueryResult::Message(text)
}
