//! The parallel grace hash join: partitions both sides by a hash of their
//! keys, then joins the partitions side by side, both on worker threads.
//!
//! ```text
//! this thread: right rows, then left rows, in batches --> queue
//!
//! partitioner 1: batch --h1--> its own L0..L15, R0..R15     each takes the next batch,
//! partitioner 2: batch --h1--> its own L0..L15, R0..R15     works out keys and hashes
//!
//! pair p: Lp of every partitioner, Rp of every partitioner
//! worker 1: pair 0, then pair 5 ...    a hash table of Rp, probed by Lp
//! worker 2: pair 1, ...                --> joined rows, in batches, to this thread
//! ```
//!
//! Rows with equal keys hash to the same partition, so each pair of
//! partitions joins on its own. The sides come from steps that run in the
//! thread the query runs in, so that thread reads them; it only hands
//! their rows, a batch at a time, to a queue that holds a few. Partitioners,
//! as many as the workers, take the batches and split them by the hash of
//! their keys into partitions of their own, each holding them in memory
//! within its share of the hash join's budget and writing the largest out to
//! temporary files past it, as a grace hash join does. Partition p of the
//! join is partition p of every partitioner, one after another.
//! Then [`FANOUT`] pairs of partitions wait in a queue, the largest first,
//! for the workers to take. Each worker joins its pair as a hash join of
//! its own (see `probe.rs` and `hash.rs`), with its share of the budget,
//! splitting a partition that outgrows it by the next bits of the hash.
//! The workers hand their rows back in batches, through a channel that
//! holds a few, so that they wait while the query is not reading them.
//!
//! A left row with a NULL key matches nothing: a LEFT JOIN still returns
//! it, so it goes to partition 0 to be padded there; otherwise it is
//! dropped, as is a right row with a NULL key. The rows come out in no
//! order, and a hash join that must keep its left side's order does not run
//! in parallel. If the query stops reading, the workers stop at their next
//! batch.

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use super::Key;
use super::probe::{Join, Right};
use crate::catalog::table::Row;
use crate::error::{Error, Result, err};
use crate::executor::operator::Rows;
use crate::executor::sort::value_size;
use crate::executor::spill::{FANOUT, RowFile, partition};
use crate::sql::ast::{Expr, JoinKind};
use crate::sql::eval::Field;
use crate::types::Value;

/// Rows a worker hands back at once.
const BATCH: usize = 1024;

pub struct ParallelHashJoin<'a> {
    /// The two sides, until they are partitioned.
    inputs: Option<(Rows<'a>, Rows<'a>)>,
    spec: Arc<Spec>,
    workers: usize,
    /// Once the workers run: the batches they send, and the one being read.
    running: Option<Running>,
}

/// What a worker needs to join a pair of partitions.
struct Spec {
    left_keys: Vec<Expr>,
    right_keys: Vec<Expr>,
    left_fields: Vec<Field>,
    right_fields: Vec<Field>,
    kind: JoinKind,
    condition: Option<Expr>,
    /// The hash join's budget: for the partitions held, then shared by the workers.
    budget: usize,
}

struct Running {
    batches: Receiver<Result<Vec<Row>>>,
    batch: std::vec::IntoIter<Row>,
    workers: Vec<JoinHandle<()>>,
}

/// A pair of partitions to join: its left rows, and its right rows, each
/// in the parts the partitioners made of it.
type Task = (Vec<Partition>, Vec<Partition>);

impl<'a> ParallelHashJoin<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        left: Rows<'a>,
        left_fields: Vec<Field>,
        right: Rows<'a>,
        right_fields: Vec<Field>,
        left_keys: Vec<Expr>,
        right_keys: Vec<Expr>,
        kind: JoinKind,
        condition: Option<Expr>,
        budget: usize,
        workers: usize,
    ) -> Self {
        ParallelHashJoin {
            inputs: Some((left, right)),
            spec: Arc::new(Spec {
                left_keys,
                right_keys,
                left_fields,
                right_fields,
                kind,
                condition,
                budget,
            }),
            workers: workers.max(1),
            running: None,
        }
    }

    /// Partitions both sides, on worker threads, and sets the workers on the pairs.
    fn start(&mut self) -> Result<Running> {
        let (left, right) = self.inputs.take().expect("started once");
        let spec = &self.spec;
        let left_join = spec.kind == JoinKind::Left;

        // The partitioners take batches from a queue that holds a few, so
        // that this thread waits while they are behind.
        let partitioners = self.workers;
        let (batches_out, batches_in) = sync_channel::<(Side, Vec<Row>)>(partitioners * 2);
        let batches_in = Arc::new(Mutex::new(batches_in));
        let mut handles = Vec::new();
        for i in 0..partitioners {
            let (batches, spec) = (batches_in.clone(), spec.clone());
            let budget = spec.budget / 2 / partitioners;
            handles.push(spawn(format!("sequoia-partition-{i}"), move || {
                partition_batches(&batches, &spec, budget)
            })?);
        }
        let read = deal(right, Side::Right, &batches_out).and_then(|rights| match rights {
            // With no right row, an inner join has nothing to return.
            Some(0) if !left_join => Ok(Some(0)),
            Some(_) => deal(left, Side::Left, &batches_out),
            None => Ok(None),
        });
        drop(batches_out);

        // Each partitioner's partition p, together: the pairs to join.
        let mut lefts: Vec<Vec<Partition>> = (0..FANOUT).map(|_| Vec::new()).collect();
        let mut rights: Vec<Vec<Partition>> = (0..FANOUT).map(|_| Vec::new()).collect();
        let mut failed = read.err();
        for handle in handles {
            match handle.join() {
                Ok(Ok((l, r))) => {
                    for (p, (l, r)) in l.parts.into_iter().zip(r.parts).enumerate() {
                        lefts[p].extend((l.rows() > 0).then_some(l));
                        rights[p].extend((r.rows() > 0).then_some(r));
                    }
                }
                Ok(Err(e)) => _ = failed.get_or_insert(e),
                Err(_) => _ = failed.get_or_insert(Error("a partition worker panicked".into())),
            }
        }
        if let Some(e) = failed {
            return Err(e);
        }

        // The pairs with something to join, the largest taken first.
        let rows = |parts: &[Partition]| parts.iter().map(Partition::rows).sum::<u64>();
        let mut tasks: Vec<Task> = lefts
            .into_iter()
            .zip(rights)
            .filter(|(l, r)| rows(l) > 0 && (left_join || rows(r) > 0))
            .collect();
        tasks.sort_by_key(|(l, r)| rows(l) + rows(r));
        let workers = self.workers.min(tasks.len()).max(1);
        let queue = Arc::new(Mutex::new(tasks));
        let (sender, batches) = sync_channel(workers * 2);
        let mut handles = Vec::new();
        for i in 0..workers {
            let (queue, sender, spec) = (queue.clone(), sender.clone(), spec.clone());
            let budget = spec.budget / workers;
            handles.push(spawn(format!("sequoia-join-{i}"), move || {
                work(&queue, &spec, budget, &sender)
            })?);
        }
        Ok(Running {
            batches,
            batch: Vec::new().into_iter(),
            workers: handles,
        })
    }
}

/// Which side a batch of rows is from.
#[derive(Clone, Copy, PartialEq)]
enum Side {
    Left,
    Right,
}

fn spawn<T: Send + 'static>(
    name: String,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<JoinHandle<T>> {
    thread::Builder::new()
        .name(name)
        .spawn(f)
        .or_else(|e| err(format!("no thread for a join worker: {e}")))
}

/// Reads a side and hands its rows to the partitioners in batches: all this
/// thread does of the partitioning. Returns how many rows it read, or None
/// if the partitioners stopped taking them, as one does when it fails.
fn deal(rows: Rows, side: Side, batches: &SyncSender<(Side, Vec<Row>)>) -> Result<Option<u64>> {
    let mut read = 0;
    let mut batch = Vec::with_capacity(BATCH);
    for row in rows {
        batch.push(row?);
        read += 1;
        if batch.len() == BATCH && batches.send((side, std::mem::take(&mut batch))).is_err() {
            return Ok(None);
        }
    }
    match batch.is_empty() || batches.send((side, batch)).is_ok() {
        true => Ok(Some(read)),
        false => Ok(None),
    }
}

/// A partitioner: takes batches until there are none, and splits their
/// rows by the hash of their keys into partitions of its own, held within
/// `budget` for each side. Stops at its first error, which becomes the
/// join's once the others are done.
fn partition_batches(
    batches: &Mutex<Receiver<(Side, Vec<Row>)>>,
    spec: &Spec,
    budget: usize,
) -> Result<(Partitions, Partitions)> {
    let left_keys = Key::all(spec.left_keys.clone(), &spec.left_fields);
    let right_keys = Key::all(spec.right_keys.clone(), &spec.right_fields);
    let (mut lefts, mut rights) = (Partitions::new(budget), Partitions::new(budget));
    let (mut keys, mut scratch) = (Vec::new(), Vec::new());
    let left_join = spec.kind == JoinKind::Left;
    loop {
        let next = batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv();
        let Ok((side, batch)) = next else {
            return Ok((lefts, rights));
        };
        for row in batch {
            match side {
                Side::Right => {
                    Key::eval_all(&right_keys, &spec.right_fields, &row, &mut keys)?;
                    if !keys.iter().any(Value::is_null) {
                        rights.push(partition(&keys, 0, &mut scratch), row)?;
                    }
                }
                Side::Left => {
                    Key::eval_all(&left_keys, &spec.left_fields, &row, &mut keys)?;
                    match keys.iter().any(Value::is_null) {
                        false => lefts.push(partition(&keys, 0, &mut scratch), row)?,
                        true if left_join => lefts.push(0, row)?,
                        true => {}
                    }
                }
            }
        }
    }
}

/// Joins pairs of partitions from the queue until it is empty, or the
/// query stops reading, or a pair fails, which it sends on.
fn work(
    queue: &Mutex<Vec<Task>>,
    spec: &Spec,
    budget: usize,
    sender: &SyncSender<Result<Vec<Row>>>,
) {
    loop {
        let task = queue.lock().unwrap_or_else(PoisonError::into_inner).pop();
        let Some((left, right)) = task else {
            return;
        };
        match join(left, right, spec, budget, sender) {
            Ok(true) => {}
            Ok(false) => return, // no one reads any more
            Err(e) => {
                let _ = sender.send(Err(e));
                return;
            }
        }
    }
}

/// Joins a pair of partitions as a hash join of its own, whose table splits
/// by the hash bits after those that made the partitions, and sends its rows
/// in batches. Returns false if they are no longer read.
fn join(
    left: Vec<Partition>,
    right: Vec<Partition>,
    spec: &Spec,
    budget: usize,
    sender: &SyncSender<Result<Vec<Row>>>,
) -> Result<bool> {
    let right = Right::hash(
        Partition::all(right),
        spec.left_keys.clone(),
        spec.right_keys.clone(),
        &spec.left_fields,
        spec.right_fields.clone(),
        budget,
        1,
    );
    let join = Join::new(
        Partition::all(left),
        spec.left_fields.clone(),
        right,
        spec.right_fields.clone(),
        spec.kind,
        spec.condition.clone(),
    );
    let mut batch = Vec::with_capacity(BATCH);
    for row in join {
        batch.push(row?);
        if batch.len() == BATCH && sender.send(Ok(std::mem::take(&mut batch))).is_err() {
            return Ok(false);
        }
    }
    Ok(batch.is_empty() || sender.send(Ok(batch)).is_ok())
}

impl Iterator for ParallelHashJoin<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.inputs.is_some() {
            match self.start() {
                Ok(running) => self.running = Some(running),
                Err(e) => return Some(Err(e)),
            }
        }
        let running = self.running.as_mut()?;
        loop {
            if let Some(row) = running.batch.next() {
                return Some(Ok(row));
            }
            match running.batches.recv() {
                Ok(Ok(batch)) => running.batch = batch.into_iter(),
                Ok(Err(e)) => {
                    self.running = None;
                    return Some(Err(e));
                }
                // Every worker is done: each sent all of its rows, unless it panicked.
                Err(_) => {
                    let running = self.running.take().expect("running");
                    let panicked = running.workers.into_iter().any(|w| w.join().is_err());
                    return panicked.then(|| err("a join worker panicked"));
                }
            }
        }
    }
}

/// The rows of one side, in [`FANOUT`] partitions by the hash of their keys,
/// held in memory within `budget`, and past it the largest written out.
pub(super) struct Partitions {
    pub(super) parts: Vec<Partition>,
    /// Bytes of the partitions in memory.
    bytes: usize,
    budget: usize,
}

pub(super) enum Partition {
    Memory { rows: Vec<Row>, bytes: usize },
    File(RowFile),
}

impl Partitions {
    pub(super) fn new(budget: usize) -> Self {
        Partitions {
            parts: (0..FANOUT)
                .map(|_| Partition::Memory {
                    rows: Vec::new(),
                    bytes: 0,
                })
                .collect(),
            bytes: 0,
            budget,
        }
    }

    pub(super) fn push(&mut self, p: usize, row: Row) -> Result<()> {
        match &mut self.parts[p] {
            Partition::File(file) => file.push(&row)?,
            Partition::Memory { rows, bytes } => {
                let size = 24 + row.iter().map(value_size).sum::<usize>();
                rows.push(row);
                *bytes += size;
                self.bytes += size;
                while self.bytes > self.budget {
                    self.write_out_largest()?;
                }
            }
        }
        Ok(())
    }

    /// Writes the largest partition in memory to a temporary file.
    fn write_out_largest(&mut self) -> Result<()> {
        let largest = self.parts.iter_mut().filter_map(|part| match part {
            Partition::Memory { bytes, .. } => Some((*bytes, part)),
            Partition::File(_) => None,
        });
        let Some((bytes, part)) = largest.max_by_key(|(bytes, _)| *bytes) else {
            unreachable!("bytes in memory are in a partition there");
        };
        let Partition::Memory { rows, .. } =
            std::mem::replace(part, Partition::File(RowFile::new()))
        else {
            unreachable!("chosen in memory");
        };
        let Partition::File(file) = part else {
            unreachable!("just made");
        };
        for row in &rows {
            file.push(row)?;
        }
        self.bytes -= bytes;
        Ok(())
    }
}

impl Partition {
    pub(super) fn rows(&self) -> u64 {
        match self {
            Partition::Memory { rows, .. } => rows.len() as u64,
            Partition::File(file) => file.rows(),
        }
    }

    /// The rows of the parts of a partition, one part after another.
    pub(super) fn all(parts: Vec<Partition>) -> Rows<'static> {
        Box::new(parts.into_iter().flat_map(Partition::into_rows))
    }

    fn into_rows(self) -> Rows<'static> {
        match self {
            Partition::Memory { rows, .. } => Box::new(rows.into_iter().map(Ok)),
            Partition::File(file) => Box::new(file.read()),
        }
    }
}
