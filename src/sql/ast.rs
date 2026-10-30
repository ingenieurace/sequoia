use crate::datetime;
use crate::error::Result;
pub use crate::sql::lexer::quote_ident;
use crate::types::{Column, DataType, StorageKind, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable {
        name: String,
        columns: Vec<Column>,
        storage: StorageKind,
    },
    DropTable {
        name: String,
    },
    CreateIndex(CreateIndex),
    DropIndex {
        name: String,
    },
    /// `EXPLAIN statement`: describes how the statement would read its table.
    /// `EXPLAIN ANALYZE` (of a SELECT) also runs it, and tells how many rows
    /// each step returned and how long it took.
    Explain {
        statement: Box<Statement>,
        analyze: bool,
    },
    /// Copies the committed pages from the write-ahead log into the database file.
    Checkpoint,
    /// `ANALYZE [table]`: computes the statistics of a table, or of every
    /// table, for the planner to estimate how many rows conditions keep.
    Analyze {
        table: Option<String>,
    },
    /// `VACUUM [table]`: removes the row versions of a table, or of every
    /// table, that no transaction can see any more (see `mvcc/mod.rs`).
    Vacuum {
        table: Option<String>,
    },
    /// `BEGIN [TRANSACTION]`: the statements up to COMMIT or ROLLBACK form one transaction.
    Begin,
    /// `COMMIT [TRANSACTION]`
    Commit,
    /// `ROLLBACK [TRANSACTION]`, or with a name, `ROLLBACK [TRANSACTION] TO [SAVEPOINT] name`.
    Rollback {
        savepoint: Option<String>,
    },
    /// `SAVEPOINT name`
    Savepoint {
        name: String,
    },
    /// `RELEASE [SAVEPOINT] name`
    Release {
        name: String,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        source: InsertSource,
        returning: Option<Projection>,
    },
    Select(Box<Select>),
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
        returning: Option<Projection>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
        returning: Option<Projection>,
    },
}

/// The rows an INSERT adds.
#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    /// `VALUES (expr, ...), ...`
    Values(Vec<Vec<Expr>>),
    /// `SELECT ...`: the query's rows, all read before the first is inserted,
    /// so that a query of the table itself never sees the rows it adds.
    Select(Box<Select>),
}

/// `CREATE [UNIQUE] INDEX name ON table [USING BTREE | HASH] (key, ...) [INCLUDE (column, ...)] [WHERE condition]`
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    pub name: String,
    pub table: String,
    /// What the index is stored in.
    pub using: IndexKind,
    /// What the index is sorted by: columns, or expressions of the table's columns.
    pub keys: Vec<Expr>,
    /// Columns stored in each entry besides the keys, so a query that reads
    /// only those and the keys never reads the table.
    pub include: Vec<String>,
    /// A partial index's condition: only the rows it is true for are indexed.
    pub predicate: Option<Expr>,
    /// Whether two indexed rows may not have the same keys, unless one is NULL.
    pub unique: bool,
}

/// How an index is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexKind {
    /// A B+ tree: finds keys equal to values, or in a range, in order. The default.
    #[default]
    BTree,
    /// A hash file: finds only keys equal to values, of every key of the index.
    Hash,
}

impl std::fmt::Display for IndexKind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            IndexKind::BTree => "BTREE",
            IndexKind::Hash => "HASH",
        })
    }
}

impl std::fmt::Display for CreateIndex {
    /// Writes the statement as SQL that parses back to it, as the catalog stores it.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let keys = self
            .keys
            .iter()
            .map(Expr::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let unique = if self.unique { "UNIQUE " } else { "" };
        write!(
            f,
            "CREATE {unique}INDEX {} ON {}",
            quote_ident(&self.name),
            quote_ident(&self.table)
        )?;
        if self.using != IndexKind::BTree {
            write!(f, " USING {}", self.using)?;
        }
        write!(f, " ({keys})")?;
        if !self.include.is_empty() {
            write!(f, " INCLUDE ({})", quoted_list(&self.include))?;
        }
        if let Some(predicate) = &self.predicate {
            write!(f, " WHERE {predicate}")?;
        }
        Ok(())
    }
}

/// A query: a SELECT statement, or a subquery in an expression.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Select {
    /// `SELECT DISTINCT`: drops rows equal to one returned before.
    pub distinct: bool,
    pub from: From,
    pub projection: Projection,
    pub filter: Option<Expr>,
    /// `GROUP BY`: empty for none.
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
    /// `OFFSET n`: the first `n` rows are left out.
    pub offset: Option<usize>,
    /// `UNION [ALL] query`, `INTERSECT [ALL] query`, `EXCEPT [ALL] query`...:
    /// the queries this one's rows are combined with, left to right. They
    /// have no ORDER BY, LIMIT or OFFSET of their own: this query's apply
    /// to all of the rows. INTERSECT binds tighter than the others, so the
    /// parser puts `a UNION b INTERSECT c` here as `a UNION (b INTERSECT c)`:
    /// b with an INTERSECT of its own.
    pub compound: Vec<Compound>,
}

/// One query combined with those before it by a set operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Compound {
    pub op: SetOp,
    /// `ALL`: rows equal to others count as many times as they appear.
    pub all: bool,
    pub query: Select,
}

/// How a query's rows combine with those of the queries before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SetOp {
    /// The rows of both.
    Union,
    /// The rows of the ones before that this one has too.
    Intersect,
    /// The rows of the ones before that this one does not have.
    Except,
}

impl SetOp {
    pub fn name(self) -> &'static str {
        match self {
            SetOp::Union => "UNION",
            SetOp::Intersect => "INTERSECT",
            SetOp::Except => "EXCEPT",
        }
    }
}

impl Select {
    /// The query's own expressions, not those of its subqueries.
    pub fn own_exprs(&self) -> impl Iterator<Item = &Expr> {
        let projection = match &self.projection {
            Projection::All => &[][..],
            Projection::Exprs(items) => items,
        };
        projection
            .iter()
            .map(|item| &item.expr)
            .chain(self.from.joins.iter().filter_map(|j| j.on.as_ref()))
            .chain(&self.filter)
            .chain(&self.group_by)
            .chain(&self.having)
            .chain(self.order_by.iter().map(|o| &o.expr))
    }

    /// The query's own expressions and those of the subqueries in its FROM
    /// and of the queries UNION adds to it, not looking inside the
    /// subqueries in those expressions.
    pub fn exprs(&self) -> Vec<&Expr> {
        let mut exprs = Vec::new();
        for table in self.from.tables() {
            match &table.source {
                TableSource::Subquery(query) => exprs.extend(query.exprs()),
                TableSource::Recursive(cte) => {
                    exprs.extend(cte.anchor.exprs());
                    exprs.extend(cte.step.exprs());
                }
                TableSource::Values(rows) => exprs.extend(rows.iter().flatten()),
                TableSource::Table(_) | TableSource::Dual | TableSource::WorkTable { .. } => {}
            }
        }
        exprs.extend(self.own_exprs());
        for part in &self.compound {
            exprs.extend(part.query.exprs());
        }
        exprs
    }

    /// The query with each of its expressions (those `exprs` gives) replaced by `f`'s result.
    pub fn try_map(&self, f: &mut dyn FnMut(&Expr) -> Result<Expr>) -> Result<Select> {
        let mut query = self.clone();
        for table in query.from.tables_mut() {
            match &mut table.source {
                TableSource::Subquery(sub) => **sub = sub.try_map(f)?,
                TableSource::Recursive(cte) => {
                    cte.anchor = cte.anchor.try_map(f)?;
                    cte.step = cte.step.try_map(f)?;
                }
                TableSource::Values(rows) => {
                    for e in rows.iter_mut().flatten() {
                        *e = f(e)?;
                    }
                }
                TableSource::Table(_) | TableSource::Dual | TableSource::WorkTable { .. } => {}
            }
        }
        for part in &mut query.compound {
            part.query = part.query.try_map(f)?;
        }
        query.try_map_own(f)
    }

    /// The query with each of its own expressions replaced by `f`'s result.
    pub fn try_map_own(&self, f: &mut dyn FnMut(&Expr) -> Result<Expr>) -> Result<Select> {
        let mut query = self.clone();
        if let Projection::Exprs(items) = &mut query.projection {
            for item in items {
                item.expr = f(&item.expr)?;
            }
        }
        for on in query.from.joins.iter_mut().filter_map(|j| j.on.as_mut()) {
            *on = f(on)?;
        }
        for e in query
            .filter
            .iter_mut()
            .chain(&mut query.group_by)
            .chain(&mut query.having)
        {
            *e = f(e)?;
        }
        for o in &mut query.order_by {
            o.expr = f(&o.expr)?;
        }
        Ok(query)
    }
}

/// `FROM t1 [JOIN t2 ON ...]...`: a table, then the tables joined to it in order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct From {
    pub table: TableRef,
    pub joins: Vec<Join>,
}

impl From {
    /// The tables, in order.
    pub fn tables(&self) -> impl Iterator<Item = &TableRef> {
        std::iter::once(&self.table).chain(self.joins.iter().map(|j| &j.table))
    }

    pub fn tables_mut(&mut self) -> impl Iterator<Item = &mut TableRef> {
        std::iter::once(&mut self.table).chain(self.joins.iter_mut().map(|j| &mut j.table))
    }
}

/// A table in FROM, and the name it goes by there.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    pub source: TableSource,
    /// Always given for a subquery.
    pub alias: Option<String>,
    /// `alias (name, ...)`: new names for the first columns of a derived
    /// table, in order; empty to keep its own.
    pub columns: Vec<String>,
    /// `LATERAL (SELECT ...)`: a subquery that may read the columns of the
    /// tables before it in FROM, run again for each of their rows.
    pub lateral: bool,
}

/// What a table in FROM reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TableSource {
    /// A table of the database, by name.
    Table(String),
    /// `(SELECT ...)`: a derived table, whose columns are the query's. A
    /// common table expression (`WITH name AS (...)`) is one wherever it is
    /// named, as the parser puts it there.
    Subquery(Box<Select>),
    /// No table, for a SELECT without FROM: a single row of no columns.
    Dual,
    /// A recursive common table expression, wherever it is named.
    Recursive(Box<RecursiveCte>),
    /// `VALUES (expr, ...), ...`: these rows, of columns `column1`,
    /// `column2`...; the FROM of the query a VALUES is, `SELECT *` of it.
    Values(Vec<Vec<Expr>>),
    /// Inside the recursive part of a recursive common table expression,
    /// the rows its last round added, by the expression's name. `columns`
    /// are the expression's, and `cell` what the rows are handed in: both
    /// are given by the planner, when it plans the expression.
    WorkTable {
        name: String,
        columns: Vec<String>,
        cell: Option<WorkCell>,
    },
}

/// `WITH RECURSIVE name [(column, ...)] AS (anchor UNION [ALL] step)`: the
/// rows of `anchor`, then those of `step` run on them, then those of `step`
/// run on the rows it last returned, and so on until it returns none. With
/// `UNION`, not `ALL`, a row equal to one returned before is dropped, and
/// not run on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecursiveCte {
    pub name: String,
    /// The names of its columns; empty for those of `anchor`.
    pub columns: Vec<String>,
    /// The part that reads no rows of the expression itself.
    pub anchor: Select,
    /// The part that reads them: its FROM names the expression once, as a
    /// [`TableSource::WorkTable`], and no subquery of it does.
    pub step: Select,
    pub all: bool,
}

/// Where a recursive query's working rows are handed to the step that reads
/// them, for each round. Two are equal only if they are the same cell.
#[derive(Debug, Clone, Default)]
pub struct WorkCell(pub std::sync::Arc<std::sync::Mutex<std::sync::Arc<Vec<Vec<Value>>>>>);

impl WorkCell {
    /// The rows the cell holds now.
    pub fn rows(&self) -> std::sync::Arc<Vec<Vec<Value>>> {
        self.0.lock().expect("never poisoned").clone()
    }

    pub fn set(&self, rows: Vec<Vec<Value>>) {
        *self.0.lock().expect("never poisoned") = std::sync::Arc::new(rows);
    }
}

impl PartialEq for WorkCell {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for WorkCell {}

impl std::hash::Hash for WorkCell {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::sync::Arc::as_ptr(&self.0).hash(state)
    }
}

impl TableRef {
    /// A table of the database, under its own name.
    pub fn table(name: impl Into<String>) -> Self {
        TableRef::of(TableSource::Table(name.into()), None)
    }

    /// A table in FROM read from `source`, under `alias` if given.
    pub fn of(source: TableSource, alias: Option<String>) -> Self {
        TableRef {
            source,
            alias,
            columns: Vec::new(),
            lateral: false,
        }
    }

    /// What its columns are qualified by: the alias, or else the table's name.
    pub fn qualifier(&self) -> &str {
        match (&self.alias, &self.source) {
            (Some(alias), _) => alias,
            (None, TableSource::Table(name) | TableSource::WorkTable { name, .. }) => name,
            (None, TableSource::Recursive(cte)) => &cte.name,
            (None, TableSource::Dual) => "",
            (None, TableSource::Values(_)) => "values",
            (None, TableSource::Subquery(_)) => unreachable!("a subquery in FROM has an alias"),
        }
    }
}

/// `[NATURAL] [INNER | LEFT | RIGHT | FULL] JOIN table [ON condition |
/// USING (column, ...)]`; `CROSS JOIN` and `,` have no condition.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    pub on: Option<Expr>,
    /// `USING (column, ...)`: the columns of the same names on both sides
    /// are equal, and each pair is one column of the joined rows. The
    /// planner makes an ON of them.
    pub using: Vec<String>,
    /// `NATURAL`: USING the columns both sides have.
    pub natural: bool,
}

impl Join {
    /// A join on `on`, or on nothing.
    pub fn on(kind: JoinKind, table: TableRef, on: Option<Expr>) -> Self {
        Join {
            kind,
            table,
            on,
            using: Vec::new(),
            natural: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JoinKind {
    /// Pairs of rows the condition holds for.
    Inner,
    /// Those, plus each left row no right row matches, with NULLs for the right columns.
    Left,
    /// The pairs, plus each right row no left row matches, with NULLs for the left columns.
    Right,
    /// The pairs, plus the left rows and the right rows that match none, padded with NULLs.
    Full,
}

impl JoinKind {
    /// Whether the left rows that match no right row are kept.
    pub fn keeps_left(self) -> bool {
        matches!(self, JoinKind::Left | JoinKind::Full)
    }

    /// Whether the right rows that match no left row are kept.
    pub fn keeps_right(self) -> bool {
        matches!(self, JoinKind::Right | JoinKind::Full)
    }

    /// How SQL writes it before `JOIN`, if it writes anything.
    pub fn keyword(self) -> &'static str {
        match self {
            JoinKind::Inner => "",
            JoinKind::Left => "LEFT ",
            JoinKind::Right => "RIGHT ",
            JoinKind::Full => "FULL ",
        }
    }
}

/// A column, `name` or `table.name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnRef {
    pub table: Option<String>,
    pub name: String,
}

impl ColumnRef {
    /// A column referred to by name alone.
    pub fn bare(name: impl Into<String>) -> Self {
        ColumnRef {
            table: None,
            name: name.into(),
        }
    }
}

/// The mark at the start of the name of a column the planner makes of an
/// expression, named by its SQL text (an aggregate's result, a GROUP BY
/// expression's value, a window function's): no name a query writes has
/// it, and such a name prints as that text.
pub const DERIVED: char = '\u{1}';

/// The name of a column that holds the value of `expr` (see [`DERIVED`]).
pub fn derived_name(expr: impl std::fmt::Display) -> String {
    format!("{DERIVED}{expr}")
}

impl std::fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if let Some(text) = self.name.strip_prefix(DERIVED) {
            return f.write_str(text);
        }
        match &self.table {
            Some(table) => write!(f, "{}.{}", quote_ident(table), quote_ident(&self.name)),
            None => f.write_str(&quote_ident(&self.name)),
        }
    }
}

/// A select list, or the `RETURNING` list of an INSERT, UPDATE or DELETE.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Projection {
    /// `SELECT *`, or `RETURNING *`
    All,
    Exprs(Vec<SelectItem>),
}

/// `expr [AS name]` in a select list.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SelectItem {
    pub expr: Expr,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OrderBy {
    pub expr: Expr,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Expr {
    Literal(Value),
    Column(ColumnRef),
    Unary(UnaryOp, Box<Expr>),
    Binary(Box<Expr>, BinaryOp, Box<Expr>),
    /// `expr IS NULL`, or `expr IS NOT NULL` when the flag is true.
    IsNull(Box<Expr>, bool),
    /// An aggregate function over a group of rows; no argument is `COUNT(*)`.
    /// The flag is `DISTINCT`: each distinct argument value counts once.
    Aggregate(Aggregate, Option<Box<Expr>>, bool),
    /// `expr [NOT] IN (expr, ...)`; the flag is NOT.
    InList(Box<Expr>, Vec<Expr>, bool),
    /// `(SELECT ...)`: the single value of a query returning one column and at most one row.
    Subquery(Box<Select>),
    /// `EXISTS (SELECT ...)`: whether the query returns any row.
    Exists(Box<Select>),
    /// `expr [NOT] IN (SELECT ...)`; the flag is NOT.
    InSubquery(Box<Expr>, Box<Select>, bool),
    /// A column, inside a subquery, of the query around it. The planner marks
    /// these, and each is replaced by its value before the subquery runs.
    Outer(ColumnRef),
    /// `CASE [operand] WHEN a THEN b ... [ELSE c] END`: the value of the first
    /// branch whose `a` equals the operand, or without one, whose `a` is
    /// true; else `c`, or NULL. Only the branches up to the one taken are
    /// evaluated, and of those only their `a`.
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    /// A scalar function of its arguments.
    Function(Function, Vec<Expr>),
    /// A column found already among the columns of the rows the expression
    /// is evaluated on, at this position: what `sql::eval::locate` makes of
    /// a `Column` when a plan step starts, so that it is not looked up by
    /// name for every row. The parser and planner never make one. It prints
    /// as the column, so a step's columns are named as before.
    Slot(usize, ColumnRef),
    /// `function(args) OVER (...)`: a value computed from the rows of the
    /// current row's window. The planner computes each in a step of its own
    /// and reads it as a column, named by its SQL text, as it does an
    /// aggregate's result.
    Window(Box<Window>),
}

/// `function(args) OVER ([PARTITION BY expr, ...] [ORDER BY ...] [frame])`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Window {
    pub function: WindowFunction,
    /// None for `COUNT(*)`.
    pub args: Vec<Expr>,
    /// The rows of a row's partition are those with the same values of these.
    pub partition_by: Vec<Expr>,
    /// The order of the rows within a partition; rows equal in it are peers.
    pub order_by: Vec<OrderBy>,
    /// The rows an aggregate, `FIRST_VALUE`, `LAST_VALUE` or `NTH_VALUE`
    /// reads, around the current row; if not given, those from the start
    /// of the partition to the current row's last peer.
    pub frame: Option<Frame>,
    /// `OVER w` or `OVER (w ...)`: the window of the query's `WINDOW w AS
    /// (...)` this one is, or adds to. The parser puts that window's parts
    /// in and leaves this None, so nothing after it sees a name.
    pub base: Option<String>,
}

impl Window {
    /// The window's frame, as given or by default.
    pub fn frame(&self) -> Frame {
        self.frame.clone().unwrap_or(Frame {
            units: FrameUnits::Range,
            start: FrameBound::UnboundedPreceding,
            end: FrameBound::CurrentRow,
        })
    }

    /// Whether `other` reads the same partitions in the same order, so that
    /// one step can compute both.
    pub fn same_window(&self, other: &Window) -> bool {
        self.partition_by == other.partition_by && self.order_by == other.order_by
    }
}

/// What a window function computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowFunction {
    /// The row's number in its partition, from 1.
    RowNumber,
    /// 1 plus the number of rows before its first peer.
    Rank,
    /// The number of its peer group, from 1.
    DenseRank,
    /// `(rank - 1) / (rows - 1)`, as a REAL; 0 for a partition of one row.
    PercentRank,
    /// The share of the partition's rows up to its last peer, as a REAL.
    CumeDist,
    /// `NTILE(n)`: which of `n` groups as equal as can be the row falls in, from 1.
    Ntile,
    /// `LAG(value[, offset[, default]])`: `value` on the row `offset` rows
    /// (1 by default) before this one in its partition, or else `default`.
    Lag,
    /// `LEAD(...)`: likewise, on the row after.
    Lead,
    /// `value` on the first row of the frame.
    FirstValue,
    /// `value` on the last row of the frame.
    LastValue,
    /// `NTH_VALUE(value, n)`: `value` on the `n`th row of the frame, from 1.
    NthValue,
    /// An aggregate of the frame's rows.
    Aggregate(Aggregate),
}

impl WindowFunction {
    /// The function with this name, in any case: a window function's or an aggregate's.
    pub fn from_name(name: &str) -> Option<WindowFunction> {
        if let Some(aggregate) = Aggregate::from_name(name) {
            return Some(WindowFunction::Aggregate(aggregate));
        }
        Some(match name.to_uppercase().as_str() {
            "ROW_NUMBER" => WindowFunction::RowNumber,
            "RANK" => WindowFunction::Rank,
            "DENSE_RANK" => WindowFunction::DenseRank,
            "PERCENT_RANK" => WindowFunction::PercentRank,
            "CUME_DIST" => WindowFunction::CumeDist,
            "NTILE" => WindowFunction::Ntile,
            "LAG" => WindowFunction::Lag,
            "LEAD" => WindowFunction::Lead,
            "FIRST_VALUE" => WindowFunction::FirstValue,
            "LAST_VALUE" => WindowFunction::LastValue,
            "NTH_VALUE" => WindowFunction::NthValue,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            WindowFunction::RowNumber => "row_number",
            WindowFunction::Rank => "rank",
            WindowFunction::DenseRank => "dense_rank",
            WindowFunction::PercentRank => "percent_rank",
            WindowFunction::CumeDist => "cume_dist",
            WindowFunction::Ntile => "ntile",
            WindowFunction::Lag => "lag",
            WindowFunction::Lead => "lead",
            WindowFunction::FirstValue => "first_value",
            WindowFunction::LastValue => "last_value",
            WindowFunction::NthValue => "nth_value",
            WindowFunction::Aggregate(aggregate) => aggregate.name(),
        }
    }

    /// The fewest and most arguments it takes.
    pub fn arity(self) -> (usize, usize) {
        match self {
            WindowFunction::RowNumber
            | WindowFunction::Rank
            | WindowFunction::DenseRank
            | WindowFunction::PercentRank
            | WindowFunction::CumeDist => (0, 0),
            WindowFunction::Ntile | WindowFunction::FirstValue | WindowFunction::LastValue => {
                (1, 1)
            }
            WindowFunction::Lag | WindowFunction::Lead => (1, 3),
            WindowFunction::NthValue => (2, 2),
            WindowFunction::Aggregate(Aggregate::Count) => (0, 1),
            WindowFunction::Aggregate(_) => (1, 1),
        }
    }
}

/// `{ROWS | RANGE | GROUPS} [BETWEEN] start [AND end]`: the rows of a window's frame.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Frame {
    pub units: FrameUnits,
    pub start: FrameBound,
    pub end: FrameBound,
}

/// What a frame's bounds count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameUnits {
    /// Rows: `n PRECEDING` is the row `n` before the current one.
    Rows,
    /// Values of the window's one ORDER BY key: `CURRENT ROW` takes in all
    /// of the current row's peers, and `n PRECEDING` the rows whose key is
    /// at most `n` before the current row's (a number, or an INTERVAL for
    /// dates and times).
    Range,
    /// Peer groups: `n PRECEDING` starts `n` groups of peers before the
    /// current row's.
    Groups,
}

/// A bound of a frame. An offset is a number not below zero (an INT for
/// ROWS and GROUPS), or for RANGE over dates and times, an INTERVAL.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(Value),
    CurrentRow,
    Following(Value),
    UnboundedFollowing,
}

impl std::fmt::Display for FrameBound {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            FrameBound::UnboundedPreceding => f.write_str("UNBOUNDED PRECEDING"),
            FrameBound::Preceding(n) => write!(f, "{} PRECEDING", n.to_sql()),
            FrameBound::CurrentRow => f.write_str("CURRENT ROW"),
            FrameBound::Following(n) => write!(f, "{} FOLLOWING", n.to_sql()),
            FrameBound::UnboundedFollowing => f.write_str("UNBOUNDED FOLLOWING"),
        }
    }
}

impl std::fmt::Display for Window {
    /// `function(args) OVER (...)`, as SQL that parses back to it.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let list = |exprs: &mut dyn Iterator<Item = String>| exprs.collect::<Vec<_>>().join(", ");
        let name = self.function.name().to_uppercase();
        match (self.function, self.args.is_empty()) {
            (WindowFunction::Aggregate(Aggregate::Count), true) => write!(f, "{name}(*)")?,
            _ => write!(f, "{name}({})", list(&mut self.args.iter().map(Expr::to_string)))?,
        }
        let mut parts = Vec::new();
        if !self.partition_by.is_empty() {
            let keys = list(&mut self.partition_by.iter().map(Expr::to_string));
            parts.push(format!("PARTITION BY {keys}"));
        }
        if !self.order_by.is_empty() {
            let keys = self
                .order_by
                .iter()
                .map(|o| format!("{}{}", o.expr, if o.desc { " DESC" } else { "" }));
            parts.push(format!("ORDER BY {}", list(&mut keys.into_iter())));
        }
        if let Some(frame) = &self.frame {
            let units = match frame.units {
                FrameUnits::Rows => "ROWS",
                FrameUnits::Range => "RANGE",
                FrameUnits::Groups => "GROUPS",
            };
            parts.push(format!(
                "{units} BETWEEN {} AND {}",
                frame.start, frame.end
            ));
        }
        write!(f, " OVER ({})", parts.join(" "))
    }
}

/// The scalar functions, which take values and give one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Function {
    /// The first argument that is not NULL; the later ones are not evaluated.
    Coalesce,
    /// NULL if its two arguments are equal, else the first.
    Nullif,
    /// The number of characters in a TEXT (also `CHAR_LENGTH`).
    Length,
    /// A TEXT with its ASCII letters in upper case, as SQLite does.
    Upper,
    /// A TEXT with its ASCII letters in lower case, as SQLite does.
    Lower,
    /// `SUBSTR(text, start[, length])`: characters counted from 1, a negative
    /// start counting from the end, a negative length taking the characters
    /// before the start, as SQLite does.
    Substr,
    /// The absolute value of a number.
    Abs,
    /// `ROUND(number[, digits])`: a REAL rounded to `digits` decimal places
    /// (none by default), halves away from zero, as SQLite does; a DECIMAL
    /// likewise, exactly, to a DECIMAL with that many places.
    Round,
    /// `CAST(value AS type)`: the value as one of the type (see `eval.rs`).
    Cast(DataType),
    /// `EXTRACT(field FROM value)`: a part of a date, a time, a timestamp or
    /// an interval, as an INT (EPOCH as a REAL).
    Extract(datetime::Field),
    /// `TRIM(text[, characters])`: the text without the characters (spaces
    /// by default) at either end; also written `TRIM([BOTH] [characters]
    /// FROM text)`.
    Trim,
    /// `LTRIM`, or `TRIM(LEADING ...)`: at its start only.
    Ltrim,
    /// `RTRIM`, or `TRIM(TRAILING ...)`: at its end only.
    Rtrim,
    /// `REPLACE(text, from, to)`: each `from` in the text as `to`.
    Replace,
    /// `INSTR(text, part)`, or `POSITION(part IN text)`: where `part` first
    /// starts, counted in characters from 1; 0 if nowhere.
    Instr,
    /// `LEFT(text, n)`: its first `n` characters; with a negative `n`, all
    /// but its last `-n`.
    Left,
    /// `RIGHT(text, n)`: its last `n` characters; with a negative `n`, all
    /// but its first `-n`.
    Right,
    /// The characters of a TEXT in the other order.
    Reverse,
    /// `REPEAT(text, n)`: the text `n` times.
    Repeat,
    /// `LPAD(text, length[, fill])`: the text, filled out at its start to
    /// `length` characters with `fill` (spaces by default), or cut to them.
    Lpad,
    /// `RPAD(...)`: likewise, at its end.
    Rpad,
    /// `CONCAT(value, ...)`: the values as TEXT, one after another, NULLs left out.
    Concat,
    /// The least whole number not below a number (also `CEILING`).
    Ceil,
    /// The largest whole number not above a number.
    Floor,
    /// `TRUNC(number[, digits])`: cut to `digits` decimal places, toward zero.
    Trunc,
    /// -1, 0 or 1, as a number is below, at or above zero.
    Sign,
    Sqrt,
    /// `POWER(base, exponent)` (also `POW`), as a REAL.
    Power,
    Exp,
    /// The natural logarithm.
    Ln,
    /// `LOG(number)`, the logarithm to base 10, or `LOG(base, number)`.
    Log,
    Log10,
    /// `MOD(a, b)`: `a % b`.
    Mod,
    Pi,
    /// The greatest of the values, NULLs left out, as in PostgreSQL.
    Greatest,
    /// The least of the values, NULLs left out.
    Least,
    /// `NOW()`, or `CURRENT_TIMESTAMP`: the time the statement started, as
    /// a TIMESTAMP WITH TIME ZONE.
    Now,
    /// `CURRENT_DATE`: the date the statement started on, in UTC.
    CurrentDate,
    /// `CURRENT_TIME`, or `LOCALTIME`: the time of day it started at, in UTC.
    CurrentTime,
    /// `LOCALTIMESTAMP`: when it started, in UTC, as a TIMESTAMP.
    LocalTimestamp,
    /// `DATE_TRUNC(unit, timestamp)`: the timestamp cut to the unit (see
    /// `datetime::truncate`); of a DATE, its midnight's.
    DateTrunc,
    /// `value AT TIME ZONE zone`: a TIMESTAMP WITH TIME ZONE as the wall
    /// time in that zone, a TIMESTAMP; or a TIMESTAMP, as a wall time there,
    /// as the instant it is. The zone is a TEXT (see `datetime::parse_zone`)
    /// or an INTERVAL, its offset east of UTC.
    AtTimeZone,
}

impl Function {
    /// The function with this name, in any case.
    pub fn from_name(name: &str) -> Option<Function> {
        Some(match name.to_uppercase().as_str() {
            "COALESCE" => Function::Coalesce,
            "NULLIF" => Function::Nullif,
            "LENGTH" | "CHAR_LENGTH" | "CHARACTER_LENGTH" => Function::Length,
            "UPPER" => Function::Upper,
            "LOWER" => Function::Lower,
            "SUBSTR" | "SUBSTRING" => Function::Substr,
            "ABS" => Function::Abs,
            "ROUND" => Function::Round,
            "TRIM" => Function::Trim,
            "LTRIM" => Function::Ltrim,
            "RTRIM" => Function::Rtrim,
            "REPLACE" => Function::Replace,
            "INSTR" => Function::Instr,
            "LEFT" => Function::Left,
            "RIGHT" => Function::Right,
            "REVERSE" => Function::Reverse,
            "REPEAT" => Function::Repeat,
            "LPAD" => Function::Lpad,
            "RPAD" => Function::Rpad,
            "CONCAT" => Function::Concat,
            "CEIL" | "CEILING" => Function::Ceil,
            "FLOOR" => Function::Floor,
            "TRUNC" => Function::Trunc,
            "SIGN" => Function::Sign,
            "SQRT" => Function::Sqrt,
            "POWER" | "POW" => Function::Power,
            "EXP" => Function::Exp,
            "LN" => Function::Ln,
            "LOG" => Function::Log,
            "LOG10" => Function::Log10,
            "MOD" => Function::Mod,
            "PI" => Function::Pi,
            "GREATEST" => Function::Greatest,
            "LEAST" => Function::Least,
            "NOW" => Function::Now,
            "DATE_TRUNC" => Function::DateTrunc,
            _ => return None,
        })
    }

    /// The function a word written without parentheses calls, as SQL has
    /// them: `CURRENT_DATE` and the like.
    pub fn from_bare_name(name: &str) -> Option<Function> {
        Some(match name.to_uppercase().as_str() {
            "CURRENT_TIMESTAMP" => Function::Now,
            "CURRENT_DATE" => Function::CurrentDate,
            "CURRENT_TIME" | "LOCALTIME" => Function::CurrentTime,
            "LOCALTIMESTAMP" => Function::LocalTimestamp,
            _ => return None,
        })
    }

    /// The name as a result column, lower-case like an aggregate's.
    pub fn name(self) -> &'static str {
        match self {
            Function::Coalesce => "coalesce",
            Function::Nullif => "nullif",
            Function::Length => "length",
            Function::Upper => "upper",
            Function::Lower => "lower",
            Function::Substr => "substr",
            Function::Abs => "abs",
            Function::Round => "round",
            Function::Cast(_) => "cast",
            Function::Extract(_) => "extract",
            Function::Trim => "trim",
            Function::Ltrim => "ltrim",
            Function::Rtrim => "rtrim",
            Function::Replace => "replace",
            Function::Instr => "instr",
            Function::Left => "left",
            Function::Right => "right",
            Function::Reverse => "reverse",
            Function::Repeat => "repeat",
            Function::Lpad => "lpad",
            Function::Rpad => "rpad",
            Function::Concat => "concat",
            Function::Ceil => "ceil",
            Function::Floor => "floor",
            Function::Trunc => "trunc",
            Function::Sign => "sign",
            Function::Sqrt => "sqrt",
            Function::Power => "power",
            Function::Exp => "exp",
            Function::Ln => "ln",
            Function::Log => "log",
            Function::Log10 => "log10",
            Function::Mod => "mod",
            Function::Pi => "pi",
            Function::Greatest => "greatest",
            Function::Least => "least",
            Function::Now => "now",
            Function::CurrentDate => "current_date",
            Function::CurrentTime => "current_time",
            Function::LocalTimestamp => "localtimestamp",
            Function::DateTrunc => "date_trunc",
            Function::AtTimeZone => "timezone",
        }
    }

    /// The fewest and most arguments it takes.
    pub fn arity(self) -> (usize, usize) {
        match self {
            Function::Coalesce | Function::Greatest | Function::Least => (2, usize::MAX),
            Function::Concat => (1, usize::MAX),
            Function::Nullif
            | Function::Instr
            | Function::Left
            | Function::Right
            | Function::Repeat
            | Function::Power
            | Function::Mod
            | Function::DateTrunc
            | Function::AtTimeZone => (2, 2),
            Function::Substr | Function::Lpad | Function::Rpad => (2, 3),
            Function::Replace => (3, 3),
            Function::Round | Function::Trim | Function::Ltrim | Function::Rtrim => (1, 2),
            Function::Trunc | Function::Log => (1, 2),
            Function::Pi
            | Function::Now
            | Function::CurrentDate
            | Function::CurrentTime
            | Function::LocalTimestamp => (0, 0),
            Function::Length
            | Function::Upper
            | Function::Lower
            | Function::Abs
            | Function::Cast(_)
            | Function::Extract(_)
            | Function::Reverse
            | Function::Ceil
            | Function::Floor
            | Function::Sign
            | Function::Sqrt
            | Function::Exp
            | Function::Ln
            | Function::Log10 => (1, 1),
        }
    }

    /// Whether it gives the time the statement started: replaced by that
    /// value before the statement runs (see `sql::eval::at_time`).
    pub fn reads_clock(self) -> bool {
        matches!(
            self,
            Function::Now | Function::CurrentDate | Function::CurrentTime | Function::LocalTimestamp
        )
    }
}

impl Expr {
    /// Rebuilds the expression, replacing each part that `f` gives a
    /// replacement for, without looking inside that part. Other parts are
    /// kept, with their insides replaced alike, subqueries' included.
    pub fn replace(&self, f: &mut dyn FnMut(&Expr) -> Option<Result<Expr>>) -> Result<Expr> {
        if let Some(replacement) = f(self) {
            return replacement;
        }
        let mut sub = |e: &Expr| e.replace(f).map(Box::new);
        Ok(match self {
            Expr::Literal(_) | Expr::Column(_) | Expr::Outer(_) | Expr::Slot(..) => self.clone(),
            Expr::Unary(op, inner) => Expr::Unary(*op, sub(inner)?),
            Expr::Binary(left, op, right) => Expr::Binary(sub(left)?, *op, sub(right)?),
            Expr::IsNull(inner, negated) => Expr::IsNull(sub(inner)?, *negated),
            Expr::Aggregate(function, arg, distinct) => Expr::Aggregate(
                *function,
                arg.as_deref().map(&mut sub).transpose()?,
                *distinct,
            ),
            Expr::InList(left, items, negated) => {
                let items = items.iter().map(|e| e.replace(f)).collect::<Result<_>>()?;
                Expr::InList(Box::new(left.replace(f)?), items, *negated)
            }
            Expr::Subquery(query) => {
                Expr::Subquery(Box::new(query.try_map(&mut |e| e.replace(f))?))
            }
            Expr::Exists(query) => Expr::Exists(Box::new(query.try_map(&mut |e| e.replace(f))?)),
            Expr::InSubquery(left, query, negated) => {
                let left = Box::new(left.replace(f)?);
                Expr::InSubquery(
                    left,
                    Box::new(query.try_map(&mut |e| e.replace(f))?),
                    *negated,
                )
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => Expr::Case {
                operand: operand.as_deref().map(&mut sub).transpose()?,
                branches: branches
                    .iter()
                    .map(|(w, t)| Ok((w.replace(f)?, t.replace(f)?)))
                    .collect::<Result<_>>()?,
                otherwise: otherwise
                    .as_deref()
                    .map(|e| e.replace(f).map(Box::new))
                    .transpose()?,
            },
            Expr::Function(function, args) => Expr::Function(
                *function,
                args.iter().map(|e| e.replace(f)).collect::<Result<_>>()?,
            ),
            Expr::Window(_) => self.try_map_parts(&mut |e| e.replace(f))?,
        })
    }

    /// Calls `f` on this expression and every one inside it, subqueries' included.
    pub fn visit(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Literal(_)
            | Expr::Column(_)
            | Expr::Outer(_)
            | Expr::Slot(..)
            | Expr::Aggregate(_, None, _) => {}
            Expr::Unary(_, inner) | Expr::IsNull(inner, _) | Expr::Aggregate(_, Some(inner), _) => {
                inner.visit(f)
            }
            Expr::Binary(left, _, right) => {
                left.visit(f);
                right.visit(f);
            }
            Expr::InList(left, items, _) => {
                left.visit(f);
                items.iter().for_each(|e| e.visit(f));
            }
            Expr::Subquery(query) | Expr::Exists(query) => {
                query.exprs().into_iter().for_each(|e| e.visit(f))
            }
            Expr::InSubquery(left, query, _) => {
                left.visit(f);
                query.exprs().into_iter().for_each(|e| e.visit(f));
            }
            Expr::Case { .. } | Expr::Function(..) | Expr::Window(_) => {
                self.parts().into_iter().for_each(|e| e.visit(f))
            }
        }
    }

    /// The expressions a CASE, a function call or a window function is
    /// made of, in order (a window's arguments, then its PARTITION BY, then
    /// its ORDER BY); none for others.
    pub fn parts(&self) -> Vec<&Expr> {
        match self {
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => operand
                .as_deref()
                .into_iter()
                .chain(branches.iter().flat_map(|(w, t)| [w, t]))
                .chain(otherwise.as_deref())
                .collect(),
            Expr::Function(_, args) => args.iter().collect(),
            Expr::Window(window) => window
                .args
                .iter()
                .chain(&window.partition_by)
                .chain(window.order_by.iter().map(|o| &o.expr))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// A CASE or function call with each of its parts (in the order of
    /// [`Expr::parts`]) replaced by `f`'s result; others as they are.
    pub fn try_map_parts(&self, f: &mut dyn FnMut(&Expr) -> Result<Expr>) -> Result<Expr> {
        Ok(match self {
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => Expr::Case {
                operand: operand.as_deref().map(|e| f(e).map(Box::new)).transpose()?,
                branches: branches
                    .iter()
                    .map(|(w, t)| Ok((f(w)?, f(t)?)))
                    .collect::<Result<_>>()?,
                otherwise: otherwise
                    .as_deref()
                    .map(|e| f(e).map(Box::new))
                    .transpose()?,
            },
            Expr::Function(function, args) => {
                Expr::Function(*function, args.iter().map(f).collect::<Result<_>>()?)
            }
            Expr::Window(window) => {
                let mut window = (**window).clone();
                for e in window.args.iter_mut().chain(&mut window.partition_by) {
                    *e = f(e)?;
                }
                for o in &mut window.order_by {
                    o.expr = f(&o.expr)?;
                }
                Expr::Window(Box::new(window))
            }
            other => other.clone(),
        })
    }

    /// Whether the expression has a window function in it, not counting
    /// those of its subqueries.
    pub fn has_window(&self) -> bool {
        match self {
            Expr::Window(_) => true,
            Expr::Subquery(_) | Expr::Exists(_) => false,
            Expr::InSubquery(left, ..) => left.has_window(),
            Expr::Literal(_) | Expr::Column(_) | Expr::Outer(_) | Expr::Slot(..) => false,
            Expr::Unary(_, inner) | Expr::IsNull(inner, _) => inner.has_window(),
            Expr::Aggregate(_, arg, _) => arg.as_deref().is_some_and(Expr::has_window),
            Expr::Binary(left, _, right) => left.has_window() || right.has_window(),
            Expr::InList(left, items, _) => {
                left.has_window() || items.iter().any(Expr::has_window)
            }
            Expr::Case { .. } | Expr::Function(..) => self.parts().into_iter().any(Expr::has_window),
        }
    }

    /// Whether the expression has a subquery in it.
    pub fn has_subquery(&self) -> bool {
        let mut found = false;
        self.visit(&mut |e| {
            found |= matches!(
                e,
                Expr::Subquery(_) | Expr::Exists(_) | Expr::InSubquery(..)
            )
        });
        found
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Aggregate {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl Aggregate {
    /// The function with this name, in any case.
    pub fn from_name(name: &str) -> Option<Aggregate> {
        Some(match name.to_uppercase().as_str() {
            "COUNT" => Aggregate::Count,
            "SUM" => Aggregate::Sum,
            "MIN" => Aggregate::Min,
            "MAX" => Aggregate::Max,
            "AVG" => Aggregate::Avg,
            _ => return None,
        })
    }

    /// The name as a result column, lower-case like PostgreSQL's.
    pub fn name(self) -> &'static str {
        match self {
            Aggregate::Count => "count",
            Aggregate::Sum => "sum",
            Aggregate::Min => "min",
            Aggregate::Max => "max",
            Aggregate::Avg => "avg",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    /// `a % b`: the remainder of INTs (with the sign of `a`) or DECIMALs.
    Mod,
    /// `text LIKE pattern`: `%` matches any characters, `_` any one; ASCII
    /// letters match in either case, as in SQLite.
    Like,
    /// `||`: two TEXTs joined.
    Concat,
}

impl std::fmt::Display for Expr {
    /// Writes the expression as SQL that parses back to it. Operands that are
    /// themselves operations are parenthesized, whatever their precedence.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let operand = |e: &Expr| match e {
            Expr::Binary(..)
            | Expr::IsNull(..)
            | Expr::InList(..)
            | Expr::InSubquery(..)
            | Expr::Unary(UnaryOp::Not, _) => format!("({e})"),
            e => e.to_string(),
        };
        match self {
            Expr::Literal(value) => f.write_str(&value.to_sql()),
            Expr::Column(column) | Expr::Outer(column) | Expr::Slot(_, column) => {
                write!(f, "{column}")
            }
            Expr::Unary(UnaryOp::Not, inner) => write!(f, "NOT {}", operand(inner)),
            Expr::Unary(UnaryOp::Neg, inner) => match operand(inner) {
                // Not `--`, which starts a comment.
                inner if inner.starts_with('-') => write!(f, "-({inner})"),
                inner => write!(f, "-{inner}"),
            },
            Expr::Binary(left, op, right) => {
                write!(f, "{} {} {}", operand(left), op.symbol(), operand(right))
            }
            Expr::IsNull(inner, negated) => {
                write!(
                    f,
                    "{} IS {}NULL",
                    operand(inner),
                    if *negated { "NOT " } else { "" }
                )
            }
            Expr::Aggregate(function, None, _) => {
                write!(f, "{}(*)", function.name().to_uppercase())
            }
            Expr::Aggregate(function, Some(arg), distinct) => {
                let distinct = if *distinct { "DISTINCT " } else { "" };
                write!(f, "{}({distinct}{arg})", function.name().to_uppercase())
            }
            Expr::InList(left, items, negated) => {
                let items = items
                    .iter()
                    .map(Expr::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "{} {}IN ({items})",
                    operand(left),
                    if *negated { "NOT " } else { "" }
                )
            }
            Expr::Subquery(query) => write!(f, "({query})"),
            Expr::Exists(query) => write!(f, "EXISTS ({query})"),
            Expr::InSubquery(left, query, negated) => {
                write!(
                    f,
                    "{} {}IN ({query})",
                    operand(left),
                    if *negated { "NOT " } else { "" }
                )
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                f.write_str("CASE")?;
                if let Some(operand) = operand {
                    write!(f, " {operand}")?;
                }
                for (when, then) in branches {
                    write!(f, " WHEN {when} THEN {then}")?;
                }
                if let Some(otherwise) = otherwise {
                    write!(f, " ELSE {otherwise}")?;
                }
                f.write_str(" END")
            }
            Expr::Function(Function::Cast(data_type), args) => {
                write!(f, "CAST({} AS {data_type})", args[0])
            }
            Expr::Function(Function::Extract(field), args) => {
                write!(f, "EXTRACT({} FROM {})", field.name(), args[0])
            }
            Expr::Function(Function::AtTimeZone, args) => {
                write!(f, "({} AT TIME ZONE {})", operand(&args[0]), operand(&args[1]))
            }
            Expr::Function(
                function @ (Function::Now
                | Function::CurrentDate
                | Function::CurrentTime
                | Function::LocalTimestamp),
                _,
            ) => f.write_str(match function {
                Function::Now => "NOW()",
                Function::CurrentDate => "CURRENT_DATE",
                Function::CurrentTime => "CURRENT_TIME",
                _ => "LOCALTIMESTAMP",
            }),
            Expr::Function(function, args) => {
                let args = args
                    .iter()
                    .map(Expr::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{}({args})", function.name().to_uppercase())
            }
            Expr::Window(window) => write!(f, "{window}"),
        }
    }
}

impl std::fmt::Display for Select {
    /// Writes the query as SQL that parses back to it.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let list = |exprs: &mut dyn Iterator<Item = String>| exprs.collect::<Vec<_>>().join(", ");
        // A VALUES query: `SELECT *` of its rows.
        if let (TableSource::Values(rows), Projection::All, None, true, false) = (
            &self.from.table.source,
            &self.projection,
            &self.from.table.alias,
            self.from.joins.is_empty() && self.filter.is_none() && self.group_by.is_empty(),
            self.distinct || self.having.is_some(),
        ) {
            f.write_str(&values_sql(rows))?;
            return self.write_tail(f);
        }
        f.write_str(if self.distinct {
            "SELECT DISTINCT "
        } else {
            "SELECT "
        })?;
        match &self.projection {
            Projection::All => f.write_str("*")?,
            Projection::Exprs(items) => {
                let items = items.iter().map(|item| match &item.alias {
                    Some(alias) => format!("{} AS {}", item.expr, quote_ident(alias)),
                    None => item.expr.to_string(),
                });
                f.write_str(&list(&mut items.into_iter()))?
            }
        }
        if self.from.table.source != TableSource::Dual {
            write!(f, " FROM {}", self.from.table)?;
        }
        for join in &self.from.joins {
            let natural = if join.natural { "NATURAL " } else { "" };
            match (join.kind, &join.on) {
                (JoinKind::Inner, None) if !join.natural && join.using.is_empty() => {
                    write!(f, " CROSS JOIN {}", join.table)?
                }
                (kind, on) => {
                    write!(f, " {natural}{}JOIN {}", kind.keyword(), join.table)?;
                    if let Some(on) = on {
                        write!(f, " ON {on}")?;
                    }
                    if !join.using.is_empty() {
                        write!(f, " USING ({})", quoted_list(&join.using))?;
                    }
                }
            }
        }
        if let Some(filter) = &self.filter {
            write!(f, " WHERE {filter}")?;
        }
        if !self.group_by.is_empty() {
            write!(
                f,
                " GROUP BY {}",
                list(&mut self.group_by.iter().map(Expr::to_string))
            )?;
        }
        if let Some(having) = &self.having {
            write!(f, " HAVING {having}")?;
        }
        self.write_tail(f)
    }
}

impl Select {
    /// Writes the queries combined with this one, then ORDER BY, LIMIT and OFFSET.
    fn write_tail(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let list = |exprs: &mut dyn Iterator<Item = String>| exprs.collect::<Vec<_>>().join(", ");
        for part in &self.compound {
            let all = if part.all { "ALL " } else { "" };
            write!(f, " {} {all}{}", part.op.name(), part.query)?;
        }
        if !self.order_by.is_empty() {
            let keys = self
                .order_by
                .iter()
                .map(|o| format!("{}{}", o.expr, if o.desc { " DESC" } else { "" }));
            write!(f, " ORDER BY {}", list(&mut keys.into_iter()))?;
        }
        if let Some(limit) = self.limit {
            write!(f, " LIMIT {limit}")?;
        }
        if let Some(offset) = self.offset {
            write!(f, " OFFSET {offset}")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for TableRef {
    /// Writes the table as SQL that means the same: a recursive common table
    /// expression as a subquery that defines it and reads all of it.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if self.lateral {
            f.write_str("LATERAL ")?;
        }
        match &self.source {
            TableSource::Table(name) | TableSource::WorkTable { name, .. } => {
                f.write_str(&quote_ident(name))?
            }
            TableSource::Subquery(query) => write!(f, "({query})")?,
            TableSource::Dual => {}
            TableSource::Values(rows) => write!(f, "({})", values_sql(rows))?,
            TableSource::Recursive(cte) => {
                let columns = match cte.columns.is_empty() {
                    true => String::new(),
                    false => format!("({})", quoted_list(&cte.columns)),
                };
                let all = if cte.all { "ALL " } else { "" };
                write!(
                    f,
                    "(WITH RECURSIVE {name}{columns} AS ({} UNION {all}{}) SELECT * FROM {name}) {}",
                    cte.anchor,
                    cte.step,
                    quote_ident(self.qualifier()),
                    name = quote_ident(&cte.name)
                )?;
                return self.write_columns(f);
            }
        }
        if let Some(alias) = &self.alias {
            write!(f, " {}", quote_ident(alias))?;
        }
        self.write_columns(f)
    }
}

impl TableRef {
    fn write_columns(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.columns.is_empty() {
            true => Ok(()),
            false => write!(f, "({})", quoted_list(&self.columns)),
        }
    }
}

/// `VALUES (a, b), (c, d)`.
fn values_sql(rows: &[Vec<Expr>]) -> String {
    let rows: Vec<String> = rows
        .iter()
        .map(|row| {
            let values: Vec<String> = row.iter().map(Expr::to_string).collect();
            format!("({})", values.join(", "))
        })
        .collect();
    format!("VALUES {}", rows.join(", "))
}

/// Names, each as [`quote_ident`] writes it, separated by commas.
pub fn quoted_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| quote_ident(name))
        .collect::<Vec<_>>()
        .join(", ")
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::And => "AND",
            BinaryOp::Or => "OR",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Like => "LIKE",
            BinaryOp::Concat => "||",
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::sql::ast::{Expr, Select, Statement};
    use crate::sql::parse;

    fn where_clause(sql: &str) -> Expr {
        match parse(&format!("SELECT * FROM t WHERE {sql}"))
            .unwrap()
            .pop()
        {
            Some(Statement::Select(query)) if query.filter.is_some() => {
                let Select { filter, .. } = *query;
                filter.expect("checked above")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn expressions_print_as_sql_that_parses_back() {
        for sql in [
            "a + b * c",
            "(a + b) * -c",
            "NOT a = 1 OR b IS NOT NULL AND c",
            "x - -1 >= 'it''s'",
            "COUNT(*) > 1 AND SUM(a + 1) < MAX(b) / 2",
            "COUNT(DISTINCT a) + SUM(DISTINCT (b * 2))",
            "a IN (1, 2 + 3) AND NOT b NOT IN ('x') OR (c IN (d)) = TRUE",
            "EXISTS (SELECT * FROM t2 WHERE t2.a = t.a) AND x > (SELECT MAX(y) FROM u)",
            "a NOT IN (SELECT DISTINCT b FROM u x JOIN v ON v.c = x.c LEFT JOIN w ON w.d = v.d CROSS JOIN z \
             WHERE b > 0 GROUP BY b HAVING COUNT(*) > 1 ORDER BY b DESC, 2 LIMIT 3)",
            "NOT EXISTS (SELECT 1 FROM u WHERE u.a IN (SELECT a FROM v))",
            "x IN (SELECT d.b + 1 AS c FROM (SELECT b, COUNT(*) AS n FROM u GROUP BY b) d JOIN (SELECT * FROM v) e ON e.b = d.b)",
            "-(a - b) IS NULL",
            "- -0.75 < ABS(c1) AND - -a > - - -1 AND -(-2) = 2",
            "NOT (NOT a)",
            "a = NULL OR b = TRUE",
            "CASE WHEN a > 1 THEN 'x' WHEN b IS NULL THEN 'y' ELSE 'z' END = c",
            "CASE a + 1 WHEN 2 THEN b END IS NULL",
            "COALESCE(a, NULLIF(b, 2), 3) + ABS(-c) > LENGTH(UPPER(d || 'x'))",
            "SUBSTR(LOWER(a), 2, -1) LIKE 'a%' AND NOT b LIKE '_'",
            "ROUND(a * 2.5, 1) > 1e20 OR b < 0.1 OR c BETWEEN 1.5 AND 2",
            "x IN (SELECT a FROM t UNION ALL SELECT b FROM u UNION SELECT c FROM v ORDER BY a LIMIT 3 OFFSET 1)",
            "x IN (SELECT a FROM t INTERSECT SELECT b FROM u EXCEPT ALL SELECT c FROM v)",
            "x IN (SELECT 1 UNION SELECT 2 INTERSECT ALL SELECT 2 EXCEPT SELECT 3)",
            "EXISTS (SELECT 1 WHERE x > 2) AND (SELECT 2) = 2",
            "x IN (SELECT a.x FROM a RIGHT JOIN b ON b.y = a.y FULL JOIN c ON c.z = b.z LEFT JOIN d ON TRUE)",
            "EXISTS (SELECT * FROM a, LATERAL (SELECT a.x AS y) d(z) LEFT JOIN LATERAL (SELECT 1) e ON TRUE)",
            "x > (SELECT SUM(a) OVER (PARTITION BY b, c ORDER BY c DESC ROWS BETWEEN 2 PRECEDING AND 1 FOLLOWING) FROM t)",
            "(SELECT ROW_NUMBER() OVER () FROM t) = (SELECT COUNT(*) OVER (ORDER BY a RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM t)",
            "(SELECT LAG(a, 2, 0) OVER (ORDER BY a ROWS UNBOUNDED PRECEDING) + NTILE(4) OVER (PARTITION BY b) FROM t) > 1",
            "x IN (WITH c (k) AS (SELECT a FROM t), d AS (SELECT k FROM c) SELECT k FROM d)",
            r#""select" % 3 = "Mixed Case" AND "it""s" > t."order""#,
            "(ts AT TIME ZONE '+02:00') > (NOW() AT TIME ZONE INTERVAL '-01:30:00') OR CURRENT_DATE IS NULL",
            "d + INTERVAL '1 year 2 mons 3 days 04:05:06.5' < TIMESTAMPTZ '2024-01-01 08:00:00+00'",
            "TRIM(LEADING 'x' FROM a) = LEFT(b, 2) AND POSITION('z' IN c) > 0 AND GREATEST(d, 1) % 2 = 1",
            "EXISTS (SELECT * FROM a NATURAL LEFT JOIN b JOIN c USING (k, \"j k\") FULL JOIN d USING (k))",
            "x IN (VALUES (1, 'a'), (2, 'b')) OR y IN (SELECT column1 FROM (VALUES (1)) v (n))",
            "(SELECT SUM(a) OVER w FROM t WINDOW w AS (PARTITION BY b ORDER BY c GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW)) > 1",
            "(SELECT COUNT(*) OVER (ORDER BY d RANGE BETWEEN INTERVAL '3 days' PRECEDING AND 2.5 FOLLOWING) FROM t) = 1",
            "(SELECT SUM((SELECT 1)) FROM t) = 1",
        ] {
            let expr = where_clause(sql);
            assert_eq!(
                where_clause(&expr.to_string()),
                expr,
                "{sql} printed as {expr}"
            );
        }
        assert_eq!(where_clause("a + b * c").to_string(), "a + (b * c)");
        assert_eq!(
            where_clause("count(*) + sum(x)").to_string(),
            "COUNT(*) + SUM(x)"
        );
    }
}
