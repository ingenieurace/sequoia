//! Query plans: a SELECT as a tree of steps, each reading rows from the one
//! below it, for the executor to run.
//!
//! ```text
//! SELECT city, COUNT(*) FROM users WHERE active GROUP BY city HAVING COUNT(*) > 1 ORDER BY city LIMIT 5
//!
//! Project  city, COUNT(*)
//!   Sort   city, keeping 5
//!     Filter  COUNT(*) > 1                 HAVING
//!       Aggregate  COUNT(*) by city
//!         Filter  active                   WHERE
//!           Scan  users, by the access path `access` picks
//! ```
//!
//! Every SELECT is a scan, a filter if it has WHERE, an aggregate (and a
//! filter for HAVING) if it groups, a sort if it has ORDER BY (which applies
//! the LIMIT itself, as a top-N sort) or else a limit, and a projection
//! unless it is `SELECT *`. The executor pulls rows through the tree one at a
//! time; only a sort and an aggregate need all of their input first.
//!
//! An aggregate's rows hold its group keys, then its aggregates. Expressions
//! above it are rewritten to read those: each `GROUP BY` expression and each
//! distinct aggregate becomes the column named by its SQL text (`city`,
//! `COUNT(*)`), and a column left over is rejected, as it is neither grouped
//! nor aggregated. The names are unique: a `GROUP BY` expression has no
//! aggregate in it, and no other expression prints the same.

use crate::catalog::table::{Access, Table};
use crate::error::{Result, err};
use crate::plan::{access, filter};
use crate::predicate::Predicate;
use crate::sql::ast::{Aggregate, Expr, OrderBy, Projection};
use crate::types::{Column, DataType};

/// A SELECT's plan: the tree, and the names of the columns it returns.
pub struct Plan<'a> {
    pub root: Node<'a>,
    pub columns: Vec<String>,
}

/// One step of a plan.
pub enum Node<'a> {
    /// Reads a table's rows the way `access` says.
    Scan { table: &'a Table, needed: Vec<bool>, predicates: Vec<Predicate>, access: Access },
    /// Keeps the rows `predicate` is true for.
    Filter { input: Box<Node<'a>>, predicate: Expr },
    /// Folds rows into one row per group: the keys, then the aggregates.
    Aggregate { input: Box<Node<'a>>, group_by: Vec<Expr>, aggregates: Vec<AggregateCall> },
    /// Orders the rows, keeping the first `limit` if given.
    Sort { input: Box<Node<'a>>, keys: Vec<OrderBy>, limit: Option<usize> },
    /// Keeps the first `count` rows.
    Limit { input: Box<Node<'a>>, count: usize },
    /// Evaluates the select list on each row.
    Project { input: Box<Node<'a>>, exprs: Vec<Expr> },
}

/// One aggregate an `Aggregate` step computes per group: the function and
/// its argument (none for `COUNT(*)`).
pub type AggregateCall = (Aggregate, Option<Expr>);

/// Plans a SELECT on `table`.
#[allow(clippy::too_many_arguments)]
pub fn select<'a>(
    table: &'a Table,
    projection: Projection,
    filter: Option<Expr>,
    group_by: Vec<Expr>,
    having: Option<Expr>,
    order_by: Vec<OrderBy>,
    limit: Option<usize>,
) -> Result<Plan<'a>> {
    forbid(&filter, "WHERE")?;
    let grouped = is_grouped(&projection, &group_by, &having, &order_by);
    let (exprs, columns) = match projection {
        Projection::All if grouped => return err("SELECT * cannot be used with GROUP BY or aggregate functions"),
        Projection::All => (None, table.columns.iter().map(|c| c.name.clone()).collect()),
        Projection::Exprs(exprs) => {
            let names = exprs.iter().map(output_name).collect();
            (Some(exprs), names)
        }
    };

    let mut node;
    let (exprs, having, order_by) = if grouped {
        forbid(&group_by, "GROUP BY")?;
        let exprs = exprs.expect("checked above");
        // Rewrite everything evaluated on groups; this also collects the aggregates.
        let mut aggregates = Vec::new();
        let mut rewrite = |expr: &Expr| rewrite(table, expr, &group_by, &mut aggregates);
        let exprs = exprs.iter().map(&mut rewrite).collect::<Result<Vec<_>>>()?;
        let having = having.as_ref().map(&mut rewrite).transpose()?;
        let order_by = order_by
            .iter()
            .map(|o| Ok(OrderBy { expr: rewrite(&o.expr)?, desc: o.desc }))
            .collect::<Result<Vec<_>>>()?;

        let args = aggregates.iter().filter_map(|(_, arg)| arg.as_ref());
        node = scan(table, &filter, filter::needed_columns(&table.columns, group_by.iter().chain(args)));
        node = Node::Aggregate { input: Box::new(node), group_by, aggregates };
        (Some(exprs), having, order_by)
    } else {
        let needed = match &exprs {
            None => vec![true; table.columns.len()],
            Some(exprs) => {
                let keys = order_by.iter().map(|o| &o.expr);
                filter::needed_columns(&table.columns, exprs.iter().chain(keys))
            }
        };
        node = scan(table, &filter, needed);
        (exprs, None, order_by)
    };

    if let Some(predicate) = having {
        node = Node::Filter { input: Box::new(node), predicate };
    }
    node = match (order_by.is_empty(), limit) {
        (false, _) => Node::Sort { input: Box::new(node), keys: order_by, limit },
        (true, Some(count)) => Node::Limit { input: Box::new(node), count },
        (true, None) => node,
    };
    if let Some(exprs) = exprs {
        node = Node::Project { input: Box::new(node), exprs };
    }
    Ok(Plan { root: node, columns })
}

/// Plans reading the rows of `table` that `filter` matches: a scan, and a
/// filter if there is one. `needed` marks the columns read above the filter.
pub fn scan<'a>(table: &'a Table, filter: &Option<Expr>, needed: Vec<bool>) -> Node<'a> {
    let needed_by_filter = filter::needed_columns(&table.columns, filter);
    let needed = needed.iter().zip(needed_by_filter).map(|(a, b)| *a || b).collect();
    let predicates = filter::predicates(table, filter);
    let access = access::choose(table, &predicates);
    let scan = Node::Scan { table, needed, predicates, access };
    match filter {
        Some(predicate) => Node::Filter { input: Box::new(scan), predicate: predicate.clone() },
        None => scan,
    }
}

impl Node<'_> {
    /// The columns of the rows this step produces. Expressions on them refer
    /// to them by name.
    pub fn columns(&self) -> Vec<Column> {
        let named = |name: String| Column { name, data_type: DataType::Int, primary_key: false };
        match self {
            Node::Scan { table, .. } => table.columns.clone(),
            Node::Filter { input, .. } | Node::Sort { input, .. } | Node::Limit { input, .. } => input.columns(),
            // Only names matter for evaluating expressions, so the types are left out.
            Node::Aggregate { group_by, aggregates, .. } => group_by
                .iter()
                .map(Expr::to_string)
                .chain(aggregates.iter().map(aggregate_name))
                .map(named)
                .collect(),
            Node::Project { exprs, .. } => exprs.iter().map(output_name).map(named).collect(),
        }
    }

    /// Describes the tree as EXPLAIN prints it: one step per line, each
    /// indented below the one reading from it, the scan last.
    pub fn explain(&self) -> String {
        let mut lines = Vec::new();
        self.explain_into(0, &mut lines);
        lines.join("\n")
    }

    fn explain_into(&self, depth: usize, lines: &mut Vec<String>) {
        let list = |exprs: &mut dyn Iterator<Item = String>| exprs.collect::<Vec<_>>().join(", ");
        let (line, input) = match self {
            Node::Scan { table, predicates, access, .. } => (access::describe(table, predicates, access), None),
            Node::Filter { input, predicate } => (format!("FILTER {predicate}"), Some(input)),
            Node::Aggregate { input, group_by, aggregates } => {
                let mut line = format!("AGGREGATE {}", list(&mut aggregates.iter().map(aggregate_name)));
                if !group_by.is_empty() {
                    line += &format!(" BY {}", list(&mut group_by.iter().map(Expr::to_string)));
                }
                (line, Some(input))
            }
            Node::Sort { input, keys, limit } => {
                let keys = list(&mut keys.iter().map(|k| format!("{}{}", k.expr, if k.desc { " DESC" } else { "" })));
                let top = limit.map_or(String::new(), |n| format!(" (TOP {n})"));
                (format!("SORT BY {keys}{top}"), Some(input))
            }
            Node::Limit { input, count } => (format!("LIMIT {count}"), Some(input)),
            Node::Project { input, exprs } => {
                (format!("PROJECT {}", list(&mut exprs.iter().map(Expr::to_string))), Some(input))
            }
        };
        lines.push(format!("{}{line}", "  ".repeat(depth)));
        if let Some(input) = input {
            input.explain_into(depth + 1, lines);
        }
    }
}

/// Whether a SELECT groups its rows: it has `GROUP BY` or `HAVING`, or uses an aggregate.
fn is_grouped(projection: &Projection, group_by: &[Expr], having: &Option<Expr>, order_by: &[OrderBy]) -> bool {
    let exprs = match projection {
        Projection::All => &[][..],
        Projection::Exprs(exprs) => exprs,
    };
    !group_by.is_empty()
        || having.is_some()
        || exprs.iter().chain(order_by.iter().map(|o| &o.expr)).any(contains_aggregate)
}

/// The name of a result column: a column's name, an aggregate's function, or `?column?`.
fn output_name(expr: &Expr) -> String {
    match expr {
        Expr::Column(name) => name.clone(),
        Expr::Aggregate(function, _) => function.name().into(),
        _ => "?column?".into(),
    }
}

/// The column an aggregate's result is in: its SQL text.
fn aggregate_name((function, arg): &AggregateCall) -> String {
    Expr::Aggregate(*function, arg.clone().map(Box::new)).to_string()
}

/// Rewrites an expression to read an aggregate's rows, as the module docs describe.
fn rewrite(table: &Table, expr: &Expr, group_by: &[Expr], aggregates: &mut Vec<AggregateCall>) -> Result<Expr> {
    if group_by.contains(expr) {
        return Ok(Expr::Column(expr.to_string()));
    }
    let mut rewrite = |e: &Expr| rewrite(table, e, group_by, aggregates).map(Box::new);
    Ok(match expr {
        Expr::Literal(_) => expr.clone(),
        Expr::Column(name) => {
            table.column_index(name)?;
            return err(format!("column {name} must appear in GROUP BY or be used in an aggregate function"));
        }
        Expr::Unary(op, inner) => Expr::Unary(*op, rewrite(inner)?),
        Expr::Binary(left, op, right) => Expr::Binary(rewrite(left)?, *op, rewrite(right)?),
        Expr::IsNull(inner, negated) => Expr::IsNull(rewrite(inner)?, *negated),
        Expr::Aggregate(function, arg) => {
            if arg.as_deref().is_some_and(contains_aggregate) {
                return err("aggregate functions cannot be nested");
            }
            let call = (*function, arg.as_deref().cloned());
            if !aggregates.contains(&call) {
                aggregates.push(call.clone());
            }
            Expr::Column(aggregate_name(&call))
        }
    })
}

/// Fails if one of the expressions uses an aggregate function: rows are only
/// grouped after WHERE, and never for INSERT, UPDATE or DELETE.
pub fn forbid<'a>(exprs: impl IntoIterator<Item = &'a Expr>, clause: &str) -> Result<()> {
    match exprs.into_iter().any(contains_aggregate) {
        true => err(format!("aggregate functions are not allowed in {clause}")),
        false => Ok(()),
    }
}

fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate(..) => true,
        Expr::Literal(_) | Expr::Column(_) => false,
        Expr::Unary(_, inner) | Expr::IsNull(inner, _) => contains_aggregate(inner),
        Expr::Binary(left, _, right) => contains_aggregate(left) || contains_aggregate(right),
    }
}
