//! Simple conditions on one column, shared by the planner and the storage
//! layer, which evaluates them without decoding whole rows.

use std::cmp::Ordering;

use crate::types::Value;

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compare {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

impl Compare {
    /// Whether `a op b` holds, given how `a` compares with `b`.
    pub fn holds(self, ord: Ordering) -> bool {
        match self {
            Compare::Eq => ord.is_eq(),
            Compare::NotEq => ord.is_ne(),
            Compare::Lt => ord.is_lt(),
            Compare::LtEq => ord.is_le(),
            Compare::Gt => ord.is_gt(),
            Compare::GtEq => ord.is_ge(),
        }
    }

    /// The operator as SQL writes it.
    pub fn symbol(self) -> &'static str {
        match self {
            Compare::Eq => "=",
            Compare::NotEq => "!=",
            Compare::Lt => "<",
            Compare::LtEq => "<=",
            Compare::Gt => ">",
            Compare::GtEq => ">=",
        }
    }

    /// The comparison with its sides swapped: `a < b` is `b > a`.
    pub fn flip(self) -> Self {
        match self {
            Compare::Lt => Compare::Gt,
            Compare::LtEq => Compare::GtEq,
            Compare::Gt => Compare::Lt,
            Compare::GtEq => Compare::LtEq,
            op => op,
        }
    }
}

/// A condition `column op literal`, taken from a WHERE clause: a column store
/// evaluates it on compressed chunks, the planner picks an index with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    /// Index of the column in the schema.
    pub column: usize,
    pub op: Compare,
    /// A non-NULL value of the column's type.
    pub literal: Value,
}

impl Predicate {
    /// Whether a decoded row satisfies the predicate; NULL never does.
    pub fn matches(&self, row: &[Value]) -> bool {
        row[self.column]
            .compare(&self.literal)
            .is_some_and(|ord| self.op.holds(ord))
    }
}
