//! Operators: each step of a query plan, as an iterator over its rows that
//! pulls rows from the step below it (the Volcano model).
//!
//! A scan, filter, limit, projection and distinct hand rows on one at a time, so a
//! `LIMIT` stops the scan once it has enough, and a query's rows can be read
//! without holding them all. A sort and an aggregate are blocking: they read
//! all of their input when their first row is asked for. So is the right side
//! of a hash join (see `join.rs`); the left side streams. A block nested loop
//! join reads its left side a block at a time, and its right side again for
//! each block. A merge join sorts both sides when its first row is asked for.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::Settings;
use super::aggregate;
use super::join::{BlockJoin, Join, Lookup, MergeJoin, MergeSort, Right};
use super::sort::Sorter;
use super::subquery::Runner;
use crate::catalog::Catalog;
use crate::catalog::table::Row;
use crate::error::Result;
use crate::plan::query::{JoinMethod, Node};
use crate::sql::eval::{Field, eval_with, matches_with};

/// Rows produced by an operator, or the error that stopped it.
pub type Rows<'a> = Box<dyn Iterator<Item = Result<Row>> + 'a>;

/// Opens a plan step and everything below it, on tables of `catalog` (which
/// subqueries read). No row is read until asked for.
pub fn open<'a>(node: Node<'a>, catalog: &'a Catalog, settings: &Settings) -> Rows<'a> {
    open_step(node, catalog, settings, None)
}

/// Like [`open`], but counts in `profile` the rows and time of each step,
/// for EXPLAIN ANALYZE.
pub fn open_profiled<'a>(node: Node<'a>, catalog: &'a Catalog, settings: &Settings, profile: &Rc<Profile>) -> Rows<'a> {
    open_step(node, catalog, settings, Some((profile.clone(), 0)))
}

/// What EXPLAIN ANALYZE counts of each step of a plan. Steps are numbered
/// from 0 in the order EXPLAIN lists them (see `Node::explain_lines`).
pub struct Profile {
    steps: RefCell<Vec<StepCount>>,
}

/// What one step of a plan did.
#[derive(Debug, Clone, Copy, Default)]
pub struct StepCount {
    /// How many times the step was opened: once, but for the right side of a
    /// block nested loop join, once per block of left rows; 0 if never.
    pub runs: u32,
    /// The rows it returned, over all of its runs.
    pub rows: u64,
    /// The time spent opening it and asking it for rows, the time of the
    /// steps below it included.
    pub time: Duration,
    /// Whether it runs inside the join above it, which looks rows up through
    /// it, rather than as a step of its own: it is counted with the join.
    pub inside: bool,
}

impl Profile {
    pub fn new(steps: usize) -> Self {
        Profile { steps: RefCell::new(vec![StepCount::default(); steps]) }
    }

    pub fn counts(&self) -> Vec<StepCount> {
        self.steps.borrow().clone()
    }
}

/// Where a step's counts go, if they are kept: the profile and its number.
type Step = Option<(Rc<Profile>, usize)>;

/// The step `offset` steps after the first one below `step`.
fn below(step: &Step, offset: usize) -> Step {
    step.as_ref().map(|(profile, id)| (profile.clone(), id + 1 + offset))
}

fn open_step<'a>(node: Node<'a>, catalog: &'a Catalog, settings: &Settings, step: Step) -> Rows<'a> {
    let Some((profile, id)) = step else { return open_node(node, catalog, settings, &None) };
    let start = Instant::now();
    let rows = open_node(node, catalog, settings, &Some((profile.clone(), id)));
    {
        let count = &mut profile.steps.borrow_mut()[id];
        count.runs += 1;
        count.time += start.elapsed();
    }
    Box::new(Profiled { rows, profile, id })
}

/// A step's rows, counted as they are asked for.
struct Profiled<'a> {
    rows: Rows<'a>,
    profile: Rc<Profile>,
    id: usize,
}

impl Iterator for Profiled<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = Instant::now();
        let item = self.rows.next();
        let count = &mut self.profile.steps.borrow_mut()[self.id];
        count.time += start.elapsed();
        count.rows += matches!(item, Some(Ok(_))) as u64;
        item
    }
}

fn open_node<'a>(node: Node<'a>, catalog: &'a Catalog, settings: &Settings, step: &Step) -> Rows<'a> {
    let open = |node, step| open_step(node, catalog, settings, step);
    match node {
        Node::Scan { table, needed, predicates, access, .. } => {
            Box::new(table.scan_filtered(&needed, &predicates, &access).map(|item| item.map(|(_, row)| row)))
        }
        Node::Lookup { .. } => unreachable!("a lookup is only an index join's right side"),
        Node::Join { left, right, kind, method, condition } => {
            let (left_fields, right_fields) = (left.columns(), right.columns());
            let right_step = below(step, left.steps());
            let left = open(*left, below(step, 0));
            let right = match method {
                JoinMethod::Block => {
                    let budget = settings.join_memory;
                    let settings = settings.clone();
                    let right = Box::new(move || open_step((*right).clone(), catalog, &settings, right_step.clone()));
                    return Box::new(BlockJoin::new(left, left_fields, right, right_fields, kind, condition, budget));
                }
                JoinMethod::Merge { left_keys, right_keys, desc, then } => {
                    let right = open(*right, right_step);
                    let sort = MergeSort { left_keys, right_keys, desc, then, budget: settings.sort_memory };
                    return Box::new(MergeJoin::new(left, left_fields, right, right_fields, sort, kind, condition));
                }
                JoinMethod::Hash { left_keys, right_keys } => {
                    let source = open(*right, right_step);
                    Right::hash(source, left_keys, right_keys, &left_fields, right_fields.clone())
                }
                JoinMethod::Index => {
                    if let Some((profile, first)) = &right_step {
                        let mut steps = profile.steps.borrow_mut();
                        steps[*first..*first + right.steps()].iter_mut().for_each(|count| count.inside = true);
                    }
                    let (filter, lookup) = match *right {
                        Node::Filter { input, predicate } => (Some(predicate), *input),
                        node => (None, node),
                    };
                    let Node::Lookup { table, qualifier, needed, column, key, left_key } = lookup else {
                        unreachable!("an index join reads its right side through a lookup");
                    };
                    let fields = Field::of_table(&qualifier, &table.columns);
                    let lookup = Lookup { table, needed, column, key, left_key, filter, fields };
                    Right::Index { lookup, rows: Vec::new() }
                }
            };
            Box::new(Join::new(left, left_fields, right, right_fields, kind, condition))
        }
        Node::Filter { input, predicate } => {
            let columns = input.columns();
            let predicate = Some(predicate);
            let mut subqueries = Runner::new(catalog, settings);
            Box::new(open(*input, below(step, 0)).filter_map(move |item| {
                match item.and_then(|row| Ok((matches_with(&predicate, &columns, &row, &mut subqueries)?, row))) {
                    Ok((true, row)) => Some(Ok(row)),
                    Ok((false, _)) => None,
                    Err(e) => Some(Err(e)),
                }
            }))
        }
        Node::Distinct { input } => {
            let mut seen = HashSet::new();
            Box::new(open(*input, below(step, 0)).filter(move |item| match item {
                Ok(row) => seen.insert(row.clone()),
                Err(_) => true,
            }))
        }
        Node::Limit { input, count, offset } => {
            // Skips rows, not errors, which end the query.
            let mut skipped = 0;
            let rows = open(*input, below(step, 0)).filter(move |item| {
                let skip = item.is_ok() && skipped < offset;
                skipped += skip as usize;
                !skip
            });
            match count {
                Some(count) => Box::new(rows.take(count)),
                None => Box::new(rows),
            }
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
        Node::Project { input, exprs } => {
            let columns = input.columns();
            let mut subqueries = Runner::new(catalog, settings);
            Box::new(open(*input, below(step, 0)).map(move |item| {
                let row = item?;
                exprs.iter().map(|e| eval_with(e, &columns, &row, &mut subqueries)).collect()
            }))
        }
        Node::Sort { input, keys, limit } => {
            let columns = input.columns();
            let input = open(*input, below(step, 0));
            let budget = settings.sort_memory;
            let mut subqueries = Runner::new(catalog, settings);
            deferred(move || {
                let mut sorter = Sorter::new(keys.iter().map(|k| k.desc).collect(), limit, budget);
                let mut values = Vec::new();
                for row in input {
                    let row = row?;
                    values.clear();
                    for k in &keys {
                        values.push(eval_with(&k.expr, &columns, &row, &mut subqueries)?);
                    }
                    sorter.push(&values, row)?;
                }
                Ok(Box::new(sorter.finish()?))
            })
        }
        Node::Aggregate { input, group_by, aggregates } => {
            let columns = input.columns();
            let input = open(*input, below(step, 0));
            deferred(move || {
                let groups = aggregate::fold(input, &columns, &group_by, &aggregates)?;
                Ok(Box::new(groups.into_iter().map(Ok)))
            })
        }
    }
}

/// Rows that `start` produces, called when the first one is asked for.
fn deferred<'a>(start: impl FnOnce() -> Result<Rows<'a>> + 'a) -> Rows<'a> {
    Box::new(Deferred { start: Some(Box::new(start)), rows: None })
}

struct Deferred<'a> {
    start: Option<Box<dyn FnOnce() -> Result<Rows<'a>> + 'a>>,
    rows: Option<Rows<'a>>,
}

impl Iterator for Deferred<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(start) = self.start.take() {
            match start() {
                Ok(rows) => self.rows = Some(rows),
                Err(e) => return Some(Err(e)),
            }
        }
        self.rows.as_mut()?.next()
    }
}
