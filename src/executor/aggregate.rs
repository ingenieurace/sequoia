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

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use super::operator::Rows;
use crate::catalog::table::Row;
use crate::error::{Error, Result, err};
use crate::plan::query::AggregateCall;
use crate::sql::ast::{Aggregate, Expr};
use crate::sql::eval::{Field, eval};
use crate::types::Value;

/// Reads every row of `input` (whose columns are `columns`) and returns one
/// row per group: its `group_by` values, then the result of each aggregate.
pub fn fold(input: Rows, columns: &[Field], group_by: &[Expr], aggregates: &[AggregateCall]) -> Result<Vec<Row>> {
    let fresh = || aggregates.iter().map(|(function, _, distinct)| State::new(*function, *distinct)).collect::<Vec<_>>();
    let mut groups: Vec<(Row, Vec<State>)> = Vec::new();
    let mut index: HashMap<Row, usize> = HashMap::new();
    for row in input {
        let row = row?;
        let key: Row = group_by.iter().map(|e| eval(e, columns, &row)).collect::<Result<_>>()?;
        let i = match index.get(&key) {
            Some(&i) => i,
            None => {
                index.insert(key.clone(), groups.len());
                groups.push((key, fresh()));
                groups.len() - 1
            }
        };
        for ((_, arg, _), state) in aggregates.iter().zip(&mut groups[i].1) {
            let value = arg.as_ref().map(|arg| eval(arg, columns, &row)).transpose()?;
            if let (Some(seen), Some(value)) = (&mut state.seen, &value)
                && !seen.insert(value.clone())
            {
                continue;
            }
            state.accumulator.add(value)?;
        }
    }
    if group_by.is_empty() && groups.is_empty() {
        groups.push((Vec::new(), fresh()));
    }
    Ok(groups
        .into_iter()
        .map(|(mut row, states)| {
            row.extend(states.iter().map(|state| state.accumulator.result()));
            row
        })
        .collect())
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
