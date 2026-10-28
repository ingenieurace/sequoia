//! The window step: each row with the values of window functions, computed
//! over its partition (the rows with its `PARTITION BY` values) in the order
//! of the window's `ORDER BY`, the step of every model.
//!
//! ```text
//! input --> sort by PARTITION BY, then ORDER BY --> a partition at a time --> each row ++ the functions' values
//!           (the sort of ORDER BY, which spills)     (held in memory)
//! ```
//!
//! Every row is read and sorted first, carrying the values of the window's
//! keys and of the functions' arguments, evaluated once each. Then the sorted
//! rows are cut into partitions, and each partition, held whole, gives each
//! of its rows its functions' values: so the step holds one partition at a
//! time, besides the sort's budget. Rows equal in ORDER BY are peers: they
//! share a rank, and a RANGE frame takes them all in, as the default frame
//! (from the start of the partition to the current row's last peer) does.
//!
//! An aggregate over a frame folds the frame's rows into a running state,
//! which it keeps from one row to the next while the frame starts where it
//! did and does not shrink at its end: so a running total costs a row's fold
//! per row, and a whole partition's aggregate one fold for all of its rows.
//! A sliding frame (`ROWS BETWEEN 2 PRECEDING AND 2 FOLLOWING`) folds its
//! rows again for each row.
//!
//! The rows come out in the window's order, partition by partition.

use std::collections::VecDeque;

use super::aggregate::Running;
use super::operator::Rows;
use super::sort::{Sorted, Sorter};
use crate::catalog::table::Row;
use crate::error::{Result, err};
use crate::sql::ast::{
    Expr, FrameBound, FrameUnits, OrderBy, Window as WindowCall, WindowFunction,
};
use crate::sql::eval::{Field, Subqueries, eval_with, locate, locate_all};
use crate::types::Value;

/// Reads all of `input` (whose columns are `fields`), and returns each row
/// with the value of each of `functions` after it, over the partitions of
/// `partition_by` ordered by `order_by`, sorting within `budget`.
pub(super) fn window<'a>(
    input: Rows<'a>,
    fields: &[Field],
    partition_by: &[Expr],
    order_by: &[OrderBy],
    functions: &[WindowCall],
    budget: usize,
    subqueries: &mut dyn Subqueries,
) -> Result<Rows<'a>> {
    let keys: Vec<Expr> = locate_all(partition_by, fields)
        .into_iter()
        .chain(order_by.iter().map(|o| locate(&o.expr, fields)))
        .collect();
    let args: Vec<Vec<Expr>> = functions
        .iter()
        .map(|f| locate_all(&f.args, fields))
        .collect();
    let desc = std::iter::repeat_n(false, partition_by.len())
        .chain(order_by.iter().map(|o| o.desc))
        .collect();
    let mut sorter = Sorter::new(desc, None, budget);
    let mut values = Vec::new();
    for row in input {
        let mut row = row?;
        values.clear();
        for e in &keys {
            values.push(eval_with(e, fields, &row, subqueries)?);
        }
        // The row carries its keys and its functions' arguments with it.
        let mut extra = values.clone();
        for e in args.iter().flatten() {
            extra.push(eval_with(e, fields, &row, subqueries)?);
        }
        row.extend(extra);
        sorter.push(&values, row)?;
    }
    Ok(Box::new(Windowed {
        sorted: sorter.finish()?,
        width: fields.len(),
        partition_keys: partition_by.len(),
        order_keys: order_by.len(),
        functions: functions.to_vec(),
        arity: functions.iter().map(|f| f.args.len()).collect(),
        next: None,
        out: VecDeque::new(),
        done: false,
    }))
}

/// The sorted rows, given their functions' values a partition at a time.
struct Windowed {
    sorted: Sorted,
    /// How many of a sorted row's values are the input row's; after them
    /// come its partition keys, its order keys, then each function's arguments.
    width: usize,
    partition_keys: usize,
    order_keys: usize,
    functions: Vec<WindowCall>,
    arity: Vec<usize>,
    /// The first row of the next partition, read past the end of the last one.
    next: Option<Row>,
    /// The rows of the partition done, to hand on.
    out: VecDeque<Row>,
    done: bool,
}

impl Iterator for Windowed {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.out.pop_front() {
                return Some(Ok(row));
            }
            if self.done {
                return None;
            }
            if let Err(e) = self.partition() {
                self.done = true;
                return Some(Err(e));
            }
        }
    }
}

/// A partition's rows, each as the input row, then the values the
/// functions read: its order keys, and each function's arguments.
struct Partition {
    rows: Vec<Row>,
    order: Vec<Vec<Value>>,
    args: Vec<Vec<Vec<Value>>>,
    /// For each row, where its peers start and end (past the last).
    peers: Vec<(usize, usize)>,
}

impl Windowed {
    /// Reads the next partition, and puts its rows with their values in `out`.
    fn partition(&mut self) -> Result<()> {
        let first = match self.next.take() {
            Some(row) => row,
            None => match self.sorted.next() {
                Some(row) => row?,
                None => {
                    self.done = true;
                    return Ok(());
                }
            },
        };
        let key = |row: &Row| row[self.width..self.width + self.partition_keys].to_vec();
        let partition_key = key(&first);
        let mut rows = vec![first];
        for row in self.sorted.by_ref() {
            let row = row?;
            if key(&row) != partition_key {
                self.next = Some(row);
                break;
            }
            rows.push(row);
        }
        if self.next.is_none() {
            self.done = true;
        }
        let partition = self.split(rows);
        let values = self
            .functions
            .iter()
            .enumerate()
            .map(|(f, function)| compute(function, f, &partition))
            .collect::<Result<Vec<_>>>()?;
        for (i, mut row) in partition.rows.into_iter().enumerate() {
            row.extend(values.iter().map(|v| v[i].clone()));
            self.out.push_back(row);
        }
        Ok(())
    }

    /// Takes the partition's rows apart into the input rows and what the
    /// functions read, and finds each row's peers.
    fn split(&self, rows: Vec<Row>) -> Partition {
        let mut partition = Partition {
            rows: Vec::with_capacity(rows.len()),
            order: Vec::with_capacity(rows.len()),
            args: Vec::with_capacity(rows.len()),
            peers: Vec::with_capacity(rows.len()),
        };
        for mut row in rows {
            let mut extra = row.split_off(self.width).into_iter().skip(self.partition_keys);
            partition
                .order
                .push(extra.by_ref().take(self.order_keys).collect());
            let args = self
                .arity
                .iter()
                .map(|&n| extra.by_ref().take(n).collect())
                .collect();
            partition.args.push(args);
            partition.rows.push(row);
        }
        let n = partition.rows.len();
        let mut start = 0;
        while start < n {
            let mut end = start + 1;
            while end < n && partition.order[end] == partition.order[start] {
                end += 1;
            }
            partition.peers.extend(std::iter::repeat_n((start, end), end - start));
            start = end;
        }
        partition
    }
}

/// The values of `function`, the `f`th function of the step, for each row
/// of the partition.
fn compute(function: &WindowCall, f: usize, partition: &Partition) -> Result<Vec<Value>> {
    let n = partition.rows.len();
    let arg = |i: usize, a: usize| partition.args[i][f].get(a).cloned();
    let name = || function.function.name().to_uppercase();
    let frame = function.frame();
    // The rows of row i's frame, from the first to past the last.
    let bounds = |i: usize| {
        let (first, last) = partition.peers[i];
        let offset = |k: u64| usize::try_from(k).unwrap_or(usize::MAX);
        let start = match frame.start {
            FrameBound::UnboundedPreceding => 0,
            FrameBound::Preceding(k) => i.saturating_sub(offset(k)),
            FrameBound::CurrentRow if frame.units == FrameUnits::Range => first,
            FrameBound::CurrentRow => i,
            FrameBound::Following(k) => i.saturating_add(offset(k)).min(n),
            FrameBound::UnboundedFollowing => n,
        };
        let end = match frame.end {
            FrameBound::UnboundedPreceding => 0,
            FrameBound::Preceding(k) => (i + 1).saturating_sub(offset(k)),
            FrameBound::CurrentRow if frame.units == FrameUnits::Range => last,
            FrameBound::CurrentRow => i + 1,
            FrameBound::Following(k) => i.saturating_add(offset(k)).saturating_add(1).min(n),
            FrameBound::UnboundedFollowing => n,
        };
        (start, end.max(start))
    };
    let positive = |value: Option<Value>, what: &str| -> Result<Option<usize>> {
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Int(k)) if k > 0 => Ok(Some(k as usize)),
            Some(other) => err(format!(
                "the {what} of {} must be a positive INT, got {}",
                name(),
                other.to_sql()
            )),
        }
    };
    let mut values = Vec::with_capacity(n);
    match function.function {
        WindowFunction::RowNumber => values.extend((1..=n).map(|i| Value::Int(i as i64))),
        WindowFunction::Rank => {
            values.extend((0..n).map(|i| Value::Int(partition.peers[i].0 as i64 + 1)))
        }
        WindowFunction::DenseRank => {
            let mut rank = 0;
            for i in 0..n {
                if partition.peers[i].0 == i {
                    rank += 1;
                }
                values.push(Value::Int(rank));
            }
        }
        WindowFunction::PercentRank => values.extend((0..n).map(|i| match n {
            1 => Value::Real(0.0),
            _ => Value::Real(partition.peers[i].0 as f64 / (n - 1) as f64),
        })),
        WindowFunction::CumeDist => {
            values.extend((0..n).map(|i| Value::Real(partition.peers[i].1 as f64 / n as f64)))
        }
        WindowFunction::Ntile => {
            for i in 0..n {
                let Some(buckets) = positive(arg(i, 0), "number of buckets")? else {
                    values.push(Value::Null);
                    continue;
                };
                // As equal as can be: the first `n % buckets` hold one row more.
                let (size, larger) = (n / buckets, n % buckets);
                let bucket = match i < larger * (size + 1) {
                    true => i / (size + 1),
                    false => larger + (i - larger * (size + 1)) / size.max(1),
                };
                values.push(Value::Int(bucket as i64 + 1));
            }
        }
        WindowFunction::Lag | WindowFunction::Lead => {
            for i in 0..n {
                let offset = match arg(i, 1) {
                    None => 1,
                    Some(Value::Null) => {
                        values.push(Value::Null);
                        continue;
                    }
                    Some(Value::Int(k)) => k,
                    Some(other) => {
                        return err(format!(
                            "the offset of {} must be an INT, got {}",
                            name(),
                            other.to_sql()
                        ));
                    }
                };
                let offset = match function.function {
                    WindowFunction::Lag => offset.checked_neg(),
                    _ => Some(offset),
                };
                let target = offset
                    .and_then(|k| (i as i64).checked_add(k))
                    .filter(|&t| t >= 0 && (t as usize) < n);
                values.push(match target {
                    Some(t) => arg(t as usize, 0).unwrap_or(Value::Null),
                    None => arg(i, 2).unwrap_or(Value::Null),
                });
            }
        }
        WindowFunction::FirstValue | WindowFunction::LastValue | WindowFunction::NthValue => {
            for i in 0..n {
                let (start, end) = bounds(i);
                let at = match function.function {
                    WindowFunction::FirstValue => Some(start),
                    WindowFunction::LastValue => end.checked_sub(1),
                    _ => positive(arg(i, 1), "row number")?.map(|k| start + (k - 1)),
                };
                values.push(match at.filter(|&at| at >= start && at < end) {
                    Some(at) => arg(at, 0).unwrap_or(Value::Null),
                    None => Value::Null,
                });
            }
        }
        WindowFunction::Aggregate(aggregate) => {
            // The state of the frame folded last, and its rows.
            let mut running: Option<(Running, usize, usize)> = None;
            for i in 0..n {
                let (start, end) = bounds(i);
                let (state, from) = match running.take() {
                    Some((state, s, e)) if s == start && e <= end => (state, e),
                    _ => (Running::new(aggregate), start),
                };
                let mut state = state;
                for j in from..end {
                    state.add(arg(j, 0))?;
                }
                values.push(state.result());
                running = Some((state, start, end));
            }
        }
    }
    Ok(values)
}
