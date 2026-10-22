//! Hash aggregation: folds rows into one row per group, for the aggregate
//! step of a plan (see `plan/query.rs`, which checks the query and rewrites
//! the expressions above the step).
//!
//! A hash table maps each group's key to its aggregates' running state, one
//! entry per group, held in memory. Rows with equal keys share a group, NULL
//! keys included, and groups come out in the order their first row was read.
//! Without `GROUP BY`, all rows form one group, which exists even when there
//! are none. Aggregates skip NULLs, except `COUNT(*)`; over no values, `COUNT`
//! is 0 and the others NULL. With `DISTINCT`, an aggregate remembers the
//! values it has added in each group and adds each only once. `SUM` is an
//! INT over INTs and a REAL once a REAL is added; `AVG` is a REAL.
//!
//! The groups may take a budget of memory ([`HASH_MEMORY`] by default).
//! Once they do, a row of a group already in memory still adds to it, but
//! a row of a new group is written out, its key and aggregates' arguments
//! only, to one of [`FANOUT`] partitions by a hash of its key (see
//! `spill.rs`). After the last row, the groups in memory come out, and then
//! each partition's are folded in turn the same way, a partition that
//! outgrows the budget too spilling into partitions of its own, by the next
//! bits of the hash. Groups of rows written out come after the others,
//! then, not in the order of their first rows. The values `DISTINCT`
//! remembers grow the groups in memory past the budget: they cannot be
//! written out.
//!
//! **In parallel.** Under a gather, each worker folds its own rows into
//! groups of its own (see `gather.rs`), and hands on their aggregates'
//! running states rather than their results, a row per group: its key,
//! then each state as values ([`Folder::finish_states`]). The query's
//! thread then merges the states of each group ([`merge`]): two counts add
//! up, two sums too, the lower of two minimums wins, two averages' sums and
//! counts add up. Within its budget as above, a row of a new group past it
//! written out, its key and states, to be merged with its partition's. A
//! `DISTINCT` aggregate's values are not merged so: the planner folds those
//! in one thread.
//!
//! Rows come to a [`Folder`] one at a time, or, from the vectorized
//! executor, a batch at a time: without `GROUP BY`, each aggregate then adds
//! a whole column of INTs or REALs in one loop ([`Folder::add_columns`]).
//! Over INTs, those loops are written for the compiler to turn into SIMD
//! instructions: a count adds up flags, a minimum or maximum folds the
//! column with a NULL's place taken by a value that cannot win, an average
//! sums each value's high and low 32 bits apart, exactly, and a sum adds
//! the column at once when no running total can overflow, which its lowest
//! and highest values tell; else it adds a value at a time, checking each.
//! REALs are summed in order, as their rounding depends on it.
//!
//! [`HASH_MEMORY`]: super::joins::HASH_MEMORY

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::operator::Rows;
use super::sort::value_size;
use super::spill::{FANOUT, MAX_LEVEL, RowFile, partition};
use super::vectorized::batch::Vector;
use crate::catalog::table::Row;
use crate::error::{Error, Result, err};
use crate::plan::query::AggregateCall;
use crate::sql::ast::{Aggregate, Expr};
use crate::sql::eval::{Field, eval};
use crate::types::Value;

/// Reads every row of `input` (whose columns are `columns`) and returns one
/// row per group: its `group_by` values, then the result of each aggregate,
/// holding groups within `budget` bytes, about.
pub fn fold(
    input: Rows,
    columns: &[Field],
    group_by: &[Expr],
    aggregates: &[AggregateCall],
    budget: usize,
) -> Result<Groups> {
    let (group_by, aggregates) = (
        crate::sql::eval::locate_all(group_by, columns),
        locate(aggregates, columns),
    );
    let mut folder = Folder::new(group_by.len(), &aggregates, budget);
    for row in input {
        folder.add_row(&row?, columns, &group_by)?;
    }
    Ok(folder.finish())
}

/// Reads every row of `input`, each a group's key of `keys` values and then
/// its aggregates' states as [`Folder::finish_states`] gives them, from
/// workers that folded rows of their own, and returns one row per group,
/// as [`fold`] does, its states merged.
pub fn merge(
    input: Rows,
    keys: usize,
    aggregates: &[AggregateCall],
    budget: usize,
) -> Result<Groups> {
    let mut folder = Folder::new(keys, aggregates, budget);
    for row in input {
        folder.merge_row(row?)?;
    }
    Ok(folder.finish())
}

/// The aggregates with the columns of their arguments found among `columns`
/// (see [`crate::sql::eval::locate`]).
pub fn locate(aggregates: &[AggregateCall], columns: &[Field]) -> Vec<AggregateCall> {
    aggregates
        .iter()
        .map(|(function, arg, distinct)| {
            (
                *function,
                arg.as_ref()
                    .map(|arg| crate::sql::eval::locate(arg, columns)),
                *distinct,
            )
        })
        .collect()
}

/// Folds rows into their groups, which [`Folder::finish`] returns.
pub struct Folder {
    table: Table,
    scratch: Vec<u8>,
    aggregates: Vec<AggregateCall>,
    /// How many `GROUP BY` expressions there are.
    keys: usize,
    budget: usize,
}

impl Folder {
    pub fn new(keys: usize, aggregates: &[AggregateCall], budget: usize) -> Self {
        Folder {
            table: Table::new(0),
            scratch: Vec::new(),
            aggregates: aggregates.to_vec(),
            keys,
            budget,
        }
    }

    /// Adds a row whose columns are `columns`, grouped by `group_by`.
    pub fn add_row(&mut self, row: &[Value], columns: &[Field], group_by: &[Expr]) -> Result<()> {
        let key: Row = group_by
            .iter()
            .map(|e| eval(e, columns, row))
            .collect::<Result<_>>()?;
        let Folder {
            table,
            scratch,
            aggregates,
            budget,
            ..
        } = self;
        let args = aggregates
            .iter()
            .map(|(_, arg, _)| arg.as_ref().map(|arg| eval(arg, columns, row)).transpose());
        table.add(key, args, aggregates, *budget, scratch)
    }

    /// Adds a row of this group key and these aggregate arguments (`None`
    /// for `COUNT(*)`), evaluated already.
    pub fn add(&mut self, key: Row, args: impl Iterator<Item = Option<Value>>) -> Result<()> {
        self.table.add(
            key,
            args.map(Ok),
            &self.aggregates,
            self.budget,
            &mut self.scratch,
        )
    }

    /// Adds `rows` rows to the one group of an aggregation without `GROUP
    /// BY`, an aggregate at a time, from their arguments' columns (`None`
    /// for `COUNT(*)`), if every aggregate can take a column at once: it is
    /// not `DISTINCT`, and its argument is all INTs or all REALs (NULLs
    /// aside). Returns false, having added nothing, if one cannot, or if
    /// adding them fails: they must then be added a row at a time, which
    /// fails at the row the error is in.
    pub fn add_columns(&mut self, rows: usize, args: &[Option<Rc<Vector>>]) -> bool {
        if self.keys > 0 || self.aggregates.iter().any(|(_, _, distinct)| *distinct) {
            return false;
        }
        let group = self.table.group(Vec::new(), &self.aggregates);
        let states = &mut self.table.groups[group].1;
        let mut added: Vec<Accumulator> = states
            .iter()
            .map(|state| state.accumulator.clone())
            .collect();
        for (accumulator, arg) in added.iter_mut().zip(args) {
            let result = match arg.as_deref() {
                None => {
                    accumulator.count_rows(rows);
                    Ok(())
                }
                Some(Vector::Int(values, nulls)) => accumulator.add_ints(values, nulls.as_deref()),
                Some(Vector::Real(values, nulls)) => {
                    accumulator.add_reals(values, nulls.as_deref())
                }
                Some(Vector::Const(Value::Null, _)) => Ok(()), // skipped by every aggregate
                Some(_) => return false,
            };
            if result.is_err() {
                return false;
            }
        }
        states
            .iter_mut()
            .zip(added)
            .for_each(|(state, accumulator)| state.accumulator = accumulator);
        true
    }

    /// Merges a row of a group's key and its aggregates' states, as
    /// [`Folder::finish_states`] gives them, into its group.
    pub fn merge_row(&mut self, row: Row) -> Result<()> {
        let mut values = row.into_iter();
        let key: Row = values.by_ref().take(self.keys).collect();
        let states = decode_states(&self.aggregates, &mut values)?;
        let Folder {
            table,
            scratch,
            aggregates,
            budget,
            ..
        } = self;
        table.merge(key, states, aggregates, *budget, scratch)
    }

    /// The groups, with their aggregates' results. Without `GROUP BY`, all
    /// rows form one group, which exists even when there are none.
    pub fn finish(self) -> Groups {
        self.finish_as(Output::Results)
    }

    /// The groups, with their aggregates' running states instead of their
    /// results, for [`merge`] to merge with other folders': each state as
    /// values, an `AVG` as four. Never with `DISTINCT`.
    pub fn finish_states(self) -> Groups {
        assert!(
            !self.aggregates.iter().any(|(_, _, distinct)| *distinct),
            "a DISTINCT aggregate's state is not merged"
        );
        self.finish_as(Output::States)
    }

    fn finish_as(mut self, output: Output) -> Groups {
        if self.keys == 0 && self.table.groups.is_empty() {
            self.table
                .groups
                .push((Vec::new(), fresh(&self.aggregates)));
        }
        let Folder {
            table,
            aggregates,
            keys,
            budget,
            ..
        } = self;
        let merging = table.merging;
        let mut groups = Groups {
            aggregates,
            keys,
            budget,
            output,
            merging,
            rows: Vec::new().into_iter(),
            pending: Vec::new(),
        };
        groups.start(table);
        groups
    }
}

/// What a group's row holds after its key: each aggregate's result, or its state.
#[derive(Clone, Copy, PartialEq)]
enum Output {
    Results,
    States,
}

/// The rows of the groups: those in memory, then those of each partition
/// written out, folded when their turn comes.
pub struct Groups {
    aggregates: Vec<AggregateCall>,
    /// How many group keys each row written out starts with.
    keys: usize,
    budget: usize,
    output: Output,
    /// Whether the rows written out hold states to merge, rather than
    /// arguments to add.
    merging: bool,
    rows: std::vec::IntoIter<Row>,
    /// Partitions written out, and the level to split each at if it must be.
    pending: Vec<(RowFile, u32)>,
}

impl Groups {
    /// Makes the groups of `table` the next to come out, and its partitions pending.
    fn start(&mut self, table: Table) {
        let level = table.level + 1;
        self.pending.extend(
            table
                .spilled
                .into_iter()
                .filter(|file| file.rows() > 0)
                .map(|file| (file, level)),
        );
        let rows: Vec<Row> = table
            .groups
            .into_iter()
            .map(|(mut row, states)| {
                match self.output {
                    Output::Results => {
                        row.extend(states.iter().map(|state| state.accumulator.result()))
                    }
                    Output::States => states
                        .iter()
                        .for_each(|state| state.accumulator.encode(&mut row)),
                }
                row
            })
            .collect();
        self.rows = rows.into_iter();
    }

    /// Folds the rows of a partition written out, each its key and then
    /// the arguments of the aggregates that have one, or when merging, the
    /// states of all of them.
    fn fold_partition(&mut self, file: RowFile, level: u32) -> Result<()> {
        let mut table = Table::new(level);
        let mut scratch = Vec::new();
        for record in file.read() {
            let mut values = record?.into_iter();
            let key: Row = values.by_ref().take(self.keys).collect();
            if self.merging {
                let states = decode_states(&self.aggregates, &mut values)?;
                table.merge(key, states, &self.aggregates, self.budget, &mut scratch)?;
                continue;
            }
            let args: Vec<Option<Value>> = self
                .aggregates
                .iter()
                .map(|(_, arg, _)| arg.as_ref().map(|_| values.next().expect("written")))
                .collect();
            table.add(
                key,
                args.into_iter().map(Ok),
                &self.aggregates,
                self.budget,
                &mut scratch,
            )?;
        }
        self.start(table);
        Ok(())
    }
}

impl Iterator for Groups {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.rows.next() {
                return Some(Ok(row));
            }
            let (file, level) = self.pending.pop()?;
            if let Err(e) = self.fold_partition(file, level) {
                self.pending.clear();
                return Some(Err(e));
            }
        }
    }
}

/// Groups by their keys, and the partitions the rows of new groups were
/// written to once they took the budget.
struct Table {
    level: u32,
    groups: Vec<(Row, Vec<State>)>,
    index: HashMap<Row, usize>,
    /// Bytes the groups take, about.
    bytes: usize,
    /// [`FANOUT`] partitions once rows were written out; none before.
    spilled: Vec<RowFile>,
    /// Whether it merges states, which then are what it writes out.
    merging: bool,
}

impl Table {
    fn new(level: u32) -> Self {
        Table {
            level,
            groups: Vec::new(),
            index: HashMap::new(),
            bytes: 0,
            spilled: Vec::new(),
            merging: false,
        }
    }

    /// Merges a group's aggregates' `states` into its group; writes them
    /// out with its key instead if it is a new group and the groups take
    /// `budget`, as [`Table::add`] does.
    fn merge(
        &mut self,
        key: Row,
        states: Vec<Accumulator>,
        aggregates: &[AggregateCall],
        budget: usize,
        scratch: &mut Vec<u8>,
    ) -> Result<()> {
        self.merging = true;
        let i = match self.index.get(&key) {
            Some(&i) => i,
            None if self.bytes > budget && self.level <= MAX_LEVEL => {
                if self.spilled.is_empty() {
                    self.spilled = (0..FANOUT).map(|_| RowFile::new()).collect();
                }
                let p = partition(&key, self.level, scratch);
                let mut record = key;
                states.iter().for_each(|state| state.encode(&mut record));
                return self.spilled[p].push(&record);
            }
            None => self.group(key, aggregates),
        };
        for (state, other) in self.groups[i].1.iter_mut().zip(states) {
            state.accumulator.merge(other)?;
        }
        Ok(())
    }

    /// Adds a row with this key and these aggregate arguments (`None` for
    /// `COUNT(*)`) to its group; writes it out instead if it is the first of
    /// its group and the groups take `budget` (unless there are no hash bits
    /// left to partition by).
    fn add(
        &mut self,
        key: Row,
        args: impl Iterator<Item = Result<Option<Value>>>,
        aggregates: &[AggregateCall],
        budget: usize,
        scratch: &mut Vec<u8>,
    ) -> Result<()> {
        let i = match self.index.get(&key) {
            Some(&i) => i,
            None if self.bytes > budget && self.level <= MAX_LEVEL => {
                if self.spilled.is_empty() {
                    self.spilled = (0..FANOUT).map(|_| RowFile::new()).collect();
                }
                let p = partition(&key, self.level, scratch);
                let mut record = key;
                for arg in args {
                    record.extend(arg?);
                }
                return self.spilled[p].push(&record);
            }
            None => self.group(key, aggregates),
        };
        for (state, value) in self.groups[i].1.iter_mut().zip(args) {
            let value = value?;
            if let (Some(seen), Some(value)) = (&mut state.seen, &value) {
                if !seen.insert(value.clone()) {
                    continue;
                }
                self.bytes += 16 + value_size(value);
            }
            state.accumulator.add(value)?;
        }
        Ok(())
    }
}

impl Table {
    /// The group with this key, made if there is none yet.
    fn group(&mut self, key: Row, aggregates: &[AggregateCall]) -> usize {
        if let Some(&i) = self.index.get(&key) {
            return i;
        }
        self.bytes +=
            2 * (24 + key.iter().map(value_size).sum::<usize>()) + 16 + 48 * aggregates.len();
        self.index.insert(key.clone(), self.groups.len());
        self.groups.push((key, fresh(aggregates)));
        self.groups.len() - 1
    }
}

/// The states of `aggregates` that [`Accumulator::encode`] wrote, read from `values`.
fn decode_states(
    aggregates: &[AggregateCall],
    values: &mut impl Iterator<Item = Value>,
) -> Result<Vec<Accumulator>> {
    aggregates
        .iter()
        .map(|(function, _, _)| Accumulator::decode(*function, values))
        .collect()
}

/// The states of a new group's aggregates.
fn fresh(aggregates: &[AggregateCall]) -> Vec<State> {
    aggregates
        .iter()
        .map(|(function, _, distinct)| State::new(*function, *distinct))
        .collect()
}

/// One aggregate in one group: its accumulator, and for `DISTINCT` the
/// argument values already added (not kept for MIN and MAX, which repeats
/// cannot change).
struct State {
    accumulator: Accumulator,
    seen: Option<HashSet<Value>>,
}

impl State {
    fn new(function: Aggregate, distinct: bool) -> Self {
        let tracks = distinct && !matches!(function, Aggregate::Min | Aggregate::Max);
        State {
            accumulator: Accumulator::new(function),
            seen: tracks.then(HashSet::new),
        }
    }
}

/// The running state of one aggregate in one group.
#[derive(Clone)]
enum Accumulator {
    Count(i64),
    /// An INT while every value added is, a REAL once one is.
    Sum(Option<Value>),
    Min(Option<Value>),
    Max(Option<Value>),
    /// INTs summed wider than INT, so that only the average must fit in one;
    /// REALs summed apart.
    Avg {
        ints: i128,
        reals: f64,
        count: i64,
    },
}

impl Accumulator {
    fn new(function: Aggregate) -> Self {
        match function {
            Aggregate::Count => Accumulator::Count(0),
            Aggregate::Sum => Accumulator::Sum(None),
            Aggregate::Min => Accumulator::Min(None),
            Aggregate::Max => Accumulator::Max(None),
            Aggregate::Avg => Accumulator::Avg {
                ints: 0,
                reals: 0.0,
                count: 0,
            },
        }
    }

    /// Adds one row's argument value; `None` is a row of `COUNT(*)`.
    fn add(&mut self, value: Option<Value>) -> Result<()> {
        let value = match value {
            None => {
                if let Accumulator::Count(n) = self {
                    *n += 1;
                }
                return Ok(());
            }
            Some(Value::Null) => return Ok(()), // skipped by every aggregate
            Some(value) => value,
        };
        match self {
            Accumulator::Count(n) => *n += 1,
            Accumulator::Sum(sum) => {
                number(&value, "SUM")?;
                *sum = Some(match (sum.take(), value) {
                    (None, value) => value,
                    (Some(Value::Int(a)), Value::Int(b)) => Value::Int(
                        a.checked_add(b)
                            .ok_or_else(|| Error("integer overflow".into()))?,
                    ),
                    (Some(a), b) => {
                        let total = a.as_real().expect("a number") + b.as_real().expect("a number");
                        if !total.is_finite() {
                            return err("REAL overflow");
                        }
                        Value::Real(total)
                    }
                });
            }
            Accumulator::Avg { ints, reals, count } => {
                match number(&value, "AVG")? {
                    Value::Int(n) => *ints += *n as i128,
                    other => *reals += other.as_real().expect("a number"),
                }
                *count += 1;
            }
            Accumulator::Min(best) => keep(best, value, Ordering::Less)?,
            Accumulator::Max(best) => keep(best, value, Ordering::Greater)?,
        }
        Ok(())
    }

    /// Adds rows of `COUNT(*)`.
    fn count_rows(&mut self, rows: usize) {
        if let Accumulator::Count(n) = self {
            *n += rows as i64;
        }
    }

    /// Adds the INTs that are not NULL, in order, as [`Accumulator::add`]
    /// would one by one.
    fn add_ints(&mut self, values: &[i64], nulls: Option<&[bool]>) -> Result<()> {
        let count = present_count(values.len(), nulls);
        match self {
            Accumulator::Count(n) => {
                *n += count as i64;
                return Ok(());
            }
            Accumulator::Avg { ints, count: c, .. } => {
                *ints += exact_sum(values, nulls);
                *c += count as i64;
                return Ok(());
            }
            Accumulator::Min(best) if count > 0 => {
                let min = fold_masked(values, nulls, i64::MAX, i64::MAX, i64::min);
                return keep(best, Value::Int(min), Ordering::Less);
            }
            Accumulator::Max(best) if count > 0 => {
                let max = fold_masked(values, nulls, i64::MIN, i64::MIN, i64::max);
                return keep(best, Value::Int(max), Ordering::Greater);
            }
            Accumulator::Min(_) | Accumulator::Max(_) => return Ok(()),
            Accumulator::Sum(sum @ (None | Some(Value::Int(_)))) => {
                if count == 0 {
                    return Ok(());
                }
                let start = match sum {
                    Some(Value::Int(total)) => *total,
                    _ => 0,
                };
                if no_running_total_overflows(start, values, nulls) {
                    let total = fold_masked(values, nulls, 0, start, i64::wrapping_add);
                    *sum = Some(Value::Int(total));
                    return Ok(());
                }
            }
            Accumulator::Sum(_) => {}
        }
        let mut present = values
            .iter()
            .enumerate()
            .filter(|(i, _)| nulls.is_none_or(|nulls| !nulls[*i]))
            .map(|(_, &n)| n);
        match self {
            Accumulator::Count(n) => *n += present.count() as i64,
            Accumulator::Sum(sum) => loop {
                match sum {
                    None => match present.next() {
                        Some(n) => *sum = Some(Value::Int(n)),
                        None => break,
                    },
                    Some(Value::Int(total)) => {
                        for n in present.by_ref() {
                            *total = total
                                .checked_add(n)
                                .ok_or_else(|| Error("integer overflow".into()))?;
                        }
                        break;
                    }
                    Some(Value::Real(total)) => {
                        for n in present.by_ref() {
                            *total = finite(*total + n as f64)?;
                        }
                        break;
                    }
                    Some(_) => unreachable!("a sum is a number"),
                }
            },
            Accumulator::Avg { ints, count, .. } => {
                for n in present {
                    *ints += n as i128;
                    *count += 1;
                }
            }
            Accumulator::Min(best) => {
                if let Some(n) = present.min() {
                    keep(best, Value::Int(n), Ordering::Less)?;
                }
            }
            Accumulator::Max(best) => {
                if let Some(n) = present.max() {
                    keep(best, Value::Int(n), Ordering::Greater)?;
                }
            }
        }
        Ok(())
    }

    /// Adds the REALs that are not NULL, in order, as [`Accumulator::add`]
    /// would one by one.
    fn add_reals(&mut self, values: &[f64], nulls: Option<&[bool]>) -> Result<()> {
        let mut present = values
            .iter()
            .enumerate()
            .filter(|(i, _)| nulls.is_none_or(|nulls| !nulls[*i]))
            .map(|(_, &r)| r);
        // The first of the values that compares to all the others as `wanted`
        // or equal, as `keep` keeps.
        if let Accumulator::Count(n) = self {
            *n += present_count(values.len(), nulls) as i64;
            return Ok(());
        }
        let best = |values: &mut dyn Iterator<Item = f64>, wanted: Ordering| {
            values.fold(None, |best: Option<f64>, r| match best {
                Some(b) if r.partial_cmp(&b) != Some(wanted) => Some(b),
                _ => Some(r),
            })
        };
        match self {
            Accumulator::Count(n) => *n += present.count() as i64,
            Accumulator::Sum(sum) => {
                let Some(first) = present.next() else {
                    return Ok(());
                };
                let mut total = match sum {
                    Some(total) => finite(total.as_real().expect("a sum is a number") + first)?,
                    None => first,
                };
                for r in present {
                    total = finite(total + r)?;
                }
                *sum = Some(Value::Real(total));
            }
            Accumulator::Avg { reals, count, .. } => {
                for r in present {
                    *reals += r;
                    *count += 1;
                }
            }
            Accumulator::Min(min) => {
                if let Some(r) = best(&mut present, Ordering::Less) {
                    keep(min, Value::Real(r), Ordering::Less)?;
                }
            }
            Accumulator::Max(max) => {
                if let Some(r) = best(&mut present, Ordering::Greater) {
                    keep(max, Value::Real(r), Ordering::Greater)?;
                }
            }
        }
        Ok(())
    }

    /// Adds the values this state stands for to `out`, for [`Accumulator::decode`].
    fn encode(&self, out: &mut Row) {
        match self {
            Accumulator::Count(n) => out.push(Value::Int(*n)),
            Accumulator::Sum(value) | Accumulator::Min(value) | Accumulator::Max(value) => {
                out.push(value.clone().unwrap_or(Value::Null))
            }
            // The sum of INTs, an i128, as its high and low 64 bits.
            Accumulator::Avg { ints, reals, count } => out.extend([
                Value::Int((*ints >> 64) as i64),
                Value::Int(*ints as i64),
                Value::Real(*reals),
                Value::Int(*count),
            ]),
        }
    }

    /// The state of `function` that [`Accumulator::encode`] wrote, read
    /// from `values`.
    fn decode(function: Aggregate, values: &mut impl Iterator<Item = Value>) -> Result<Self> {
        let mut next = || {
            values
                .next()
                .ok_or_else(|| Error("an aggregate's state is cut short".into()))
        };
        let present = |value: Value| (!value.is_null()).then_some(value);
        Ok(match function {
            Aggregate::Count => match next()? {
                Value::Int(n) => Accumulator::Count(n),
                other => return err(format!("not a count: {other:?}")),
            },
            Aggregate::Sum => Accumulator::Sum(present(next()?)),
            Aggregate::Min => Accumulator::Min(present(next()?)),
            Aggregate::Max => Accumulator::Max(present(next()?)),
            Aggregate::Avg => match (next()?, next()?, next()?, next()?) {
                (Value::Int(high), Value::Int(low), Value::Real(reals), Value::Int(count)) => {
                    Accumulator::Avg {
                        ints: ((high as i128) << 64) | (low as u64 as i128),
                        reals,
                        count,
                    }
                }
                other => return err(format!("not an average's state: {other:?}")),
            },
        })
    }

    /// Adds another state of the same aggregate to this one, as if its
    /// values had been added to it.
    fn merge(&mut self, other: Accumulator) -> Result<()> {
        match (self, other) {
            (Accumulator::Count(a), Accumulator::Count(b)) => {
                *a = a
                    .checked_add(b)
                    .ok_or_else(|| Error("integer overflow".into()))?
            }
            // Another sum is added as a value of the sum's would be.
            (sum @ Accumulator::Sum(_), Accumulator::Sum(Some(value))) => sum.add(Some(value))?,
            (Accumulator::Min(best), Accumulator::Min(Some(value))) => {
                keep(best, value, Ordering::Less)?
            }
            (Accumulator::Max(best), Accumulator::Max(Some(value))) => {
                keep(best, value, Ordering::Greater)?
            }
            (
                Accumulator::Avg { ints, reals, count },
                Accumulator::Avg {
                    ints: i,
                    reals: r,
                    count: c,
                },
            ) => {
                *ints += i;
                *reals += r;
                *count += c;
            }
            // A sum, minimum or maximum of no values adds nothing.
            (Accumulator::Sum(_), Accumulator::Sum(None))
            | (Accumulator::Min(_), Accumulator::Min(None))
            | (Accumulator::Max(_), Accumulator::Max(None)) => {}
            _ => unreachable!("states of the same aggregate"),
        }
        Ok(())
    }

    fn result(&self) -> Value {
        match self {
            Accumulator::Count(n) => Value::Int(*n),
            Accumulator::Sum(sum) => sum.clone().unwrap_or(Value::Null),
            Accumulator::Min(best) | Accumulator::Max(best) => best.clone().unwrap_or(Value::Null),
            Accumulator::Avg { count: 0, .. } => Value::Null,
            Accumulator::Avg { ints, reals, count } => {
                Value::Real((*ints as f64 + reals) / *count as f64)
            }
        }
    }
}

/// How many of `n` values are not NULL, counting the flags with no branch.
fn present_count(n: usize, nulls: Option<&[bool]>) -> usize {
    match nulls {
        Some(nulls) => n - nulls.iter().fold(0, |count, &null| count + null as usize),
        None => n,
    }
}

/// Whether adding the values to `start` one by one keeps every running
/// total an INT: true if each value is smaller in size than the room left
/// above `start`, shared among them, rounded down to a power of two, 2^k.
/// That every value lies in -2^k..2^k is one check of all at once, as the
/// bits from k + 1 up of `v + 2^k`, ORed together, are all zero then.
fn no_running_total_overflows(start: i64, values: &[i64], nulls: Option<&[bool]>) -> bool {
    let room = i64::MAX as i128 - (start as i128).abs();
    let share = room / values.len().max(1) as i128;
    if share < 1 {
        return false;
    }
    let k = (127 - share.leading_zeros()).min(62);
    let offset = 1i64 << k;
    let outside = fold_masked(values, nulls, 0, 0, |acc, v| {
        acc | ((v.wrapping_add(offset) as u64) >> (k + 1)) as i64
    });
    outside == 0
}

/// `f` folded over the values from `init`, each NULL's place taken by
/// `null`: one loop with no branch, without NULLs and with them.
fn fold_masked(
    values: &[i64],
    nulls: Option<&[bool]>,
    null: i64,
    init: i64,
    f: impl Fn(i64, i64) -> i64,
) -> i64 {
    match nulls {
        None => values.iter().fold(init, |acc, &v| f(acc, v)),
        Some(nulls) => values.iter().zip(nulls).fold(init, |acc, (&v, &is_null)| {
            f(acc, if is_null { null } else { v })
        }),
    }
}

/// The sum of the values that are not NULL, exactly: their high 32 bits
/// and their low 32 bits summed apart, neither of which can overflow for
/// fewer than 2^31 values.
fn exact_sum(values: &[i64], nulls: Option<&[bool]>) -> i128 {
    let high = fold_masked(values, nulls, 0, 0, |acc, v| acc.wrapping_add(v >> 32));
    let low = fold_masked(values, nulls, 0, 0, |acc, v| {
        (acc as u64).wrapping_add(v as u32 as u64) as i64
    }) as u64;
    ((high as i128) << 32) + low as i128
}

/// A sum of REALs, if it is not too big to be one.
fn finite(total: f64) -> Result<f64> {
    if total.is_finite() {
        Ok(total)
    } else {
        err("REAL overflow")
    }
}

/// Replaces `best` by `value` if there is none yet or `value` compares to it as `wanted`.
fn keep(best: &mut Option<Value>, value: Value, wanted: Ordering) -> Result<()> {
    if let Some(current) = best {
        match value.compare(current) {
            Some(ord) if ord != wanted => return Ok(()),
            Some(_) => {}
            None => {
                return err(format!(
                    "cannot compare {} and {}",
                    value.to_sql(),
                    current.to_sql()
                ));
            }
        }
    }
    *best = Some(value);
    Ok(())
}

fn number<'v>(value: &'v Value, function: &str) -> Result<&'v Value> {
    match value {
        Value::Int(_) | Value::Real(_) => Ok(value),
        other => err(format!(
            "{function} expects a number, got {}",
            other.to_sql()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Running totals one value at a time, as a plain loop checks them.
    fn running_totals_fit(start: i64, values: &[i64], nulls: Option<&[bool]>) -> bool {
        let mut total = start;
        for (i, &v) in values.iter().enumerate() {
            if nulls.is_some_and(|nulls| nulls[i]) {
                continue;
            }
            match total.checked_add(v) {
                Some(t) => total = t,
                None => return false,
            }
        }
        true
    }

    #[test]
    fn column_loops_sum_exactly_and_never_miss_an_overflow() {
        let edges = [
            i64::MAX,
            i64::MIN,
            i64::MAX - 1,
            i64::MIN + 1,
            -1,
            0,
            1,
            1 << 40,
            -(1 << 40),
        ];
        let columns: Vec<Vec<i64>> = vec![
            edges.to_vec(),
            vec![i64::MAX, 1, -1],
            vec![1 << 52; 1024],
            vec![-(1 << 52); 1024],
            (0..1000).map(|i| i * 1_000_003 - 500_000_000).collect(),
            vec![],
        ];
        for values in &columns {
            let every_third: Vec<bool> = (0..values.len()).map(|i| i % 3 == 0).collect();
            for nulls in [None, Some(&every_third[..])] {
                let present = values
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| nulls.is_none_or(|nulls: &[bool]| !nulls[*i]));
                let expected: i128 = present.map(|(_, &v)| v as i128).sum();
                assert_eq!(exact_sum(values, nulls), expected, "{values:?}");
                for start in [0, 1, -1, i64::MAX, i64::MIN, 1 << 62, -(1 << 62)] {
                    // It may say no where the totals would fit, never yes where one would not.
                    if no_running_total_overflows(start, values, nulls) {
                        assert!(
                            running_totals_fit(start, values, nulls),
                            "{start} {values:?}"
                        );
                    }
                }
            }
        }
        // Small values from a small start: the fast way.
        assert!(no_running_total_overflows(0, &[5; 1024], None));
        assert!(no_running_total_overflows(-7, &[-(1 << 50); 1024], None));
    }
}
