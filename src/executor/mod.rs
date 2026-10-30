//! Executes parsed statements against the catalog. A SELECT is planned (see
//! `plan/query.rs`) and run in one of three execution models (see
//! `model.rs`): `operator`s pass rows on one at a time, `materialize` runs
//! each step to completion, `vectorized` passes batches of columns. `sort`
//! orders rows, `aggregate` groups them, and `subquery` runs the subqueries
//! of their expressions.

pub mod aggregate;
mod gather;
pub mod joins;
mod materialize;
pub mod model;
pub mod operator;
pub mod sort;
pub mod spill;
pub mod subquery;
pub mod vectorized;
mod window;

use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::catalog::Catalog;
use crate::catalog::table::{Row, RowId, Table};
use crate::error::{Result, err};
use crate::mvcc::Snapshot;
use crate::plan::query::{self, Choices, ForceJoin, forbid};
use crate::plan::{access, filter};
use crate::result::QueryResult;
use crate::sql::ast::{ColumnRef, Expr, InsertSource, Projection, Select, SelectItem, Statement};
use crate::datetime;
use crate::sql::eval::{Field, at_time, eval_with, locate, matches_with};
use crate::types::Value;
use joins::{HASH_MEMORY, JOIN_MEMORY};
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
    /// The method every join is made to use; by default, none: the planner
    /// picks each (see [`ForceJoin`]).
    pub force_join: Option<ForceJoin>,
    /// Threads a hash join runs on; by default one. More makes each hash
    /// join that need not keep its order a parallel grace hash join (see
    /// `joins/parallel.rs`), whose workers share `hash_memory`.
    pub join_workers: usize,
    /// Threads a scan of a heap runs on, with the filters and projection
    /// above it; by default one. More gathers it from that many (see
    /// `gather.rs`): its rows come out in no order.
    pub scan_workers: usize,
    /// Which versions of rows the statement running reads, and the id it
    /// writes with (see `mvcc/snapshot.rs`). Not a knob: the database sets
    /// it for each statement.
    pub snapshot: Option<Snapshot>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            sort_memory: SORT_MEMORY,
            join_memory: JOIN_MEMORY,
            hash_memory: HASH_MEMORY,
            model: None,
            force_join: None,
            join_workers: 1,
            scan_workers: 1,
            snapshot: None,
        }
    }
}

impl Settings {
    /// What the planner is asked to choose by these settings.
    pub fn choices(&self) -> Choices {
        Choices {
            force_join: self.force_join,
            join_workers: self.join_workers,
            scan_workers: self.scan_workers,
        }
    }

    /// The snapshot of the statement running.
    pub fn snapshot(&self) -> &Snapshot {
        self.snapshot
            .as_ref()
            .expect("the database gives every statement a snapshot")
    }

    /// These settings, for a statement that runs under `snapshot`.
    pub fn under(&self, snapshot: Snapshot) -> Settings {
        Settings {
            snapshot: Some(snapshot),
            ..self.clone()
        }
    }
}

/// Whether a statement only reads: SELECT and EXPLAIN, which
/// [`execute_read`] runs on a catalog it cannot change.
pub fn reads_only(statement: &Statement) -> bool {
    matches!(statement, Statement::Select(_) | Statement::Explain { .. })
}

/// Whether a statement changes the catalog, whose rows have no versions:
/// it needs the database to itself until its transaction ends (see
/// `mvcc/mod.rs`).
pub fn changes_schema(statement: &Statement) -> bool {
    matches!(
        statement,
        Statement::CreateTable { .. }
            | Statement::DropTable { .. }
            | Statement::AlterTable { .. }
            | Statement::CreateIndex(_)
            | Statement::DropIndex { .. }
            | Statement::Analyze { .. }
    )
}

/// Runs a statement that only reads (see [`reads_only`]).
pub fn execute_read(
    catalog: &Catalog,
    statement: Statement,
    settings: &Settings,
) -> Result<QueryResult> {
    match at_time(statement, datetime::now())? {
        Statement::Explain {
            statement,
            analyze: false,
        } => explain(catalog, *statement, settings),
        Statement::Explain {
            statement,
            analyze: true,
        } => match *statement {
            Statement::Select(query) => explain_analyze(catalog, *query, settings),
            _ => err("EXPLAIN ANALYZE supports only SELECT"),
        },
        Statement::Select(query) => {
            let query = select(catalog, *query, settings)?;
            let columns = query.columns.clone();
            Ok(QueryResult::Rows {
                columns,
                rows: query.collect::<Result<_>>()?,
            })
        }
        _ => unreachable!("a statement that changes nothing"),
    }
}

pub fn execute(
    catalog: &mut Catalog,
    statement: Statement,
    settings: &Settings,
) -> Result<QueryResult> {
    if reads_only(&statement) {
        return execute_read(catalog, statement, settings);
    }
    // The current time, as the statement starts: the same for all of it.
    let now = datetime::now();
    match at_time(statement, now)? {
        Statement::CreateTable {
            name,
            columns,
            constraints,
            storage,
        } => {
            catalog.create_table(
                name.clone(),
                columns,
                constraints,
                storage,
                settings.snapshot(),
            )?;
            Ok(message(format!("table {name} created")))
        }
        Statement::DropTable { name } => {
            catalog.drop_table(&name)?;
            Ok(message(format!("table {name} dropped")))
        }
        Statement::AlterTable { table, action } => {
            catalog.alter_table(&table, action, settings.snapshot(), now)?;
            Ok(message(format!("table {table} altered")))
        }
        Statement::CreateIndex(definition) => {
            let name = definition.name.clone();
            catalog.create_index(definition, settings.snapshot())?;
            Ok(message(format!("index {name} created")))
        }
        Statement::DropIndex { name } => {
            catalog.drop_index(&name)?;
            Ok(message(format!("index {name} dropped")))
        }
        Statement::Select(_) | Statement::Explain { .. } => unreachable!("run above"),
        Statement::Checkpoint => {
            catalog.checkpoint()?;
            Ok(message("checkpoint done".into()))
        }
        Statement::Analyze { table: Some(name) } => {
            catalog.analyze(&name, settings.snapshot())?;
            Ok(message(format!("table {name} analyzed")))
        }
        Statement::Analyze { table: None } => {
            let names: Vec<String> = catalog
                .table_names()
                .into_iter()
                .map(String::from)
                .collect();
            for name in &names {
                catalog.analyze(name, settings.snapshot())?;
            }
            let s = if names.len() == 1 { "" } else { "s" };
            Ok(message(format!("{} table{s} analyzed", names.len())))
        }
        Statement::Vacuum { table } => {
            let removed = catalog.vacuum(table.as_deref())?;
            let s = if removed == 1 { "" } else { "s" };
            Ok(message(format!("{removed} dead row version{s} removed")))
        }
        Statement::Begin
        | Statement::Commit
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::Release { .. } => err("transaction statements are run by the database"),
        Statement::Insert {
            table,
            columns,
            source,
            returning,
        } => insert(catalog, &table, columns, source, returning, settings, now),
        Statement::Update {
            table,
            assignments,
            filter,
            returning,
        } => update(
            catalog,
            &table,
            assignments,
            filter,
            returning,
            settings,
            now,
        ),
        Statement::Delete {
            table,
            filter,
            returning,
        } => delete(catalog, &table, filter, returning, settings),
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
    let Statement::Select(query) = at_time(Statement::Select(Box::new(query)), datetime::now())?
    else {
        unreachable!("a SELECT stays one")
    };
    let query = *query;
    let plan = query::select(catalog, query, settings.choices())?;
    Ok(Query {
        columns: plan.columns,
        rows: model::open(plan.root, catalog, settings),
    })
}

/// Describes how a statement would run: its plan, one step per line, the
/// scan (and the index it uses, if any) last.
fn explain(catalog: &Catalog, statement: Statement, settings: &Settings) -> Result<QueryResult> {
    let plan = match statement {
        Statement::Select(query) => query::select(catalog, *query, settings.choices())?.root,
        Statement::Update { table, filter, .. } | Statement::Delete { table, filter, .. } => {
            let table = catalog.get(&table)?;
            let filter = filter
                .map(|f| query::bind(catalog, Some(table), &f))
                .transpose()?;
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
/// ran in and why; after the steps, a line for each column table read says
/// how many of the row groups it looked at its zone maps let it skip; a last
/// line gives the query's rows and time.
fn explain_analyze(catalog: &Catalog, query: Select, settings: &Settings) -> Result<QueryResult> {
    let root = query::select(catalog, query, settings.choices())?.root;
    let lines = root.explain_lines();
    // What the planner expected of each step, in the order of the lines,
    // and whether it is what each lookup of an index join finds.
    fn estimates(node: &query::Node, lookup: bool, out: &mut Vec<(f64, bool)>) {
        out.push((query::estimate(node), lookup));
        let index = matches!(
            node,
            query::Node::Join {
                method: query::JoinMethod::Index,
                ..
            }
        );
        for (i, input) in node.inputs().into_iter().enumerate() {
            estimates(input, lookup || (index && i == 1), out);
        }
    }
    let mut expected = Vec::new();
    estimates(&root, false, &mut expected);
    let (model, why) = model::choose(&root, settings);
    let profile = Rc::new(Profile::new(lines.len()));
    let zones = |catalog: &Catalog| -> Vec<(String, (u64, u64))> {
        let tables = catalog
            .table_names()
            .into_iter()
            .filter_map(|name| catalog.get(name).ok());
        tables
            .filter_map(|table| Some((table.name.clone(), table.zone_stats()?)))
            .collect()
    };
    let before = zones(catalog);
    let start = Instant::now();
    let mut returned = 0;
    for row in model::open_in(model, root, catalog, settings, Some((profile.clone(), 0))) {
        row?;
        returned += 1;
    }
    let time = start.elapsed();
    let after = zones(catalog);

    let mut out = vec![format!("MODEL {model}  ({why})")];
    out.extend(lines.into_iter().zip(profile.counts()).zip(expected).map(
        |((line, count), (expected, lookup))| {
            let each = match (lookup, count.runs > 1) {
                (true, _) => " a lookup",
                (false, true) => " a run",
                (false, false) => "",
            };
            let expected = format!("estimated {}{each}", about(expected));
            let did = match count {
                StepCount { inside: true, .. } => "counted with the step above".to_string(),
                StepCount { runs: 0, .. } => "never run".to_string(),
                StepCount {
                    runs: 1,
                    rows,
                    time,
                    ..
                } => format!("{}, {}", rows_of(rows), ms(time)),
                StepCount {
                    runs, rows, time, ..
                } => format!("{} in {runs} runs, {}", rows_of(rows), ms(time)),
            };
            format!("{line}  ({did}; {expected})")
        },
    ));
    for ((table, (read, skipped)), (_, (read_before, skipped_before))) in
        after.into_iter().zip(before)
    {
        let (read, skipped) = (read - read_before, skipped - skipped_before);
        if read + skipped > 0 {
            out.push(format!(
                "ZONE MAPS {table}  ({skipped} of {} row groups skipped)",
                read + skipped
            ));
        }
    }
    out.push(format!("{} in {}", rows_of(returned), ms(time)));
    Ok(message(out.join("\n")))
}

/// An estimate of rows, whole, or `<1` for less than half of one.
fn about(rows: f64) -> String {
    match rows {
        r if r > 0.0 && r < 0.5 => "<1".into(),
        r => format!("{r:.0}"),
    }
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

/// Inserts the rows of VALUES, or of a query: all of its rows are read
/// before the first is inserted, so a query of the table never reads the
/// rows it adds.
fn insert(
    catalog: &mut Catalog,
    table: &str,
    columns: Option<Vec<String>>,
    source: InsertSource,
    returning: Option<Projection>,
    settings: &Settings,
    now: i64,
) -> Result<QueryResult> {
    let (new_rows, returned) = {
        let catalog: &Catalog = catalog;
        let table = catalog.get(table)?;
        let returning = Returning::bind(catalog, table, returning)?;
        // Omitted columns, and those given DEFAULT, take their defaults, the
        // same for every row: NULL for a column without one.
        let defaults = table.defaults(now)?;
        // Which column each supplied value goes to.
        let targets: Vec<usize> = match columns {
            Some(names) => names
                .iter()
                .map(|n| table.column_index(n))
                .collect::<Result<_>>()?,
            None => (0..table.columns.len()).collect(),
        };
        let mut subqueries = Runner::new(catalog, settings);
        let mut new_rows = Vec::new();
        let rows = match source {
            InsertSource::Values(rows) => rows,
            InsertSource::DefaultValues => {
                new_rows.push(defaults.clone());
                Vec::new()
            }
            InsertSource::Select(query) => {
                let query = select(catalog, *query, settings)?;
                if query.columns.len() != targets.len() {
                    return err(format!(
                        "expected {} values, got {} from the query",
                        targets.len(),
                        query.columns.len()
                    ));
                }
                for values in query {
                    let mut row = defaults.clone();
                    for (&i, value) in targets.iter().zip(values?) {
                        row[i] = value;
                    }
                    new_rows.push(row);
                }
                Vec::new()
            }
        };
        for exprs in rows {
            if exprs.len() != targets.len() {
                return err(format!(
                    "expected {} values, got {}",
                    targets.len(),
                    exprs.len()
                ));
            }
            let mut row = defaults.clone();
            forbid(exprs.iter().flatten(), "VALUES")?;
            for (&i, expr) in targets.iter().zip(&exprs) {
                if let Some(expr) = expr {
                    row[i] = eval_with(
                        &query::bind(catalog, None, expr)?,
                        &[],
                        &[],
                        &mut subqueries,
                    )?;
                }
            }
            new_rows.push(row);
        }
        let returned = returning
            .map(|returning| -> Result<_> { Ok((returning, table.coerce_all(new_rows.clone())?)) })
            .transpose()?;
        (new_rows, returned)
    };

    let count = new_rows.len();
    catalog
        .get_mut(table)?
        .insert(settings.snapshot(), new_rows)?;
    match returned {
        Some((returning, rows)) => returning.rows(catalog, &rows, settings),
        None => Ok(message(format!("{count} row(s) inserted"))),
    }
}

fn update(
    catalog: &mut Catalog,
    table: &str,
    assignments: Vec<(String, Option<Expr>)>,
    filter: Option<Expr>,
    returning: Option<Projection>,
    settings: &Settings,
    now: i64,
) -> Result<QueryResult> {
    forbid(&filter, "WHERE")?;
    forbid(assignments.iter().filter_map(|(_, e)| e.as_ref()), "UPDATE")?;
    let (changes, returned) = {
        let catalog: &Catalog = catalog;
        let table = catalog.get(table)?;
        let returning = Returning::bind(catalog, table, returning)?;
        let filter = filter
            .map(|f| query::bind(catalog, Some(table), &f))
            .transpose()?;
        // `SET column = DEFAULT` sets the default, the same for every row.
        let defaults = match assignments.iter().any(|(_, e)| e.is_none()) {
            true => table.defaults(now)?,
            false => Vec::new(),
        };
        let assignments: Vec<(usize, Expr)> = assignments
            .into_iter()
            .map(|(name, expr)| {
                let i = table.column_index(&name)?;
                Ok(match expr {
                    Some(expr) => (i, query::bind(catalog, Some(table), &expr)?),
                    None => (i, Expr::Literal(defaults[i].clone())),
                })
            })
            .collect::<Result<_>>()?;

        // Collect every change before writing, so the scan never sees its own
        // updates, and subqueries see the table as it was.
        let fields = Field::of_table(&table.name, &table.columns);
        // Found once among the table's columns, not by name for each row.
        let located = filter.as_ref().map(|f| locate(f, &fields));
        let assignments: Vec<(usize, Expr)> = assignments
            .iter()
            .map(|(i, e)| (*i, locate(e, &fields)))
            .collect();
        let mut subqueries = Runner::new(catalog, settings);
        let mut changes = Vec::new();
        for item in scan(
            table,
            settings.snapshot(),
            &vec![true; table.columns.len()],
            &filter,
        ) {
            let (rid, old) = item?;
            if !matches_with(&located, &fields, &old, &mut subqueries)? {
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
        let returned = returning
            .map(|returning| -> Result<_> {
                let new_rows = changes.iter().map(|(_, _, new)| new.clone()).collect();
                Ok((returning, table.coerce_all(new_rows)?))
            })
            .transpose()?;
        (changes, returned)
    };

    let count = changes.len();
    catalog
        .get_mut(table)?
        .update(settings.snapshot(), changes)?;
    match returned {
        Some((returning, rows)) => returning.rows(catalog, &rows, settings),
        None => Ok(message(format!("{count} row(s) updated"))),
    }
}

fn delete(
    catalog: &mut Catalog,
    table: &str,
    filter: Option<Expr>,
    returning: Option<Projection>,
    settings: &Settings,
) -> Result<QueryResult> {
    forbid(&filter, "WHERE")?;
    let (rows, returning) = {
        let catalog: &Catalog = catalog;
        let table = catalog.get(table)?;
        let returning = Returning::bind(catalog, table, returning)?;
        let filter = filter
            .map(|f| query::bind(catalog, Some(table), &f))
            .transpose()?;
        let fields = Field::of_table(&table.name, &table.columns);
        let mut subqueries = Runner::new(catalog, settings);
        // The filter's columns, and the indexed ones, whose entries the delete
        // removes; every column if RETURNING may show them.
        let mut needed = match returning {
            Some(_) => vec![true; table.columns.len()],
            None => filter::needed_columns(table, &table.name, &filter),
        };
        needed
            .iter_mut()
            .zip(table.indexed_columns())
            .for_each(|(needed, indexed)| *needed |= indexed);
        let located = filter.as_ref().map(|f| locate(f, &fields));
        let mut rows = Vec::new();
        for item in scan(table, settings.snapshot(), &needed, &filter) {
            let (rid, row) = item?;
            if matches_with(&located, &fields, &row, &mut subqueries)? {
                rows.push((rid, row));
            }
        }
        (rows, returning)
    };
    catalog.get_mut(table)?.delete(settings.snapshot(), &rows)?;
    match returning {
        Some(returning) => {
            let rows: Vec<Row> = rows.into_iter().map(|(_, row)| row).collect();
            returning.rows(catalog, &rows, settings)
        }
        None => Ok(message(format!("{} row(s) deleted", rows.len()))),
    }
}

/// The RETURNING list of an INSERT, UPDATE or DELETE, bound to its table:
/// what it gives for each row written, as it was stored (or for DELETE, as
/// it was before).
struct Returning {
    /// The names of its columns, as a select list's are named.
    columns: Vec<String>,
    exprs: Vec<Expr>,
    /// The table's columns, which the expressions read.
    fields: Vec<Field>,
}

impl Returning {
    /// Binds a RETURNING list to `table`, if there is one. `*` is every column of the table.
    fn bind(
        catalog: &Catalog,
        table: &Table,
        projection: Option<Projection>,
    ) -> Result<Option<Returning>> {
        let items = match projection {
            None => return Ok(None),
            Some(Projection::All) => table
                .columns
                .iter()
                .map(|column| SelectItem {
                    expr: Expr::Column(ColumnRef::bare(column.name.clone())),
                    alias: None,
                })
                .collect(),
            Some(Projection::Exprs(items)) => items,
        };
        forbid(items.iter().map(|item| &item.expr), "RETURNING")?;
        if items.iter().any(|item| item.expr.has_subquery()) {
            return err("subqueries are not allowed in RETURNING");
        }
        let columns = items.iter().map(query::item_name).collect();
        let fields = Field::of_table(&table.name, &table.columns);
        let exprs = items
            .iter()
            .map(|item| {
                Ok(locate(
                    &query::bind(catalog, Some(table), &item.expr)?,
                    &fields,
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Some(Returning {
            columns,
            exprs,
            fields,
        }))
    }

    /// The rows it gives for these rows of its table, in their order.
    fn rows(self, catalog: &Catalog, rows: &[Row], settings: &Settings) -> Result<QueryResult> {
        let mut subqueries = Runner::new(catalog, settings);
        let rows = rows
            .iter()
            .map(|row| {
                self.exprs
                    .iter()
                    .map(|e| eval_with(e, &self.fields, row, &mut subqueries))
                    .collect()
            })
            .collect::<Result<_>>()?;
        Ok(QueryResult::Rows {
            columns: self.columns,
            rows,
        })
    }
}

/// Reads the rows of `table` that `filter` may match and `snapshot` sees,
/// the way the planner chooses.
fn scan<'a>(
    table: &'a Table,
    snapshot: &Snapshot,
    needed: &[bool],
    filter: &Option<Expr>,
) -> Box<dyn Iterator<Item = Result<(RowId, Row)>> + 'a> {
    let conditions = filter::conditions(table, &table.name, filter);
    let predicates = filter::predicates(table, &conditions);
    let access = access::choose(table, &table.name, filter, &conditions, needed);
    table.scan_filtered(snapshot, needed, &predicates, &access)
}

fn message(text: String) -> QueryResult {
    QueryResult::Message(text)
}
