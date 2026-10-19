//! Joins: for each left row, the right rows that may match it, paired with
//! it where the join's condition holds.
//!
//! ```text
//! left row --> candidates on the right --> left ++ right, if the condition holds
//!                  hash:  the right rows with equal keys, from a hash table built once
//!                  index: the right rows an index lookup finds, per left row
//!
//! block of left rows --> every right row, read again per block --> left ++ right, if the condition holds
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

use std::collections::HashMap;

use super::operator::Rows;
use super::sort::value_size;
use crate::catalog::index::Bound;
use crate::catalog::table::{Access, Row, Table};
use crate::error::Result;
use crate::plan::query::LookupKey;
use crate::predicate::{Compare, Predicate};
use crate::sql::ast::{Expr, JoinKind};
use crate::sql::eval::{Field, eval, matches};
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
    /// The left row being joined, and which of `right.rows()` may match it.
    current: Option<Row>,
    candidates: Vec<usize>,
    next: usize,
    matched: bool,
}

/// How a join finds the right rows that may match a left row.
pub enum Right<'a> {
    /// Right rows by their keys, read on first use. Rows with a NULL key are dropped: they match nothing.
    Hash {
        source: Option<Rows<'a>>,
        rows: Vec<Row>,
        buckets: HashMap<Vec<Value>, Vec<usize>>,
        left_keys: Vec<Expr>,
        right_keys: Vec<Expr>,
        right_fields: Vec<Field>,
    },
    /// The right rows a lookup finds for the current left row.
    Index { lookup: Lookup<'a>, rows: Vec<Row> },
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
            candidates: Vec::new(),
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
                match self.right.candidates(&row, &self.left_fields) {
                    Ok(candidates) => self.candidates = candidates,
                    Err(e) => return Some(Err(e)),
                }
                (self.current, self.next, self.matched) = (Some(row), 0, false);
            }
            let left = self.current.as_ref().expect("set above");
            while self.next < self.candidates.len() {
                let right = &self.right.rows()[self.candidates[self.next]];
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

impl Right<'_> {
    fn rows(&self) -> &[Row] {
        match self {
            Right::Hash { rows, .. } | Right::Index { rows, .. } => rows,
        }
    }

    /// The positions in `rows()` of the right rows that may match `left`.
    fn candidates(&mut self, left: &Row, left_fields: &[Field]) -> Result<Vec<usize>> {
        match self {
            Right::Hash { source, rows, buckets, left_keys, right_keys, right_fields } => {
                if let Some(source) = source.take() {
                    for row in source {
                        let row = row?;
                        let key = keys(right_keys, right_fields, &row)?;
                        if !key.iter().any(Value::is_null) {
                            buckets.entry(key).or_default().push(rows.len());
                            rows.push(row);
                        }
                    }
                }
                let key = keys(left_keys, left_fields, left)?;
                Ok(buckets.get(&key).cloned().unwrap_or_default())
            }
            Right::Index { lookup, rows } => {
                *rows = lookup.find(&eval(&lookup.left_key, left_fields, left)?)?;
                Ok((0..rows.len()).collect())
            }
        }
    }
}

impl Lookup<'_> {
    /// The rows whose key column equals `value`, and that the filter matches.
    fn find(&self, value: &Value) -> Result<Vec<Row>> {
        if value.is_null() {
            return Ok(Vec::new());
        }
        let data_type = self.table.columns[self.column].data_type;
        if !value.fits(data_type) {
            return Ok(Vec::new()); // never equal, as in a hash join
        }
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
    /// Bytes of left rows a block may hold; it holds at least one row.
    budget: usize,
    /// The left rows being joined, and which of them a right row matched.
    block: Vec<Row>,
    matched: Vec<bool>,
    phase: Phase<'a>,
}

enum Phase<'a> {
    /// Reading the next block of left rows.
    Fill,
    /// Pairing the right row `row` with the block's rows from `next` on, reading `right` for the next one.
    Pair { right: Rows<'a>, row: Option<Row>, next: usize },
    /// The right side has ended: padding the block's unmatched rows from `next` on, for a LEFT JOIN.
    Pad { next: usize },
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
            phase: Phase::Fill,
        }
    }
}

impl Iterator for BlockJoin<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match &mut self.phase {
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
                    self.phase = Phase::Pair { right: (self.right)(), row: None, next: 0 };
                }
                Phase::Pair { right, row, next } => {
                    let Some(right_row) = row.as_ref().filter(|_| *next < self.block.len()) else {
                        match right.next() {
                            Some(Ok(new)) => (*row, *next) = (Some(new), 0),
                            Some(Err(e)) => return Some(Err(e)),
                            None => self.phase = Phase::Pad { next: 0 },
                        }
                        continue;
                    };
                    let i = *next;
                    *next += 1;
                    let joined: Row = self.block[i].iter().chain(right_row).cloned().collect();
                    match matches(&self.condition, &self.fields, &joined) {
                        Ok(true) => {
                            self.matched[i] = true;
                            return Some(Ok(joined));
                        }
                        Ok(false) => {}
                        Err(e) => return Some(Err(e)),
                    }
                }
                Phase::Pad { next } => {
                    while self.kind == JoinKind::Left && *next < self.block.len() {
                        let i = *next;
                        *next += 1;
                        if !self.matched[i] {
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

/// The values of the key expressions on a row.
fn keys(exprs: &[Expr], fields: &[Field], row: &Row) -> Result<Vec<Value>> {
    exprs.iter().map(|e| eval(e, fields, row)).collect()
}
