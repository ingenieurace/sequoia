//! The joins that find the right rows that may match each left row by its
//! keys: a hash join, whose right side is in `hash.rs`, and an index join,
//! whose lookups are in `index.rs`. What they share is here: pairing a left
//! row with its candidates where the condition holds, LEFT JOIN padding,
//! and sorting the rows back into their left rows' order when a hash join
//! spills and that order must be kept.

use super::hash::HashSide;
use super::index::Lookup;
use crate::catalog::table::Row;
use crate::error::Result;
use crate::executor::operator::Rows;
use crate::executor::sort::{Sorted, Sorter};
use crate::sql::ast::{Expr, JoinKind};
use crate::sql::eval::{Field, eval, locate, matches, resolve};
use crate::types::Value;

pub struct Join<'a> {
    left: Rows<'a>,
    left_fields: Vec<Field>,
    right: Right<'a>,
    right_width: usize,
    kind: JoinKind,
    condition: Option<Expr>,
    /// The left fields, then the right ones: the fields of a joined row.
    fields: Vec<Field>,
    /// The left row being joined, its number among the left rows, how
    /// many right rows may match it (see [`Right::candidate`]), and the next
    /// of them to try.
    current: Option<Row>,
    number: u64,
    candidates: usize,
    next: usize,
    matched: bool,
    /// Whether the left side has run out, and how many rows it gave.
    left_done: bool,
    left_rows: u64,
    /// Whether the rows must come out in their left rows' order. If the
    /// right side spills, they are sorted back into it, in `sorted`, with
    /// this budget; a left row written out carries its number with it.
    keep_order: Option<usize>,
    sorted: Option<Sorted>,
}

/// How a join finds the right rows that may match a left row.
pub enum Right<'a> {
    /// Right rows grouped by their keys, read on first use, partly written
    /// out past a budget. Rows with a NULL key are dropped: they match nothing.
    Hash(HashSide<'a>),
    /// The right rows a lookup finds for the current left row.
    Index { lookup: Lookup<'a>, rows: Vec<Row> },
}

/// A join key: a column, found by its position, or any other expression.
pub enum Key {
    Column(usize),
    Expr(Expr),
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
        let fields = [left_fields.clone(), right_fields].concat();
        Join {
            left,
            right,
            right_width: fields.len() - left_fields.len(),
            kind,
            condition: condition.map(|c| locate(&c, &fields)),
            fields,
            left_fields,
            current: None,
            number: 0,
            candidates: 0,
            next: 0,
            matched: false,
            left_done: false,
            left_rows: 0,
            keep_order: None,
            sorted: None,
        }
    }

    /// Makes the rows come out in their left rows' order even if a hash
    /// join's right side spills, by sorting them back into it within `budget`.
    pub fn keeping_order(self, budget: usize) -> Self {
        Join {
            keep_order: Some(budget),
            ..self
        }
    }

    /// The next joined row, and the number of the left row in it.
    fn step(&mut self) -> Option<Result<(u64, Row)>> {
        loop {
            if self.current.is_none() {
                let (mut row, number) = match self.next_left() {
                    Ok(Some(row)) => row,
                    Ok(None) => return None,
                    Err(e) => return Some(Err(e)),
                };
                // A row that may be written out takes its number along.
                if self.keep_order.is_some() {
                    row.push(Value::Int(number as i64));
                }
                match self.right.find(&row, &self.left_fields) {
                    Ok(Some(candidates)) => self.candidates = candidates,
                    Ok(None) => continue, // comes back from `deferred`
                    Err(e) => return Some(Err(e)),
                }
                if self.keep_order.is_some() {
                    row.pop();
                }
                (self.current, self.number, self.next, self.matched) =
                    (Some(row), number, 0, false);
            }
            let left = self.current.as_ref().expect("set above");
            while self.next < self.candidates {
                let right = self.right.candidate(self.next);
                self.next += 1;
                let joined: Row = left.iter().chain(right).cloned().collect();
                match matches(&self.condition, &self.fields, &joined) {
                    Ok(true) => {
                        self.matched = true;
                        return Some(Ok((self.number, joined)));
                    }
                    Ok(false) => {}
                    Err(e) => return Some(Err(e)),
                }
            }
            let mut left = self.current.take().expect("set above");
            if self.kind == JoinKind::Left && !self.matched {
                left.extend(std::iter::repeat_n(Value::Null, self.right_width));
                return Some(Ok((self.number, left)));
            }
        }
    }

    /// The next left row and its number: from the left side, then those a
    /// hash join wrote out, which carry their numbers if order is kept.
    fn next_left(&mut self) -> Result<Option<(Row, u64)>> {
        if !self.left_done {
            match self.left.next() {
                Some(row) => {
                    self.left_rows += 1;
                    return Ok(Some((row?, self.left_rows - 1)));
                }
                None => self.left_done = true,
            }
        }
        let Some(mut row) = self.right.deferred()? else {
            return Ok(None);
        };
        let number = match self.keep_order {
            Some(_) => match row.pop() {
                Some(Value::Int(n)) => n as u64,
                _ => unreachable!("a left row written out carries its number"),
            },
            None => 0,
        };
        Ok(Some((row, number)))
    }
}

impl Iterator for Join<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(sorted) = &mut self.sorted {
            return sorted.next();
        }
        let first = self.step();
        let Some(budget) = self.keep_order.filter(|_| self.right.spilled()) else {
            return first.map(|item| item.map(|(_, row)| row));
        };
        // The right side spilled, before any row came out: sort them all
        // by their left rows' numbers, stably, which keeps their order within one.
        let mut sorter = Sorter::new(vec![false], None, budget);
        let mut item = first;
        while let Some(result) = item {
            match result {
                Ok((number, row)) => {
                    if let Err(e) = sorter.push(&[Value::Int(number as i64)], row) {
                        return Some(Err(e));
                    }
                }
                Err(e) => return Some(Err(e)),
            }
            item = self.step();
        }
        match sorter.finish() {
            Ok(sorted) => self.sorted.insert(sorted).next(),
            Err(e) => Some(Err(e)),
        }
    }
}

impl<'a> Right<'a> {
    /// A hash join's right side: the rows of `source`, by `right_keys`, for
    /// left rows to find by `left_keys`, held within `budget`, split past it
    /// by the hash bits of `level` first.
    pub fn hash(
        source: Rows<'a>,
        left_keys: Vec<Expr>,
        right_keys: Vec<Expr>,
        left_fields: &[Field],
        right_fields: Vec<Field>,
        budget: usize,
        level: u32,
    ) -> Self {
        let (left_keys, right_keys) = (
            Key::all(left_keys, left_fields),
            Key::all(right_keys, &right_fields),
        );
        Right::Hash(HashSide::new(
            source,
            left_keys,
            right_keys,
            left_fields.to_vec(),
            right_fields,
            budget,
            level,
        ))
    }

    /// Finds the right rows that may match `left`, and returns how many there
    /// are, which [`candidate`](Self::candidate) gives, or `None` if a hash
    /// join wrote `left` out, to come back from [`deferred`](Self::deferred).
    fn find(&mut self, left: &Row, left_fields: &[Field]) -> Result<Option<usize>> {
        match self {
            Right::Hash(side) => side.find(left),
            Right::Index { lookup, rows } => {
                *rows = lookup.find(&eval(&lookup.left_key, left_fields, left)?)?;
                Ok(Some(rows.len()))
            }
        }
    }

    /// The `i`th right row [`find`](Self::find) found.
    fn candidate(&self, i: usize) -> &Row {
        match self {
            Right::Hash(side) => side.candidate(i),
            Right::Index { rows, .. } => &rows[i],
        }
    }

    /// Once the left side has run out, the left rows a hash join wrote out.
    fn deferred(&mut self) -> Result<Option<Row>> {
        match self {
            Right::Hash(side) => side.deferred(),
            Right::Index { .. } => Ok(None),
        }
    }

    fn spilled(&self) -> bool {
        match self {
            Right::Hash(side) => side.spilled(),
            Right::Index { .. } => false,
        }
    }
}

impl Key {
    /// The keys `exprs`, on rows with these fields.
    pub(super) fn all(exprs: Vec<Expr>, fields: &[Field]) -> Vec<Key> {
        exprs
            .into_iter()
            .map(|expr| match &expr {
                Expr::Column(column) => {
                    resolve(fields, column).map_or(Key::Expr(expr), Key::Column)
                }
                _ => Key::Expr(locate(&expr, fields)),
            })
            .collect()
    }

    /// Puts the values of `keys` on `row` in `values`.
    pub(super) fn eval_all(
        keys: &[Key],
        fields: &[Field],
        row: &Row,
        values: &mut Vec<Value>,
    ) -> Result<()> {
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
