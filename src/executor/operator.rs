//! Operators: each step of a query plan, as an iterator over its rows that
//! pulls rows from the step below it (the Volcano model).
//!
//! A scan, filter, limit and projection hand rows on one at a time, so a
//! `LIMIT` stops the scan once it has enough, and a query's rows can be read
//! without holding them all. A sort and an aggregate are blocking: they read
//! all of their input when their first row is asked for. So is the right side
//! of a hash or nested loop join (see `join.rs`); the left side streams.

use std::collections::HashMap;

use super::Settings;
use super::aggregate;
use super::join::{Join, Lookup, Right};
use super::sort::Sorter;
use crate::catalog::table::Row;
use crate::error::Result;
use crate::plan::query::{JoinMethod, Node};
use crate::sql::eval::{Field, eval, matches};

/// Rows produced by an operator, or the error that stopped it.
pub type Rows<'a> = Box<dyn Iterator<Item = Result<Row>> + 'a>;

/// Opens a plan step and everything below it. No row is read until asked for.
pub fn open<'a>(node: Node<'a>, settings: &Settings) -> Rows<'a> {
    match node {
        Node::Scan { table, needed, predicates, access, .. } => {
            Box::new(table.scan_filtered(&needed, &predicates, &access).map(|item| item.map(|(_, row)| row)))
        }
        Node::Lookup { .. } => unreachable!("a lookup is only an index join's right side"),
        Node::Join { left, right, kind, method, condition } => {
            let (left_fields, right_fields) = (left.columns(), right.columns());
            let left = open(*left, settings);
            let right = match method {
                JoinMethod::NestedLoop => Right::Loop { source: Some(open(*right, settings)), rows: Vec::new() },
                JoinMethod::Hash { left_keys, right_keys } => Right::Hash {
                    source: Some(open(*right, settings)),
                    rows: Vec::new(),
                    buckets: HashMap::new(),
                    left_keys,
                    right_keys,
                    right_fields: right_fields.clone(),
                },
                JoinMethod::Index => {
                    let (filter, lookup) = match *right {
                        Node::Filter { input, predicate } => (Some(predicate), *input),
                        node => (None, node),
                    };
                    let Node::Lookup { table, qualifier, needed, column, key, left_key } = lookup else {
                        unreachable!("an index join reads its right side through a lookup");
                    };
                    let fields = Field::of_table(&qualifier, &table.columns);
                    let lookup = Lookup { table, needed, column, key, left_key, filter, fields };
                    Right::Index { lookup, rows: Vec::new() }
                }
            };
            Box::new(Join::new(left, left_fields, right, right_fields, kind, condition))
        }
        Node::Filter { input, predicate } => {
            let columns = input.columns();
            let predicate = Some(predicate);
            Box::new(open(*input, settings).filter_map(move |item| {
                match item.and_then(|row| Ok((matches(&predicate, &columns, &row)?, row))) {
                    Ok((true, row)) => Some(Ok(row)),
                    Ok((false, _)) => None,
                    Err(e) => Some(Err(e)),
                }
            }))
        }
        Node::Limit { input, count } => Box::new(open(*input, settings).take(count)),
        Node::Project { input, exprs } => {
            let columns = input.columns();
            Box::new(open(*input, settings).map(move |item| {
                let row = item?;
                exprs.iter().map(|e| eval(e, &columns, &row)).collect()
            }))
        }
        Node::Sort { input, keys, limit } => {
            let columns = input.columns();
            let input = open(*input, settings);
            let budget = settings.sort_memory;
            deferred(move || {
                let mut sorter = Sorter::new(keys.iter().map(|k| k.desc).collect(), limit, budget);
                for row in input {
                    let row = row?;
                    let values = keys.iter().map(|k| eval(&k.expr, &columns, &row)).collect::<Result<_>>()?;
                    sorter.push(values, row)?;
                }
                Ok(Box::new(sorter.finish()?))
            })
        }
        Node::Aggregate { input, group_by, aggregates } => {
            let columns = input.columns();
            let input = open(*input, settings);
            deferred(move || {
                let groups = aggregate::fold(input, &columns, &group_by, &aggregates)?;
                Ok(Box::new(groups.into_iter().map(Ok)))
            })
        }
    }
}

/// Rows that `start` produces, called when the first one is asked for.
fn deferred<'a>(start: impl FnOnce() -> Result<Rows<'a>> + 'a) -> Rows<'a> {
    Box::new(Deferred { start: Some(Box::new(start)), rows: None })
}

struct Deferred<'a> {
    start: Option<Box<dyn FnOnce() -> Result<Rows<'a>> + 'a>>,
    rows: Option<Rows<'a>>,
}

impl Iterator for Deferred<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(start) = self.start.take() {
            match start() {
                Ok(rows) => self.rows = Some(rows),
                Err(e) => return Some(Err(e)),
            }
        }
        self.rows.as_mut()?.next()
    }
}
