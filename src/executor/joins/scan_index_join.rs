//! An index join of a heap scan run by worker threads from end to end:
//! each worker takes the next page of the left side, and for each row the
//! filters keep, looks its key up in the right table's index itself (see
//! `Lookups` in `gather.rs`), so that both the scan and the lookups are
//! shared among the workers. The pairs the condition keeps, or the row
//! padded with NULLs for a LEFT JOIN, go through the filters and the
//! projection above the join, to the gather (or into the worker's groups,
//! under a partial aggregate).
//!
//! ```text
//!              gather   <-- joined rows, filtered and projected, in batches
//!          +------------+------------+
//!   worker 1             worker 2             worker 3
//!     next page of the left side, filter
//!     each row: its key --> the index --> its right rows, filtered
//!     join, filter, project
//! ```
//!
//! The rows come out in no order.

use std::sync::Arc;
use std::sync::mpsc::sync_channel;

use crate::catalog::table::Row;
use crate::error::Result;
use crate::executor::gather::{Gathered, Lookups, PartialAggregate, Sink, Source, Steps, spawn};
use crate::sql::ast::{Expr, JoinKind};
use crate::sql::eval::{Field, locate, matches};
use crate::types::Value;

/// What every worker needs: the left side, the lookups, and how to join.
struct Spec {
    left: Source,
    right: Lookups,
    kind: JoinKind,
    /// The condition, located in the fields of a joined row.
    condition: Option<Expr>,
    /// The left fields, then the right ones.
    fields: Vec<Field>,
    steps: Steps,
    partial: Option<PartialAggregate>,
}

/// Starts the workers on the join of `left` with the rows `right` looks
/// up, whose rows the gather reads.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_index_join(
    left: Source,
    right: Lookups,
    kind: JoinKind,
    condition: Option<Expr>,
    steps: Steps,
    partial: Option<PartialAggregate>,
    workers: usize,
) -> Result<Gathered> {
    let workers = workers.max(1);
    let fields = [left.fields(), right.fields()].concat();
    let spec = Arc::new(Spec {
        condition: condition.as_ref().map(|c| locate(c, &fields)),
        fields,
        left,
        right,
        kind,
        steps,
        partial,
    });
    let (sender, batches) = sync_channel(workers * 2);
    let mut gathered = Gathered::new(batches);
    for i in 0..workers {
        let (spec, sender, stop) = (spec.clone(), sender.clone(), gathered.stop.clone());
        // Dropping the gather stops those already started.
        gathered.add(spawn(format!("sequoia-index-join-{i}"), move || {
            let mut sink = Sink::new(&sender, &stop, spec.partial.as_ref());
            if let Err(e) = join(&spec, &mut sink).and_then(|()| sink.finish()) {
                sink.fail(e);
            }
        })?);
    }
    Ok(gathered)
}

/// A worker's share: the left rows of the pages it takes, each joined with
/// the right rows its key finds.
fn join(spec: &Spec, sink: &mut Sink) -> Result<()> {
    let left_join = spec.kind == JoinKind::Left;
    let right_width = spec.right.fields().len();
    while !sink.stopped() {
        let Some(rows) = spec.left.next_rows()? else {
            return Ok(());
        };
        for row in rows {
            let mut matched = false;
            for right in spec.right.find(&row, spec.left.fields())? {
                let joined: Row = row.iter().chain(&right).cloned().collect();
                if matches(&spec.condition, &spec.fields, &joined)? {
                    matched = true;
                    emit(spec, joined, sink)?;
                }
            }
            if left_join && !matched {
                let mut padded = row;
                padded.extend(std::iter::repeat_n(Value::Null, right_width));
                emit(spec, padded, sink)?;
            }
        }
    }
    Ok(())
}

/// Hands a joined row on, through the steps above the join.
fn emit(spec: &Spec, row: Row, sink: &mut Sink) -> Result<()> {
    match spec.steps.apply(row)? {
        Some(row) => sink.push(row),
        None => Ok(()),
    }
}
