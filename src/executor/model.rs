//! Execution models: how the steps of a plan hand rows to each other. The
//! same plan runs in any of three:
//!
//! | model           | a step returns                      | suits                                    | code             |
//! |-----------------|-------------------------------------|------------------------------------------|------------------|
//! | iterator        | one row per call (Volcano)          | anything; joins; holds little            | `operator.rs`    |
//! | materialization | all of its rows, in one call        | few rows at every step (OLTP)            | `materialize.rs` |
//! | vectorized      | a batch of rows, a column at a time | scans and aggregates of many rows (OLAP) | `vectorized/`    |
//!
//! They return the same rows, errors included: a step of any model that
//! fails hands on the rows before the error first, so a LIMIT that has its
//! rows by then ends a query without the error in every model. Joins and
//! sorts are the iterator model's in all three.
//!
//! Which model runs a query: the one the session set ([`Settings::model`]),
//! or else one [`choose`] picks for the query from the planner's estimates
//! of how many rows each step reads or returns (a scan of a whole table
//! reads all of its rows, however few it returns):
//!
//! - **materialization** if no step has more than [`FEW_ROWS`]: holding a
//!   step's rows then costs nothing, and a LIMIT still stops the scan below
//!   it. With more, it would hold them all, and a filter under a LIMIT
//!   would make every row of its input (`benches/models.rs` has one 180
//!   times slower than in the iterator model).
//! - **vectorized** if a step has [`MANY_ROWS`] or more and the query has no
//!   join and no subquery: what such a query spends its time on is the work
//!   per row that batches do once per batch, and a column table's rows come
//!   in columns already (a column table's aggregates run 5 to 10 times
//!   faster). Joins work on rows in every model, so batches would only be
//!   taken apart and put together around them, and subqueries run once
//!   per row.
//! - **iterator** otherwise: rows stream through the plan one at a time, a
//!   join's straight from the step below it.

use std::fmt;

use super::Settings;
use super::materialize;
use super::operator::{self, Rows, Step};
use super::vectorized::{self, batch::BATCH_SIZE};
use crate::catalog::Catalog;
use crate::catalog::table::Access;
use crate::plan::query::{Node, estimate};
use crate::sql::ast::Expr;
use crate::types::StorageKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    Iterator,
    Materialization,
    Vectorized,
}

impl Model {
    pub const ALL: [Model; 3] = [Model::Iterator, Model::Materialization, Model::Vectorized];

    pub fn name(self) -> &'static str {
        match self {
            Model::Iterator => "iterator",
            Model::Materialization => "materialization",
            Model::Vectorized => "vectorized",
        }
    }

    /// The model with this name, in any case.
    pub fn from_name(name: &str) -> Option<Model> {
        Model::ALL
            .into_iter()
            .find(|model| model.name().eq_ignore_ascii_case(name))
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The most rows every step of a plan may be guessed to return for it to
/// run in the materialization model.
pub const FEW_ROWS: f64 = 100.0;
/// The fewest rows a step of a plan must be guessed to return for it to run
/// in the vectorized model: a few batches.
pub const MANY_ROWS: f64 = (4 * BATCH_SIZE) as f64;

/// The model to run a plan in, and why.
pub fn choose(root: &Node, settings: &Settings) -> (Model, String) {
    if let Some(model) = settings.model {
        return (model, "set for the session".into());
    }
    let mut most = 0.0f64;
    let (mut joins, mut subqueries) = (false, false);
    visit(root, &mut |node| {
        most = most.max(rows(node));
        match node {
            Node::Join { condition, .. } => {
                joins = true;
                subqueries |= condition.as_ref().is_some_and(Expr::has_subquery);
            }
            Node::Filter { predicate, .. } => subqueries |= predicate.has_subquery(),
            Node::Project { exprs, .. } => subqueries |= exprs.iter().any(Expr::has_subquery),
            Node::Sort { keys, .. } => subqueries |= keys.iter().any(|key| key.expr.has_subquery()),
            _ => {}
        }
    });
    let rows = format!(
        "{most:.0} row{}",
        if most.round() == 1.0 { "" } else { "s" }
    );
    if most <= FEW_ROWS {
        return (
            Model::Materialization,
            format!("at most about {rows} at any step: each step hands all of its rows on at once"),
        );
    }
    let why = match (joins, subqueries) {
        (_, true) => "with subqueries, which run once per row",
        (true, _) => "through a join, which works on rows",
        _ if most >= MANY_ROWS => {
            let why = format!(
                "about {rows} at a step, no join: steps hand on batches of {BATCH_SIZE} rows, a column at a time"
            );
            return (Model::Vectorized, why);
        }
        _ => "too many to hold at once, too few for batches to pay",
    };
    (
        Model::Iterator,
        format!("up to about {rows} at a step, {why}: each step hands rows on one at a time"),
    )
}

/// Roughly how many rows a step reads or returns, whichever is more: a scan
/// of a whole table (a column table's is always) reads every row.
fn rows(node: &Node) -> f64 {
    match node {
        Node::Scan { table, access, .. }
            if *access == Access::Scan || table.storage_kind() == StorageKind::Column =>
        {
            table.row_count().map_or(1000.0, |rows| rows as f64)
        }
        node => estimate(node),
    }
}

/// Calls `f` on `node` and every step below it.
fn visit(node: &Node, f: &mut dyn FnMut(&Node)) {
    f(node);
    node.inputs().into_iter().for_each(|input| visit(input, f));
}

/// Opens a plan in the model [`choose`] picks. No row is read until asked for.
pub fn open<'a>(root: Node<'a>, catalog: &'a Catalog, settings: &Settings) -> Rows<'a> {
    let (model, _) = choose(&root, settings);
    open_in(model, root, catalog, settings, None)
}

/// Opens a plan in `model`, counting what its steps do in `step` if given.
pub(super) fn open_in<'a>(
    model: Model,
    root: Node<'a>,
    catalog: &'a Catalog,
    settings: &Settings,
    step: Step,
) -> Rows<'a> {
    match model {
        Model::Iterator => operator::open_step(root, catalog, settings, step),
        Model::Materialization => materialize::open(root, catalog, settings, step),
        Model::Vectorized => vectorized::open(root, catalog, settings, step),
    }
}
