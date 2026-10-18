//! Hash aggregation: folds rows into one row per group, for the aggregate
//! step of a plan (see `plan/query.rs`, which checks the query and rewrites
//! the expressions above the step).
//!
//! A hash table maps each group's key to its aggregates' running state, one
//! entry per group, held in memory. Rows with equal keys share a group, NULL
//! keys included, and groups come out in the order their first row was read.
//! Without `GROUP BY`, all rows form one group, which exists even when there
//! are none. Aggregates skip NULLs, except `COUNT(*)`; over no values, `COUNT`
//! is 0 and the others NULL. There is no fractional type, so `AVG` truncates
//! toward zero like `/`.

use std::cmp::Ordering;
use std::collections::HashMap;

use super::operator::Rows;
use crate::catalog::table::Row;
use crate::error::{Error, Result, err};
use crate::plan::query::AggregateCall;
use crate::sql::ast::{Aggregate, Expr};
use crate::sql::eval::eval;
use crate::types::{Column, Value};

/// Reads every row of `input` (whose columns are `columns`) and returns one
/// row per group: its `group_by` values, then the result of each aggregate.
pub fn fold(input: Rows, columns: &[Column], group_by: &[Expr], aggregates: &[AggregateCall]) -> Result<Vec<Row>> {
    let fresh = || aggregates.iter().map(|(function, _)| Accumulator::new(*function)).collect::<Vec<_>>();
    let mut groups: Vec<(Row, Vec<Accumulator>)> = Vec::new();
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
        for ((_, arg), accumulator) in aggregates.iter().zip(&mut groups[i].1) {
            let value = arg.as_ref().map(|arg| eval(arg, columns, &row)).transpose()?;
            accumulator.add(value)?;
        }
    }
    if group_by.is_empty() && groups.is_empty() {
        groups.push((Vec::new(), fresh()));
    }
    Ok(groups
        .into_iter()
        .map(|(mut row, accumulators)| {
            row.extend(accumulators.iter().map(Accumulator::result));
            row
        })
        .collect())
}

/// The running state of one aggregate in one group.
enum Accumulator {
    Count(i64),
    Sum(Option<i64>),
    Min(Option<Value>),
    Max(Option<Value>),
    /// Summed wider than INT, so that only the average must fit in one.
    Avg { sum: i128, count: i64 },
}

impl Accumulator {
    fn new(function: Aggregate) -> Self {
        match function {
            Aggregate::Count => Accumulator::Count(0),
            Aggregate::Sum => Accumulator::Sum(None),
            Aggregate::Min => Accumulator::Min(None),
            Aggregate::Max => Accumulator::Max(None),
            Aggregate::Avg => Accumulator::Avg { sum: 0, count: 0 },
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
                let n = int(&value, "SUM")?;
                *sum = Some(sum.unwrap_or(0).checked_add(n).ok_or_else(|| Error("integer overflow".into()))?);
            }
            Accumulator::Avg { sum, count } => {
                *sum += int(&value, "AVG")? as i128;
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
            Accumulator::Sum(sum) => sum.map_or(Value::Null, Value::Int),
            Accumulator::Min(best) | Accumulator::Max(best) => best.clone().unwrap_or(Value::Null),
            Accumulator::Avg { count: 0, .. } => Value::Null,
            // |sum / count| <= max |value|, so the average fits.
            Accumulator::Avg { sum, count } => Value::Int((sum / *count as i128) as i64),
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

fn int(value: &Value, function: &str) -> Result<i64> {
    match value {
        Value::Int(n) => Ok(*n),
        other => err(format!("{function} expects INT, got {}", other.to_sql())),
    }
}
