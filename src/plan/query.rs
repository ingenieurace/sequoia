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
//!   column (or an LSM table's primary key), once per left row;
//! - otherwise, with any equality between the two sides, a hash join reads
//!   the right table once into a hash table keyed by the right sides;
//! - otherwise a nested loop join reads the right table once and tries every
//!   row with every left row.
//!
//! The rest of the condition is checked on each pair. Keys are compared as
//! values, so a NULL key matches nothing, and neither do keys of different
//! types, where `=` would report an error.
//!
//! **Steps.** On top of the scans and joins, a SELECT has an aggregate (and a
//! filter for HAVING) if it groups, a sort if it has ORDER BY (which applies
//! the LIMIT itself, as a top-N sort) or else a limit, and a projection unless
//! it is `SELECT *`. `SELECT DISTINCT` adds a distinct step above the
//! projection, which drops each row equal to one it has already returned, and
//! the limit moves above it, as rows the sort keeps may still be dropped. Its
//! ORDER BY may only use expressions of the select list: rows that differ in
//! anything else become one, which would have no single place in the order.
//! The executor pulls rows through the tree one at a time;
//! sorts, aggregates and the right sides of hash and nested loop joins read
//! all of their input first. A distinct step streams, but remembers every row
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
use crate::catalog::table::{Access, Table};
use crate::error::{Result, err};
use crate::plan::{access, filter};
use crate::predicate::Predicate;
use crate::sql::ast::{Aggregate, BinaryOp, ColumnRef, Expr, From, JoinKind, OrderBy, Projection, Select};
use crate::sql::eval::{Field, resolve};
use crate::types::{StorageKind, Value};

/// A SELECT's plan: the tree, and the names of the columns it returns.
pub struct Plan<'a> {
    pub root: Node<'a>,
    pub columns: Vec<String>,
}

/// One step of a plan.
pub enum Node<'a> {
    /// Reads a table's rows the way `access` says. `qualifier` is what the
    /// query calls the table: its alias, or else its name.
    Scan { table: &'a Table, qualifier: String, needed: Vec<bool>, predicates: Vec<Predicate>, access: Access },
    /// Reads the rows of a table whose `column` equals a value an index join
    /// gives, through an index or the primary key. Only an index join's right side.
    Lookup { table: &'a Table, qualifier: String, needed: Vec<bool>, column: usize, key: LookupKey, left_key: Expr },
    /// Keeps the rows `predicate` is true for.
    Filter { input: Box<Node<'a>>, predicate: Expr },
    /// Pairs each left row with the right rows `condition` holds for, as `method` finds them.
    Join { left: Box<Node<'a>>, right: Box<Node<'a>>, kind: JoinKind, method: JoinMethod, condition: Option<Expr> },
    /// Folds rows into one row per group: the keys, then the aggregates.
    Aggregate { input: Box<Node<'a>>, group_by: Vec<Expr>, aggregates: Vec<AggregateCall> },
    /// Orders the rows, keeping the first `limit` if given.
    Sort { input: Box<Node<'a>>, keys: Vec<OrderBy>, limit: Option<usize> },
    /// Drops each row equal to one already returned, NULLs included.
    Distinct { input: Box<Node<'a>> },
    /// Keeps the first `count` rows.
    Limit { input: Box<Node<'a>>, count: usize },
    /// Evaluates the select list on each row.
    Project { input: Box<Node<'a>>, exprs: Vec<Expr> },
}

/// How a join finds the right rows that may match a left row.
pub enum JoinMethod {
    /// Tries every right row.
    NestedLoop,
    /// Those whose `right_keys` equal the left row's `left_keys`, from a hash table.
    Hash { left_keys: Vec<Expr>, right_keys: Vec<Expr> },
    /// Those the right side, a `Lookup` (maybe under filters), finds for the left row.
    Index,
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

/// A table in FROM: the table, and what the query calls it.
struct Source<'a> {
    table: &'a Table,
    qualifier: String,
}

/// Plans a SELECT on the tables of the catalog.
pub fn select<'a>(catalog: &'a Catalog, query: Select) -> Result<Plan<'a>> {
    let Select { distinct, from, projection, filter, group_by, having, order_by, limit } = query;
    let sources = from
        .tables()
        .map(|r| Ok(Source { table: catalog.get(&r.name)?, qualifier: r.qualifier().into() }))
        .collect::<Result<Vec<_>>>()?;
    for (i, source) in sources.iter().enumerate() {
        if sources[..i].iter().any(|s| s.qualifier == source.qualifier) {
            return err(format!("table name {} appears more than once in FROM", source.qualifier));
        }
    }
    forbid(&filter, "WHERE")?;
    forbid(from.joins.iter().filter_map(|j| j.on.as_ref()), "JOIN ... ON")?;
    forbid_subqueries(from.joins.iter().filter_map(|j| j.on.as_ref()), "JOIN ... ON")?;
    forbid_subqueries(&group_by, "GROUP BY")?;

    // Bind every column reference to its table.
    let binder = Binder::new(catalog, &sources);
    let bind = |e: &Expr| binder.bind(e);
    let bind_all = |exprs: &[Expr]| exprs.iter().map(bind).collect::<Result<Vec<_>>>();
    let projection = match projection {
        Projection::All => Projection::All,
        Projection::Exprs(exprs) => Projection::Exprs(bind_all(&exprs)?),
    };
    let filter = filter.as_ref().map(bind).transpose()?;
    let ons = from.joins.iter().map(|j| j.on.as_ref().map(bind).transpose()).collect::<Result<Vec<_>>>()?;
    let group_by = bind_all(&group_by)?;
    let having = having.as_ref().map(bind).transpose()?;
    let order_by = order_by
        .iter()
        .map(|o| Ok(OrderBy { expr: bind(&o.expr)?, desc: o.desc }))
        .collect::<Result<Vec<_>>>()?;
    for (i, on) in ons.iter().enumerate() {
        if let Some(later) = on.iter().flat_map(|on| binder.tables(on)).find(|&t| t > i + 1) {
            let (this, later) = (&sources[i + 1].qualifier, &sources[later].qualifier);
            return err(format!("the ON of {this} refers to {later}, which is joined after it"));
        }
    }

    let grouped = is_grouped(&projection, &group_by, &having, &order_by);
    let (exprs, columns) = match projection {
        Projection::All if grouped => return err("SELECT * cannot be used with GROUP BY or aggregate functions"),
        Projection::All => {
            let names = sources.iter().flat_map(|s| s.table.columns.iter().map(|c| c.name.clone())).collect();
            (None, names)
        }
        Projection::Exprs(exprs) => {
            let names = exprs.iter().map(output_name).collect();
            (Some(exprs), names)
        }
    };
    if let (true, Some(exprs)) = (distinct, &exprs)
        && let Some(key) = order_by.iter().find(|o| !exprs.contains(&o.expr))
    {
        return err(format!("for SELECT DISTINCT, ORDER BY {} must appear in the select list", key.expr));
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
        .map(|s| match exprs {
            None => vec![true; s.table.columns.len()],
            Some(_) => filter::needed_columns(s.table, &s.qualifier, used.iter().copied()),
        })
        .collect();

    let mut node = joins(&sources, &from, filter, ons, needed, &binder);

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
            .map(|o| Ok(OrderBy { expr: rewrite(&o.expr)?, desc: o.desc }))
            .collect::<Result<Vec<_>>>()?;
        node = Node::Aggregate { input: Box::new(node), group_by, aggregates };
        (Some(exprs), having, order_by)
    } else {
        (exprs, None, order_by)
    };

    if let Some(predicate) = having {
        node = Node::Filter { input: Box::new(node), predicate };
    }
    node = match (order_by.is_empty(), limit) {
        (false, _) => Node::Sort { input: Box::new(node), keys: order_by, limit: limit.filter(|_| !distinct) },
        (true, Some(count)) if !distinct => Node::Limit { input: Box::new(node), count },
        _ => node,
    };
    if let Some(exprs) = exprs {
        node = Node::Project { input: Box::new(node), exprs };
    }
    if distinct {
        node = Node::Distinct { input: Box::new(node) };
        if let Some(count) = limit {
            node = Node::Limit { input: Box::new(node), count };
        }
    }
    Ok(Plan { root: node, columns })
}

/// Plans the scans and joins of FROM, with WHERE and the ONs placed as the
/// module docs describe.
fn joins<'a>(
    sources: &[Source<'a>],
    from: &From,
    filter: Option<Expr>,
    ons: Vec<Option<Expr>>,
    mut needed: Vec<Vec<bool>>,
    binder: &Binder,
) -> Node<'a> {
    let n = sources.len();
    let kinds: Vec<JoinKind> = std::iter::once(JoinKind::Inner).chain(from.joins.iter().map(|j| j.kind)).collect();
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
                        if right_only { &mut pushed[i + 1] } else { &mut conditions[i + 1] }.push(part);
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
                Some(&last) if tables.len() == 1 && kinds[last] == JoinKind::Inner => pushed[last].push(part),
                Some(&last) if kinds[last] == JoinKind::Inner => conditions[last].push(part),
                Some(&last) => after[last].push(part),
            }
        }
    }

    let first = &sources[0];
    let mut node = scan(first.table, &first.qualifier, &and(std::mem::take(&mut pushed[0])), needed[0].clone());
    for i in 1..n {
        let source = &sources[i];
        let right_filter = and(std::mem::take(&mut pushed[i]));
        let mut residual = std::mem::take(&mut conditions[i]);
        // Equalities between the tables joined so far and this one: (position, left side, right side).
        let keys: Vec<(usize, Expr, Expr)> = residual
            .iter()
            .enumerate()
            .filter_map(|(at, part)| {
                let Expr::Binary(a, BinaryOp::Eq, b) = part else { return None };
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
            .collect();

        let index = keys.iter().find_map(|(at, left, right)| {
            let Expr::Column(column) = right else { return None };
            let column = source.table.column_index(&column.name).ok()?;
            Some((*at, left.clone(), column, lookup_key(source.table, column)?))
        });
        let (method, right) = if let Some((at, left_key, column, key)) = index {
            residual.remove(at);
            needed[i][column] = true;
            let lookup = Node::Lookup {
                table: source.table,
                qualifier: source.qualifier.clone(),
                needed: needed[i].clone(), // includes the filter's columns: it is part of the query
                column,
                key,
                left_key,
            };
            let right = match right_filter {
                Some(predicate) => Node::Filter { input: Box::new(lookup), predicate },
                None => lookup,
            };
            (JoinMethod::Index, right)
        } else {
            let right = scan(source.table, &source.qualifier, &right_filter, needed[i].clone());
            if keys.is_empty() {
                (JoinMethod::NestedLoop, right)
            } else {
                for (at, ..) in keys.iter().rev() {
                    residual.remove(*at);
                }
                let (left_keys, right_keys) = keys.into_iter().map(|(_, l, r)| (l, r)).unzip();
                (JoinMethod::Hash { left_keys, right_keys }, right)
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
            node = Node::Filter { input: Box::new(node), predicate };
        }
    }
    node
}

/// How a lookup on this column of the table can find rows, if it can: through
/// an index on it, or the primary key of an LSM table.
fn lookup_key(table: &Table, column: usize) -> Option<LookupKey> {
    if let Some(i) = table.indexes().iter().position(|index| index.column == column) {
        return Some(LookupKey::Index(i));
    }
    (table.storage_kind() == StorageKind::Lsm && table.primary_key() == Some(column)).then_some(LookupKey::PrimaryKey)
}

/// Plans reading the rows of `table`, which the query calls `qualifier`, that
/// `filter` matches: a scan, and a filter if there is one. `needed` marks the
/// columns read above the filter.
pub fn scan<'a>(table: &'a Table, qualifier: &str, filter: &Option<Expr>, needed: Vec<bool>) -> Node<'a> {
    let needed_by_filter = filter::needed_columns(table, qualifier, filter);
    let needed = needed.iter().zip(needed_by_filter).map(|(a, b)| *a || b).collect();
    let predicates = filter::predicates(table, qualifier, filter);
    let access = access::choose(table, &predicates);
    let scan = Node::Scan { table, qualifier: qualifier.into(), needed, predicates, access };
    match filter {
        Some(predicate) => Node::Filter { input: Box::new(scan), predicate: predicate.clone() },
        None => scan,
    }
}

/// Binds an expression of a statement on one table (or on none, for VALUES)
/// as a SELECT's are bound: its columns are checked and its subqueries'
/// outer references marked.
pub fn bind(catalog: &Catalog, table: Option<&Table>, expr: &Expr) -> Result<Expr> {
    let sources: Vec<Source> = table.into_iter().map(|table| Source { table, qualifier: table.name.clone() }).collect();
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
            let own = Field::of_table(&source.qualifier, &source.table.columns);
            owners.extend(std::iter::repeat_n(i, own.len()));
            fields.extend(own);
        }
        Binder { catalog, fields, owners, single: sources.len() == 1 }
    }

    /// The expression with each column qualified by its table (left as it is
    /// with a single table). Fails on an unknown or ambiguous column.
    fn bind(&self, expr: &Expr) -> Result<Expr> {
        let bind = |e: &Expr| self.bind(e).map(Box::new);
        Ok(match expr {
            Expr::Literal(_) => expr.clone(),
            Expr::Column(column) => {
                let field = &self.fields[resolve(&self.fields, column)?];
                match self.single {
                    true => Expr::Column(ColumnRef::bare(&field.name)),
                    false => Expr::Column(ColumnRef { table: field.table.clone(), name: field.name.clone() }),
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
            Expr::InList(left, items, negated) => {
                Expr::InList(bind(left)?, items.iter().map(|e| self.bind(e)).collect::<Result<_>>()?, *negated)
            }
            Expr::Subquery(query) => Expr::Subquery(Box::new(self.subquery(query, true)?)),
            Expr::Exists(query) => Expr::Exists(Box::new(self.subquery(query, false)?)),
            Expr::InSubquery(left, query, negated) => {
                Expr::InSubquery(bind(left)?, Box::new(self.subquery(query, true)?), *negated)
            }
            Expr::Outer(_) => expr.clone(),
        })
    }

    /// A subquery with its outer references marked, after checking that it
    /// plans (returning one column, if `one_column`).
    fn subquery(&self, query: &Select, one_column: bool) -> Result<Select> {
        let query = self.correlate(query, &mut Vec::new())?;
        let check = substitute(&query, &mut |_| Ok(Value::Null))?;
        if select(self.catalog, check)?.columns.len() != 1 && one_column {
            return err("subquery must return only one column");
        }
        Ok(query)
    }

    /// Marks the outer references of `query`, a subquery of the query this
    /// binds, and of the subqueries inside it: the columns that none of their
    /// own tables, nor those of the queries between (`scopes`), have, but
    /// this query's do.
    fn correlate(&self, query: &Select, scopes: &mut Vec<Vec<Field>>) -> Result<Select> {
        let mut own = Vec::new();
        for table in query.from.tables() {
            own.extend(Field::of_table(table.qualifier(), &self.catalog.get(&table.name)?.columns));
        }
        scopes.push(own);
        let query = query.try_map(&mut |e| self.correlate_expr(e, scopes));
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
                    Expr::Outer(ColumnRef { table: field.table.clone(), name: field.name.clone() })
                }))
            }
            Expr::Subquery(query) => Some(self.correlate(query, scopes).map(|q| Expr::Subquery(Box::new(q)))),
            Expr::Exists(query) => Some(self.correlate(query, scopes).map(|q| Expr::Exists(Box::new(q)))),
            Expr::InSubquery(left, query, negated) => Some((|| {
                let left = self.correlate_expr(left, scopes)?;
                Ok(Expr::InSubquery(Box::new(left), Box::new(self.correlate(query, scopes)?), *negated))
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
pub fn substitute(query: &Select, value: &mut dyn FnMut(&ColumnRef) -> Result<Value>) -> Result<Select> {
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
    parts.into_iter().reduce(|a, b| Expr::Binary(Box::new(a), BinaryOp::And, Box::new(b)))
}

impl Node<'_> {
    /// The columns of the rows this step produces. Expressions on them refer
    /// to them by name, and by table if they come from one.
    pub fn columns(&self) -> Vec<Field> {
        match self {
            Node::Scan { table, qualifier, .. } | Node::Lookup { table, qualifier, .. } => {
                Field::of_table(qualifier, &table.columns)
            }
            Node::Filter { input, .. }
            | Node::Sort { input, .. }
            | Node::Distinct { input }
            | Node::Limit { input, .. } => input.columns(),
            Node::Join { left, right, .. } => [left.columns(), right.columns()].concat(),
            Node::Aggregate { group_by, aggregates, .. } => group_by
                .iter()
                .map(Expr::to_string)
                .chain(aggregates.iter().map(aggregate_name))
                .map(Field::named)
                .collect(),
            Node::Project { exprs, .. } => exprs.iter().map(output_name).map(Field::named).collect(),
        }
    }

    /// Describes the tree as EXPLAIN prints it: one step per line, each
    /// indented below the one reading from it, a join's left side first.
    pub fn explain(&self) -> String {
        let mut lines = Vec::new();
        self.explain_into(0, &mut lines);
        lines.join("\n")
    }

    fn explain_into(&self, depth: usize, lines: &mut Vec<String>) {
        let list = |exprs: &mut dyn Iterator<Item = String>| exprs.collect::<Vec<_>>().join(", ");
        let (line, inputs): (String, Vec<&Node>) = match self {
            Node::Scan { table, qualifier, predicates, access, .. } => {
                (access::describe(table, &label(table, qualifier), predicates, access), vec![])
            }
            Node::Lookup { table, qualifier, column, key, left_key, .. } => {
                let through = match key {
                    LookupKey::Index(i) => format!("INDEX {}", table.indexes()[*i].name),
                    LookupKey::PrimaryKey => "PRIMARY KEY".into(),
                };
                let column = ColumnRef { table: Some(qualifier.clone()), name: table.columns[*column].name.clone() };
                (format!("LOOKUP {} USING {through} ({column} = {left_key})", label(table, qualifier)), vec![])
            }
            Node::Filter { input, predicate } => (format!("FILTER {predicate}"), vec![input]),
            Node::Join { left, right, kind, method, condition } => {
                let kind = if *kind == JoinKind::Left { "LEFT " } else { "" };
                let (name, mut on) = match method {
                    JoinMethod::NestedLoop => ("NESTED LOOP", Vec::new()),
                    JoinMethod::Index => ("INDEX", Vec::new()),
                    JoinMethod::Hash { left_keys, right_keys } => {
                        ("HASH", left_keys.iter().zip(right_keys).map(|(l, r)| format!("{l} = {r}")).collect())
                    }
                };
                on.extend(condition.iter().map(Expr::to_string));
                let on = if on.is_empty() { String::new() } else { format!(" ON {}", on.join(" AND ")) };
                (format!("{kind}{name} JOIN{on}"), vec![left, right])
            }
            Node::Aggregate { input, group_by, aggregates } => {
                let mut line = format!("AGGREGATE {}", list(&mut aggregates.iter().map(aggregate_name)));
                if !group_by.is_empty() {
                    line += &format!(" BY {}", list(&mut group_by.iter().map(Expr::to_string)));
                }
                (line, vec![input])
            }
            Node::Sort { input, keys, limit } => {
                let keys = list(&mut keys.iter().map(|k| format!("{}{}", k.expr, if k.desc { " DESC" } else { "" })));
                let top = limit.map_or(String::new(), |n| format!(" (TOP {n})"));
                (format!("SORT BY {keys}{top}"), vec![input])
            }
            Node::Distinct { input } => ("DISTINCT".into(), vec![input]),
            Node::Limit { input, count } => (format!("LIMIT {count}"), vec![input]),
            Node::Project { input, exprs } => {
                (format!("PROJECT {}", list(&mut exprs.iter().map(Expr::to_string))), vec![input])
            }
        };
        lines.push(format!("{}{line}", "  ".repeat(depth)));
        for input in inputs {
            input.explain_into(depth + 1, lines);
        }
    }
}

/// A table as EXPLAIN names it: `users`, or `users u` under an alias.
fn label(table: &Table, qualifier: &str) -> String {
    match qualifier == table.name {
        true => table.name.clone(),
        false => format!("{} {qualifier}", table.name),
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
        Expr::Column(column) => column.name.clone(),
        Expr::Aggregate(function, ..) => function.name().into(),
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
        Expr::Column(column) => {
            return err(format!("column {column} must appear in GROUP BY or be used in an aggregate function"));
        }
        Expr::Unary(op, inner) => Expr::Unary(*op, rewrite(inner)?),
        Expr::Binary(left, op, right) => Expr::Binary(rewrite(left)?, *op, rewrite(right)?),
        Expr::IsNull(inner, negated) => Expr::IsNull(rewrite(inner)?, *negated),
        Expr::InList(left, items, negated) => {
            let items = items.iter().map(|e| rewrite(e).map(|e| *e)).collect::<Result<_>>()?;
            Expr::InList(rewrite(left)?, items, *negated)
        }
        Expr::Subquery(query) => Expr::Subquery(Box::new(regroup(query, group_by)?)),
        Expr::Exists(query) => Expr::Exists(Box::new(regroup(query, group_by)?)),
        Expr::InSubquery(left, query, negated) => {
            Expr::InSubquery(rewrite(left)?, Box::new(regroup(query, group_by)?), *negated)
        }
        Expr::Outer(_) => unreachable!("outer references are inside subqueries"),
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
        Expr::Literal(_) | Expr::Column(_) | Expr::Outer(_) | Expr::Subquery(_) | Expr::Exists(_) => false,
        Expr::InList(left, items, _) => contains_aggregate(left) || items.iter().any(contains_aggregate),
        Expr::InSubquery(left, ..) => contains_aggregate(left),
        Expr::Unary(_, inner) | Expr::IsNull(inner, _) => contains_aggregate(inner),
        Expr::Binary(left, _, right) => contains_aggregate(left) || contains_aggregate(right),
    }
}
