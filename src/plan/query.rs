//! Query plans: a SELECT as a tree of steps, each reading rows from the one
//! below it, for the executor to run.
//!
//! ```text
//! SELECT u.city, COUNT(*) FROM users u JOIN orders o ON o.user_id = u.id
//! WHERE u.active GROUP BY u.city HAVING COUNT(*) > 1 ORDER BY u.city LIMIT 5
//!
//! PROJECT u.city, COUNT(*)
//!   SORT BY u.city (TOP 5)
//!     FILTER COUNT(*) > 1                           HAVING
//!       AGGREGATE COUNT(*) BY u.city
//!         INDEX JOIN                                for each user, its orders
//!           FILTER u.active                         WHERE, pushed down to users
//!             SCAN users u
//!           LOOKUP orders o USING INDEX o_user (o.user_id = u.id)
//! ```
//!
//! **Tables.** The tables of FROM are joined left to right, each join
//! reading the rows joined so far on its left and one table on its right.
//! A subquery in FROM (a derived table) is planned like any SELECT, and its
//! columns are named by its select list (an `AS` name, a column's name, an
//! aggregate's function or `?column?`) and qualified by its alias. It cannot
//! refer to the other tables of the FROM it is in, only, if it is itself in
//! a subquery, to the queries around that.
//! Every column reference is first bound to the table it belongs to, which
//! fails if it names no column or, unqualified, a column of several tables.
//! With a single table, references stay as written.
//!
//! **Filters.** WHERE is split at its ANDs, and so is the ON of an inner
//! join, which means the same as WHERE. A condition on one table is pushed
//! down to that table's scan (where it may also pick an index); one on
//! several tables goes to the join that brings in the last of them. The ON
//! of a LEFT JOIN decides which right rows match, not which left rows are
//! kept, so only its conditions on the right table alone are pushed down, and
//! the rest stay in the join. Nothing in WHERE is pushed down to the right
//! table of a LEFT JOIN, or into its condition: it is checked after the join,
//! on rows padded with NULLs too.
//!
//! **Joins.** Each join picks how to find the right rows that may match a
//! left row from its equalities `left expression = right column`:
//!
//! - an index join looks them up in an index of the right table on that
//!   column (or an LSM table's primary key), once per left row, if that
//!   costs less than a hash join (see below);
//! - otherwise, with any equality between the two sides, a hash join reads
//!   the right table once into a hash table keyed by the right sides;
//! - otherwise a block nested loop join reads the left rows a block at a
//!   time, as many as fit in its memory budget, and reads the right table
//!   again for each block (or keeps its rows, if they fit in the budget
//!   too), trying every right row with every row of it.
//!
//! The rest of the condition is checked on each pair. Keys are compared as
//! values, so a NULL key matches nothing, and neither do keys of different
//! types, where `=` would report an error.
//!
//! **Costs.** An index join costs a lookup per left row, a hash join a read
//! of every right row (and a cheap probe per left row), so which is cheaper
//! depends on how many rows each side has. The planner counts a table's rows
//! the first time it needs them (the writes keep the count after that) and
//! guesses, with no statistics, what share of them a condition keeps, the
//! way PostgreSQL does without them: 1 in 200 for an equality, a third for a
//! comparison. A join with equalities is guessed to return as many rows as
//! its larger side. The costs of a lookup, of a hash table row and of a
//! probe, relative to reading a row, were measured with `benches/joins.rs`.
//!
//! A merge join, which sorts both sides by the equalities and merges them,
//! puts its rows in its left side's sorted order. With no statistics to
//! weigh it against the others, the planner picks it only to save a sort: for
//! the join whose equalities ORDER BY starts with (their left sides, or the
//! right sides of an inner join), when what follows reads only its left
//! side, and only the joins after it, which keep their left side's order, are
//! hash or index joins. The left side is sorted by the rest of ORDER BY too,
//! and the ORDER BY needs no sort of its own.
//!
//! A setting can make every join use one method instead ([`ForceJoin`]),
//! to compare them on the same query, as `benches/joins.rs` does: also the
//! naive nested loop join, which the planner never picks on its own. Where
//! a join cannot use it, planning fails rather than pick another. A forced
//! merge join saves ORDER BY its sort only if it is the last join, as the
//! merge joins after another would sort its rows again.
//!
//! With more than one join worker set, a hash join is a parallel one on
//! that many threads (`PARALLEL HASH JOIN`), unless it must keep its left
//! side's order: its rows come out in no order.
//!
//! **Steps.** On top of the scans and joins, a SELECT has an aggregate (and a
//! filter for HAVING) if it groups, a sort if it has ORDER BY that no merge
//! join sorted for (which applies the LIMIT itself, as a top-N sort) or else
//! a limit, and a projection unless
//! it is `SELECT *`. `SELECT DISTINCT` adds a distinct step above the
//! projection, which drops each row equal to one it has already returned, and
//! the limit moves above it, as rows the sort keeps may still be dropped. Its
//! ORDER BY may only use expressions of the select list: rows that differ in
//! anything else become one, which would have no single place in the order.
//! The executor pulls rows through the tree one at a time;
//! sorts, aggregates, both sides of merge joins and the right sides of hash
//! joins read all of their input first, and a block nested loop join reads
//! its right side once per block of left rows. A distinct step streams, but remembers every row
//! it has returned.
//!
//! **Groups.** An aggregate's rows hold its group keys, then its aggregates.
//! Expressions above it are rewritten to read those: each `GROUP BY`
//! expression and each distinct aggregate becomes the column named by its SQL
//! text (`city`, `COUNT(*)`, `COUNT(DISTINCT age)`), and a column left over is rejected, as it is
//! neither grouped nor aggregated. The names are unique: a `GROUP BY`
//! expression has no aggregate in it, and no other expression prints the same.
//!
//! **Subqueries.** A subquery may be used as a value, with `EXISTS` or with
//! `IN`, in the select list, WHERE, HAVING and ORDER BY, and in UPDATE,
//! DELETE and INSERT; not in ON or GROUP BY, nor inside an aggregate. Binding
//! marks the subquery's columns that belong to the query around it, not to
//! its own tables (whose columns win when names clash), as outer references,
//! qualified by their table.
//! For each row, the executor replaces those by the row's values, then plans
//! and runs the subquery like any SELECT, so a correlated subquery may use an
//! index on what it compares with them. The subquery is planned once when
//! the query is, too, so that its mistakes are reported even if it never runs.
//! A condition with a subquery in it is checked after the joins that bring in
//! the tables it reads, never inside one. In a grouped query, a subquery's
//! outer references must be `GROUP BY` expressions, like any column there.

use std::collections::BTreeSet;

use crate::catalog::Catalog;
use crate::catalog::index::{Index, Key};
use crate::catalog::table::{Access, Table};
use crate::error::{Result, err};
use crate::plan::{access, filter};
use crate::predicate::{Compare, Predicate};
use crate::sql::ast::{
    Aggregate, BinaryOp, ColumnRef, Expr, From, JoinKind, OrderBy, Projection, Select, SelectItem,
    TableRef, TableSource,
};
use crate::sql::eval::{Field, resolve};
use crate::types::{StorageKind, Value};

/// A SELECT's plan: the tree, and the names of the columns it returns.
pub struct Plan<'a> {
    pub root: Node<'a>,
    pub columns: Vec<String>,
}

/// One step of a plan.
#[derive(Clone)]
pub enum Node<'a> {
    /// Reads a table's rows the way `access` says. `qualifier` is what the
    /// query calls the table: its alias, or else its name.
    Scan {
        table: &'a Table,
        qualifier: String,
        needed: Vec<bool>,
        predicates: Vec<Predicate>,
        access: Access,
    },
    /// Reads the rows of a table whose `column` equals a value an index join
    /// gives, through an index or the primary key. Only an index join's right side.
    Lookup {
        table: &'a Table,
        qualifier: String,
        needed: Vec<bool>,
        column: usize,
        key: LookupKey,
        left_key: Expr,
    },
    /// Keeps the rows `predicate` is true for.
    Filter {
        input: Box<Node<'a>>,
        predicate: Expr,
    },
    /// Pairs each left row with the right rows `condition` holds for, as `method` finds them.
    Join {
        left: Box<Node<'a>>,
        right: Box<Node<'a>>,
        kind: JoinKind,
        method: JoinMethod,
        condition: Option<Expr>,
    },
    /// Folds rows into one row per group: the keys, then the aggregates.
    Aggregate {
        input: Box<Node<'a>>,
        group_by: Vec<Expr>,
        aggregates: Vec<AggregateCall>,
    },
    /// Orders the rows, keeping the first `limit` if given.
    Sort {
        input: Box<Node<'a>>,
        keys: Vec<OrderBy>,
        limit: Option<usize>,
    },
    /// The rows of each input in turn, as `UNION ALL` returns them; with a
    /// distinct step above, as `UNION` does.
    Union { inputs: Vec<Node<'a>> },
    /// Drops each row equal to one already returned, NULLs included.
    Distinct { input: Box<Node<'a>> },
    /// Keeps the first `count` rows.
    Limit {
        input: Box<Node<'a>>,
        count: Option<usize>,
        offset: usize,
    },
    /// Evaluates the select list on each row.
    Project {
        input: Box<Node<'a>>,
        exprs: Vec<Expr>,
    },
    /// A derived table: its query's rows, with columns `names` qualified by `qualifier`.
    Derived {
        input: Box<Node<'a>>,
        qualifier: String,
        names: Vec<String>,
    },
}

/// How a join finds the right rows that may match a left row.
#[derive(Clone)]
pub enum JoinMethod {
    /// Tries every right row, reading them again for each block of left rows.
    Block,
    /// Tries every right row, reading them again for each left row. Only
    /// when forced: see [`ForceJoin`].
    Naive,
    /// Those whose `right_keys` equal the left row's `left_keys`, from a hash
    /// table. `keep_order` if the rows must come out in the left side's
    /// order (after a merge join), even if the table spills.
    Hash {
        left_keys: Vec<Expr>,
        right_keys: Vec<Expr>,
        keep_order: bool,
        /// Threads that join its partitions side by side, if more than one
        /// (see `executor/joins/parallel.rs`).
        workers: usize,
    },
    /// Those the right side, a `Lookup` (maybe under filters), finds for the left row.
    Index,
    /// Those whose `right_keys` equal the left row's `left_keys`, found by
    /// sorting both sides by their keys, each in its direction in `desc`, the
    /// left side then by `then`, and merging them. Its rows come out in the
    /// left side's order.
    Merge {
        left_keys: Vec<Expr>,
        right_keys: Vec<Expr>,
        desc: Vec<bool>,
        then: Vec<OrderBy>,
    },
}

/// What a session asks of the planner, besides the query: a join method to
/// force, and how many threads a hash join may run on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Choices {
    pub force_join: Option<ForceJoin>,
    /// More than one makes every hash join that need not keep its left
    /// side's order a parallel one, on that many threads.
    pub join_workers: usize,
}

/// A join method every join of a query is made to use, rather than the one
/// the planner would pick (see the module docs). A hash or merge join needs
/// an equality between the two sides, an index join one with a column of
/// the right table that an index or its primary key looks up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceJoin {
    Hash,
    Index,
    Merge,
    Block,
    Naive,
}

impl ForceJoin {
    fn name(self) -> &'static str {
        match self {
            ForceJoin::Hash => "a hash join",
            ForceJoin::Index => "an index join",
            ForceJoin::Merge => "a merge join",
            ForceJoin::Block => "a block nested loop join",
            ForceJoin::Naive => "a naive nested loop join",
        }
    }
}

/// What a `Lookup` reads the table through.
#[derive(Debug, Clone, Copy)]
pub enum LookupKey {
    /// The table's index with this position in [`Table::indexes`].
    Index(usize),
    /// An LSM table's primary key.
    PrimaryKey,
}

/// One aggregate an `Aggregate` step computes per group: the function, its
/// argument (none for `COUNT(*)`), and whether it is over distinct values.
pub type AggregateCall = (Aggregate, Option<Expr>, bool);

/// A table in FROM, what the query calls it, and its columns' names.
struct Source<'a> {
    kind: SourceKind<'a>,
    qualifier: String,
    names: Vec<String>,
}

enum SourceKind<'a> {
    Table(&'a Table),
    /// A derived table's plan, until `joins` takes it.
    Derived(Option<Node<'a>>),
}

impl<'a> Source<'a> {
    fn table(table: &'a Table, qualifier: &str) -> Self {
        let names = table.columns.iter().map(|c| c.name.clone()).collect();
        Source {
            kind: SourceKind::Table(table),
            qualifier: qualifier.into(),
            names,
        }
    }

    fn fields(&self) -> Vec<Field> {
        self.names
            .iter()
            .map(|name| Field {
                table: Some(self.qualifier.clone()),
                name: name.clone(),
            })
            .collect()
    }

    /// Plans reading the rows `filter` matches; `needed` marks the columns read above.
    fn read(&mut self, filter: Option<Expr>, needed: Vec<bool>) -> Node<'a> {
        match &mut self.kind {
            SourceKind::Table(table) => scan(table, &self.qualifier, &filter, needed),
            SourceKind::Derived(plan) => {
                let input = Box::new(plan.take().expect("read once"));
                let node = Node::Derived {
                    input,
                    qualifier: self.qualifier.clone(),
                    names: self.names.clone(),
                };
                match filter {
                    Some(predicate) => Node::Filter {
                        input: Box::new(node),
                        predicate,
                    },
                    None => node,
                }
            }
        }
    }
}

/// The names of the columns a query returns, as its plan names them.
pub fn output_names(catalog: &Catalog, query: &Select) -> Result<Vec<String>> {
    match &query.projection {
        Projection::Exprs(items) => Ok(items.iter().map(item_name).collect()),
        Projection::All => {
            let mut names = Vec::new();
            for table in query.from.tables() {
                match &table.source {
                    TableSource::Table(name) => {
                        names.extend(catalog.get(name)?.columns.iter().map(|c| c.name.clone()))
                    }
                    TableSource::Subquery(query) => names.extend(output_names(catalog, query)?),
                }
            }
            Ok(names)
        }
    }
}

/// Plans a SELECT on the tables of the catalog, each of its joins (and
/// those of its subqueries in FROM) as `choices` says.
pub fn select<'a>(catalog: &'a Catalog, query: Select, choices: Choices) -> Result<Plan<'a>> {
    if !query.compound.is_empty() {
        return union(catalog, query, choices);
    }
    let Select {
        distinct,
        from,
        projection,
        filter,
        group_by,
        having,
        mut order_by,
        limit,
        offset,
        ..
    } = query;
    let offset = offset.unwrap_or(0);
    let mut sources = from
        .tables()
        .map(|r| match &r.source {
            TableSource::Table(name) => Ok(Source::table(catalog.get(name)?, r.qualifier())),
            TableSource::Subquery(query) => {
                let plan = select(catalog, (**query).clone(), choices)?;
                let kind = SourceKind::Derived(Some(plan.root));
                Ok(Source {
                    kind,
                    qualifier: r.qualifier().into(),
                    names: plan.columns,
                })
            }
        })
        .collect::<Result<Vec<_>>>()?;
    for (i, source) in sources.iter().enumerate() {
        if sources[..i].iter().any(|s| s.qualifier == source.qualifier) {
            return err(format!(
                "table name {} appears more than once in FROM",
                source.qualifier
            ));
        }
    }
    forbid(&filter, "WHERE")?;
    forbid(
        from.joins.iter().filter_map(|j| j.on.as_ref()),
        "JOIN ... ON",
    )?;
    forbid_subqueries(
        from.joins.iter().filter_map(|j| j.on.as_ref()),
        "JOIN ... ON",
    )?;
    forbid_subqueries(&group_by, "GROUP BY")?;

    // ORDER BY may name a column of the select list by its AS name.
    if let Projection::Exprs(items) = &projection {
        for key in &mut order_by {
            if let Expr::Column(ColumnRef { table: None, name }) = &key.expr
                && let Some(item) = items.iter().find(|item| item.alias.as_ref() == Some(name))
            {
                key.expr = item.expr.clone();
            }
        }
    }

    // Bind every column reference to its table.
    let binder = Binder::new(catalog, &sources);
    let bind = |e: &Expr| binder.bind(e);
    let bind_all = |exprs: &[Expr]| exprs.iter().map(bind).collect::<Result<Vec<_>>>();
    let projection = match projection {
        Projection::All => Projection::All,
        Projection::Exprs(items) => Projection::Exprs(
            items
                .into_iter()
                .map(|item| {
                    Ok(SelectItem {
                        expr: bind(&item.expr)?,
                        alias: item.alias,
                    })
                })
                .collect::<Result<_>>()?,
        ),
    };
    let filter = filter.as_ref().map(bind).transpose()?;
    let ons = from
        .joins
        .iter()
        .map(|j| j.on.as_ref().map(bind).transpose())
        .collect::<Result<Vec<_>>>()?;
    // A repeated GROUP BY expression groups by nothing more, and would make its column ambiguous.
    let mut distinct_group_by: Vec<Expr> = Vec::new();
    for expr in bind_all(&group_by)? {
        if !distinct_group_by.contains(&expr) {
            distinct_group_by.push(expr);
        }
    }
    let group_by = distinct_group_by;
    let having = having.as_ref().map(bind).transpose()?;
    let order_by = order_by
        .iter()
        .map(|o| {
            Ok(OrderBy {
                expr: bind(&o.expr)?,
                desc: o.desc,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    for (i, on) in ons.iter().enumerate() {
        if let Some(later) = on
            .iter()
            .flat_map(|on| binder.tables(on))
            .find(|&t| t > i + 1)
        {
            let (this, later) = (&sources[i + 1].qualifier, &sources[later].qualifier);
            return err(format!(
                "the ON of {this} refers to {later}, which is joined after it"
            ));
        }
    }

    let grouped = is_grouped(&projection, &group_by, &having, &order_by);
    let (exprs, columns) = match projection {
        Projection::All if grouped => {
            return err("SELECT * cannot be used with GROUP BY or aggregate functions");
        }
        Projection::All => (None, sources.iter().flat_map(|s| s.names.clone()).collect()),
        Projection::Exprs(items) => {
            let names = items.iter().map(item_name).collect();
            (
                Some(items.into_iter().map(|item| item.expr).collect::<Vec<_>>()),
                names,
            )
        }
    };
    if let (true, Some(exprs)) = (distinct, &exprs)
        && let Some(key) = order_by.iter().find(|o| !exprs.contains(&o.expr))
    {
        return err(format!(
            "for SELECT DISTINCT, ORDER BY {} must appear in the select list",
            key.expr
        ));
    }

    // The columns each table must read: those any expression uses.
    let used: Vec<&Expr> = exprs
        .iter()
        .flatten()
        .chain(&filter)
        .chain(ons.iter().flatten())
        .chain(&group_by)
        .chain(&having)
        .chain(order_by.iter().map(|o| &o.expr))
        .collect();
    let needed: Vec<Vec<bool>> = sources
        .iter()
        .map(|s| match (&s.kind, &exprs) {
            (SourceKind::Table(table), Some(_)) => {
                filter::needed_columns(table, &s.qualifier, used.iter().copied())
            }
            _ => vec![true; s.names.len()],
        })
        .collect();

    let order = if grouped { &[][..] } else { &order_by };
    let (mut node, ordered) = joins(
        &mut sources,
        &from,
        filter,
        ons,
        needed,
        &binder,
        order,
        choices,
    )?;

    let (exprs, having, order_by) = if grouped {
        forbid(&group_by, "GROUP BY")?;
        let exprs = exprs.expect("checked above");
        // Rewrite everything evaluated on groups; this also collects the aggregates.
        let mut aggregates = Vec::new();
        let mut rewrite = |expr: &Expr| rewrite(expr, &group_by, &mut aggregates);
        let exprs = exprs.iter().map(&mut rewrite).collect::<Result<Vec<_>>>()?;
        let having = having.as_ref().map(&mut rewrite).transpose()?;
        let order_by = order_by
            .iter()
            .map(|o| {
                Ok(OrderBy {
                    expr: rewrite(&o.expr)?,
                    desc: o.desc,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        node = Node::Aggregate {
            input: Box::new(node),
            group_by,
            aggregates,
        };
        (Some(exprs), having, order_by)
    } else {
        (exprs, None, order_by)
    };

    if let Some(predicate) = having {
        node = Node::Filter {
            input: Box::new(node),
            predicate,
        };
    }
    // A sort keeps the rows the limit and offset take, and is all the limit
    // there is without an offset. Under DISTINCT, they wait until rows equal
    // to earlier ones are dropped.
    let sorted = !order_by.is_empty() && !ordered;
    let limited = limit.is_some() || offset > 0;
    if sorted {
        let top = limit
            .map(|n| n.saturating_add(offset))
            .filter(|_| !distinct);
        node = Node::Sort {
            input: Box::new(node),
            keys: order_by,
            limit: top,
        };
    }
    if limited && !distinct && !(sorted && offset == 0) {
        node = Node::Limit {
            input: Box::new(node),
            count: limit,
            offset,
        };
    }
    if let Some(exprs) = exprs {
        node = Node::Project {
            input: Box::new(node),
            exprs,
        };
    }
    if distinct {
        node = Node::Distinct {
            input: Box::new(node),
        };
        if limited {
            node = Node::Limit {
                input: Box::new(node),
                count: limit,
                offset,
            };
        }
    }
    Ok(Plan {
        root: node,
        columns,
    })
}

/// Plans queries joined by `UNION [ALL]`: the rows of each in turn, left to
/// right, a distinct step dropping repeats wherever `UNION` without ALL
/// stands. With an ORDER BY, LIMIT or OFFSET, the union is planned as a
/// subquery in FROM of a query that has them, so that ORDER BY names its
/// columns as it would a table's.
fn union<'a>(catalog: &'a Catalog, mut query: Select, choices: Choices) -> Result<Plan<'a>> {
    if !query.order_by.is_empty() || query.limit.is_some() || query.offset.is_some() {
        let (order_by, limit, offset) = (
            std::mem::take(&mut query.order_by),
            query.limit.take(),
            query.offset.take(),
        );
        let table = TableRef {
            source: TableSource::Subquery(Box::new(query)),
            alias: Some("union".into()),
        };
        let projection = Projection::All;
        let (filter, group_by, having, compound) = (None, Vec::new(), None, Vec::new());
        let from = From {
            table,
            joins: Vec::new(),
        };
        let outer = Select {
            distinct: false,
            from,
            projection,
            filter,
            group_by,
            having,
            order_by,
            limit,
            offset,
            compound,
        };
        return select(catalog, outer, choices);
    }
    let parts = std::mem::take(&mut query.compound);
    let first = select(catalog, query, choices)?;
    let columns = first.columns;
    let mut node = first.root;
    for (all, part) in parts {
        let part = select(catalog, part, choices)?;
        if part.columns.len() != columns.len() {
            return err(format!(
                "each query of a UNION must return as many columns: {} and {}",
                columns.len(),
                part.columns.len()
            ));
        }
        node = match node {
            Node::Union { mut inputs } => {
                inputs.push(part.root);
                Node::Union { inputs }
            }
            node => Node::Union {
                inputs: vec![node, part.root],
            },
        };
        if !all {
            node = Node::Distinct {
                input: Box::new(node),
            };
        }
    }
    Ok(Plan {
        root: node,
        columns,
    })
}

/// Plans the scans and joins of FROM, with WHERE and the ONs placed as the
/// module docs describe, and whether a merge join puts its rows in the order
/// `order_by` asks for. Every join uses the method `choices` forces, if any.
#[allow(clippy::too_many_arguments)]
fn joins<'a>(
    sources: &mut [Source<'a>],
    from: &From,
    filter: Option<Expr>,
    ons: Vec<Option<Expr>>,
    mut needed: Vec<Vec<bool>>,
    binder: &Binder,
    order_by: &[OrderBy],
    choices: Choices,
) -> Result<(Node<'a>, bool)> {
    let force = choices.force_join;
    let n = sources.len();
    let kinds: Vec<JoinKind> = std::iter::once(JoinKind::Inner)
        .chain(from.joins.iter().map(|j| j.kind))
        .collect();
    // For each table: conditions pushed down to its scan, conditions of the
    // join that brings it in, and conditions checked just after that join.
    let mut pushed: Vec<Vec<Expr>> = vec![Vec::new(); n];
    let mut conditions: Vec<Vec<Expr>> = vec![Vec::new(); n];
    let mut after: Vec<Vec<Expr>> = vec![Vec::new(); n];

    if n == 1 {
        pushed[0].extend(filter); // kept whole, as written
    } else {
        let mut pool = Vec::new();
        filter.iter().for_each(|f| conjuncts(f, &mut pool));
        for (i, on) in ons.iter().enumerate() {
            match (kinds[i + 1], on) {
                (JoinKind::Inner, Some(on)) => conjuncts(on, &mut pool),
                (JoinKind::Left, Some(on)) => {
                    let mut parts = Vec::new();
                    conjuncts(on, &mut parts);
                    for part in parts {
                        let tables = binder.tables(&part);
                        let right_only = tables.len() == 1 && tables.contains(&(i + 1));
                        if right_only {
                            &mut pushed[i + 1]
                        } else {
                            &mut conditions[i + 1]
                        }
                        .push(part);
                    }
                }
                (_, None) => {}
            }
        }
        for part in pool {
            let tables = binder.tables(&part);
            if part.has_subquery() {
                match tables.last() {
                    Some(&last) if last > 0 => after[last].push(part),
                    _ => pushed[0].push(part), // a filter step above the first table's scan
                }
                continue;
            }
            match tables.last() {
                None => pushed[0].push(part), // constant: filters all rows or none
                Some(&last) if tables.len() == 1 && kinds[last] == JoinKind::Inner => {
                    pushed[last].push(part)
                }
                Some(&last) if kinds[last] == JoinKind::Inner => conditions[last].push(part),
                Some(&last) => after[last].push(part),
            }
        }
    }

    // The join that can sort its sides into ORDER BY's order for a merge join.
    // The joins after it must keep their left side's order, as hash and index
    // joins do and a block nested loop join does not.
    // Forced, only a merge join may, and only the last.
    let mut merge = None;
    let tried = match force {
        None => n,
        Some(ForceJoin::Merge) => 1,
        Some(_) => 0,
    };
    if !order_by.is_empty() {
        for i in (1..n).rev().take(tried) {
            let keys = equalities(&conditions[i], i, binder);
            if keys.is_empty() {
                break;
            }
            if let Some(order) = merge_order(order_by, &keys, kinds[i], i, binder) {
                merge = Some((i, order));
                break;
            }
        }
    }
    let ordered = merge.is_some();
    // The joins after the merge join keep its order.
    let merge_at = merge.as_ref().map(|(at, _)| *at);

    let mut node = sources[0].read(and(std::mem::take(&mut pushed[0])), needed[0].clone());
    for i in 1..n {
        let source = &mut sources[i];
        let right_filter = and(std::mem::take(&mut pushed[i]));
        let mut residual = std::mem::take(&mut conditions[i]);
        let keys = equalities(&residual, i, binder);

        let index = keys.iter().find_map(|(at, left, right)| {
            let (Expr::Column(column), SourceKind::Table(table)) = (right, &source.kind) else {
                return None;
            };
            let column = table.column_index(&column.name).ok()?;
            Some((
                *at,
                left.clone(),
                column,
                *table,
                lookup_key(table, column)?,
            ))
        });
        let index = match force {
            None => index.filter(|(.., table, key)| lookups_win(estimate(&node), table, *key)),
            Some(ForceJoin::Index) if index.is_none() => {
                return cannot(
                    ForceJoin::Index,
                    &source.qualifier,
                    "no equality with a column of it that an index or its primary key looks up",
                );
            }
            Some(ForceJoin::Index) => index,
            Some(_) => None,
        };
        let (method, right) =
            if let Some((_, (order, desc, then))) = merge.take_if(|(at, _)| *at == i) {
                for (at, ..) in keys.iter().rev() {
                    residual.remove(*at);
                }
                let (left_keys, right_keys) = order
                    .iter()
                    .map(|&k| (keys[k].1.clone(), keys[k].2.clone()))
                    .unzip();
                (
                    JoinMethod::Merge {
                        left_keys,
                        right_keys,
                        desc,
                        then,
                    },
                    source.read(right_filter, needed[i].clone()),
                )
            } else if let Some((at, left_key, column, table, key)) = index {
                residual.remove(at);
                needed[i][column] = true;
                let lookup = Node::Lookup {
                    table,
                    qualifier: source.qualifier.clone(),
                    needed: needed[i].clone(), // includes the filter's columns: it is part of the query
                    column,
                    key,
                    left_key,
                };
                let right = match right_filter {
                    Some(predicate) => Node::Filter {
                        input: Box::new(lookup),
                        predicate,
                    },
                    None => lookup,
                };
                (JoinMethod::Index, right)
            } else {
                // A nested loop join checks the equalities with the rest.
                match force {
                    Some(ForceJoin::Block) => (
                        JoinMethod::Block,
                        source.read(right_filter, needed[i].clone()),
                    ),
                    Some(ForceJoin::Naive) => (
                        JoinMethod::Naive,
                        source.read(right_filter, needed[i].clone()),
                    ),
                    Some(method) if keys.is_empty() => {
                        return cannot(
                            method,
                            &source.qualifier,
                            "no equality between its two sides",
                        );
                    }
                    None if keys.is_empty() => (
                        JoinMethod::Block,
                        source.read(right_filter, needed[i].clone()),
                    ),
                    _ => {
                        let right = source.read(right_filter, needed[i].clone());
                        for (at, ..) in keys.iter().rev() {
                            residual.remove(*at);
                        }
                        let (left_keys, right_keys): (Vec<_>, Vec<_>) =
                            keys.into_iter().map(|(_, l, r)| (l, r)).unzip();
                        let method = if force == Some(ForceJoin::Merge) {
                            // In no order anyone asked for: ascending, by each key in turn.
                            JoinMethod::Merge {
                                desc: vec![false; left_keys.len()],
                                left_keys,
                                right_keys,
                                then: Vec::new(),
                            }
                        } else {
                            // Only a hash join that need not keep its order runs in parallel.
                            let keep_order = merge_at.is_some_and(|at| i > at);
                            JoinMethod::Hash {
                                left_keys,
                                right_keys,
                                keep_order,
                                workers: match keep_order {
                                    true => 1,
                                    false => choices.join_workers.max(1),
                                },
                            }
                        };
                        (method, right)
                    }
                }
            };
        node = Node::Join {
            left: Box::new(node),
            right: Box::new(right),
            kind: kinds[i],
            method,
            condition: and(residual),
        };
        if let Some(predicate) = and(std::mem::take(&mut after[i])) {
            node = Node::Filter {
                input: Box::new(node),
                predicate,
            };
        }
    }
    Ok((node, ordered))
}

/// The error of a join of `table` that cannot use the method forced on it.
fn cannot<T>(force: ForceJoin, table: &str, why: &str) -> Result<T> {
    err(format!(
        "the join of {table} cannot be {}: {why}",
        force.name()
    ))
}

/// The equalities among `parts` between the tables before table `i` and
/// table `i` alone: (position in `parts`, left side, right side).
fn equalities(parts: &[Expr], i: usize, binder: &Binder) -> Vec<(usize, Expr, Expr)> {
    parts
        .iter()
        .enumerate()
        .filter_map(|(at, part)| {
            let Expr::Binary(a, BinaryOp::Eq, b) = part else {
                return None;
            };
            let side = |e: &Expr| {
                let tables = binder.tables(e);
                match tables.last() {
                    Some(&last) if last < i => Some(false),
                    Some(&last) if last == i && tables.len() == 1 => Some(true),
                    _ => None,
                }
            };
            match (side(a)?, side(b)?) {
                (false, true) => Some((at, (**a).clone(), (**b).clone())),
                (true, false) => Some((at, (**b).clone(), (**a).clone())),
                _ => None,
            }
        })
        .collect()
}

/// How a merge join on `keys`, the equalities of join `i`, can sort its sides
/// so that its rows come out in the order `order_by` asks for, if it can: the
/// keys to sort by (positions in `keys`), their directions, and what the left
/// side is sorted by after them.
///
/// `order_by` must start with keys: their left sides, or for an inner join
/// their right sides, which equal them on every row. Whatever follows may only
/// read the left side, and only if every equality is among those keys, as any
/// other would sort the left rows before it does.
fn merge_order(
    order_by: &[OrderBy],
    keys: &[(usize, Expr, Expr)],
    kind: JoinKind,
    i: usize,
    binder: &Binder,
) -> Option<(Vec<usize>, Vec<bool>, Vec<OrderBy>)> {
    let (mut order, mut desc) = (Vec::new(), Vec::new());
    let mut rest: &[OrderBy] = &[];
    for (at, o) in order_by.iter().enumerate() {
        let Some(k) = keys
            .iter()
            .position(|(_, l, r)| *l == o.expr || (kind == JoinKind::Inner && *r == o.expr))
        else {
            rest = &order_by[at..];
            break;
        };
        if !order.contains(&k) {
            order.push(k);
            desc.push(o.desc);
        }
    }
    let left_only =
        |o: &OrderBy| !o.expr.has_subquery() && binder.tables(&o.expr).iter().all(|&t| t < i);
    if order.is_empty()
        || (!rest.is_empty() && (order.len() < keys.len() || !rest.iter().all(left_only)))
    {
        return None;
    }
    for k in 0..keys.len() {
        if !order.contains(&k) {
            order.push(k);
            desc.push(false);
        }
    }
    Some((order, desc, rest.to_vec()))
}

/// What the planner's choices cost, in rows read by a scan, as measured with
/// `benches/joins.rs`: a lookup through a B+ tree index or a hash index,
/// then of the row it finds;
/// a lookup of an LSM table's primary key; reading a right row into a hash
/// join's hash table; and looking a left row up in it. Worth measuring again
/// when one of them gets faster.
const INDEX_LOOKUP_COST: f64 = 6.0;
const HASH_INDEX_LOOKUP_COST: f64 = 4.0;
const KEY_LOOKUP_COST: f64 = 2.0;
const HASH_BUILD_COST: f64 = 2.0;
const HASH_PROBE_COST: f64 = 0.4;

/// How many of its rows a condition keeps, guessed with no statistics as
/// PostgreSQL guesses: an equality 1 in 200 (but at most one row on a
/// primary key or unique index), `<>` all but that, and a comparison or
/// anything else 1 in 3.
const EQUAL_KEEPS: f64 = 0.005;
const OTHER_KEEPS: f64 = 1.0 / 3.0;

/// Whether an index join, looking each of `left` rows up in `table` by
/// `key`, costs less than a hash join reading all of `table` once.
fn lookups_win(left: f64, table: &Table, key: LookupKey) -> bool {
    let right = table.row_count().map_or(f64::INFINITY, |rows| rows as f64);
    let lookup = match key {
        LookupKey::Index(i) if table.indexes()[i].is_hash() => HASH_INDEX_LOOKUP_COST,
        LookupKey::Index(_) => INDEX_LOOKUP_COST,
        LookupKey::PrimaryKey => KEY_LOOKUP_COST,
    };
    left * lookup <= right * HASH_BUILD_COST + left * HASH_PROBE_COST
}

/// Roughly how many rows a plan step returns: tables' row counts, cut down
/// by the guesses above for what conditions keep. A join with equalities
/// returns about as many rows as its larger side (as if each row of that
/// side matched one of the other), and one without, a third of every pair.
pub fn estimate(node: &Node) -> f64 {
    match node {
        Node::Scan {
            table, predicates, ..
        } => {
            let rows = table.row_count().map_or(1000.0, |rows| rows as f64);
            let unique = |column| {
                table.primary_key() == Some(column)
                    || table
                        .indexes()
                        .iter()
                        .any(|i| i.unique && i.is_on_column(column))
            };
            predicates.iter().fold(rows, |rows, p| match p.op {
                Compare::Eq if unique(p.column) => rows.min(1.0),
                Compare::Eq => rows * EQUAL_KEEPS,
                Compare::NotEq => rows * (1.0 - EQUAL_KEEPS),
                _ => rows * OTHER_KEEPS,
            })
        }
        Node::Lookup { .. } => 1.0,
        // Above a scan, a filter checks again what the scan's predicates did.
        Node::Filter { input, .. } => match &**input {
            Node::Scan { predicates, .. } if !predicates.is_empty() => estimate(input),
            _ => estimate(input) * OTHER_KEEPS,
        },
        Node::Join {
            left,
            right,
            method,
            kind,
            ..
        } => {
            let (left, right) = (estimate(left), estimate(right));
            let rows = match method {
                JoinMethod::Block | JoinMethod::Naive => left * right * OTHER_KEEPS,
                _ => left.max(right),
            };
            if *kind == JoinKind::Left {
                rows.max(left)
            } else {
                rows
            }
        }
        Node::Aggregate {
            input, group_by, ..
        } => match group_by.is_empty() {
            true => 1.0,
            false => (estimate(input) * OTHER_KEEPS).max(1.0),
        },
        Node::Sort { input, limit, .. } => {
            limit.map_or(estimate(input), |n| estimate(input).min(n as f64))
        }
        Node::Limit {
            input,
            count,
            offset,
        } => {
            let rows = (estimate(input) - *offset as f64).max(0.0);
            count.map_or(rows, |n| rows.min(n as f64))
        }
        Node::Union { inputs } => inputs.iter().map(estimate).sum(),
        Node::Distinct { input } | Node::Project { input, .. } | Node::Derived { input, .. } => {
            estimate(input)
        }
    }
}

/// How a lookup on this column of the table can find rows, if it can: through
/// an index whose first key it is and that holds every row, a hash index
/// only if it is the only key (and before a B+ tree), or the primary key of
/// an LSM table.
fn lookup_key(table: &Table, column: usize) -> Option<LookupKey> {
    let first = |index: &Index| {
        index.keys.first() == Some(&Key::Column(column)) && index.predicate.is_none()
    };
    let hash = |index: &Index| index.is_hash() && first(index) && index.keys.len() == 1;
    let tree = |index: &Index| !index.is_hash() && first(index);
    if let Some(i) = table
        .indexes()
        .iter()
        .position(hash)
        .or_else(|| table.indexes().iter().position(tree))
    {
        return Some(LookupKey::Index(i));
    }
    (table.storage_kind() == StorageKind::Lsm && table.primary_key() == Some(column))
        .then_some(LookupKey::PrimaryKey)
}

/// Plans reading the rows of `table`, which the query calls `qualifier`, that
/// `filter` matches: a scan, and a filter if there is one. `needed` marks the
/// columns read above the filter.
pub fn scan<'a>(
    table: &'a Table,
    qualifier: &str,
    filter: &Option<Expr>,
    needed: Vec<bool>,
) -> Node<'a> {
    let needed_by_filter = filter::needed_columns(table, qualifier, filter);
    let needed: Vec<bool> = needed
        .iter()
        .zip(needed_by_filter)
        .map(|(a, b)| *a || b)
        .collect();
    let conditions = filter::conditions(table, qualifier, filter);
    let predicates = filter::predicates(table, &conditions);
    let access = access::choose(table, qualifier, filter, &conditions, &needed);
    let scan = Node::Scan {
        table,
        qualifier: qualifier.into(),
        needed,
        predicates,
        access,
    };
    match filter {
        Some(predicate) => Node::Filter {
            input: Box::new(scan),
            predicate: predicate.clone(),
        },
        None => scan,
    }
}

/// Binds an expression of a statement on one table (or on none, for VALUES)
/// as a SELECT's are bound: its columns are checked and its subqueries'
/// outer references marked.
pub fn bind(catalog: &Catalog, table: Option<&Table>, expr: &Expr) -> Result<Expr> {
    let sources: Vec<Source> = table
        .into_iter()
        .map(|table| Source::table(table, &table.name))
        .collect();
    Binder::new(catalog, &sources).bind(expr)
}

/// Resolves column references against the tables of FROM.
struct Binder<'c> {
    catalog: &'c Catalog,
    fields: Vec<Field>,
    /// The position in FROM of the table of each field.
    owners: Vec<usize>,
    single: bool,
}

impl<'c> Binder<'c> {
    fn new(catalog: &'c Catalog, sources: &[Source]) -> Self {
        let mut fields = Vec::new();
        let mut owners = Vec::new();
        for (i, source) in sources.iter().enumerate() {
            let own = source.fields();
            owners.extend(std::iter::repeat_n(i, own.len()));
            fields.extend(own);
        }
        Binder {
            catalog,
            fields,
            owners,
            single: sources.len() == 1,
        }
    }

    /// The expression with each column qualified by its table (left as it is
    /// with a single table). Fails on an unknown or ambiguous column.
    fn bind(&self, expr: &Expr) -> Result<Expr> {
        let bind = |e: &Expr| self.bind(e).map(Box::new);
        Ok(match expr {
            Expr::Literal(_) | Expr::Slot(..) => expr.clone(),
            Expr::Column(column) => {
                let field = &self.fields[resolve(&self.fields, column)?];
                match self.single {
                    true => Expr::Column(ColumnRef::bare(&field.name)),
                    false => Expr::Column(ColumnRef {
                        table: field.table.clone(),
                        name: field.name.clone(),
                    }),
                }
            }
            Expr::Unary(op, inner) => Expr::Unary(*op, bind(inner)?),
            Expr::Binary(left, op, right) => Expr::Binary(bind(left)?, *op, bind(right)?),
            Expr::IsNull(inner, negated) => Expr::IsNull(bind(inner)?, *negated),
            Expr::Aggregate(function, arg, distinct) => {
                if arg.as_deref().is_some_and(Expr::has_subquery) {
                    return err("subqueries are not allowed in aggregate functions");
                }
                Expr::Aggregate(*function, arg.as_deref().map(bind).transpose()?, *distinct)
            }
            Expr::InList(left, items, negated) => Expr::InList(
                bind(left)?,
                items.iter().map(|e| self.bind(e)).collect::<Result<_>>()?,
                *negated,
            ),
            Expr::Subquery(query) => Expr::Subquery(Box::new(self.subquery(query, true)?)),
            Expr::Exists(query) => Expr::Exists(Box::new(self.subquery(query, false)?)),
            Expr::InSubquery(left, query, negated) => {
                Expr::InSubquery(bind(left)?, Box::new(self.subquery(query, true)?), *negated)
            }
            Expr::Outer(_) => expr.clone(),
            Expr::Case { .. } | Expr::Function(..) => expr.try_map_parts(&mut |e| self.bind(e))?,
        })
    }

    /// A subquery with its outer references marked, after checking that it
    /// plans (returning one column, if `one_column`).
    fn subquery(&self, query: &Select, one_column: bool) -> Result<Select> {
        let query = self.correlate(query, &mut Vec::new())?;
        let check = substitute(&query, &mut |_| Ok(Value::Null))?;
        if select(self.catalog, check, Choices::default())?
            .columns
            .len()
            != 1
            && one_column
        {
            return err("subquery must return only one column");
        }
        Ok(query)
    }

    /// Marks the outer references of `query`, a subquery of the query this
    /// binds, and of the subqueries inside it: the columns that none of their
    /// own tables, nor those of the queries between (`scopes`), have, but
    /// this query's do.
    /// A subquery in its FROM sees only the queries around this one, not the
    /// other tables of that FROM.
    fn correlate(&self, query: &Select, scopes: &mut Vec<Vec<Field>>) -> Result<Select> {
        let mut query = query.clone();
        let mut own = Vec::new();
        for table in query.from.tables_mut() {
            let names = match &mut table.source {
                TableSource::Table(name) => self
                    .catalog
                    .get(name)?
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect(),
                TableSource::Subquery(sub) => {
                    **sub = self.correlate(sub, scopes)?;
                    output_names(self.catalog, sub)?
                }
            };
            own.extend(names.into_iter().map(|name| Field {
                table: Some(table.qualifier().into()),
                name,
            }));
        }
        scopes.push(own);
        let query = query.try_map_own(&mut |e| self.correlate_expr(e, scopes));
        scopes.pop();
        query
    }

    fn correlate_expr(&self, expr: &Expr, scopes: &mut Vec<Vec<Field>>) -> Result<Expr> {
        expr.replace(&mut |e| match e {
            Expr::Column(column)
                if !scopes.iter().flatten().any(|f| f.is_named_by(column))
                    && self.fields.iter().any(|f| f.is_named_by(column)) =>
            {
                // Qualified even with a single table, so that it never reads as the subquery's own.
                Some(resolve(&self.fields, column).map(|i| {
                    let field = &self.fields[i];
                    Expr::Outer(ColumnRef {
                        table: field.table.clone(),
                        name: field.name.clone(),
                    })
                }))
            }
            Expr::Subquery(query) => Some(
                self.correlate(query, scopes)
                    .map(|q| Expr::Subquery(Box::new(q))),
            ),
            Expr::Exists(query) => Some(
                self.correlate(query, scopes)
                    .map(|q| Expr::Exists(Box::new(q))),
            ),
            Expr::InSubquery(left, query, negated) => Some((|| {
                let left = self.correlate_expr(left, scopes)?;
                Ok(Expr::InSubquery(
                    Box::new(left),
                    Box::new(self.correlate(query, scopes)?),
                    *negated,
                ))
            })()),
            _ => None,
        })
    }

    /// The positions in FROM of the tables a bound expression reads, in order.
    fn tables(&self, expr: &Expr) -> Vec<usize> {
        let mut tables = BTreeSet::new();
        filter::visit_columns(expr, &mut |column| {
            if let Ok(i) = resolve(&self.fields, column) {
                tables.insert(self.owners[i]);
            }
        });
        tables.into_iter().collect()
    }
}

/// The subquery with each outer reference replaced by the value `value` gives it.
pub fn substitute(
    query: &Select,
    value: &mut dyn FnMut(&ColumnRef) -> Result<Value>,
) -> Result<Select> {
    query.try_map(&mut |e| {
        e.replace(&mut |e| match e {
            Expr::Outer(column) => Some(value(column).map(Expr::Literal)),
            _ => None,
        })
    })
}

/// Splits an expression at its ANDs.
fn conjuncts(expr: &Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::Binary(left, BinaryOp::And, right) => {
            conjuncts(left, out);
            conjuncts(right, out);
        }
        expr => out.push(expr.clone()),
    }
}

/// The conditions joined by AND, if there are any.
fn and(parts: Vec<Expr>) -> Option<Expr> {
    parts
        .into_iter()
        .reduce(|a, b| Expr::Binary(Box::new(a), BinaryOp::And, Box::new(b)))
}

impl Node<'_> {
    /// The columns of the rows this step produces. Expressions on them refer
    /// to them by name, and by table if they come from one.
    pub fn columns(&self) -> Vec<Field> {
        match self {
            Node::Scan {
                table, qualifier, ..
            }
            | Node::Lookup {
                table, qualifier, ..
            } => Field::of_table(qualifier, &table.columns),
            Node::Filter { input, .. }
            | Node::Sort { input, .. }
            | Node::Distinct { input }
            | Node::Limit { input, .. } => input.columns(),
            Node::Join { left, right, .. } => [left.columns(), right.columns()].concat(),
            // Named by the first query's columns, which ORDER BY above refers to.
            Node::Union { inputs } => inputs[0]
                .columns()
                .into_iter()
                .map(|f| Field::named(f.name))
                .collect(),
            Node::Aggregate {
                group_by,
                aggregates,
                ..
            } => group_by
                .iter()
                .map(Expr::to_string)
                .chain(aggregates.iter().map(aggregate_name))
                .map(Field::named)
                .collect(),
            Node::Project { exprs, .. } => {
                exprs.iter().map(output_name).map(Field::named).collect()
            }
            Node::Derived {
                qualifier, names, ..
            } => names
                .iter()
                .map(|name| Field {
                    table: Some(qualifier.clone()),
                    name: name.clone(),
                })
                .collect(),
        }
    }

    /// Describes the tree as EXPLAIN prints it: one step per line, each
    /// indented below the one reading from it, a join's left side first.
    pub fn explain(&self) -> String {
        self.explain_lines().join("\n")
    }

    /// The lines of [`Node::explain`]. Step `i` of the tree, counted in
    /// this order from 0, is line `i`.
    pub fn explain_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        self.explain_into(0, &mut lines);
        lines
    }

    /// How many steps the tree has, this one included: the lines it explains to.
    pub fn steps(&self) -> usize {
        1 + self
            .inputs()
            .iter()
            .map(|input| input.steps())
            .sum::<usize>()
    }

    /// The steps this one reads from, in the order EXPLAIN lists them.
    pub fn inputs(&self) -> Vec<&Self> {
        match self {
            Node::Scan { .. } | Node::Lookup { .. } => vec![],
            Node::Join { left, right, .. } => vec![left, right],
            Node::Union { inputs } => inputs.iter().collect(),
            Node::Filter { input, .. }
            | Node::Aggregate { input, .. }
            | Node::Sort { input, .. }
            | Node::Distinct { input }
            | Node::Limit { input, .. }
            | Node::Project { input, .. }
            | Node::Derived { input, .. } => vec![input],
        }
    }

    fn explain_into(&self, depth: usize, lines: &mut Vec<String>) {
        let list = |exprs: &mut dyn Iterator<Item = String>| exprs.collect::<Vec<_>>().join(", ");
        let line = match self {
            Node::Scan {
                table,
                qualifier,
                predicates,
                access,
                ..
            } => access::describe(table, &label(table, qualifier), predicates, access),
            Node::Lookup {
                table,
                qualifier,
                column,
                key,
                left_key,
                ..
            } => {
                let through = match key {
                    LookupKey::Index(i) => {
                        let index = &table.indexes()[*i];
                        format!(
                            "{}INDEX {}",
                            if index.is_hash() { "HASH " } else { "" },
                            index.name
                        )
                    }
                    LookupKey::PrimaryKey => "PRIMARY KEY".into(),
                };
                let column = ColumnRef {
                    table: Some(qualifier.clone()),
                    name: table.columns[*column].name.clone(),
                };
                format!(
                    "LOOKUP {} USING {through} ({column} = {left_key})",
                    label(table, qualifier)
                )
            }
            Node::Filter { predicate, .. } => format!("FILTER {predicate}"),
            Node::Join {
                kind,
                method,
                condition,
                ..
            } => {
                let kind = if *kind == JoinKind::Left { "LEFT " } else { "" };
                let (name, mut on) = match method {
                    JoinMethod::Block => ("BLOCK NESTED LOOP", Vec::new()),
                    JoinMethod::Naive => ("NAIVE NESTED LOOP", Vec::new()),
                    JoinMethod::Index => ("INDEX", Vec::new()),
                    JoinMethod::Hash {
                        left_keys,
                        right_keys,
                        ..
                    }
                    | JoinMethod::Merge {
                        left_keys,
                        right_keys,
                        ..
                    } => {
                        let name = match method {
                            JoinMethod::Hash { workers, .. } if *workers > 1 => "PARALLEL HASH",
                            JoinMethod::Hash { .. } => "HASH",
                            _ => "MERGE",
                        };
                        (
                            name,
                            left_keys
                                .iter()
                                .zip(right_keys)
                                .map(|(l, r)| format!("{l} = {r}"))
                                .collect(),
                        )
                    }
                };
                on.extend(condition.iter().map(Expr::to_string));
                let on = if on.is_empty() {
                    String::new()
                } else {
                    format!(" ON {}", on.join(" AND "))
                };
                let by = match method {
                    JoinMethod::Merge {
                        left_keys,
                        desc,
                        then,
                        ..
                    } => {
                        let keys = left_keys
                            .iter()
                            .zip(desc)
                            .map(|(e, &desc)| sort_key(e, desc));
                        let then = then.iter().map(|o| sort_key(&o.expr, o.desc));
                        format!(" (SORTED BY {})", list(&mut keys.chain(then)))
                    }
                    JoinMethod::Hash { workers, .. } if *workers > 1 => {
                        format!(" ({workers} WORKERS)")
                    }
                    _ => String::new(),
                };
                format!("{kind}{name} JOIN{on}{by}")
            }
            Node::Aggregate {
                group_by,
                aggregates,
                ..
            } => {
                let mut line = format!(
                    "AGGREGATE {}",
                    list(&mut aggregates.iter().map(aggregate_name))
                );
                if !group_by.is_empty() {
                    line += &format!(" BY {}", list(&mut group_by.iter().map(Expr::to_string)));
                }
                line
            }
            Node::Sort { keys, limit, .. } => {
                let keys = list(&mut keys.iter().map(|k| sort_key(&k.expr, k.desc)));
                let top = limit.map_or(String::new(), |n| format!(" (TOP {n})"));
                format!("SORT BY {keys}{top}")
            }
            Node::Distinct { .. } => "DISTINCT".into(),
            Node::Limit { count, offset, .. } => match (count, offset) {
                (Some(count), 0) => format!("LIMIT {count}"),
                (Some(count), offset) => format!("LIMIT {count} OFFSET {offset}"),
                (None, offset) => format!("OFFSET {offset}"),
            },
            Node::Union { .. } => "UNION ALL".into(),
            Node::Project { exprs, .. } => {
                format!("PROJECT {}", list(&mut exprs.iter().map(Expr::to_string)))
            }
            Node::Derived { qualifier, .. } => format!("SUBQUERY {qualifier}"),
        };
        lines.push(format!("{}{line}", "  ".repeat(depth)));
        for input in self.inputs() {
            input.explain_into(depth + 1, lines);
        }
    }
}

/// A sort key as EXPLAIN shows it: `u.city`, or `u.city DESC`.
fn sort_key(expr: &Expr, desc: bool) -> String {
    format!("{expr}{}", if desc { " DESC" } else { "" })
}

/// A table as EXPLAIN names it: `users`, or `users u` under an alias.
fn label(table: &Table, qualifier: &str) -> String {
    match qualifier == table.name {
        true => table.name.clone(),
        false => format!("{} {qualifier}", table.name),
    }
}

/// Whether a SELECT groups its rows: it has `GROUP BY` or `HAVING`, or uses an aggregate.
fn is_grouped(
    projection: &Projection,
    group_by: &[Expr],
    having: &Option<Expr>,
    order_by: &[OrderBy],
) -> bool {
    let items = match projection {
        Projection::All => &[][..],
        Projection::Exprs(items) => items,
    };
    !group_by.is_empty()
        || having.is_some()
        || items
            .iter()
            .map(|item| &item.expr)
            .chain(order_by.iter().map(|o| &o.expr))
            .any(contains_aggregate)
}

/// The name of a select list item's column: its AS name, or else its expression's.
pub fn item_name(item: &SelectItem) -> String {
    item.alias
        .clone()
        .unwrap_or_else(|| output_name(&item.expr))
}

/// The name of a result column: a column's name, an aggregate's function, or `?column?`.
fn output_name(expr: &Expr) -> String {
    match expr {
        Expr::Column(column) | Expr::Slot(_, column) => column.name.clone(),
        Expr::Aggregate(function, ..) => function.name().into(),
        Expr::Function(function, _) => function.name().into(),
        Expr::Case { .. } => "case".into(),
        _ => "?column?".into(),
    }
}

/// The column an aggregate's result is in: its SQL text.
fn aggregate_name((function, arg, distinct): &AggregateCall) -> String {
    Expr::Aggregate(*function, arg.clone().map(Box::new), *distinct).to_string()
}

/// Rewrites a bound expression to read an aggregate's rows, as the module docs describe.
fn rewrite(expr: &Expr, group_by: &[Expr], aggregates: &mut Vec<AggregateCall>) -> Result<Expr> {
    if group_by.contains(expr) {
        return Ok(Expr::Column(ColumnRef::bare(expr.to_string())));
    }
    let mut rewrite = |e: &Expr| rewrite(e, group_by, aggregates).map(Box::new);
    Ok(match expr {
        Expr::Literal(_) => expr.clone(),
        Expr::Column(column) | Expr::Slot(_, column) => {
            return err(format!(
                "column {column} must appear in GROUP BY or be used in an aggregate function"
            ));
        }
        Expr::Unary(op, inner) => Expr::Unary(*op, rewrite(inner)?),
        Expr::Binary(left, op, right) => Expr::Binary(rewrite(left)?, *op, rewrite(right)?),
        Expr::IsNull(inner, negated) => Expr::IsNull(rewrite(inner)?, *negated),
        Expr::InList(left, items, negated) => {
            let items = items
                .iter()
                .map(|e| rewrite(e).map(|e| *e))
                .collect::<Result<_>>()?;
            Expr::InList(rewrite(left)?, items, *negated)
        }
        Expr::Subquery(query) => Expr::Subquery(Box::new(regroup(query, group_by)?)),
        Expr::Exists(query) => Expr::Exists(Box::new(regroup(query, group_by)?)),
        Expr::InSubquery(left, query, negated) => Expr::InSubquery(
            rewrite(left)?,
            Box::new(regroup(query, group_by)?),
            *negated,
        ),
        Expr::Outer(_) => unreachable!("outer references are inside subqueries"),
        Expr::Case { .. } | Expr::Function(..) => {
            expr.try_map_parts(&mut |e| rewrite(e).map(|e| *e))?
        }
        Expr::Aggregate(function, arg, distinct) => {
            if arg.as_deref().is_some_and(contains_aggregate) {
                return err("aggregate functions cannot be nested");
            }
            let call = (*function, arg.as_deref().cloned(), *distinct);
            if !aggregates.contains(&call) {
                aggregates.push(call.clone());
            }
            Expr::Column(ColumnRef::bare(aggregate_name(&call)))
        }
    })
}

/// Rewrites the outer references of a subquery in a grouped query to read
/// the aggregate's rows: each must be a `GROUP BY` expression.
fn regroup(query: &Select, group_by: &[Expr]) -> Result<Select> {
    query.try_map(&mut |e| {
        e.replace(&mut |e| match e {
            // With a single table, GROUP BY names the column unqualified.
            Expr::Outer(column) => Some(
                match [column.clone(), ColumnRef::bare(&column.name)]
                    .into_iter()
                    .map(Expr::Column)
                    .find(|e| group_by.contains(e))
                {
                    Some(grouped) => Ok(Expr::Outer(ColumnRef::bare(grouped.to_string()))),
                    None => err(format!("column {column} must appear in GROUP BY or be used in an aggregate function")),
                },
            ),
            _ => None,
        })
    })
}

/// Fails if one of the expressions has a subquery in it.
fn forbid_subqueries<'a>(exprs: impl IntoIterator<Item = &'a Expr>, clause: &str) -> Result<()> {
    match exprs.into_iter().any(Expr::has_subquery) {
        true => err(format!("subqueries are not allowed in {clause}")),
        false => Ok(()),
    }
}

/// Fails if one of the expressions uses an aggregate function: rows are only
/// grouped after WHERE, and never for INSERT, UPDATE or DELETE.
pub fn forbid<'a>(exprs: impl IntoIterator<Item = &'a Expr>, clause: &str) -> Result<()> {
    match exprs.into_iter().any(contains_aggregate) {
        true => err(format!("aggregate functions are not allowed in {clause}")),
        false => Ok(()),
    }
}

/// Whether the expression uses an aggregate function, not counting those of
/// its subqueries, which group the subquery's rows.
fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate(..) => true,
        Expr::Literal(_)
        | Expr::Column(_)
        | Expr::Outer(_)
        | Expr::Slot(..)
        | Expr::Subquery(_)
        | Expr::Exists(_) => false,
        Expr::InList(left, items, _) => {
            contains_aggregate(left) || items.iter().any(contains_aggregate)
        }
        Expr::InSubquery(left, ..) => contains_aggregate(left),
        Expr::Unary(_, inner) | Expr::IsNull(inner, _) => contains_aggregate(inner),
        Expr::Binary(left, _, right) => contains_aggregate(left) || contains_aggregate(right),
        Expr::Case { .. } | Expr::Function(..) => expr.parts().into_iter().any(contains_aggregate),
    }
}
