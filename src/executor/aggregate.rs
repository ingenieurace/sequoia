//! Grouped SELECTs: `GROUP BY`, `HAVING` and the aggregate functions
//! `COUNT`, `SUM`, `MIN`, `MAX` and `AVG`, by hash aggregation.
//!
//! ```text
//! SELECT city, COUNT(*), MAX(age) FROM users WHERE active GROUP BY city HAVING COUNT(*) > 1
//!
//! scan + WHERE --> group key (city) --> hash table: key -> [count, max]
//!                                                      |
//!         one row per group: [#g0 = city, #a0 = COUNT(*), #a1 = MAX(age)]
//!                                                      |
//!                         HAVING, ORDER BY, LIMIT, then the select list
//! ```
//!
//! Before anything runs, each expression of the select list, `HAVING` and
//! `ORDER BY` is rewritten to read a group's row instead of a table row: a
//! `GROUP BY` expression becomes its column `#g<i>`, and an aggregate becomes
//! its column `#a<j>` (the same aggregate written twice is computed once).
//! A column left over is neither grouped nor aggregated, which SQL forbids.
//! Names starting with `#` cannot be written in SQL, so they never clash.
//!
//! Rows with equal group keys share a group, NULL keys included. Groups come
//! out in the order their first row was read. Without `GROUP BY`, all rows
//! form one group, which exists even when there are none. Aggregates skip
//! NULLs, except `COUNT(*)`; over no values, `COUNT` is 0 and the others NULL.
//! There is no fractional type, so `AVG` truncates toward zero like `/`.
//!
//! The hash table is kept in memory: one entry per group.

use std::cmp::Ordering;
use std::collections::HashMap;

use super::sort::Sorter;
use super::{Settings, scan};
use crate::catalog::table::{Row, Table};
use crate::error::{Error, Result, err};
use crate::plan::filter;
use crate::result::QueryResult;
use crate::sql::ast::{Aggregate, Expr, OrderBy, Projection};
use crate::sql::eval::{eval, matches};
use crate::types::{Column, DataType, Value};

/// Whether a SELECT groups its rows: it has `GROUP BY` or uses an aggregate.
pub fn is_grouped(projection: &Projection, group_by: &[Expr], having: &Option<Expr>, order_by: &[OrderBy]) -> bool {
    let exprs = match projection {
        Projection::All => &[][..],
        Projection::Exprs(exprs) => exprs,
    };
    !group_by.is_empty()
        || having.is_some()
        || exprs.iter().chain(order_by.iter().map(|o| &o.expr)).any(contains_aggregate)
}

#[allow(clippy::too_many_arguments)]
pub fn select(
    table: &Table,
    projection: Projection,
    filter: Option<Expr>,
    group_by: Vec<Expr>,
    having: Option<Expr>,
    order_by: Vec<OrderBy>,
    limit: Option<usize>,
    settings: &Settings,
) -> Result<QueryResult> {
    let Projection::Exprs(exprs) = projection else {
        return err("SELECT * cannot be used with GROUP BY or aggregate functions");
    };
    forbid(&filter, "WHERE")?;
    forbid(&group_by, "GROUP BY")?;

    // Rewrite everything evaluated on groups; this also collects the aggregates.
    let mut aggregates = Vec::new();
    let mut rewrite = |expr: &Expr| rewrite(table, expr, &group_by, &mut aggregates);
    let select_list = exprs.iter().map(&mut rewrite).collect::<Result<Vec<_>>>()?;
    let having = having.as_ref().map(&mut rewrite).transpose()?;
    let order_keys = order_by.iter().map(|o| rewrite(&o.expr)).collect::<Result<Vec<_>>>()?;

    let groups = aggregate(table, &filter, &group_by, &aggregates)?;

    // A group's row: its key, then its aggregates' results.
    let group_columns: Vec<Column> = (0..group_by.len())
        .map(group_column)
        .chain((0..aggregates.len()).map(aggregate_column))
        // Only names matter for evaluating expressions.
        .map(|name| Column { name, data_type: DataType::Int, primary_key: false })
        .collect();
    let mut rows = Vec::new();
    for (key, accumulators) in groups {
        let mut row = key;
        row.extend(accumulators.iter().map(Accumulator::result));
        if matches(&having, &group_columns, &row)? {
            rows.push(row);
        }
    }

    if order_keys.is_empty() {
        rows.truncate(limit.unwrap_or(usize::MAX));
    } else {
        let mut sorter = Sorter::new(order_by.iter().map(|o| o.desc).collect(), limit, settings.sort_memory);
        for row in rows {
            let keys = order_keys.iter().map(|k| eval(k, &group_columns, &row)).collect::<Result<_>>()?;
            sorter.push(keys, row)?;
        }
        rows = sorter.finish()?.collect::<Result<_>>()?;
    }

    let names = exprs
        .iter()
        .map(|e| match e {
            Expr::Column(name) => name.clone(),
            Expr::Aggregate(function, _) => function.name().into(),
            _ => "?column?".into(),
        })
        .collect();
    let rows = rows
        .into_iter()
        .map(|row| select_list.iter().map(|e| eval(e, &group_columns, &row)).collect())
        .collect::<Result<_>>()?;
    Ok(QueryResult::Rows { columns: names, rows })
}

/// One aggregate to compute per group: the function and its argument (none for `COUNT(*)`).
type AggregateCall = (Aggregate, Option<Expr>);

/// Reads the rows `filter` matches and folds them into groups, in the order
/// their first row was read.
fn aggregate(
    table: &Table,
    filter: &Option<Expr>,
    group_by: &[Expr],
    aggregates: &[AggregateCall],
) -> Result<Vec<(Row, Vec<Accumulator>)>> {
    let columns = &table.columns;
    let args = aggregates.iter().filter_map(|(_, arg)| arg.as_ref());
    let needed = filter::needed_columns(columns, group_by.iter().chain(filter).chain(args));
    let fresh = || aggregates.iter().map(|(function, _)| Accumulator::new(*function)).collect::<Vec<_>>();

    let mut groups: Vec<(Row, Vec<Accumulator>)> = Vec::new();
    let mut index: HashMap<Row, usize> = HashMap::new();
    for item in scan(table, &needed, filter) {
        let (_, row) = item?;
        if !matches(filter, columns, &row)? {
            continue;
        }
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
    Ok(groups)
}

/// Rewrites an expression to read a group's row, as the module docs describe.
fn rewrite(table: &Table, expr: &Expr, group_by: &[Expr], aggregates: &mut Vec<AggregateCall>) -> Result<Expr> {
    if let Some(i) = group_by.iter().position(|g| g == expr) {
        return Ok(Expr::Column(group_column(i)));
    }
    let mut rewrite = |e: &Expr| rewrite(table, e, group_by, aggregates).map(Box::new);
    Ok(match expr {
        Expr::Literal(_) => expr.clone(),
        Expr::Column(name) => {
            table.column_index(name)?;
            return err(format!("column {name} must appear in GROUP BY or be used in an aggregate function"));
        }
        Expr::Unary(op, inner) => Expr::Unary(*op, rewrite(inner)?),
        Expr::Binary(left, op, right) => Expr::Binary(rewrite(left)?, *op, rewrite(right)?),
        Expr::IsNull(inner, negated) => Expr::IsNull(rewrite(inner)?, *negated),
        Expr::Aggregate(function, arg) => {
            if arg.as_deref().is_some_and(contains_aggregate) {
                return err("aggregate functions cannot be nested");
            }
            let call = (*function, arg.as_deref().cloned());
            let i = match aggregates.iter().position(|a| *a == call) {
                Some(i) => i,
                None => {
                    aggregates.push(call);
                    aggregates.len() - 1
                }
            };
            Expr::Column(aggregate_column(i))
        }
    })
}

/// Fails if one of the expressions uses an aggregate function: rows are only
/// grouped after WHERE, and never for INSERT, UPDATE or DELETE.
pub fn forbid<'a>(exprs: impl IntoIterator<Item = &'a Expr>, clause: &str) -> Result<()> {
    match exprs.into_iter().any(contains_aggregate) {
        true => err(format!("aggregate functions are not allowed in {clause}")),
        false => Ok(()),
    }
}

fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate(..) => true,
        Expr::Literal(_) | Expr::Column(_) => false,
        Expr::Unary(_, inner) | Expr::IsNull(inner, _) => contains_aggregate(inner),
        Expr::Binary(left, _, right) => contains_aggregate(left) || contains_aggregate(right),
    }
}

fn group_column(i: usize) -> String {
    format!("#g{i}")
}

fn aggregate_column(i: usize) -> String {
    format!("#a{i}")
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
