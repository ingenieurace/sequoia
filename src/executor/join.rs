//! Joins: for each left row, the right rows that may match it, paired with
//! it where the join's condition holds.
//!
//! ```text
//! left row --> candidates on the right --> left ++ right, if the condition holds
//!                  hash:  the right rows with equal keys, from a hash table built once
//!                  index: the right rows an index lookup finds, per left row
//!
//! block of left rows --> every right row, read again per block --> left ++ right, if the condition holds
//!
//! left, sorted by keys  --+
//!                         +--> merge: each left row with the right rows of equal keys
//! right, sorted by keys --+
//! ```
//!
//! A LEFT JOIN also returns each left row that no right row matched, with a
//! NULL for every right column. The right side of a hash join is held in
//! memory; its first row is read when the first left row is.
//!
//! A block nested loop join holds left rows instead: a block of as many as
//! fit in its memory budget ([`JOIN_MEMORY`] by default). For each block it
//! reads the right side again from the start and pairs each right row with
//! every row of the block, so it reads the right side once per block rather
//! than once per left row, and never holds more than one block. Its rows come
//! out a block at a time, by right row within a block; a LEFT JOIN's
//! unmatched rows of a block come after its pairs.
//!
//! A sort-merge join sorts both sides by their keys with the sort ORDER BY
//! uses, which writes runs to a temporary file past its budget, so besides
//! those budgets it holds only the right rows of one key at a time. Its rows come out in
//! the left side's sorted order, which is why the planner picks it: for an
//! ORDER BY that starts with its keys, the sort is already done.

use std::cmp::Ordering;
use std::collections::HashMap;

use super::operator::Rows;
use super::sort::{Sorted, Sorter, value_size};
use crate::catalog::index::Bound;
use crate::catalog::table::{Access, Row, Table};
use crate::error::Result;
use crate::plan::query::LookupKey;
use crate::predicate::{Compare, Predicate};
use crate::sql::ast::{Expr, JoinKind, OrderBy};
use crate::sql::eval::{Field, eval, matches, resolve};
use crate::types::Value;

/// Default memory a block nested loop join may hold left rows in: 4 MB.
pub const JOIN_MEMORY: usize = 4 << 20;

pub struct Join<'a> {
    left: Rows<'a>,
    left_fields: Vec<Field>,
    right: Right<'a>,
    right_width: usize,
    kind: JoinKind,
    condition: Option<Expr>,
    /// The left fields, then the right ones: the fields of a joined row.
    fields: Vec<Field>,
    /// The left row being joined, how many right rows may match it (see
    /// [`Right::candidate`]), and the next of them to try.
    current: Option<Row>,
    candidates: usize,
    next: usize,
    matched: bool,
}

/// How a join finds the right rows that may match a left row.
pub enum Right<'a> {
    /// Right rows grouped by their keys, read on first use. Rows with a NULL key are dropped: they match nothing.
    Hash {
        source: Option<Rows<'a>>,
        /// Each key's position in `buckets`, which holds the rows with that key.
        index: HashMap<Vec<Value>, usize>,
        buckets: Vec<Vec<Row>>,
        left_keys: Vec<Key>,
        right_keys: Vec<Key>,
        right_fields: Vec<Field>,
        /// The current left row's keys, and its bucket, if any.
        probe: Vec<Value>,
        bucket: Option<usize>,
    },
    /// The right rows a lookup finds for the current left row.
    Index { lookup: Lookup<'a>, rows: Vec<Row> },
}

/// A join key: a column, found by its position, or any other expression.
pub enum Key {
    Column(usize),
    Expr(Expr),
}

/// Finds the rows of a table whose `column` equals a left row's `left_key`,
/// through an index or the primary key, and keeps those `filter` matches.
pub struct Lookup<'a> {
    pub table: &'a Table,
    pub needed: Vec<bool>,
    pub column: usize,
    pub key: LookupKey,
    pub left_key: Expr,
    pub filter: Option<Expr>,
    pub fields: Vec<Field>,
}

impl<'a> Join<'a> {
    pub fn new(
        left: Rows<'a>,
        left_fields: Vec<Field>,
        right: Right<'a>,
        right_fields: Vec<Field>,
        kind: JoinKind,
        condition: Option<Expr>,
    ) -> Self {
        Join {
            left,
            right,
            right_width: right_fields.len(),
            kind,
            condition,
            fields: [left_fields.clone(), right_fields].concat(),
            left_fields,
            current: None,
            candidates: 0,
            next: 0,
            matched: false,
        }
    }
}

impl Iterator for Join<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current.is_none() {
                let row = match self.left.next()? {
                    Ok(row) => row,
                    Err(e) => return Some(Err(e)),
                };
                match self.right.find(&row, &self.left_fields) {
                    Ok(candidates) => self.candidates = candidates,
                    Err(e) => return Some(Err(e)),
                }
                (self.current, self.next, self.matched) = (Some(row), 0, false);
            }
            let left = self.current.as_ref().expect("set above");
            while self.next < self.candidates {
                let right = self.right.candidate(self.next);
                self.next += 1;
                let joined: Row = left.iter().chain(right).cloned().collect();
                match matches(&self.condition, &self.fields, &joined) {
                    Ok(true) => {
                        self.matched = true;
                        return Some(Ok(joined));
                    }
                    Ok(false) => {}
                    Err(e) => return Some(Err(e)),
                }
            }
            let mut left = self.current.take().expect("set above");
            if self.kind == JoinKind::Left && !self.matched {
                left.extend(std::iter::repeat_n(Value::Null, self.right_width));
                return Some(Ok(left));
            }
        }
    }
}

impl<'a> Right<'a> {
    /// A hash join's right side: the rows of `source`, by `right_keys`, for
    /// left rows to find by `left_keys`.
    pub fn hash(
        source: Rows<'a>,
        left_keys: Vec<Expr>,
        right_keys: Vec<Expr>,
        left_fields: &[Field],
        right_fields: Vec<Field>,
    ) -> Self {
        Right::Hash {
            source: Some(source),
            index: HashMap::new(),
            buckets: Vec::new(),
            left_keys: Key::all(left_keys, left_fields),
            right_keys: Key::all(right_keys, &right_fields),
            right_fields,
            probe: Vec::new(),
            bucket: None,
        }
    }

    /// Finds the right rows that may match `left`, and returns how many there
    /// are: [`candidate`](Self::candidate) gives each.
    fn find(&mut self, left: &Row, left_fields: &[Field]) -> Result<usize> {
        match self {
            Right::Hash { source, index, buckets, left_keys, right_keys, right_fields, probe, bucket } => {
                if let Some(source) = source.take() {
                    for row in source {
                        let row = row?;
                        Key::eval_all(right_keys, right_fields, &row, probe)?;
                        if probe.iter().any(Value::is_null) {
                            continue;
                        }
                        let at = match index.get(probe.as_slice()) {
                            Some(&at) => at,
                            None => {
                                index.insert(probe.clone(), buckets.len());
                                buckets.push(Vec::new());
                                buckets.len() - 1
                            }
                        };
                        buckets[at].push(row);
                    }
                }
                Key::eval_all(left_keys, left_fields, left, probe)?;
                *bucket = index.get(probe.as_slice()).copied(); // a NULL key is in no bucket
                Ok(bucket.map_or(0, |at| buckets[at].len()))
            }
            Right::Index { lookup, rows } => {
                *rows = lookup.find(&eval(&lookup.left_key, left_fields, left)?)?;
                Ok(rows.len())
            }
        }
    }

    /// The `i`th right row [`find`](Self::find) found.
    fn candidate(&self, i: usize) -> &Row {
        match self {
            Right::Hash { buckets, bucket, .. } => &buckets[bucket.expect("found a bucket")][i],
            Right::Index { rows, .. } => &rows[i],
        }
    }
}

impl Key {
    /// The keys `exprs`, on rows with these fields.
    fn all(exprs: Vec<Expr>, fields: &[Field]) -> Vec<Key> {
        exprs
            .into_iter()
            .map(|expr| match &expr {
                Expr::Column(column) => resolve(fields, column).map_or(Key::Expr(expr), Key::Column),
                _ => Key::Expr(expr),
            })
            .collect()
    }

    /// Puts the values of `keys` on `row` in `values`.
    fn eval_all(keys: &[Key], fields: &[Field], row: &Row, values: &mut Vec<Value>) -> Result<()> {
        values.clear();
        for key in keys {
            values.push(match key {
                Key::Column(i) => row[*i].clone(),
                Key::Expr(expr) => eval(expr, fields, row)?,
            });
        }
        Ok(())
    }
}

impl Lookup<'_> {
    /// The rows whose key column equals `value`, and that the filter matches.
    fn find(&self, value: &Value) -> Result<Vec<Row>> {
        if value.is_null() {
            return Ok(Vec::new());
        }
        let data_type = self.table.columns[self.column].data_type;
        // The column's value that equals this one, if any: as in a hash join, 1 finds 1.0.
        let Some(value) = &value.coerce(data_type) else { return Ok(Vec::new()) };
        let predicates = [Predicate { column: self.column, op: Compare::Eq, literal: value.clone() }];
        let access = match self.key {
            LookupKey::Index(index) => {
                let bound = Some(Bound { value: value.clone(), inclusive: true });
                Access::Index { index, lower: bound.clone(), upper: bound }
            }
            LookupKey::PrimaryKey => Access::Key(value.clone()),
        };
        let mut rows = Vec::new();
        for item in self.table.scan_filtered(&self.needed, &predicates, &access) {
            let (_, row) = item?;
            if matches(&self.filter, &self.fields, &row)? {
                rows.push(row);
            }
        }
        Ok(rows)
    }
}

/// A block nested loop join: pairs each block of left rows with every right
/// row, reading the right side again for each block.
///
/// While it reads the right side the first time, it also keeps the rows, as
/// long as they fit in the budget too. If all of them do, the blocks after
/// the first pair with those instead of reading the right side again, so it
/// holds up to twice its budget.
pub struct BlockJoin<'a> {
    /// The left rows, until they run out.
    left: Option<Rows<'a>>,
    /// Starts reading the right side from the beginning.
    right: Box<dyn Fn() -> Rows<'a> + 'a>,
    right_width: usize,
    kind: JoinKind,
    condition: Option<Expr>,
    /// The left fields, then the right ones: the fields of a joined row.
    fields: Vec<Field>,
    /// Bytes of left rows a block may hold, and of right rows kept; a block holds at least one row.
    budget: usize,
    /// The left rows being joined, and which of them a right row matched.
    block: Vec<Row>,
    matched: Vec<bool>,
    /// The right rows read, while they fit in the budget, and their bytes;
    /// all of them once the first read ended with them all kept.
    kept: Vec<Row>,
    kept_bytes: usize,
    all_kept: bool,
    /// How the right side is being read, once the first block starts.
    reading: Option<Reading<'a>>,
    /// The right row being paired with the block's rows from `next` on.
    current: Option<Current>,
    next: usize,
    /// A pair, built in place to check the condition on: a pair that fails it
    /// costs no allocation, and one that passes is returned as it is.
    pair: Row,
    phase: Phase,
}

enum Reading<'a> {
    /// For the first time, keeping the rows in `kept`.
    First(Rows<'a>),
    /// Again, after the rows grew past the budget.
    Again(Rows<'a>),
    /// From `kept`, which holds them all: the next one's position.
    Kept(usize),
}

enum Current {
    Kept(usize),
    Read(Row),
}

#[derive(Clone, Copy)]
enum Phase {
    /// Reading the next block of left rows.
    Fill,
    /// Pairing each right row with the block's rows.
    Pair,
    /// The right side has ended: padding the block's unmatched rows from this one on, for a LEFT JOIN.
    Pad(usize),
}

impl<'a> BlockJoin<'a> {
    pub fn new(
        left: Rows<'a>,
        left_fields: Vec<Field>,
        right: Box<dyn Fn() -> Rows<'a> + 'a>,
        right_fields: Vec<Field>,
        kind: JoinKind,
        condition: Option<Expr>,
        budget: usize,
    ) -> Self {
        BlockJoin {
            left: Some(left),
            right,
            right_width: right_fields.len(),
            kind,
            condition,
            fields: [left_fields, right_fields].concat(),
            budget,
            block: Vec::new(),
            matched: Vec::new(),
            kept: Vec::new(),
            kept_bytes: 0,
            all_kept: false,
            reading: None,
            current: None,
            next: 0,
            pair: Vec::new(),
            phase: Phase::Fill,
        }
    }

    /// Starts reading the right side from the beginning, for a new block.
    fn restart(&mut self) {
        self.reading = Some(match &self.reading {
            _ if self.all_kept => Reading::Kept(0),
            None => Reading::First((self.right)()),
            Some(_) => Reading::Again((self.right)()),
        });
        self.current = None;
    }

    /// Moves `current` to the next right row; false once there is none.
    fn advance(&mut self) -> Result<bool> {
        let row = match self.reading.as_mut().expect("restarted") {
            Reading::Kept(at) => {
                if *at == self.kept.len() {
                    return Ok(false);
                }
                self.current = Some(Current::Kept(*at));
                *at += 1;
                return Ok(true);
            }
            Reading::First(rows) | Reading::Again(rows) => match rows.next() {
                Some(row) => row?,
                None => {
                    self.all_kept = matches!(self.reading, Some(Reading::First(_)));
                    return Ok(false);
                }
            },
        };
        if let Some(Reading::First(rows)) = &mut self.reading {
            self.kept_bytes += 64 + row.iter().map(value_size).sum::<usize>();
            if self.kept_bytes <= self.budget {
                self.kept.push(row);
                self.current = Some(Current::Kept(self.kept.len() - 1));
                return Ok(true);
            }
            // Too many to keep: read them again for every block.
            self.kept = Vec::new();
            let rows = std::mem::replace(rows, Box::new(std::iter::empty()));
            self.reading = Some(Reading::Again(rows));
        }
        self.current = Some(Current::Read(row));
        Ok(true)
    }
}

impl Iterator for BlockJoin<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.phase {
                Phase::Fill => {
                    self.block.clear();
                    let left = self.left.as_mut()?;
                    let mut bytes = 0;
                    while self.block.is_empty() || bytes < self.budget {
                        match left.next() {
                            Some(Ok(row)) => {
                                bytes += 64 + row.iter().map(value_size).sum::<usize>();
                                self.block.push(row);
                            }
                            Some(Err(e)) => return Some(Err(e)),
                            None => {
                                self.left = None;
                                break;
                            }
                        }
                    }
                    if self.block.is_empty() {
                        return None;
                    }
                    self.matched = vec![false; self.block.len()];
                    self.restart();
                    self.phase = Phase::Pair;
                }
                Phase::Pair => {
                    if self.current.is_none() || self.next == self.block.len() {
                        match self.advance() {
                            Ok(true) => self.next = 0,
                            Ok(false) => self.phase = Phase::Pad(0),
                            Err(e) => return Some(Err(e)),
                        }
                        continue;
                    }
                    let i = self.next;
                    self.next += 1;
                    let right = match self.current.as_ref().expect("checked above") {
                        Current::Kept(at) => &self.kept[*at],
                        Current::Read(row) => row,
                    };
                    self.pair.clear();
                    self.pair.extend(self.block[i].iter().chain(right).cloned());
                    match matches(&self.condition, &self.fields, &self.pair) {
                        Ok(true) => {
                            self.matched[i] = true;
                            return Some(Ok(std::mem::take(&mut self.pair))); // the next pair allocates anew
                        }
                        Ok(false) => {}
                        Err(e) => return Some(Err(e)),
                    }
                }
                Phase::Pad(mut next) => {
                    while self.kind == JoinKind::Left && next < self.block.len() {
                        let i = next;
                        next += 1;
                        if !self.matched[i] {
                            self.phase = Phase::Pad(next);
                            let mut row = std::mem::take(&mut self.block[i]);
                            row.extend(std::iter::repeat_n(Value::Null, self.right_width));
                            return Some(Ok(row));
                        }
                    }
                    self.phase = Phase::Fill;
                }
            }
        }
    }
}

/// How a merge join sorts its sides: see [`JoinMethod::Merge`](crate::plan::query::JoinMethod::Merge).
pub struct MergeSort {
    pub left_keys: Vec<Expr>,
    pub right_keys: Vec<Expr>,
    pub desc: Vec<bool>,
    pub then: Vec<OrderBy>,
    /// Bytes each side's sort may buffer before it writes runs out.
    pub budget: usize,
}

/// A sort-merge join: sorts both sides by their keys, then walks them
/// together, pairing each left row with the right rows of equal keys.
pub struct MergeJoin<'a> {
    /// Each side, until it is sorted: the left when the first row is asked
    /// for, the right when the first left row with no NULL key is read.
    left_input: Option<Rows<'a>>,
    right_input: Option<Rows<'a>>,
    left: Option<Sorted>,
    right: Option<Sorted>,
    left_fields: Vec<Field>,
    right_fields: Vec<Field>,
    sort: MergeSort,
    right_width: usize,
    kind: JoinKind,
    condition: Option<Expr>,
    /// The left fields, then the right ones: the fields of a joined row.
    fields: Vec<Field>,
    /// The next right row, with its keys, once read and until it joins `group`.
    lookahead: Option<(Vec<Value>, Row)>,
    /// The right rows whose keys are `group_key`, the last left keys looked up.
    group_key: Option<Vec<Value>>,
    group: Vec<Row>,
    /// The left row being joined, how many rows of `group` match it, the next one to pair.
    current: Option<Row>,
    end: usize,
    next: usize,
    matched: bool,
}

impl<'a> MergeJoin<'a> {
    pub fn new(
        left: Rows<'a>,
        left_fields: Vec<Field>,
        right: Rows<'a>,
        right_fields: Vec<Field>,
        sort: MergeSort,
        kind: JoinKind,
        condition: Option<Expr>,
    ) -> Self {
        MergeJoin {
            left_input: Some(left),
            right_input: Some(right),
            left: None,
            right: None,
            fields: [left_fields.clone(), right_fields.clone()].concat(),
            left_fields,
            right_width: right_fields.len(),
            right_fields,
            sort,
            kind,
            condition,
            lookahead: None,
            group_key: None,
            group: Vec::new(),
            current: None,
            end: 0,
            next: 0,
            matched: false,
        }
    }

    /// The next left row in order, sorting the left side first if it is not yet.
    fn next_left(&mut self) -> Option<Result<Row>> {
        if let Some(rows) = self.left_input.take() {
            let MergeSort { left_keys, desc, then, budget, .. } = &self.sort;
            match sort(rows, &self.left_fields, left_keys, desc, then, *budget) {
                Ok(sorted) => self.left = Some(sorted),
                Err(e) => return Some(Err(e)),
            }
        }
        self.left.as_mut()?.next()
    }

    /// How many right rows have the keys of `left`: they are the whole `group`.
    /// Left rows come in key order, so a right row with smaller keys than
    /// these matches no later left row either, and is dropped.
    fn find(&mut self, left: &Row) -> Result<usize> {
        let key = keys(&self.sort.left_keys, &self.left_fields, left)?;
        if key.iter().any(Value::is_null) {
            return Ok(0); // nothing is equal to NULL
        }
        if self.group_key.as_ref().is_some_and(|group| compare(group, &key, &self.sort.desc).is_eq()) {
            return Ok(self.group.len());
        }
        if let Some(rows) = self.right_input.take() {
            let MergeSort { right_keys, desc, budget, .. } = &self.sort;
            self.right = Some(sort(rows, &self.right_fields, right_keys, desc, &[], *budget)?);
        }
        let right = self.right.as_mut().expect("sorted above");
        self.group.clear();
        loop {
            if self.lookahead.is_none() {
                let Some(row) = right.next() else { break };
                let row = row?;
                self.lookahead = Some((keys(&self.sort.right_keys, &self.right_fields, &row)?, row));
            }
            // A right row with a NULL key is never equal, so never joins the group.
            let (right_key, _) = self.lookahead.as_ref().expect("read above");
            match compare(right_key, &key, &self.sort.desc) {
                Ordering::Less => self.lookahead = None,
                Ordering::Equal => self.group.push(self.lookahead.take().expect("read above").1),
                Ordering::Greater => break,
            }
        }
        self.group_key = Some(key);
        Ok(self.group.len())
    }
}

impl Iterator for MergeJoin<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current.is_none() {
                let row = match self.next_left()? {
                    Ok(row) => row,
                    Err(e) => return Some(Err(e)),
                };
                match self.find(&row) {
                    Ok(end) => self.end = end,
                    Err(e) => return Some(Err(e)),
                }
                (self.current, self.next, self.matched) = (Some(row), 0, false);
            }
            let left = self.current.as_ref().expect("set above");
            while self.next < self.end {
                let right = &self.group[self.next];
                self.next += 1;
                let joined: Row = left.iter().chain(right).cloned().collect();
                match matches(&self.condition, &self.fields, &joined) {
                    Ok(true) => {
                        self.matched = true;
                        return Some(Ok(joined));
                    }
                    Ok(false) => {}
                    Err(e) => return Some(Err(e)),
                }
            }
            let mut left = self.current.take().expect("set above");
            if self.kind == JoinKind::Left && !self.matched {
                left.extend(std::iter::repeat_n(Value::Null, self.right_width));
                return Some(Ok(left));
            }
        }
    }
}

/// Sorts rows by the values of `keys`, in the order [`compare`] puts them
/// (which is how the sorter orders values of different types too), then by
/// `then`, as ORDER BY would.
fn sort(rows: Rows, fields: &[Field], keys: &[Expr], desc: &[bool], then: &[OrderBy], budget: usize) -> Result<Sorted> {
    let directions = desc.iter().copied().chain(then.iter().map(|o| o.desc)).collect();
    let mut sorter = Sorter::new(directions, None, budget);
    let mut sort_keys = Vec::new();
    for row in rows {
        let row = row?;
        sort_keys.clear();
        for e in keys {
            sort_keys.push(eval(e, fields, &row)?);
        }
        for o in then {
            sort_keys.push(eval(&o.expr, fields, &row)?);
        }
        sorter.push(&sort_keys, row)?;
    }
    sorter.finish()
}

/// Compares keys as a merge join sorts them: each in its direction, and the
/// values of one type all before or all after those of another, which they
/// are never equal to, but for INTs and REALs, which compare by value.
fn compare(a: &[Value], b: &[Value], desc: &[bool]) -> Ordering {
    a.iter()
        .zip(b)
        .zip(desc)
        .map(|((a, b), &desc)| {
            let ord = rank(a).cmp(&rank(b)).then_with(|| a.sort_cmp(b));
            if desc { ord.reverse() } else { ord }
        })
        .find(|ord| ord.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Where a value's type sorts among the others, NULL first.
fn rank(value: &Value) -> i64 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::Real(_) => 2,
        Value::Text(_) => 3,
    }
}

/// The values of the key expressions on a row.
fn keys(exprs: &[Expr], fields: &[Field], row: &Row) -> Result<Vec<Value>> {
    exprs.iter().map(|e| eval(e, fields, row)).collect()
}
