//! The gather step: steps run by worker threads at once, their rows
//! gathered into the thread the query runs in. This is intra-operator
//! parallelism, with an exchange that gathers (see `GATHER` in
//! `plan/query.rs`). What the workers run is one of:
//!
//! - a heap table's scan, with the filters and the projection above it:
//!
//! ```text
//!                      gather  <-- rows, in batches, through a channel
//!            +-----------+-----------+
//!   worker 1: page 0   worker 2: page 1   worker 3: page 2   ... the next page untaken
//!     decode, filter,    decode, filter,    decode, filter,
//!     project            project            project
//! ```
//!
//! - a hash join of two such scans, with the filters and the projection
//!   above it: the workers scan the right side together and build its hash
//!   table, then scan the left side together and probe it, each joining,
//!   filtering and projecting its rows (see `scan_join` in
//!   `joins/parallel.rs`).
//!
//! Under a `PARTIAL AGGREGATE`, a worker folds the rows it makes into
//! groups of its own, with its share of the aggregate's budget, and once
//! done hands on a row per group: its keys and its aggregates' running
//! states, which the `FINALIZE AGGREGATE` above the gather merges (see
//! `aggregate.rs`). So only a few rows a worker reach the query's thread.
//!
//! The workers share each scan: each takes the next page of the heap in
//! turn (see [`SharedTableScan`]), and reads and filters its rows on its
//! own. Every filter, join condition and the projection must be free of
//! subqueries, as subqueries run in the query's thread; the planner gathers
//! only such steps. The workers hand their rows back in batches, through a
//! channel that holds a few, so that they wait while the query is not
//! reading them, and the rows come out in no order.
//!
//! A worker's error ends the query with it. If the query stops reading, as
//! under a LIMIT, dropping the gather tells the workers to stop and waits
//! for them, so that none still reads a table once the query is done.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};

use crate::catalog::table::{Row, SharedLookup, SharedTableScan};
use crate::error::{Result, err};
use crate::plan::query::{AggregateCall, JoinMethod, LookupKey, Node, Phase};
use crate::sql::ast::Expr;
use crate::sql::eval::{Field, eval, locate, matches};
use crate::types::Value;

use super::Settings;
use super::aggregate::{self, Folder};
use super::joins;
use super::operator::Rows;

/// Rows a worker hands back at once.
pub(crate) const BATCH: usize = 1024;

/// Opens a gather of `input`, as the planner made it: a heap's scan or a
/// hash join of two, under filters and a projection, maybe.
pub(super) fn open<'a>(input: Node<'a>, workers: usize, settings: &Settings) -> Rows<'a> {
    let workers = workers.max(1);
    let (partial, input) = match input {
        Node::Aggregate {
            input,
            group_by,
            aggregates,
            phase: Phase::Partial,
        } => {
            let fields = input.columns();
            let partial = PartialAggregate {
                group_by: crate::sql::eval::locate_all(&group_by, &fields),
                aggregates: aggregate::locate(&aggregates, &fields),
                fields,
                budget: settings.hash_memory / workers,
            };
            (Some(partial), *input)
        }
        node => (None, node),
    };
    let (project, mut node) = match input {
        Node::Project { input, exprs } => (Some(exprs), *input),
        node => (None, node),
    };
    // The filters above a join: those above a scan go with the scan.
    fn over_join(node: &Node) -> bool {
        match node {
            Node::Filter { input, .. } => over_join(input),
            node => matches!(node, Node::Join { .. }),
        }
    }
    let mut filters = Vec::new();
    if over_join(&node) {
        node = loop {
            match node {
                Node::Filter { input, predicate } => {
                    filters.push(predicate);
                    node = *input;
                }
                join => break join,
            }
        };
    }
    let started = match node {
        Node::Join {
            left,
            right,
            kind,
            method:
                JoinMethod::Hash {
                    left_keys,
                    right_keys,
                    ..
                },
            condition,
        } => {
            let fields = [left.columns(), right.columns()].concat();
            let steps = Steps::new(&filters, project.as_deref(), &fields);
            let (left, right) = (Source::of(*left), Source::of(*right));
            joins::scan_join(
                left,
                right,
                left_keys,
                right_keys,
                kind,
                condition,
                steps,
                partial,
                settings.hash_memory,
                workers,
            )
        }
        Node::Join {
            left,
            right,
            kind,
            method: JoinMethod::Index,
            condition,
        } => {
            let fields = [left.columns(), right.columns()].concat();
            let steps = Steps::new(&filters, project.as_deref(), &fields);
            let left = Source::of(*left);
            let right = Lookups::of(*right, left.fields());
            joins::scan_index_join(left, right, kind, condition, steps, partial, workers)
        }
        node => {
            let steps = Steps::new(&[], project.as_deref(), &node.columns());
            scan(Source::of(node), steps, partial, workers)
        }
    };
    match started {
        Ok(rows) => Box::new(rows),
        Err(e) => Box::new(std::iter::once(Err(e))),
    }
}

/// The right side of an index join, which workers look rows up in
/// together: a heap's rows by a column, through an index on it, each left
/// row's key worked out from it, and the filters above the lookup.
pub(crate) struct Lookups {
    lookup: SharedLookup,
    /// The left key, located in the left side's fields.
    left_key: Expr,
    /// The filters, located in the right table's fields.
    filters: Vec<Option<Expr>>,
    fields: Vec<Field>,
}

impl Lookups {
    /// The lookups of an index join's right side, a heap's under filters,
    /// maybe, for left rows whose fields are `left_fields`.
    fn of(mut node: Node, left_fields: &[Field]) -> Lookups {
        let mut filters = Vec::new();
        loop {
            match node {
                Node::Filter { input, predicate } => {
                    filters.push(predicate);
                    node = *input;
                }
                Node::Lookup {
                    table,
                    qualifier,
                    needed,
                    column,
                    key: LookupKey::Index(index),
                    left_key,
                } => {
                    let fields = Field::of_table(&qualifier, &table.columns);
                    return Lookups {
                        lookup: table
                            .shared_lookup(index, column, &needed)
                            .expect("only a heap's lookups are gathered"),
                        left_key: locate(&left_key, left_fields),
                        filters: filters.iter().map(|f| Some(locate(f, &fields))).collect(),
                        fields,
                    };
                }
                _ => unreachable!("only an index's lookups and their filters are gathered"),
            }
        }
    }

    pub(crate) fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// The right rows a left row's key finds that the filters keep.
    pub(crate) fn find(&self, left: &[Value], left_fields: &[Field]) -> Result<Vec<Row>> {
        let key = eval(&self.left_key, left_fields, left)?;
        let mut rows = self.lookup.find(&key)?;
        if !self.filters.is_empty() {
            let mut kept = Vec::with_capacity(rows.len());
            'rows: for row in rows {
                for filter in &self.filters {
                    if !matches(filter, &self.fields, &row)? {
                        continue 'rows;
                    }
                }
                kept.push(row);
            }
            rows = kept;
        }
        Ok(rows)
    }
}

/// A heap's rows, read by workers together, with the filters above its scan.
pub(crate) struct Source {
    scan: SharedTableScan,
    /// The filters, located in the scan's fields.
    filters: Vec<Option<Expr>>,
    fields: Vec<Field>,
}

impl Source {
    /// The source of a scan of a heap under filters, maybe.
    fn of(mut node: Node) -> Source {
        let mut filters = Vec::new();
        loop {
            match node {
                Node::Filter { input, predicate } => {
                    filters.push(predicate);
                    node = *input;
                }
                Node::Scan {
                    table,
                    qualifier,
                    needed,
                    ..
                } => {
                    let fields = Field::of_table(&qualifier, &table.columns);
                    return Source {
                        scan: table
                            .shared_scan(&needed)
                            .expect("only a heap's scan is gathered"),
                        filters: filters.iter().map(|f| Some(locate(f, &fields))).collect(),
                        fields,
                    };
                }
                _ => unreachable!("only scans and their filters are gathered"),
            }
        }
    }

    pub(crate) fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// The rows of the next page no worker has taken that the filters
    /// keep, or None once every page is taken.
    pub(crate) fn next_rows(&self) -> Result<Option<Vec<Row>>> {
        let Some(rows) = self.scan.next_rows()? else {
            return Ok(None);
        };
        let mut kept = Vec::with_capacity(rows.len());
        'rows: for row in rows {
            for filter in &self.filters {
                if !matches(filter, &self.fields, &row)? {
                    continue 'rows;
                }
            }
            kept.push(row);
        }
        Ok(Some(kept))
    }
}

/// What a worker does last with each row it makes: filters, and the
/// projection.
pub(crate) struct Steps {
    /// Each filter, located in the fields of the rows given.
    filters: Vec<Option<Expr>>,
    /// The select list, if there is one, located likewise.
    project: Option<Vec<Expr>>,
    fields: Vec<Field>,
}

impl Steps {
    fn new(filters: &[Expr], project: Option<&[Expr]>, fields: &[Field]) -> Steps {
        Steps {
            filters: filters.iter().map(|f| Some(locate(f, fields))).collect(),
            project: project.map(|exprs| exprs.iter().map(|e| locate(e, fields)).collect()),
            fields: fields.to_vec(),
        }
    }

    /// The row the steps make of a row, or None if a filter drops it.
    pub(crate) fn apply(&self, row: Row) -> Result<Option<Row>> {
        for filter in &self.filters {
            if !matches(filter, &self.fields, &row)? {
                return Ok(None);
            }
        }
        match &self.project {
            Some(exprs) => exprs
                .iter()
                .map(|e| eval(e, &self.fields, &row))
                .collect::<Result<Row>>()
                .map(Some),
            None => Ok(Some(row)),
        }
    }
}

/// A worker's rows on their way to the gather, a batch at a time.
pub(crate) struct Outbox<'a> {
    sender: &'a SyncSender<Result<Vec<Row>>>,
    stop: &'a AtomicBool,
    batch: Vec<Row>,
}

impl<'a> Outbox<'a> {
    pub(crate) fn new(sender: &'a SyncSender<Result<Vec<Row>>>, stop: &'a AtomicBool) -> Self {
        Outbox {
            sender,
            stop,
            batch: Vec::with_capacity(BATCH),
        }
    }

    /// Whether the gather has stopped, so that the worker should too.
    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Adds a row, sending the batch once it is full.
    pub(crate) fn push(&mut self, row: Row) {
        self.batch.push(row);
        if self.batch.len() >= BATCH {
            self.flush();
        }
    }

    /// Sends the rows held. If no one reads them any more, stops.
    pub(crate) fn flush(&mut self) {
        if !self.batch.is_empty()
            && self
                .sender
                .send(Ok(std::mem::take(&mut self.batch)))
                .is_err()
        {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Sends an error, which ends the query, and stops every worker.
    pub(crate) fn fail(&mut self, e: crate::error::Error) {
        let _ = self.sender.send(Err(e));
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The half of an aggregate each worker does: its groups, by `group_by`,
/// of the rows it makes, whose fields are `fields`, within `budget`.
pub(crate) struct PartialAggregate {
    group_by: Vec<Expr>,
    aggregates: Vec<AggregateCall>,
    fields: Vec<Field>,
    budget: usize,
}

/// Where a worker's rows go: to the gather, or under a partial aggregate,
/// into the worker's own groups first, whose states go once it is done.
pub(crate) struct Sink<'a> {
    out: Outbox<'a>,
    partial: Option<(&'a PartialAggregate, Folder)>,
}

impl<'a> Sink<'a> {
    pub(crate) fn new(
        sender: &'a SyncSender<Result<Vec<Row>>>,
        stop: &'a AtomicBool,
        partial: Option<&'a PartialAggregate>,
    ) -> Self {
        Sink {
            out: Outbox::new(sender, stop),
            partial: partial.map(|p| (p, Folder::new(p.group_by.len(), &p.aggregates, p.budget))),
        }
    }

    pub(crate) fn stopped(&self) -> bool {
        self.out.stopped()
    }

    pub(crate) fn push(&mut self, row: Row) -> Result<()> {
        match &mut self.partial {
            Some((p, folder)) => {
                folder.add_row(&row, &p.fields, &p.group_by, &mut crate::sql::eval::NoSubqueries)
            }
            None => {
                self.out.push(row);
                Ok(())
            }
        }
    }

    /// Sends what the worker holds: its groups' states, if it folds them,
    /// and the rows not sent yet.
    pub(crate) fn finish(&mut self) -> Result<()> {
        if let Some((_, folder)) = self.partial.take()
            && !self.out.stopped()
        {
            for row in folder.finish_states() {
                self.out.push(row?);
            }
        }
        self.out.flush();
        Ok(())
    }

    pub(crate) fn fail(&mut self, e: crate::error::Error) {
        self.out.fail(e);
    }
}

/// Starts workers on a scan, which send their rows to the gather.
fn scan(
    source: Source,
    steps: Steps,
    partial: Option<PartialAggregate>,
    workers: usize,
) -> Result<Gathered> {
    let (source, steps, partial) = (Arc::new(source), Arc::new(steps), Arc::new(partial));
    let (sender, batches) = sync_channel(workers * 2);
    let mut gathered = Gathered::new(batches);
    for i in 0..workers {
        let (source, steps, partial, stop, sender) = (
            source.clone(),
            steps.clone(),
            partial.clone(),
            gathered.stop.clone(),
            sender.clone(),
        );
        // Dropping the gather stops those already started.
        gathered.add(spawn(format!("sequoia-scan-{i}"), move || {
            let mut sink = Sink::new(&sender, &stop, partial.as_ref().as_ref());
            if let Err(e) = scan_pages(&source, &steps, &mut sink).and_then(|()| sink.finish()) {
                sink.fail(e);
            }
        })?);
    }
    Ok(gathered)
}

/// Takes pages until none is left or the gather stops, and hands on the
/// rows the steps make of theirs.
fn scan_pages(source: &Source, steps: &Steps, sink: &mut Sink) -> Result<()> {
    while !sink.stopped() {
        let Some(rows) = source.next_rows()? else {
            return Ok(());
        };
        for row in rows {
            if let Some(row) = steps.apply(row)? {
                sink.push(row)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn spawn<T: Send + 'static>(
    name: String,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<JoinHandle<T>> {
    thread::Builder::new()
        .name(name)
        .spawn(f)
        .or_else(|e| err(format!("no thread for a worker: {e}")))
}

/// The rows threads send, as the query reads them: the gather's side of
/// the channel.
pub(crate) struct Gathered {
    /// The batches the threads send, until the gather is done.
    batches: Option<Receiver<Result<Vec<Row>>>>,
    batch: std::vec::IntoIter<Row>,
    threads: Vec<JoinHandle<()>>,
    /// Set to tell the threads to stop.
    pub(crate) stop: Arc<AtomicBool>,
}

impl Gathered {
    pub(crate) fn new(batches: Receiver<Result<Vec<Row>>>) -> Self {
        Gathered {
            batches: Some(batches),
            batch: Vec::new().into_iter(),
            threads: Vec::new(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A thread that sends rows, to wait for at the end.
    pub(crate) fn add(&mut self, thread: JoinHandle<()>) {
        self.threads.push(thread);
    }
}

impl Iterator for Gathered {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.batch.next() {
                return Some(Ok(row));
            }
            match self.batches.as_ref()?.recv() {
                Ok(Ok(batch)) => self.batch = batch.into_iter(),
                Ok(Err(e)) => {
                    self.batches = None;
                    return Some(Err(e));
                }
                // Every thread is done: each sent all of its rows, unless it panicked.
                Err(_) => {
                    self.batches = None;
                    let panicked = self.threads.drain(..).any(|t| t.join().is_err());
                    return panicked.then(|| err("a worker panicked"));
                }
            }
        }
    }
}

impl Drop for Gathered {
    /// Stops the threads and waits for them: a thread waiting to send finds
    /// no one to send to, and one reading stops after its page.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.batches = None;
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
