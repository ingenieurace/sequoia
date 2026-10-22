//! The right side of an index join: for each left row, the rows of a table
//! whose key column equals its key, found through an index or the primary
//! key.

use crate::catalog::table::{Access, Row, Table};
use crate::error::Result;
use crate::plan::query::LookupKey;
use crate::predicate::{Compare, Predicate};
use crate::sql::ast::Expr;
use crate::sql::eval::{Field, matches};
use crate::types::Value;

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

impl Lookup<'_> {
    /// The rows whose key column equals `value`, and that the filter matches.
    pub(super) fn find(&self, value: &Value) -> Result<Vec<Row>> {
        if value.is_null() {
            return Ok(Vec::new());
        }
        let data_type = self.table.columns[self.column].data_type;
        // The column's value that equals this one, if any: as in a hash join, 1 finds 1.0.
        let Some(value) = &value.coerce(data_type) else {
            return Ok(Vec::new());
        };
        let predicates = [Predicate {
            column: self.column,
            op: Compare::Eq,
            literal: value.clone(),
        }];
        let access = match self.key {
            LookupKey::Index(index) => {
                let covering = self.table.indexes()[index].covers(&self.needed);
                Access::Index {
                    index,
                    equal: vec![value.clone()],
                    lower: None,
                    upper: None,
                    covering,
                }
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
