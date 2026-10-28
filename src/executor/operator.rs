//! Operators: each step of a query plan, as an iterator over its rows that
//! pulls rows from the step below it (the iterator, or Volcano, model; see
//! `model.rs` for the others, which reuse its joins and sorts).
//!
//! A scan, filter, limit, projection and distinct hand rows on one at a time, so a
//! `LIMIT` stops the scan once it has enough, and a query's rows can be read
//! without holding them all. A sort and an aggregate are blocking: they read
//! all of their input when their first row is asked for. So is the right side
//! of a hash join (see `joins/`); the left side streams, but for the rows of
//! partitions it writes out past its budget, which come at the end. A block
//! nested loop join reads its left side a block at a time, and its right side
//! again for each block. A merge join sorts both sides when its first row is
//! asked for.
//!
//! **Outer joins on the right.** A RIGHT JOIN is an inner join, and a FULL
//! JOIN a LEFT JOIN, of the same two sides; then the right rows no left row
//! matched come, with NULLs for the left columns. Those are found by a
//! second join, the other way round: each right row LEFT JOINed with the
//! left rows, marked, by the same method with its keys swapped, and kept if
//! no marked row came with it. So both sides are read twice, and every
//! method that can join them one way can the other, within its budget.
//!
//! The steps the models share, here too, are those that work on rows in
//! all of them: a window step (see `window.rs`), INTERSECT and EXCEPT, which
//! hold the right side's rows, a recursive query, which holds the rows of
//! its last round (and with UNION, all it returned), and a LATERAL
//! subquery's join, which plans and runs the subquery for each left row.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Settings;
use super::aggregate;
use super::joins::{
    BlockJoin, Join, Lookup, MergeJoin, MergeSort, NaiveJoin, ParallelHashJoin, Right,
};
use super::model;
use super::sort::{Sorted, Sorter};
use super::subquery::Runner;
use crate::catalog::Catalog;
use crate::catalog::table::Row;
use crate::error::Result;
use crate::plan::filter::visit_outer;
use crate::plan::query::{self, Choices, JoinMethod, Node, Phase};
use crate::sql::ast::{ColumnRef, Expr, JoinKind, OrderBy, Select, SetOp, WorkCell};
use crate::sql::eval::{Field, Subqueries, eval_with, locate, locate_all, matches_with, resolve};
use crate::types::Value;

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
    /// Whether it runs inside the step above it, rather than as a step of its
    /// own: the index join that looks rows up through it, or the gather whose
    /// workers run it. It is counted with that step.
    pub inside: bool,
}

impl Profile {
    pub fn new(steps: usize) -> Self {
        Profile {
            steps: RefCell::new(vec![StepCount::default(); steps]),
        }
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
    step.as_ref()
        .map(|(profile, id)| (profile.clone(), id + 1 + offset))
}

/// Marks `node`, whose step is `step`, and the steps below it as run inside
/// the step above, which counts them with its own.
pub(super) fn inside(step: &Step, node: &Node) {
    if let Some((profile, first)) = step {
        let mut steps = profile.steps.borrow_mut();
        steps[*first..*first + node.steps()]
            .iter_mut()
            .for_each(|count| count.inside = true);
    }
}

/// Opens a gather (see `gather.rs`), whose input runs on worker threads,
/// counted with it.
pub(super) fn gather<'a>(
    input: Node<'a>,
    workers: usize,
    settings: &Settings,
    step: &Step,
) -> Rows<'a> {
    inside(&below(step, 0), &input);
    super::gather::open(input, workers, settings)
}

/// Opens a plan step and everything below it, on tables of `catalog` (which
/// subqueries read), its steps counted in `step` if given. No row is read
/// until asked for.
pub(super) fn open_step<'a>(
    node: Node<'a>,
    catalog: &'a Catalog,
    settings: &Settings,
    step: Step,
) -> Rows<'a> {
    let Some((profile, id)) = step else {
        return open_node(node, catalog, settings, &None);
    };
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
        self.profile.add(
            self.id,
            0,
            matches!(item, Some(Ok(_))) as u64,
            start.elapsed(),
        );
        item
    }
}

fn open_node<'a>(
    node: Node<'a>,
    catalog: &'a Catalog,
    settings: &Settings,
    step: &Step,
) -> Rows<'a> {
    let open = |node, step| open_step(node, catalog, settings, step);
    match node {
        Node::Scan {
            table,
            needed,
            predicates,
            access,
            ..
        } => Box::new(
            table
                .scan_filtered(&needed, &predicates, &access)
                .map(|item| item.map(|(_, row)| row)),
        ),
        Node::Lookup { .. } => unreachable!("a lookup is only an index join's right side"),
        Node::Join {
            left,
            right,
            kind,
            method,
            condition,
        } => {
            let right_step = below(step, left.steps());
            let open_left = {
                let (settings, left_step) = (settings.clone(), below(step, 0));
                move |node| open_step(node, catalog, &settings, left_step.clone())
            };
            let open_right = {
                let (settings, right_step) = (settings.clone(), right_step.clone());
                move |node| open_step(node, catalog, &settings, right_step.clone())
            };
            join(
                *left,
                *right,
                kind,
                method,
                condition,
                catalog,
                settings,
                &right_step,
                open_left,
                open_right,
            )
        }
        Node::Values { rows, .. } => values(rows),
        Node::WorkTable { cell, .. } => values(cell.rows()),
        Node::Recursive {
            anchor,
            step: round,
            all,
            cell,
        } => {
            let round_step = below(step, anchor.steps());
            let anchor = open(*anchor, below(step, 0));
            let settings = settings.clone();
            let open_round = move || open_step((*round).clone(), catalog, &settings, round_step.clone());
            recursive(anchor, open_round, all, cell)
        }
        Node::SetOp {
            left,
            right,
            op,
            all,
        } => {
            let right_step = below(step, left.steps());
            let left = open(*left, below(step, 0));
            set_op(left, open(*right, right_step), op, all)
        }
        Node::Window {
            input,
            partition_by,
            order_by,
            functions,
        } => {
            let fields = input.columns();
            let input = open(*input, below(step, 0));
            let budget = settings.sort_memory;
            let mut subqueries = Runner::new(catalog, settings);
            deferred(move || {
                super::window::window(
                    input,
                    &fields,
                    &partition_by,
                    &order_by,
                    &functions,
                    budget,
                    &mut subqueries,
                )
            })
        }
        Node::Lateral {
            left,
            query,
            qualifier,
            names,
            kind,
            condition,
        } => {
            let left_fields = left.columns();
            let left = open(*left, below(step, 0));
            lateral(
                left,
                left_fields,
                query,
                &qualifier,
                &names,
                kind,
                condition,
                catalog,
                settings,
            )
        }
        Node::Filter { input, predicate } => {
            let columns = input.columns();
            let predicate = Some(locate(&predicate, &columns));
            let mut subqueries = Runner::new(catalog, settings);
            Box::new(open(*input, below(step, 0)).filter_map(move |item| {
                match item.and_then(|row| {
                    Ok((
                        matches_with(&predicate, &columns, &row, &mut subqueries)?,
                        row,
                    ))
                }) {
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
        Node::Limit {
            input,
            count,
            offset,
        } => {
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
        Node::Gather { input, workers } => gather(*input, workers, settings, step),
        Node::Project { input, exprs } => {
            let columns = input.columns();
            let exprs = locate_all(&exprs, &columns);
            let mut subqueries = Runner::new(catalog, settings);
            Box::new(open(*input, below(step, 0)).map(move |item| {
                let row = item?;
                exprs
                    .iter()
                    .map(|e| eval_with(e, &columns, &row, &mut subqueries))
                    .collect()
            }))
        }
        Node::Sort { input, keys, limit } => {
            let columns = input.columns();
            let input = open(*input, below(step, 0));
            let budget = settings.sort_memory;
            let mut subqueries = Runner::new(catalog, settings);
            deferred(move || {
                Ok(Box::new(sort(
                    input,
                    &columns,
                    &keys,
                    limit,
                    budget,
                    &mut subqueries,
                )?))
            })
        }
        Node::Aggregate {
            input,
            group_by,
            aggregates,
            phase,
        } => {
            let columns = input.columns();
            let input = open(*input, below(step, 0));
            let budget = settings.hash_memory;
            let mut subqueries = Runner::new(catalog, settings);
            deferred(move || {
                Ok(Box::new(match phase {
                    Phase::Whole => aggregate::fold(
                        input,
                        &columns,
                        &group_by,
                        &aggregates,
                        budget,
                        &mut subqueries,
                    )?,
                    Phase::Final => aggregate::merge(input, group_by.len(), &aggregates, budget)?,
                    Phase::Partial => {
                        unreachable!("a partial aggregate runs in a gather's workers")
                    }
                }))
            })
        }
    }
}

/// The name of the column that marks the left rows of the second join of a
/// RIGHT or FULL JOIN (see the module docs): a name no column can have.
const MATCHED: &str = "?matched?";

/// Joins the rows of `left` with those of `right` as `method` says: the
/// join step of every model, which opens its sides as rows with `open_left`
/// and `open_right` (the right side again for each block of a block nested
/// loop join, and both again for the right rows a RIGHT or FULL JOIN keeps,
/// see the module docs), the right side counted in `right_step`. The
/// condition's subqueries run on `catalog`.
#[allow(clippy::too_many_arguments)]
pub(super) fn join<'a>(
    left: Node<'a>,
    right: Node<'a>,
    kind: JoinKind,
    method: JoinMethod,
    condition: Option<Expr>,
    catalog: &'a Catalog,
    settings: &Settings,
    right_step: &Step,
    open_left: impl Fn(Node<'a>) -> Rows<'a> + 'a,
    open_right: impl Fn(Node<'a>) -> Rows<'a> + 'a,
) -> Rows<'a> {
    let (left_fields, right_fields) = (left.columns(), right.columns());
    if !kind.keeps_right() {
        let left = open_left(left);
        return join_rows(
            left,
            left_fields,
            right,
            right_fields,
            kind,
            method,
            condition,
            catalog,
            settings,
            right_step,
            open_right,
        );
    }
    let (open_left, open_right) = (Rc::new(open_left), Rc::new(open_right));
    let first = {
        let open_right = open_right.clone();
        join_rows(
            open_left(left.clone()),
            left_fields.clone(),
            right.clone(),
            right_fields.clone(),
            match kind {
                JoinKind::Full => JoinKind::Left,
                _ => JoinKind::Inner,
            },
            method.clone(),
            condition.clone(),
            catalog,
            settings,
            right_step,
            move |node| open_right(node),
        )
    };
    let settings = settings.clone();
    let unmatched = deferred(move || {
        // Each right row, with the left rows that match it, marked.
        let width = left_fields.len();
        let marked = [left_fields, vec![Field::named(MATCHED)]].concat();
        let mark = move |node| -> Rows<'a> {
            Box::new(open_left(node).map(|row| {
                row.map(|mut row| {
                    row.push(Value::Bool(true));
                    row
                })
            }))
        };
        let rows = join_rows(
            open_right(right),
            right_fields,
            left,
            marked,
            JoinKind::Left,
            swapped(method),
            condition,
            catalog,
            &settings,
            &None,
            mark,
        );
        // Those that came without one, the left columns first, NULL.
        Ok(Box::new(rows.filter_map(move |item| match item {
            Ok(row) if row.last() == Some(&Value::Null) => {
                let right = row.len() - width - 1;
                let mut padded = vec![Value::Null; width];
                padded.extend(row.into_iter().take(right));
                Some(Ok(padded))
            }
            Ok(_) => None,
            Err(e) => Some(Err(e)),
        })) as Rows<'a>)
    });
    Box::new(first.chain(unmatched))
}

/// The method of a join the other way round: its keys swapped. A merge
/// join's right side sorts by its keys alone.
fn swapped(method: JoinMethod) -> JoinMethod {
    match method {
        JoinMethod::Hash {
            left_keys,
            right_keys,
            workers,
            ..
        } => JoinMethod::Hash {
            left_keys: right_keys,
            right_keys: left_keys,
            keep_order: false,
            workers,
        },
        JoinMethod::Merge {
            left_keys,
            right_keys,
            desc,
            ..
        } => JoinMethod::Merge {
            left_keys: right_keys,
            right_keys: left_keys,
            desc,
            then: Vec::new(),
        },
        JoinMethod::Index => unreachable!("a RIGHT or FULL JOIN is never an index join"),
        method => method,
    }
}

/// Joins `left` rows, whose columns are `left_fields`, with the rows of
/// `right` (whose columns are `right_fields`) as `method` says, for an
/// inner or LEFT JOIN. `open_right` opens the right side (again for each
/// block of a block nested loop join), counted in `right_step`.
#[allow(clippy::too_many_arguments)]
fn join_rows<'a>(
    left: Rows<'a>,
    left_fields: Vec<Field>,
    right: Node<'a>,
    right_fields: Vec<Field>,
    kind: JoinKind,
    method: JoinMethod,
    condition: Option<Expr>,
    catalog: &'a Catalog,
    settings: &Settings,
    right_step: &Step,
    open_right: impl Fn(Node<'a>) -> Rows<'a> + 'a,
) -> Rows<'a> {
    // The subqueries of the condition run in the join, in this thread.
    let runs_subqueries = condition.as_ref().is_some_and(Expr::has_subquery);
    let runner = || -> Box<dyn Subqueries + 'a> { Box::new(Runner::new(catalog, settings)) };
    let right = match method {
        JoinMethod::Block => {
            let budget = settings.join_memory;
            let reopen = Box::new(move || open_right(right.clone()));
            let join = BlockJoin::new(
                left,
                left_fields,
                reopen,
                right_fields,
                kind,
                condition,
                budget,
            );
            return match runs_subqueries {
                true => Box::new(join.running(runner())),
                false => Box::new(join),
            };
        }
        JoinMethod::Naive => {
            let reopen = Box::new(move || open_right(right.clone()));
            let join = NaiveJoin::new(left, left_fields, reopen, right_fields, kind, condition);
            return match runs_subqueries {
                true => Box::new(join.running(runner())),
                false => Box::new(join),
            };
        }
        JoinMethod::Merge {
            left_keys,
            right_keys,
            desc,
            then,
        } => {
            let right = open_right(right);
            let sort = MergeSort {
                left_keys,
                right_keys,
                desc,
                then,
                budget: settings.sort_memory,
            };
            let join = MergeJoin::new(left, left_fields, right, right_fields, sort, kind, condition);
            return match runs_subqueries {
                true => Box::new(join.running(runner())),
                false => Box::new(join),
            };
        }
        JoinMethod::Hash {
            left_keys,
            right_keys,
            keep_order,
            workers,
        } => {
            let source = open_right(right);
            let budget = settings.hash_memory;
            if workers > 1 {
                return Box::new(ParallelHashJoin::new(
                    left,
                    left_fields,
                    source,
                    right_fields,
                    left_keys,
                    right_keys,
                    kind,
                    condition,
                    budget,
                    workers,
                ));
            }
            let right = Right::hash(
                source,
                left_keys,
                right_keys,
                &left_fields,
                right_fields.clone(),
                budget,
                0,
            );
            let join = Join::new(left, left_fields, right, right_fields, kind, condition);
            let join = match runs_subqueries {
                true => join.running(runner()),
                false => join,
            };
            return Box::new(if keep_order {
                join.keeping_order(settings.sort_memory)
            } else {
                join
            });
        }
        JoinMethod::Index => {
            inside(right_step, &right);
            let (filter, lookup) = match right {
                Node::Filter { input, predicate } => (Some(predicate), *input),
                node => (None, node),
            };
            let Node::Lookup {
                table,
                qualifier,
                needed,
                column,
                key,
                left_key,
            } = lookup
            else {
                unreachable!("an index join reads its right side through a lookup");
            };
            let fields = Field::of_table(&qualifier, &table.columns);
            let (left_key, filter) = (
                locate(&left_key, &left_fields),
                filter.map(|f| locate(&f, &fields)),
            );
            let lookup = Lookup {
                table,
                needed,
                column,
                key,
                left_key,
                filter,
                fields,
            };
            Right::Index {
                lookup,
                rows: Vec::new(),
            }
        }
    };
    let join = Join::new(left, left_fields, right, right_fields, kind, condition);
    match runs_subqueries {
        true => Box::new(join.running(runner())),
        false => Box::new(join),
    }
}

/// The rows given, one at a time.
pub(super) fn values<'a>(rows: Arc<Vec<Row>>) -> Rows<'a> {
    Box::new((0..rows.len()).map(move |i| Ok(rows[i].clone())))
}

/// The rows of a recursive query: those of `anchor`, then those of each
/// round, which `open_round` opens after the rows of the last one are put
/// in `cell`, until one returns none. Without `all`, a row equal to one
/// returned before is dropped.
pub(super) fn recursive<'a>(
    anchor: Rows<'a>,
    open_round: impl FnMut() -> Rows<'a> + 'a,
    all: bool,
    cell: WorkCell,
) -> Rows<'a> {
    Box::new(Recursion {
        rows: anchor,
        open_round: Box::new(open_round),
        cell,
        round: Vec::new(),
        seen: (!all).then(HashSet::new),
        done: false,
    })
}

struct Recursion<'a> {
    /// The rows of the anchor, or of the round running.
    rows: Rows<'a>,
    open_round: Box<dyn FnMut() -> Rows<'a> + 'a>,
    cell: WorkCell,
    /// The rows returned in this round, for the next one to read.
    round: Vec<Row>,
    /// Every row returned, for UNION without ALL.
    seen: Option<HashSet<Row>>,
    done: bool,
}

impl Iterator for Recursion<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            match self.rows.next() {
                Some(Ok(row)) => {
                    if let Some(seen) = &mut self.seen
                        && !seen.insert(row.clone())
                    {
                        continue;
                    }
                    self.round.push(row.clone());
                    return Some(Ok(row));
                }
                Some(Err(e)) => {
                    self.done = true;
                    return Some(Err(e));
                }
                None if self.round.is_empty() => self.done = true,
                None => {
                    self.cell.set(std::mem::take(&mut self.round));
                    self.rows = (self.open_round)();
                }
            }
        }
        None
    }
}

/// The rows of `left` that `right` has too (INTERSECT), or does not
/// (EXCEPT): with ALL, a row as many times more as it is in `left`, less
/// (or up to) as many as in `right`; without, each once. Reads all of
/// `right` when the first row is asked for, and holds its rows.
pub(super) fn set_op<'a>(left: Rows<'a>, right: Rows<'a>, op: SetOp, all: bool) -> Rows<'a> {
    deferred(move || {
        let mut counts: HashMap<Row, usize> = HashMap::new();
        for row in right {
            *counts.entry(row?).or_default() += 1;
        }
        let mut returned: HashSet<Row> = HashSet::new();
        // Takes one of the right side's copies of the row, if one is left.
        fn take(counts: &mut HashMap<Row, usize>, row: &Row) -> bool {
            match counts.get_mut(row) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    true
                }
                _ => false,
            }
        }
        Ok(Box::new(left.filter_map(move |item| {
            let row = match item {
                Ok(row) => row,
                Err(e) => return Some(Err(e)),
            };
            let keep = match (op, all) {
                (SetOp::Intersect, true) => take(&mut counts, &row),
                (SetOp::Except, true) => !take(&mut counts, &row),
                (SetOp::Intersect, false) => !returned.contains(&row) && counts.contains_key(&row),
                (SetOp::Except, false) => !returned.contains(&row) && !counts.contains_key(&row),
                (SetOp::Union, _) => unreachable!("a union has steps of its own"),
            };
            if keep && !all {
                returned.insert(row.clone());
            }
            keep.then_some(Ok(row))
        })) as Rows<'a>)
    })
}

/// Joins each `left` row (whose columns are `left_fields`) with the rows
/// `query` returns for it, its outer references taking their values from
/// the row: planned and run for each left row, but for a row with the same
/// values as the last one, which reuses its rows. The query's columns are
/// `names`, qualified by `qualifier`.
#[allow(clippy::too_many_arguments)]
pub(super) fn lateral<'a>(
    left: Rows<'a>,
    left_fields: Vec<Field>,
    query: Select,
    qualifier: &str,
    names: &[String],
    kind: JoinKind,
    condition: Option<Expr>,
    catalog: &'a Catalog,
    settings: &Settings,
) -> Rows<'a> {
    let mut outer: Vec<ColumnRef> = Vec::new();
    visit_outer(&query, &mut |column| {
        if !outer.contains(column) {
            outer.push(column.clone());
        }
    });
    let right_fields: Vec<Field> = names
        .iter()
        .map(|name| Field {
            table: Some(qualifier.into()),
            name: name.clone(),
        })
        .collect();
    let width = right_fields.len();
    let fields = [left_fields.clone(), right_fields].concat();
    Box::new(LateralJoin {
        left,
        left_fields,
        query,
        outer,
        condition: condition.map(|c| locate(&c, &fields)),
        fields,
        width,
        kind,
        catalog,
        settings: settings.clone(),
        subqueries: Runner::new(catalog, settings),
        last: None,
        current: None,
    })
}

struct LateralJoin<'a> {
    left: Rows<'a>,
    left_fields: Vec<Field>,
    query: Select,
    /// The query's outer references, which the left row gives values.
    outer: Vec<ColumnRef>,
    condition: Option<Expr>,
    /// The left fields, then the query's: the fields of a joined row.
    fields: Vec<Field>,
    /// How many columns the query returns.
    width: usize,
    kind: JoinKind,
    catalog: &'a Catalog,
    settings: Settings,
    subqueries: Runner<'a>,
    /// The values of the outer references the query last ran with, and its rows.
    last: Option<(Vec<Value>, Rc<Vec<Row>>)>,
    /// The left row being joined, the query's rows for it, the next to pair,
    /// and whether one matched.
    current: Option<(Row, Rc<Vec<Row>>, usize, bool)>,
}

impl LateralJoin<'_> {
    /// The rows the query returns for `left`.
    fn run(&mut self, left: &Row) -> Result<Rc<Vec<Row>>> {
        let values = self
            .outer
            .iter()
            .map(|c| Ok(left[resolve(&self.left_fields, c)?].clone()))
            .collect::<Result<Vec<_>>>()?;
        if let Some((last, rows)) = &self.last
            && *last == values
        {
            return Ok(rows.clone());
        }
        let bound = query::substitute(&self.query, &mut |column| {
            let i = self.outer.iter().position(|c| c == column);
            Ok(values[i.expect("collected above")].clone())
        })?;
        // Run once per left row, it reads too little to be worth threads to scan.
        let choices = Choices {
            scan_workers: 1,
            ..self.settings.choices()
        };
        let plan = query::select(self.catalog, bound, choices)?;
        let rows = model::open(plan.root, self.catalog, &self.settings).collect::<Result<Vec<_>>>()?;
        let rows = Rc::new(rows);
        self.last = Some((values, rows.clone()));
        Ok(rows)
    }
}

impl Iterator for LateralJoin<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current.is_none() {
                let left = match self.left.next()? {
                    Ok(row) => row,
                    Err(e) => return Some(Err(e)),
                };
                match self.run(&left) {
                    Ok(rows) => self.current = Some((left, rows, 0, false)),
                    Err(e) => return Some(Err(e)),
                }
            }
            let (left, rows, next, matched) = self.current.as_mut().expect("set above");
            while *next < rows.len() {
                let joined: Row = left.iter().chain(&rows[*next]).cloned().collect();
                *next += 1;
                match matches_with(&self.condition, &self.fields, &joined, &mut self.subqueries) {
                    Ok(true) => {
                        *matched = true;
                        return Some(Ok(joined));
                    }
                    Ok(false) => {}
                    Err(e) => return Some(Err(e)),
                }
            }
            let (mut left, _, _, matched) = self.current.take().expect("set above");
            if self.kind == JoinKind::Left && !matched {
                left.extend(std::iter::repeat_n(Value::Null, self.width));
                return Some(Ok(left));
            }
        }
    }
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
    Box::new(Deferred {
        start: Some(Box::new(start)),
        rows: None,
    })
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
