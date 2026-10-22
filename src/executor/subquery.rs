//! Runs the subqueries of the expressions an operator evaluates, for each
//! row: replaces the subquery's outer references by the row's values, then
//! plans and runs it like any SELECT (see `plan/query.rs`).
//!
//! Answers are kept, by the subquery and the values it was given, so
//! an uncorrelated subquery runs once, and a correlated one once per distinct
//! set of outer values. An `EXISTS` reads only the first row; a value
//! subquery fails if it returns a second.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::{Settings, model};
use crate::catalog::Catalog;
use crate::error::{Result, err};
use crate::plan::{filter, query};
use crate::sql::ast::{ColumnRef, Expr, Select};
use crate::sql::eval::{Answer, Field, Subqueries, resolve};
use crate::types::Value;

pub struct Runner<'a> {
    catalog: &'a Catalog,
    settings: Settings,
    /// Answers by subquery, then by the values of its outer references.
    answers: HashMap<Select, HashMap<Vec<Value>, Rc<Answer>>>,
}

impl<'a> Runner<'a> {
    pub fn new(catalog: &'a Catalog, settings: &Settings) -> Self {
        Runner {
            catalog,
            settings: settings.clone(),
            answers: HashMap::new(),
        }
    }
}

impl Subqueries for Runner<'_> {
    fn answer(&mut self, subquery: &Expr, fields: &[Field], row: &[Value]) -> Result<Rc<Answer>> {
        let query: &Select = match subquery {
            Expr::Subquery(query) | Expr::Exists(query) | Expr::InSubquery(_, query, _) => query,
            _ => unreachable!("not a subquery"),
        };
        let mut columns: Vec<ColumnRef> = Vec::new();
        filter::visit_outer(query, &mut |column| {
            if !columns.contains(column) {
                columns.push(column.clone());
            }
        });
        let values = columns
            .iter()
            .map(|c| Ok(row[resolve(fields, c)?].clone()))
            .collect::<Result<Vec<_>>>()?;
        if let Some(answer) = self
            .answers
            .get(query)
            .and_then(|answers| answers.get(&values))
        {
            return Ok(answer.clone());
        }

        let bound = query::substitute(query, &mut |column| {
            let i = columns
                .iter()
                .position(|c| c == column)
                .expect("collected above");
            Ok(values[i].clone())
        })?;
        let plan = query::select(self.catalog, bound, self.settings.force_join)?;
        let mut rows = model::open(plan.root, self.catalog, &self.settings);
        let answer = match subquery {
            Expr::Exists(_) => Answer::Exists(rows.next().transpose()?.is_some()),
            Expr::Subquery(_) => {
                let value = rows
                    .next()
                    .transpose()?
                    .map_or(Value::Null, |mut row| row.swap_remove(0));
                if rows.next().transpose()?.is_some() {
                    return err("more than one row returned by a subquery used as a value");
                }
                Answer::Value(value)
            }
            _ => {
                let (mut values, mut null) = (HashSet::new(), false);
                for row in rows {
                    match row?.swap_remove(0) {
                        Value::Null => null = true,
                        value => {
                            values.insert(value);
                        }
                    }
                }
                Answer::Values { values, null }
            }
        };
        let answer = Rc::new(answer);
        self.answers
            .entry(query.clone())
            .or_default()
            .insert(values, answer.clone());
        Ok(answer)
    }
}
