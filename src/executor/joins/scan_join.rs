//! A hash join of two heap scans run by worker threads from end to end:
//! the workers that scan a side are the ones that build or probe with its
//! rows, so no row of either side passes through the query's thread. Only
//! the rows of the join, filtered and projected, are gathered into it.
//!
//! ```text
//!                     gather   <-- joined rows, filtered and projected, in batches
//!          +------------+------------+
//!   probe: worker 1      worker 2      worker 3      each scans the next page of the
//!          scan, filter, probe, filter, project      left side, probes the tables
//!                         ^
//!          table 0  table 1 ... table 15             one per partition of the right side
//!                         ^
//!   build: worker 1      worker 2      worker 3      each scans the next page of the
//!          scan, filter, hash into partitions        right side
//! ```
//!
//! A thread of its own runs the join's phases, one after another, each on
//! the workers:
//!
//! 1. Build: the workers scan the right side together, and split the rows
//!    they keep by the hash of their keys into partitions of their own,
//!    each in memory within its share of the budget, past it written out.
//! 2. Tables: partition p of the join is partition p of every worker. One
//!    held whole in memory becomes a hash table, the workers making the
//!    tables side by side; one with any part written out waits.
//! 3. Probe: the workers scan the left side together. A row whose
//!    partition has a table finds its matches there at once, and the pairs
//!    the condition keeps, or the row padded with NULLs for a LEFT JOIN, go
//!    through the filters and the projection above the join, to the gather.
//!    A row whose partition waits is written out with it.
//! 4. The partitions that waited are joined in turn as a hash join of their
//!    own (see `parallel.rs`), the workers taking them side by side.
//!
//! So the right side's rows held never take much more than the budget,
//! as in a hybrid hash join. A left row with a NULL key matches nothing,
//! and a right row with one is dropped. The rows come out in no order.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};

use super::Key;
use super::parallel::{Partition, Partitions};
use super::probe::{Join, Right};
use crate::catalog::table::Row;
use crate::error::{Error, Result};
use crate::executor::gather::{Gathered, Outbox, PartialAggregate, Sink, Source, Steps, spawn};
use crate::executor::spill::{FANOUT, partition};
use crate::sql::ast::{Expr, JoinKind};
use crate::sql::eval::{Field, locate, matches};
use crate::types::Value;

/// What every worker needs: the two sides and how to join them.
struct Spec {
    left: Source,
    right: Source,
    left_keys: Vec<Key>,
    right_keys: Vec<Key>,
    /// The keys and the condition as the plan gave them, for the joins of
    /// the partitions that waited.
    left_key_exprs: Vec<Expr>,
    right_key_exprs: Vec<Expr>,
    condition: Option<Expr>,
    /// The condition, located in the fields of a joined row.
    located: Option<Expr>,
    /// The left fields, then the right ones.
    fields: Vec<Field>,
    kind: JoinKind,
    /// The filters and the projection above the join.
    steps: Steps,
    /// The half of an aggregate above the join each worker does, if any.
    partial: Option<PartialAggregate>,
    budget: usize,
    workers: usize,
    sender: SyncSender<Result<Vec<Row>>>,
    stop: Arc<AtomicBool>,
}

/// A partition of the right side once built: its rows by their keys, or
/// its parts, if any was written out.
enum Built {
    Table(HashMap<Vec<Value>, Vec<Row>>),
    Waiting(Vec<Partition>),
}

/// Starts the join of `left` with `right`, whose rows the gather reads.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_join(
    left: Source,
    right: Source,
    left_keys: Vec<Expr>,
    right_keys: Vec<Expr>,
    kind: JoinKind,
    condition: Option<Expr>,
    steps: Steps,
    partial: Option<PartialAggregate>,
    budget: usize,
    workers: usize,
) -> Result<Gathered> {
    let workers = workers.max(1);
    let fields = [left.fields(), right.fields()].concat();
    let (sender, batches) = sync_channel(workers * 2);
    let mut gathered = Gathered::new(batches);
    let spec = Arc::new(Spec {
        left_keys: Key::all(left_keys.clone(), left.fields()),
        right_keys: Key::all(right_keys.clone(), right.fields()),
        left_key_exprs: left_keys,
        right_key_exprs: right_keys,
        located: condition.as_ref().map(|c| locate(c, &fields)),
        condition,
        fields,
        kind,
        steps,
        partial,
        budget,
        workers,
        left,
        right,
        sender,
        stop: gathered.stop.clone(),
    });
    gathered.add(spawn("sequoia-join".into(), move || {
        if let Err(e) = run(&spec) {
            Outbox::new(&spec.sender, &spec.stop).fail(e);
        }
    })?);
    Ok(gathered)
}

/// Runs the phases, each on the workers, until one fails or the gather stops.
fn run(spec: &Arc<Spec>) -> Result<()> {
    let stopped = || spec.stop.load(Ordering::Relaxed);

    // 1. Build: each worker's partitions of the right side.
    let budget = spec.budget / spec.workers;
    let built = on_workers(spec, spec.workers, "build", move |spec, _| {
        build(spec, budget)
    })?;
    let mut parts: Vec<Vec<Partition>> = (0..FANOUT).map(|_| Vec::new()).collect();
    for partitions in built {
        for (p, part) in partitions.parts.into_iter().enumerate() {
            parts[p].extend((part.rows() > 0).then_some(part));
        }
    }
    let empty = parts.iter().all(Vec::is_empty);
    // With no right row, an inner join has nothing to return.
    if stopped() || (empty && spec.kind == JoinKind::Inner) {
        return Ok(());
    }

    // 2. Tables: those held whole in memory made side by side.
    let queue: Vec<(usize, Vec<Partition>)> = parts
        .into_iter()
        .enumerate()
        .filter(|(_, parts)| !parts.is_empty())
        .collect();
    let workers = spec.workers.min(queue.len()).max(1);
    let queue = Arc::new(Mutex::new(queue));
    let made = {
        let queue = queue.clone();
        on_workers(spec, workers, "table", move |spec, _| {
            let mut made = Vec::new();
            while let Some((p, parts)) = take(&queue) {
                made.push((p, table(spec, parts)?));
            }
            Ok(made)
        })?
    };
    let mut tables: Vec<Option<Built>> = (0..FANOUT).map(|_| None).collect();
    for (p, built) in made.into_iter().flatten() {
        tables[p] = Some(built);
    }
    if stopped() {
        return Ok(());
    }

    // 3. Probe: each worker's left rows of the partitions that wait.
    let tables = Arc::new(tables);
    let waiting = {
        let tables = tables.clone();
        on_workers(spec, spec.workers, "probe", move |spec, _| {
            probe(spec, &tables)
        })?
    };
    if stopped() {
        return Ok(());
    }

    // 4. The partitions that waited, with the left rows each worker wrote
    // out for them, joined side by side.
    let tables = Arc::try_unwrap(tables).ok().expect("the probes are done");
    let mut waiting = waiting;
    let mut pairs = Vec::new();
    for (p, built) in tables.into_iter().enumerate() {
        let Some(Built::Waiting(right)) = built else {
            continue;
        };
        let empty = || Partition::Memory {
            rows: Vec::new(),
            bytes: 0,
        };
        let left: Vec<Partition> = waiting
            .iter_mut()
            .map(|w| std::mem::replace(&mut w.parts[p], empty()))
            .filter(|part| part.rows() > 0)
            .collect();
        if !left.is_empty() {
            pairs.push((left, right));
        }
    }
    if pairs.is_empty() {
        return Ok(());
    }
    let workers = spec.workers.min(pairs.len());
    let budget = spec.budget / workers;
    let queue = Arc::new(Mutex::new(pairs));
    on_workers(spec, workers, "spilled", move |spec, _| {
        let mut sink = Sink::new(&spec.sender, &spec.stop, spec.partial.as_ref());
        while let Some((left, right)) = take(&queue) {
            join_waiting(spec, left, right, budget, &mut sink)?;
        }
        sink.finish()
    })?;
    Ok(())
}

/// Runs `f` on `n` threads, and returns what each returned, or the first
/// error, once all are done. A worker that fails stops the others.
fn on_workers<T: Send + 'static>(
    spec: &Arc<Spec>,
    n: usize,
    phase: &str,
    f: impl Fn(&Spec, usize) -> Result<T> + Send + Sync + 'static,
) -> Result<Vec<T>> {
    let f = Arc::new(f);
    let mut threads = Vec::new();
    for i in 0..n {
        let (f, spec) = (f.clone(), spec.clone());
        threads.push(spawn(format!("sequoia-{phase}-{i}"), move || {
            let result = f(&spec, i);
            if result.is_err() {
                spec.stop.store(true, Ordering::Relaxed);
            }
            result
        })?);
    }
    let mut results = Vec::new();
    let mut failed = None;
    for thread in threads {
        match thread.join() {
            Ok(Ok(result)) => results.push(result),
            Ok(Err(e)) => _ = failed.get_or_insert(e),
            Err(_) => _ = failed.get_or_insert(Error("a join worker panicked".into())),
        }
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(results),
    }
}

fn take<T>(queue: &Mutex<Vec<T>>) -> Option<T> {
    queue.lock().unwrap_or_else(PoisonError::into_inner).pop()
}

/// A worker's share of the build: the right rows of the pages it takes,
/// in partitions by the hash of their keys, within `budget`.
fn build(spec: &Spec, budget: usize) -> Result<Partitions> {
    let mut parts = Partitions::new(budget);
    let (mut keys, mut scratch) = (Vec::new(), Vec::new());
    while !spec.stop.load(Ordering::Relaxed) {
        let Some(rows) = spec.right.next_rows()? else {
            break;
        };
        for row in rows {
            Key::eval_all(&spec.right_keys, spec.right.fields(), &row, &mut keys)?;
            if !keys.iter().any(Value::is_null) {
                parts.push(partition(&keys, 0, &mut scratch), row)?;
            }
        }
    }
    Ok(parts)
}

/// The hash table of a partition held whole in memory, or the partition,
/// to wait, if any part of it was written out.
fn table(spec: &Spec, parts: Vec<Partition>) -> Result<Built> {
    if parts
        .iter()
        .any(|part| !matches!(part, Partition::Memory { .. }))
    {
        return Ok(Built::Waiting(parts));
    }
    let mut table: HashMap<Vec<Value>, Vec<Row>> = HashMap::new();
    let mut keys = Vec::new();
    for part in parts {
        let Partition::Memory { rows, .. } = part else {
            unreachable!("checked above");
        };
        for row in rows {
            Key::eval_all(&spec.right_keys, spec.right.fields(), &row, &mut keys)?;
            match table.get_mut(keys.as_slice()) {
                Some(rows) => rows.push(row),
                None => _ = table.insert(keys.clone(), vec![row]),
            }
        }
    }
    Ok(Built::Table(table))
}

/// A worker's share of the probe: joins the left rows of the pages it
/// takes, and returns those of partitions that wait.
fn probe(spec: &Spec, tables: &[Option<Built>]) -> Result<Partitions> {
    let mut out = Sink::new(&spec.sender, &spec.stop, spec.partial.as_ref());
    // Written out at once: the budget is the right side's.
    let mut waiting = Partitions::new(0);
    let (mut keys, mut scratch) = (Vec::new(), Vec::new());
    let left_join = spec.kind == JoinKind::Left;
    let right_width = spec.right.fields().len();
    while !out.stopped() {
        let Some(rows) = spec.left.next_rows()? else {
            break;
        };
        for row in rows {
            Key::eval_all(&spec.left_keys, spec.left.fields(), &row, &mut keys)?;
            let found = match keys.iter().any(Value::is_null) {
                true => None,
                false => {
                    let p = partition(&keys, 0, &mut scratch);
                    match &tables[p] {
                        Some(Built::Table(table)) => table.get(keys.as_slice()),
                        Some(Built::Waiting(_)) => {
                            waiting.push(p, row)?;
                            continue;
                        }
                        None => None,
                    }
                }
            };
            let mut matched = false;
            for right in found.into_iter().flatten() {
                let joined: Row = row.iter().chain(right).cloned().collect();
                if matches(&spec.located, &spec.fields, &joined)? {
                    matched = true;
                    emit(spec, joined, &mut out)?;
                }
            }
            if left_join && !matched {
                let mut padded = row;
                padded.extend(std::iter::repeat_n(Value::Null, right_width));
                emit(spec, padded, &mut out)?;
            }
        }
    }
    out.finish()?;
    Ok(waiting)
}

/// Joins a partition that waited, as a hash join of its own whose table
/// splits by the hash bits after those that made the partitions.
fn join_waiting(
    spec: &Spec,
    left: Vec<Partition>,
    right: Vec<Partition>,
    budget: usize,
    out: &mut Sink,
) -> Result<()> {
    let right = Right::hash(
        Partition::all(right),
        spec.left_key_exprs.clone(),
        spec.right_key_exprs.clone(),
        spec.left.fields(),
        spec.right.fields().to_vec(),
        budget,
        1,
    );
    let join = Join::new(
        Partition::all(left),
        spec.left.fields().to_vec(),
        right,
        spec.right.fields().to_vec(),
        spec.kind,
        spec.condition.clone(),
    );
    for row in join {
        if out.stopped() {
            break;
        }
        emit(spec, row?, out)?;
    }
    Ok(())
}

/// Hands a joined row on, through the steps above the join.
fn emit(spec: &Spec, row: Row, out: &mut Sink) -> Result<()> {
    match spec.steps.apply(row)? {
        Some(row) => out.push(row),
        None => Ok(()),
    }
}
