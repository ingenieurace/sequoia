//! The naive nested loop join: for each left row, reads the right side again
//! from the start and pairs the row with every right row. It holds nothing
//! but the left row, and reads the right side once per left row, where a
//! block nested loop join reads it once per block.
//!
//! The planner never picks it, and no plan runs it: it is here for the
//! joins benchmark to measure the other methods against.

use crate::catalog::table::Row;
use crate::error::Result;
use crate::executor::operator::Rows;
use crate::sql::ast::{Expr, JoinKind};
use crate::sql::eval::{Field, locate, matches};
use crate::types::Value;

/// A naive nested loop join: pairs each left row with every right row,
/// reading the right side again for each left row.
pub struct NaiveJoin<'a> {
    left: Rows<'a>,
    /// Starts reading the right side from the beginning.
    right: Box<dyn Fn() -> Rows<'a> + 'a>,
    right_width: usize,
    kind: JoinKind,
    condition: Option<Expr>,
    /// The left fields, then the right ones: the fields of a joined row.
    fields: Vec<Field>,
    /// The left row being joined, with the right side as read again for it,
    /// and whether a right row matched it.
    current: Option<(Row, Rows<'a>)>,
    matched: bool,
    /// A pair, built in place to check the condition on, as a block nested
    /// loop join builds it: the two differ in how often they read the right
    /// side, not in what a pair costs.
    pair: Row,
}

impl<'a> NaiveJoin<'a> {
    pub fn new(
        left: Rows<'a>,
        left_fields: Vec<Field>,
        right: Box<dyn Fn() -> Rows<'a> + 'a>,
        right_fields: Vec<Field>,
        kind: JoinKind,
        condition: Option<Expr>,
    ) -> Self {
        let right_width = right_fields.len();
        let fields = [left_fields, right_fields].concat();
        NaiveJoin {
            left,
            right,
            right_width,
            kind,
            condition: condition.map(|c| locate(&c, &fields)),
            fields,
            current: None,
            matched: false,
            pair: Vec::new(),
        }
    }
}

impl Iterator for NaiveJoin<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current.is_none() {
                let left = match self.left.next()? {
                    Ok(row) => row,
                    Err(e) => return Some(Err(e)),
                };
                (self.current, self.matched) = (Some((left, (self.right)())), false);
            }
            let (left, right) = self.current.as_mut().expect("set above");
            match right.next() {
                Some(Ok(row)) => {
                    self.pair.clear();
                    self.pair.extend(left.iter().cloned());
                    self.pair.extend(row);
                    match matches(&self.condition, &self.fields, &self.pair) {
                        Ok(true) => {
                            self.matched = true;
                            return Some(Ok(std::mem::take(&mut self.pair))); // the next pair allocates anew
                        }
                        Ok(false) => {}
                        Err(e) => return Some(Err(e)),
                    }
                }
                Some(Err(e)) => return Some(Err(e)),
                None => {
                    let (mut left, _) = self.current.take().expect("set above");
                    if self.kind == JoinKind::Left && !self.matched {
                        left.extend(std::iter::repeat_n(Value::Null, self.right_width));
                        return Some(Ok(left));
                    }
                }
            }
        }
    }
}
