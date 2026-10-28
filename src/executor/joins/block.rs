//! The block nested loop join.

use crate::catalog::table::Row;
use crate::error::Result;
use crate::executor::operator::Rows;
use crate::executor::sort::value_size;
use crate::sql::ast::{Expr, JoinKind};
use crate::sql::eval::{Field, NoSubqueries, Subqueries, locate, matches_with};
use crate::types::Value;

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
    /// Runs the subqueries of the condition, if it has any.
    subqueries: Box<dyn Subqueries + 'a>,
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
    /// Runs the subqueries of the join's condition with `subqueries`.
    pub fn running(mut self, subqueries: Box<dyn Subqueries + 'a>) -> Self {
        self.subqueries = subqueries;
        self
    }

    pub fn new(
        left: Rows<'a>,
        left_fields: Vec<Field>,
        right: Box<dyn Fn() -> Rows<'a> + 'a>,
        right_fields: Vec<Field>,
        kind: JoinKind,
        condition: Option<Expr>,
        budget: usize,
    ) -> Self {
        let right_width = right_fields.len();
        let fields = [left_fields, right_fields].concat();
        BlockJoin {
            left: Some(left),
            right,
            right_width,
            kind,
            condition: condition.map(|c| locate(&c, &fields)),
            subqueries: Box::new(NoSubqueries),
            fields,
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
                    match matches_with(&self.condition, &self.fields, &self.pair, &mut *self.subqueries) {
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
