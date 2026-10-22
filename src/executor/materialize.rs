//! The materialization model: each step runs once, to completion, and
//! hands the step above all of its rows at once.
//!
//! ```text
//! PROJECT   run(FILTER) -> [r1, r4]     then evaluates the select list on each
//!   FILTER  run(SCAN)   -> [r1 .. r9]   then keeps the rows it matches
//!     SCAN  reads the table into a Vec
//! ```
//!
//! There is a call per step, not per row, and no row is handed on until its
//! step is done; but each step's rows are all held at once, so it suits
//! queries whose steps have few rows each: an OLTP lookup.
//!
//! A step asks the one below it for only as many rows as it will use, if
//! it knows: a LIMIT asks for its offset and count, and a projection, a
//! derived table or a union passes on what it was asked for, so a scan
//! under a LIMIT stops early. A filter or a distinct step cannot tell how
//! many rows it will drop, so the step below it makes all of its rows.
//!
//! **Errors.** A step that fails returns the rows it made before the error,
//! and the error after them, as the iterator model would hand them on: a
//! step above takes those rows first, and reports the error only if it
//! still wants more. So a LIMIT that has its rows before an error never
//! reports it, in every model.
//!
//! Joins, sorts and aggregates run the iterator model's algorithms (see
//! `operator.rs`), within its memory budgets, on all of their inputs' rows,
//! before returning any. A join runs its right side once and keeps its rows
//! for each block of a block nested loop join.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Instant;

use super::Settings;
use super::aggregate;
use super::operator::{self, Rows, Step, below, deferred};
use super::subquery::Runner;
use crate::catalog::Catalog;
use crate::catalog::table::Row;
use crate::error::{Error, Result};
use crate::plan::query::{Node, Phase};
use crate::sql::eval::{eval_with, locate, locate_all, matches_with};

/// A step's rows, and the error that stopped it after them, if one did.
struct Output {
    rows: Vec<Row>,
    error: Option<Error>,
}

impl Output {
    /// Reads `rows` up to the end, an error, or `wanted` of them.
    fn read(mut rows: impl Iterator<Item = Result<Row>>, wanted: Option<usize>) -> Output {
        let mut read = Vec::new();
        while wanted.is_none_or(|wanted| read.len() < wanted) {
            match rows.next() {
                Some(Ok(row)) => read.push(row),
                Some(Err(e)) => {
                    return Output {
                        rows: read,
                        error: Some(e),
                    };
                }
                None => break,
            }
        }
        Output {
            rows: read,
            error: None,
        }
    }

    /// The rows a step made, and then the error of the step below it,
    /// unless the step has as many rows as are `wanted` of it: the step that
    /// asked would never read the rows after them, nor so their error.
    fn ended(rows: Vec<Row>, error: Option<Error>, wanted: Option<usize>) -> Output {
        let enough = wanted.is_some_and(|wanted| rows.len() >= wanted);
        Output {
            rows,
            error: error.filter(|_| !enough),
        }
    }

    fn failed(e: Error) -> Output {
        Output {
            rows: Vec::new(),
            error: Some(e),
        }
    }

    fn into_rows<'a>(self) -> Rows<'a> {
        Box::new(self.rows.into_iter().map(Ok).chain(self.error.map(Err)))
    }
}

/// Opens a plan, its steps counted in `step` if given. It runs when its
/// first row is asked for.
pub(super) fn open<'a>(
    node: Node<'a>,
    catalog: &'a Catalog,
    settings: &Settings,
    step: Step,
) -> Rows<'a> {
    let settings = settings.clone();
    deferred(move || Ok(run(node, catalog, &settings, &step, None).into_rows()))
}

/// Runs a step and everything below it, for `wanted` rows if not all of them.
fn run<'a>(
    node: Node<'a>,
    catalog: &'a Catalog,
    settings: &Settings,
    step: &Step,
    wanted: Option<usize>,
) -> Output {
    let start = Instant::now();
    let output = run_node(node, catalog, settings, step, wanted);
    if let Some((profile, id)) = step {
        profile.add(*id, 1, output.rows.len() as u64, start.elapsed());
    }
    output
}

fn run_node<'a>(
    node: Node<'a>,
    catalog: &'a Catalog,
    settings: &Settings,
    step: &Step,
    wanted: Option<usize>,
) -> Output {
    let run = |node, step, wanted| run(node, catalog, settings, &step, wanted);
    let enough = |rows: &Vec<Row>| wanted.is_some_and(|wanted| rows.len() >= wanted);
    match node {
        Node::Scan {
            table,
            needed,
            predicates,
            access,
            ..
        } => Output::read(
            table
                .scan_filtered(&needed, &predicates, &access)
                .map(|item| item.map(|(_, row)| row)),
            wanted,
        ),
        Node::Lookup { .. } => unreachable!("a lookup is only an index join's right side"),
        Node::Filter { input, predicate } => {
            let fields = input.columns();
            let input = run(*input, below(step, 0), None);
            let mut subqueries = Runner::new(catalog, settings);
            let predicate = Some(locate(&predicate, &fields));
            let mut rows = Vec::new();
            for row in input.rows {
                if enough(&rows) {
                    break;
                }
                match matches_with(&predicate, &fields, &row, &mut subqueries) {
                    Ok(true) => rows.push(row),
                    Ok(false) => {}
                    Err(e) => {
                        return Output {
                            rows,
                            error: Some(e),
                        };
                    }
                }
            }
            Output::ended(rows, input.error, wanted)
        }
        Node::Project { input, exprs } => {
            let fields = input.columns();
            let exprs = locate_all(&exprs, &fields);
            let input = run(*input, below(step, 0), wanted);
            let mut subqueries = Runner::new(catalog, settings);
            let mut rows = Vec::with_capacity(input.rows.len());
            for row in input.rows {
                match exprs
                    .iter()
                    .map(|e| eval_with(e, &fields, &row, &mut subqueries))
                    .collect()
                {
                    Ok(row) => rows.push(row),
                    Err(e) => {
                        return Output {
                            rows,
                            error: Some(e),
                        };
                    }
                }
            }
            Output::ended(rows, input.error, wanted)
        }
        Node::Limit {
            input,
            count,
            offset,
        } => {
            let count = match (count, wanted) {
                (Some(count), Some(wanted)) => Some(count.min(wanted)),
                (count, wanted) => count.or(wanted),
            };
            let input = run(
                *input,
                below(step, 0),
                count.map(|count| count.saturating_add(offset)),
            );
            let rows = input
                .rows
                .into_iter()
                .skip(offset)
                .take(count.unwrap_or(usize::MAX))
                .collect();
            Output::ended(rows, input.error, count)
        }
        Node::Distinct { input } => {
            let input = run(*input, below(step, 0), None);
            let mut seen = HashSet::new();
            let mut rows = Vec::new();
            for row in input.rows {
                if enough(&rows) {
                    break;
                }
                if seen.insert(row.clone()) {
                    rows.push(row);
                }
            }
            Output::ended(rows, input.error, wanted)
        }
        Node::Union { inputs } => {
            let mut rows = Vec::new();
            let mut before = 0;
            for input in inputs {
                if enough(&rows) {
                    break;
                }
                let steps = input.steps();
                let output = run(
                    input,
                    below(step, before),
                    wanted.map(|wanted| wanted - rows.len()),
                );
                before += steps;
                rows.extend(output.rows);
                if output.error.is_some() {
                    return Output::ended(rows, output.error, wanted);
                }
            }
            Output { rows, error: None }
        }
        Node::Derived { input, .. } => run(*input, below(step, 0), wanted),
        Node::Gather { input, workers } => {
            Output::read(operator::gather(*input, workers, settings, step), wanted)
        }
        Node::Sort { input, keys, limit } => {
            let fields = input.columns();
            let input = run(*input, below(step, 0), None);
            let mut subqueries = Runner::new(catalog, settings);
            match operator::sort(
                input.into_rows(),
                &fields,
                &keys,
                limit,
                settings.sort_memory,
                &mut subqueries,
            ) {
                Ok(sorted) => Output::read(sorted, wanted),
                Err(e) => Output::failed(e),
            }
        }
        Node::Aggregate {
            input,
            group_by,
            aggregates,
            phase,
        } => {
            let fields = input.columns();
            let input = run(*input, below(step, 0), None).into_rows();
            let budget = settings.hash_memory;
            let groups = match phase {
                Phase::Whole => aggregate::fold(input, &fields, &group_by, &aggregates, budget),
                Phase::Final => aggregate::merge(input, group_by.len(), &aggregates, budget),
                Phase::Partial => unreachable!("a partial aggregate runs in a gather's workers"),
            };
            match groups {
                Ok(groups) => Output::read(groups, wanted),
                Err(e) => Output::failed(e),
            }
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
            let left = run(*left, below(step, 0), None).into_rows();
            // The right side runs when the join first reads it, once.
            let kept: RefCell<Option<Rc<Output>>> = RefCell::new(None);
            let open_right = {
                let (settings, right_step) = (settings.clone(), right_step.clone());
                move |node| {
                    let output = kept
                        .borrow_mut()
                        .get_or_insert_with(|| {
                            Rc::new(self::run(node, catalog, &settings, &right_step, None))
                        })
                        .clone();
                    replay(output)
                }
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
                open_right,
            );
            Output::read(joined, wanted)
        }
    }
}

/// The rows of an output kept for reading more than once.
fn replay<'a>(output: Rc<Output>) -> Rows<'a> {
    let rows = output.rows.len();
    Box::new((0..=rows).map_while(move |i| match output.rows.get(i) {
        Some(row) => Some(Ok(row.clone())),
        None => output.error.clone().map(Err),
    }))
}
