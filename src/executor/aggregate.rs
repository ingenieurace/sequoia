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
//! Rows come to a [`Folder`] one at a time, or, from the vectorized
//! executor, a batch at a time: without `GROUP BY`, each aggregate then adds
//! a whole column of INTs or REALs in one loop ([`Folder::add_columns`]).
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
pub fn fold(input: Rows, columns: &[Field], group_by: &[Expr], aggregates: &[AggregateCall], budget: usize) -> Result<Groups> {
    let (group_by, aggregates) = (crate::sql::eval::locate_all(group_by, columns), locate(aggregates, columns));
    let mut folder = Folder::new(group_by.len(), &aggregates, budget);
    for row in input {
        folder.add_row(&row?, columns, &group_by)?;
    }
    Ok(folder.finish())
}

/// The aggregates with the columns of their arguments found among `columns`
/// (see [`crate::sql::eval::locate`]).
pub fn locate(aggregates: &[AggregateCall], columns: &[Field]) -> Vec<AggregateCall> {
    aggregates
        .iter()
        .map(|(function, arg, distinct)| (*function, arg.as_ref().map(|arg| crate::sql::eval::locate(arg, columns)), *distinct))
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
        Folder { table: Table::new(0), scratch: Vec::new(), aggregates: aggregates.to_vec(), keys, budget }
    }

    /// Adds a row whose columns are `columns`, grouped by `group_by`.
    pub fn add_row(&mut self, row: &[Value], columns: &[Field], group_by: &[Expr]) -> Result<()> {
        let key: Row = group_by.iter().map(|e| eval(e, columns, row)).collect::<Result<_>>()?;
        let Folder { table, scratch, aggregates, budget, .. } = self;
        let args = aggregates.iter().map(|(_, arg, _)| arg.as_ref().map(|arg| eval(arg, columns, row)).transpose());
        table.add(key, args, aggregates, *budget, scratch)
    }

    /// Adds a row of this group key and these aggregate arguments (`None`
    /// for `COUNT(*)`), evaluated already.
    pub fn add(&mut self, key: Row, args: impl Iterator<Item = Option<Value>>) -> Result<()> {
        self.table.add(key, args.map(Ok), &self.aggregates, self.budget, &mut self.scratch)
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
        let mut added: Vec<Accumulator> = states.iter().map(|state| state.accumulator.clone()).collect();
        for (accumulator, arg) in added.iter_mut().zip(args) {
            let result = match arg.as_deref() {
                None => {
                    accumulator.count_rows(rows);
                    Ok(())
                }
                Some(Vector::Int(values, nulls)) => accumulator.add_ints(values, nulls.as_deref()),
                Some(Vector::Real(values, nulls)) => accumulator.add_reals(values, nulls.as_deref()),
                Some(Vector::Const(Value::Null, _)) => Ok(()), // skipped by every aggregate
                Some(_) => return false,
            };
            if result.is_err() {
                return false;
            }
        }
        states.iter_mut().zip(added).for_each(|(state, accumulator)| state.accumulator = accumulator);
        true
    }

    /// The groups, with their aggregates' results. Without `GROUP BY`, all
    /// rows form one group, which exists even when there are none.
    pub fn finish(mut self) -> Groups {
        if self.keys == 0 && self.table.groups.is_empty() {
            self.table.groups.push((Vec::new(), fresh(&self.aggregates)));
        }
        let Folder { table, aggregates, keys, budget, .. } = self;
        let mut groups = Groups { aggregates, keys, budget, rows: Vec::new().into_iter(), pending: Vec::new() };
        groups.start(table);
        groups
    }
}

/// The rows of the groups: those in memory, then those of each partition
/// written out, folded when their turn comes.
pub struct Groups {
    aggregates: Vec<AggregateCall>,
    /// How many group keys each row written out starts with.
    keys: usize,
    budget: usize,
    rows: std::vec::IntoIter<Row>,
    /// Partitions written out, and the level to split each at if it must be.
    pending: Vec<(RowFile, u32)>,
}

impl Groups {
    /// Makes the groups of `table` the next to come out, and its partitions pending.
    fn start(&mut self, table: Table) {
        let level = table.level + 1;
        self.pending.extend(table.spilled.into_iter().filter(|file| file.rows() > 0).map(|file| (file, level)));
        let rows: Vec<Row> = table
            .groups
            .into_iter()
            .map(|(mut row, states)| {
                row.extend(states.iter().map(|state| state.accumulator.result()));
                row
            })
            .collect();
        self.rows = rows.into_iter();
    }

    /// Folds the rows of a partition written out, each its key and then
    /// the arguments of the aggregates that have one.
    fn fold_partition(&mut self, file: RowFile, level: u32) -> Result<()> {
        let mut table = Table::new(level);
        let mut scratch = Vec::new();
        for record in file.read() {
            let mut values = record?.into_iter();
            let key: Row = values.by_ref().take(self.keys).collect();
            let args: Vec<Option<Value>> =
                self.aggregates.iter().map(|(_, arg, _)| arg.as_ref().map(|_| values.next().expect("written"))).collect();
            table.add(key, args.into_iter().map(Ok), &self.aggregates, self.budget, &mut scratch)?;
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
}

impl Table {
    fn new(level: u32) -> Self {
        Table { level, groups: Vec::new(), index: HashMap::new(), bytes: 0, spilled: Vec::new() }
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
        self.bytes += 2 * (24 + key.iter().map(value_size).sum::<usize>()) + 16 + 48 * aggregates.len();
        self.index.insert(key.clone(), self.groups.len());
        self.groups.push((key, fresh(aggregates)));
        self.groups.len() - 1
    }
}

/// The states of a new group's aggregates.
fn fresh(aggregates: &[AggregateCall]) -> Vec<State> {
    aggregates.iter().map(|(function, _, distinct)| State::new(*function, *distinct)).collect()
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
        State { accumulator: Accumulator::new(function), seen: tracks.then(HashSet::new) }
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
    Avg { ints: i128, reals: f64, count: i64 },
}

impl Accumulator {
    fn new(function: Aggregate) -> Self {
        match function {
            Aggregate::Count => Accumulator::Count(0),
            Aggregate::Sum => Accumulator::Sum(None),
            Aggregate::Min => Accumulator::Min(None),
            Aggregate::Max => Accumulator::Max(None),
            Aggregate::Avg => Accumulator::Avg { ints: 0, reals: 0.0, count: 0 },
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
                    (Some(Value::Int(a)), Value::Int(b)) => {
                        Value::Int(a.checked_add(b).ok_or_else(|| Error("integer overflow".into()))?)
                    }
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
        let mut present = values.iter().enumerate().filter(|(i, _)| nulls.is_none_or(|nulls| !nulls[*i])).map(|(_, &n)| n);
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
                            *total = total.checked_add(n).ok_or_else(|| Error("integer overflow".into()))?;
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
        let mut present = values.iter().enumerate().filter(|(i, _)| nulls.is_none_or(|nulls| !nulls[*i])).map(|(_, &r)| r);
        // The first of the values that compares to all the others as `wanted`
        // or equal, as `keep` keeps.
        let best = |values: &mut dyn Iterator<Item = f64>, wanted: Ordering| {
            values.fold(None, |best: Option<f64>, r| match best {
                Some(b) if r.partial_cmp(&b) != Some(wanted) => Some(b),
                _ => Some(r),
            })
        };
        match self {
            Accumulator::Count(n) => *n += present.count() as i64,
            Accumulator::Sum(sum) => {
                let Some(first) = present.next() else { return Ok(()) };
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

    fn result(&self) -> Value {
        match self {
            Accumulator::Count(n) => Value::Int(*n),
            Accumulator::Sum(sum) => sum.clone().unwrap_or(Value::Null),
            Accumulator::Min(best) | Accumulator::Max(best) => best.clone().unwrap_or(Value::Null),
            Accumulator::Avg { count: 0, .. } => Value::Null,
            Accumulator::Avg { ints, reals, count } => Value::Real((*ints as f64 + reals) / *count as f64),
        }
    }
}

/// A sum of REALs, if it is not too big to be one.
fn finite(total: f64) -> Result<f64> {
    if total.is_finite() { Ok(total) } else { err("REAL overflow") }
}

/// Replaces `best` by `value` if there is none yet or `value` compares to it as `wanted`.
fn keep(best: &mut Option<Value>, value: Value, wanted: Ordering) -> Result<()> {
    if let Some(current) = best {
        match value.compare(current) {
            Some(ord) if ord != wanted => return Ok(()),
            Some(_) => {}
            None => return err(format!("cannot compare {} and {}", value.to_sql(), current.to_sql())),
        }
    }
    *best = Some(value);
    Ok(())
}

fn number<'v>(value: &'v Value, function: &str) -> Result<&'v Value> {
    match value {
        Value::Int(_) | Value::Real(_) => Ok(value),
        other => err(format!("{function} expects a number, got {}", other.to_sql())),
    }
}
