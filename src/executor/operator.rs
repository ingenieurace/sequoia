//! Operators: each step of a query plan, as an iterator over its rows that
//! pulls rows from the step below it (the iterator, or Volcano, model; see
//! `model.rs` for the others, which reuse its joins and sorts).
//!
//! A scan, filter, limit, projection and distinct hand rows on one at a time, so a
//! `LIMIT` stops the scan once it has enough, and a query's rows can be read
//! without holding them all. A sort and an aggregate are blocking: they read
//! all of their input when their first row is asked for. So is the right side
//! of a hash join (see `join.rs`); the left side streams, but for the rows of
//! partitions it writes out past its budget, which come at the end. A block
//! nested loop join reads its left side a block at a time, and its right side
//! again for each block. A merge join sorts both sides when its first row is
//! asked for.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::Settings;
use super::aggregate;
use super::join::{BlockJoin, Join, Lookup, MergeJoin, MergeSort, Right};
use super::sort::{Sorted, Sorter};
use super::subquery::Runner;
use crate::catalog::Catalog;
use crate::catalog::table::Row;
use crate::error::Result;
use crate::plan::query::{JoinMethod, Node};
use crate::sql::ast::{Expr, JoinKind, OrderBy};
use crate::sql::eval::{Field, eval_with, locate, locate_all, matches_with};

/// Rows produced by an operator, or the error that stopped it.
pub type Rows<'a> = Box<dyn Iterator<Item = Result<Row>> + 'a>;

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

    /// Adds to what step `id` did.
    pub(super) fn add(&self, id: usize, runs: u32, rows: u64, time: Duration) {
        let count = &mut self.steps.borrow_mut()[id];
        count.runs += runs;
        count.rows += rows;
        count.time += time;
    }
}

/// Where a step's counts go, if they are kept: the profile and its number.
pub(super) type Step = Option<(Rc<Profile>, usize)>;

/// The step `offset` steps after the first one below `step`.
pub(super) fn below(step: &Step, offset: usize) -> Step {
    step.as_ref().map(|(profile, id)| (profile.clone(), id + 1 + offset))
}

/// Opens a plan step and everything below it, on tables of `catalog` (which
/// subqueries read), its steps counted in `step` if given. No row is read
/// until asked for.
pub(super) fn open_step<'a>(node: Node<'a>, catalog: &'a Catalog, settings: &Settings, step: Step) -> Rows<'a> {
    let Some((profile, id)) = step else { return open_node(node, catalog, settings, &None) };
    let start = Instant::now();
    let rows = open_node(node, catalog, settings, &Some((profile.clone(), id)));
    profile.add(id, 1, 0, start.elapsed());
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
        self.profile.add(self.id, 0, matches!(item, Some(Ok(_))) as u64, start.elapsed());
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
            let left_fields = left.columns();
            let right_step = below(step, left.steps());
            let left = open(*left, below(step, 0));
            let reopen = {
                let (settings, right_step) = (settings.clone(), right_step.clone());
                move |node| open_step(node, catalog, &settings, right_step.clone())
            };
            join(left, left_fields, *right, kind, method, condition, settings, &right_step, reopen)
        }
        Node::Filter { input, predicate } => {
            let columns = input.columns();
            let predicate = Some(locate(&predicate, &columns));
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
            let exprs = locate_all(&exprs, &columns);
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
            deferred(move || Ok(Box::new(sort(input, &columns, &keys, limit, budget, &mut subqueries)?)))
        }
        Node::Aggregate { input, group_by, aggregates } => {
            let columns = input.columns();
            let input = open(*input, below(step, 0));
            let budget = settings.hash_memory;
            deferred(move || Ok(Box::new(aggregate::fold(input, &columns, &group_by, &aggregates, budget)?)))
        }
    }
}

/// Joins `left` rows, whose columns are `left_fields`, with the rows of
/// `right` as `method` says: the join step of every model, which hands it
/// its sides as rows. `open_right` opens the right side (again for each
/// block of a block nested loop join), counted in `right_step`.
#[allow(clippy::too_many_arguments)]
pub(super) fn join<'a>(
    left: Rows<'a>,
    left_fields: Vec<Field>,
    right: Node<'a>,
    kind: JoinKind,
    method: JoinMethod,
    condition: Option<Expr>,
    settings: &Settings,
    right_step: &Step,
    open_right: impl Fn(Node<'a>) -> Rows<'a> + 'a,
) -> Rows<'a> {
    let right_fields = right.columns();
    let right = match method {
        JoinMethod::Block => {
            let budget = settings.join_memory;
            let reopen = Box::new(move || open_right(right.clone()));
            return Box::new(BlockJoin::new(left, left_fields, reopen, right_fields, kind, condition, budget));
        }
        JoinMethod::Merge { left_keys, right_keys, desc, then } => {
            let right = open_right(right);
            let sort = MergeSort { left_keys, right_keys, desc, then, budget: settings.sort_memory };
            return Box::new(MergeJoin::new(left, left_fields, right, right_fields, sort, kind, condition));
        }
        JoinMethod::Hash { left_keys, right_keys, keep_order } => {
            let source = open_right(right);
            let budget = settings.hash_memory;
            let right = Right::hash(source, left_keys, right_keys, &left_fields, right_fields.clone(), budget);
            let join = Join::new(left, left_fields, right, right_fields, kind, condition);
            return Box::new(if keep_order { join.keeping_order(settings.sort_memory) } else { join });
        }
        JoinMethod::Index => {
            if let Some((profile, first)) = right_step {
                let mut steps = profile.steps.borrow_mut();
                steps[*first..*first + right.steps()].iter_mut().for_each(|count| count.inside = true);
            }
            let (filter, lookup) = match right {
                Node::Filter { input, predicate } => (Some(predicate), *input),
                node => (None, node),
            };
            let Node::Lookup { table, qualifier, needed, column, key, left_key } = lookup else {
                unreachable!("an index join reads its right side through a lookup");
            };
            let fields = Field::of_table(&qualifier, &table.columns);
            let (left_key, filter) = (locate(&left_key, &left_fields), filter.map(|f| locate(&f, &fields)));
            let lookup = Lookup { table, needed, column, key, left_key, filter, fields };
            Right::Index { lookup, rows: Vec::new() }
        }
    };
    Box::new(Join::new(left, left_fields, right, right_fields, kind, condition))
}

/// Reads all of `input`, whose columns are `columns`, and sorts it by
/// `keys`, keeping the first `limit` rows if given: the sort step of every model.
pub(super) fn sort(
    input: Rows,
    columns: &[Field],
    keys: &[OrderBy],
    limit: Option<usize>,
    budget: usize,
    subqueries: &mut Runner,
) -> Result<Sorted> {
    let mut sorter = Sorter::new(keys.iter().map(|k| k.desc).collect(), limit, budget);
    let exprs: Vec<Expr> = keys.iter().map(|k| locate(&k.expr, columns)).collect();
    let mut values = Vec::new();
    for row in input {
        let row = row?;
        values.clear();
        for e in &exprs {
            values.push(eval_with(e, columns, &row, subqueries)?);
        }
        sorter.push(&values, row)?;
    }
    sorter.finish()
}

/// Rows that `start` produces, called when the first one is asked for.
pub(super) fn deferred<'a>(start: impl FnOnce() -> Result<Rows<'a>> + 'a) -> Rows<'a> {
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
