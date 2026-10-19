//! Operators: each step of a query plan, as an iterator over its rows that
//! pulls rows from the step below it (the Volcano model).
//!
//! A scan, filter, limit, projection and distinct hand rows on one at a time, so a
//! `LIMIT` stops the scan once it has enough, and a query's rows can be read
//! without holding them all. A sort and an aggregate are blocking: they read
//! all of their input when their first row is asked for. So is the right side
//! of a hash join (see `join.rs`); the left side streams. A block nested loop
//! join reads its left side a block at a time, and its right side again for
//! each block. A merge join sorts both sides when its first row is asked for.

use std::collections::{HashMap, HashSet};

use super::Settings;
use super::aggregate;
use super::join::{BlockJoin, Join, Lookup, MergeJoin, MergeSort, Right};
use super::sort::Sorter;
use super::subquery::Runner;
use crate::catalog::Catalog;
use crate::catalog::table::Row;
use crate::error::Result;
use crate::plan::query::{JoinMethod, Node};
use crate::sql::eval::{Field, eval_with, matches_with};

/// Rows produced by an operator, or the error that stopped it.
pub type Rows<'a> = Box<dyn Iterator<Item = Result<Row>> + 'a>;

/// Opens a plan step and everything below it, on tables of `catalog` (which
/// subqueries read). No row is read until asked for.
pub fn open<'a>(node: Node<'a>, catalog: &'a Catalog, settings: &Settings) -> Rows<'a> {
    match node {
        Node::Scan { table, needed, predicates, access, .. } => {
            Box::new(table.scan_filtered(&needed, &predicates, &access).map(|item| item.map(|(_, row)| row)))
        }
        Node::Lookup { .. } => unreachable!("a lookup is only an index join's right side"),
        Node::Join { left, right, kind, method, condition } => {
            let (left_fields, right_fields) = (left.columns(), right.columns());
            let left = open(*left, catalog, settings);
            let right = match method {
                JoinMethod::Block => {
                    let budget = settings.join_memory;
                    let settings = settings.clone();
                    let right = Box::new(move || open((*right).clone(), catalog, &settings));
                    return Box::new(BlockJoin::new(left, left_fields, right, right_fields, kind, condition, budget));
                }
                JoinMethod::Merge { left_keys, right_keys, desc, then } => {
                    let right = open(*right, catalog, settings);
                    let sort = MergeSort { left_keys, right_keys, desc, then, budget: settings.sort_memory };
                    return Box::new(MergeJoin::new(left, left_fields, right, right_fields, sort, kind, condition));
                }
                JoinMethod::Hash { left_keys, right_keys } => Right::Hash {
                    source: Some(open(*right, catalog, settings)),
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
            let mut subqueries = Runner::new(catalog, settings);
            Box::new(open(*input, catalog, settings).filter_map(move |item| {
                match item.and_then(|row| Ok((matches_with(&predicate, &columns, &row, &mut subqueries)?, row))) {
                    Ok((true, row)) => Some(Ok(row)),
                    Ok((false, _)) => None,
                    Err(e) => Some(Err(e)),
                }
            }))
        }
        Node::Distinct { input } => {
            let mut seen = HashSet::new();
            Box::new(open(*input, catalog, settings).filter(move |item| match item {
                Ok(row) => seen.insert(row.clone()),
                Err(_) => true,
            }))
        }
        Node::Limit { input, count } => Box::new(open(*input, catalog, settings).take(count)),
        Node::Derived { input, .. } => open(*input, catalog, settings),
        Node::Project { input, exprs } => {
            let columns = input.columns();
            let mut subqueries = Runner::new(catalog, settings);
            Box::new(open(*input, catalog, settings).map(move |item| {
                let row = item?;
                exprs.iter().map(|e| eval_with(e, &columns, &row, &mut subqueries)).collect()
            }))
        }
        Node::Sort { input, keys, limit } => {
            let columns = input.columns();
            let input = open(*input, catalog, settings);
            let budget = settings.sort_memory;
            let mut subqueries = Runner::new(catalog, settings);
            deferred(move || {
                let mut sorter = Sorter::new(keys.iter().map(|k| k.desc).collect(), limit, budget);
                for row in input {
                    let row = row?;
                    let values = keys
                        .iter()
                        .map(|k| eval_with(&k.expr, &columns, &row, &mut subqueries))
                        .collect::<Result<_>>()?;
                    sorter.push(values, row)?;
                }
                Ok(Box::new(sorter.finish()?))
            })
        }
        Node::Aggregate { input, group_by, aggregates } => {
            let columns = input.columns();
            let input = open(*input, catalog, settings);
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
