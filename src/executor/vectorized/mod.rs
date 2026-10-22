//! The vectorized model: each step's `next` returns a batch of rows, stored
//! a column at a time (see `batch.rs`), rather than one row.
//!
//! ```text
//! rows    <- one at a time, made from the batches only here, at the top
//! PROJECT    a vector per expression, a loop each (see `expr.rs`)
//! FILTER     narrows the batch's selection; copies nothing
//! SCAN       a column table's row groups as they are decoded, column by column;
//!            other tables' rows, BATCH_SIZE at a time, taken apart into columns
//! ```
//!
//! What a step does once per row in the iterator model (a call down the
//! tree, an expression walked, a `Value` matched) it does once per batch
//! here, and the work per row is a loop over arrays.
//! An aggregate without `GROUP BY` folds a whole column into each
//! aggregate at once (see `aggregate.rs`).
//!
//! Joins and sorts have no batch algorithms of their own: they run the
//! iterator model's (see `operator.rs`) on their inputs' rows, and their
//! rows are put into batches again for the steps above.
//!
//! **Errors.** A step that fails on a batch evaluates it again a row at a
//! time, with `sql::eval`, which fails on the same row, with the same error,
//! as the iterator model. It returns the rows before that one as a batch,
//! then the error: so a LIMIT above that has its rows by then ends the query
//! without it, as it would in the iterator model. The rows a scan reads
//! ahead, past those a LIMIT takes, are evaluated too, but none of their
//! errors is reported.

pub mod batch;
mod expr;

use std::collections::HashSet;
use std::rc::Rc;
use std::time::Instant;

use super::Settings;
use super::aggregate::{self, Folder, Groups};
use super::operator::{self, Profile, Rows, Step, below, deferred};
use super::subquery::Runner;
use crate::catalog::Catalog;
use crate::catalog::table::{Access, Row, Table};
use crate::error::{Error, Result};
use crate::plan::query::{AggregateCall, Node, Phase};
use crate::predicate::Predicate;
use crate::sql::ast::Expr;
use crate::sql::eval::{Field, NoSubqueries, eval_with, locate, locate_all, matches_with};
use crate::types::Value;
use batch::{BATCH_SIZE, Batch, Vector};

/// Batches produced by a step, or the error that stopped it.
pub type Batches<'a> = Box<dyn Iterator<Item = Result<Batch>> + 'a>;

/// Opens a plan step and everything below it, its steps counted in `step`
/// if given. No row is read until asked for.
pub(super) fn open<'a>(
    node: Node<'a>,
    catalog: &'a Catalog,
    settings: &Settings,
    step: Step,
) -> Rows<'a> {
    rows_of(open_step(node, catalog, settings, step))
}

fn open_step<'a>(
    node: Node<'a>,
    catalog: &'a Catalog,
    settings: &Settings,
    step: Step,
) -> Batches<'a> {
    let Some((profile, id)) = step else {
        return open_node(node, catalog, settings, &None);
    };
    let start = Instant::now();
    let batches = open_node(node, catalog, settings, &Some((profile.clone(), id)));
    profile.add(id, 1, 0, start.elapsed());
    Box::new(Profiled {
        batches,
        profile,
        id,
    })
}

/// A step's batches, their rows counted as they are asked for.
struct Profiled<'a> {
    batches: Batches<'a>,
    profile: Rc<Profile>,
    id: usize,
}

impl Iterator for Profiled<'_> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = Instant::now();
        let item = self.batches.next();
        let rows = item
            .as_ref()
            .and_then(|item| item.as_ref().ok())
            .map_or(0, Batch::rows);
        self.profile.add(self.id, 0, rows as u64, start.elapsed());
        item
    }
}

fn open_node<'a>(
    node: Node<'a>,
    catalog: &'a Catalog,
    settings: &Settings,
    step: &Step,
) -> Batches<'a> {
    let open = |node, step| open_step(node, catalog, settings, step);
    let width = node.columns().len();
    match node {
        Node::Scan {
            table,
            needed,
            predicates,
            access,
            ..
        } => scan(table, &needed, &predicates, &access),
        Node::Lookup { .. } => unreachable!("a lookup is only an index join's right side"),
        Node::Filter { input, predicate } => {
            let fields = input.columns();
            let mut subqueries = Runner::new(catalog, settings);
            let predicate = Some(locate(&predicate, &fields));
            each_batch(open(*input, below(step, 0)), move |batch| {
                let filter = predicate.as_ref().expect("a filter has a predicate");
                if let Ok(keep) = expr::select(filter, &fields, &batch, &mut subqueries) {
                    return (batch.keep(&keep), None);
                }
                let mut keep = Vec::new();
                for i in 0..batch.rows() {
                    match matches_with(&predicate, &fields, &batch.row(i), &mut subqueries) {
                        Ok(true) => keep.push(i),
                        Ok(false) => {}
                        Err(e) => return (batch.keep(&keep), Some(e)),
                    }
                }
                (batch.keep(&keep), None)
            })
        }
        Node::Project { input, exprs } => {
            let fields = input.columns();
            let exprs = locate_all(&exprs, &fields);
            let mut subqueries = Runner::new(catalog, settings);
            each_batch(open(*input, below(step, 0)), move |batch| {
                let columns = exprs
                    .iter()
                    .map(|e| expr::eval(e, &fields, &batch, &mut subqueries));
                if let Ok(columns) = columns.collect::<Result<Vec<_>>>() {
                    return (Batch::new(columns, batch.rows()), None);
                }
                let mut rows = Vec::new();
                for i in 0..batch.rows() {
                    let row = batch.row(i);
                    match exprs
                        .iter()
                        .map(|e| eval_with(e, &fields, &row, &mut subqueries))
                        .collect()
                    {
                        Ok(row) => rows.push(row),
                        Err(e) => return (Batch::from_rows(rows, exprs.len()), Some(e)),
                    }
                }
                (Batch::from_rows(rows, exprs.len()), None)
            })
        }
        Node::Limit {
            input,
            count,
            offset,
        } => Box::new(Limit {
            input: open(*input, below(step, 0)),
            count,
            offset,
            skipped: 0,
            taken: 0,
        }),
        Node::Distinct { input } => {
            let mut seen = HashSet::new();
            each_batch(open(*input, below(step, 0)), move |batch| {
                let keep: Vec<usize> = (0..batch.rows())
                    .filter(|&i| seen.insert(batch.row(i)))
                    .collect();
                (batch.keep(&keep), None)
            })
        }
        Node::Union { inputs } => {
            let mut parts = Vec::new();
            let mut before = 0;
            for input in inputs {
                let steps = input.steps();
                parts.push(open(input, below(step, before)));
                before += steps;
            }
            Box::new(parts.into_iter().flatten())
        }
        Node::Derived { input, .. } => open(*input, below(step, 0)),
        Node::Gather { input, workers } => {
            batches_of(operator::gather(*input, workers, settings, step), width)
        }
        Node::Sort { input, keys, limit } => {
            let fields = input.columns();
            let input = rows_of(open(*input, below(step, 0)));
            let budget = settings.sort_memory;
            let mut subqueries = Runner::new(catalog, settings);
            let sorted = deferred(move || {
                Ok(Box::new(operator::sort(
                    input,
                    &fields,
                    &keys,
                    limit,
                    budget,
                    &mut subqueries,
                )?))
            });
            batches_of(sorted, width)
        }
        Node::Aggregate {
            input,
            group_by,
            aggregates,
            phase,
        } => {
            let fields = input.columns();
            let input = open(*input, below(step, 0));
            let budget = settings.hash_memory;
            let groups = deferred(move || {
                Ok(match phase {
                    Phase::Whole => {
                        Box::new(aggregate(input, &fields, &group_by, &aggregates, budget)?)
                    }
                    // The gather's rows, a group's states each, merged.
                    Phase::Final => Box::new(super::aggregate::merge(
                        rows_of(input),
                        group_by.len(),
                        &aggregates,
                        budget,
                    )?),
                    Phase::Partial => {
                        unreachable!("a partial aggregate runs in a gather's workers")
                    }
                })
            });
            batches_of(groups, width)
        }
        Node::Join {
            left,
            right,
            kind,
            method,
            condition,
        } => {
            let left_fields = left.columns();
            let right_step = below(step, left.steps());
            let left = rows_of(open(*left, below(step, 0)));
            let reopen = {
                let (settings, right_step) = (settings.clone(), right_step.clone());
                move |node| rows_of(open_step(node, catalog, &settings, right_step.clone()))
            };
            let joined = operator::join(
                left,
                left_fields,
                *right,
                kind,
                method,
                condition,
                settings,
                &right_step,
                reopen,
            );
            batches_of(joined, width)
        }
    }
}

/// Reads a table's rows as `scan_filtered` does, in batches: a column
/// table's straight from its columns, others' taken apart into columns.
fn scan<'a>(
    table: &'a Table,
    needed: &[bool],
    predicates: &[Predicate],
    access: &Access,
) -> Batches<'a> {
    if let Some(chunks) = table.scan_by_column(needed, predicates, BATCH_SIZE) {
        return Box::new(chunks.map(|chunk| {
            let chunk = chunk?;
            let rows = chunk.rows;
            let columns = chunk.columns.into_iter().map(|values| {
                Rc::new(values.map_or(Vector::Const(Value::Null, rows), Vector::from_values))
            });
            Ok(Batch::new(columns.collect(), rows))
        }));
    }
    let rows = table
        .scan_filtered(needed, predicates, access)
        .map(|item| item.map(|(_, row)| row));
    batches_of(Box::new(rows), table.columns.len())
}

/// Folds batches into groups, evaluating group keys and arguments a column
/// at a time; a batch they fail on, again a row at a time.
fn aggregate(
    input: Batches,
    fields: &[Field],
    group_by: &[Expr],
    aggregates: &[AggregateCall],
    budget: usize,
) -> Result<Groups> {
    let (group_by, aggregates) = (
        locate_all(group_by, fields),
        aggregate::locate(aggregates, fields),
    );
    let (group_by, aggregates) = (&group_by[..], &aggregates[..]);
    let mut folder = Folder::new(group_by.len(), aggregates, budget);
    for batch in input {
        let batch = batch?;
        let rows = batch.rows();
        let eval = |e: &Expr| expr::eval(e, fields, &batch, &mut NoSubqueries);
        let keys = group_by.iter().map(eval).collect::<Result<Vec<_>>>();
        let args = aggregates
            .iter()
            .map(|(_, arg, _)| arg.as_ref().map(eval).transpose())
            .collect::<Result<Vec<_>>>();
        match (keys, args) {
            (Ok(keys), Ok(args)) => {
                if keys.is_empty() && folder.add_columns(rows, &args) {
                    continue;
                }
                for i in 0..rows {
                    let key = keys.iter().map(|key| key.get(i)).collect();
                    folder.add(
                        key,
                        args.iter().map(|arg| arg.as_ref().map(|arg| arg.get(i))),
                    )?;
                }
            }
            _ => {
                for i in 0..rows {
                    folder.add_row(&batch.row(i), fields, group_by)?;
                }
            }
        }
    }
    Ok(folder.finish())
}

/// Applies `f` to each batch of `input` with rows in it: `f` returns the
/// batch to pass on, and the error to report after it, if it failed.
fn each_batch<'a>(
    input: Batches<'a>,
    f: impl FnMut(Batch) -> (Batch, Option<Error>) + 'a,
) -> Batches<'a> {
    Box::new(EachBatch {
        input,
        f,
        error: None,
        done: false,
    })
}

struct EachBatch<'a, F> {
    input: Batches<'a>,
    f: F,
    /// An error to report once the batch before it is.
    error: Option<Error>,
    done: bool,
}

impl<F: FnMut(Batch) -> (Batch, Option<Error>)> Iterator for EachBatch<'_, F> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(e) = self.error.take() {
                self.done = true;
                return Some(Err(e));
            }
            if self.done {
                return None;
            }
            let batch = match self.input.next()? {
                Ok(batch) => batch,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            };
            if batch.rows() == 0 {
                continue;
            }
            let (batch, error) = (self.f)(batch);
            self.error = error;
            if batch.rows() > 0 {
                return Some(Ok(batch));
            }
        }
    }
}

/// Skips `offset` rows, then keeps `count`, and reads no batch once it has them.
struct Limit<'a> {
    input: Batches<'a>,
    count: Option<usize>,
    offset: usize,
    skipped: usize,
    taken: usize,
}

impl Iterator for Limit<'_> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.count.is_some_and(|count| self.taken >= count) {
                return None;
            }
            // Skips rows, not errors, which end the query.
            let batch = match self.input.next()? {
                Ok(batch) => batch,
                Err(e) => return Some(Err(e)),
            };
            let rows = batch.rows();
            let skip = (self.offset - self.skipped).min(rows);
            self.skipped += skip;
            let take = self
                .count
                .map_or(rows - skip, |count| (count - self.taken).min(rows - skip));
            self.taken += take;
            if take > 0 {
                return Some(Ok(batch.keep(&(skip..skip + take).collect::<Vec<_>>())));
            }
        }
    }
}

/// The rows of batches, one at a time.
fn rows_of(batches: Batches) -> Rows {
    Box::new(RowsOf {
        batches,
        rows: Vec::new().into_iter(),
    })
}

struct RowsOf<'a> {
    batches: Batches<'a>,
    rows: std::vec::IntoIter<Row>,
}

impl Iterator for RowsOf<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.rows.next() {
                return Some(Ok(row));
            }
            match self.batches.next()? {
                Ok(batch) => self.rows = batch.into_rows().into_iter(),
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

/// Rows `width` values long, put into batches of [`BATCH_SIZE`]; an error
/// comes after a batch of the rows before it.
fn batches_of(rows: Rows, width: usize) -> Batches {
    Box::new(BatchesOf {
        rows,
        width,
        error: None,
        done: false,
    })
}

struct BatchesOf<'a> {
    rows: Rows<'a>,
    width: usize,
    error: Option<Error>,
    done: bool,
}

impl Iterator for BatchesOf<'_> {
    type Item = Result<Batch>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return self.error.take().map(Err);
        }
        let mut rows = Vec::new();
        while rows.len() < BATCH_SIZE {
            match self.rows.next() {
                Some(Ok(row)) => rows.push(row),
                Some(Err(e)) => {
                    (self.error, self.done) = (Some(e), true);
                    break;
                }
                None => {
                    self.done = true;
                    break;
                }
            }
        }
        match rows.is_empty() {
            true => self.error.take().map(Err),
            false => Some(Ok(Batch::from_rows(rows, self.width))),
        }
    }
}
