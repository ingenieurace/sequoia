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
//! [`HASH_MEMORY`]: super::join::HASH_MEMORY

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use super::operator::Rows;
use super::sort::value_size;
use super::spill::{FANOUT, MAX_LEVEL, RowFile, partition};
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
    let mut table = Table::new(0);
    let mut scratch = Vec::new();
    for row in input {
        let row = row?;
        let key: Row = group_by.iter().map(|e| eval(e, columns, &row)).collect::<Result<_>>()?;
        let args = aggregates.iter().map(|(_, arg, _)| arg.as_ref().map(|arg| eval(arg, columns, &row)).transpose());
        table.add(key, args, aggregates, budget, &mut scratch)?;
    }
    if group_by.is_empty() && table.groups.is_empty() {
        table.groups.push((Vec::new(), fresh(aggregates)));
    }
    let mut groups = Groups { aggregates: aggregates.to_vec(), keys: group_by.len(), budget, rows: Vec::new().into_iter(), pending: Vec::new() };
    groups.start(table);
    Ok(groups)
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
            None => {
                self.bytes += 2 * (24 + key.iter().map(value_size).sum::<usize>()) + 16 + 48 * aggregates.len();
                self.index.insert(key.clone(), self.groups.len());
                self.groups.push((key, fresh(aggregates)));
                self.groups.len() - 1
            }
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
