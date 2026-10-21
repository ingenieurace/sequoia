use crate::error::Result;
use crate::types::{Column, StorageKind, Value};

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
    Explain { statement: Box<Statement>, analyze: bool },
    /// Copies the committed pages from the write-ahead log into the database file.
    Checkpoint,
    /// `BEGIN [TRANSACTION]`: the statements up to COMMIT or ROLLBACK form one transaction.
    Begin,
    /// `COMMIT [TRANSACTION]`
    Commit,
    /// `ROLLBACK [TRANSACTION]`, or with a name, `ROLLBACK [TRANSACTION] TO [SAVEPOINT] name`.
    Rollback { savepoint: Option<String> },
    /// `SAVEPOINT name`
    Savepoint { name: String },
    /// `RELEASE [SAVEPOINT] name`
    Release { name: String },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
        returning: Option<Projection>,
    },
    Select(Select),
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
        let keys = self.keys.iter().map(Expr::to_string).collect::<Vec<_>>().join(", ");
        let unique = if self.unique { "UNIQUE " } else { "" };
        write!(f, "CREATE {unique}INDEX {} ON {}", self.name, self.table)?;
        if self.using != IndexKind::BTree {
            write!(f, " USING {}", self.using)?;
        }
        write!(f, " ({keys})")?;
        if !self.include.is_empty() {
            write!(f, " INCLUDE ({})", self.include.join(", "))?;
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
    /// `UNION [ALL] query`...: the queries whose rows follow this one's, each
    /// with whether it is `UNION ALL`, which keeps rows equal to earlier
    /// ones. They have no ORDER BY, LIMIT or OFFSET of their own: this
    /// query's apply to all of the rows.
    pub compound: Vec<(bool, Select)>,
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
            if let TableSource::Subquery(query) = &table.source {
                exprs.extend(query.exprs());
            }
        }
        exprs.extend(self.own_exprs());
        for (_, part) in &self.compound {
            exprs.extend(part.exprs());
        }
        exprs
    }

    /// The query with each of its expressions (those `exprs` gives) replaced by `f`'s result.
    pub fn try_map(&self, f: &mut dyn FnMut(&Expr) -> Result<Expr>) -> Result<Select> {
        let mut query = self.clone();
        for table in query.from.tables_mut() {
            if let TableSource::Subquery(sub) = &mut table.source {
                **sub = sub.try_map(f)?;
            }
        }
        for (_, part) in &mut query.compound {
            *part = part.try_map(f)?;
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
        for e in query.filter.iter_mut().chain(&mut query.group_by).chain(&mut query.having) {
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
}

/// What a table in FROM reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TableSource {
    /// A table of the database, by name.
    Table(String),
    /// `(SELECT ...)`: a derived table, whose columns are the query's.
    Subquery(Box<Select>),
}

impl TableRef {
    /// A table of the database, under its own name.
    pub fn table(name: impl Into<String>) -> Self {
        TableRef { source: TableSource::Table(name.into()), alias: None }
    }

    /// What its columns are qualified by: the alias, or else the table's name.
    pub fn qualifier(&self) -> &str {
        match (&self.alias, &self.source) {
            (Some(alias), _) => alias,
            (None, TableSource::Table(name)) => name,
            (None, TableSource::Subquery(_)) => unreachable!("a subquery in FROM has an alias"),
        }
    }
}

/// `[INNER | LEFT] JOIN table ON condition`; `CROSS JOIN` and `,` have no condition.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    pub on: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JoinKind {
    /// Pairs of rows the condition holds for.
    Inner,
    /// Those, plus each left row no right row matches, with NULLs for the right columns.
    Left,
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
        ColumnRef { table: None, name: name.into() }
    }
}

impl std::fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self.table {
            Some(table) => write!(f, "{table}.{}", self.name),
            None => f.write_str(&self.name),
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
    Case { operand: Option<Box<Expr>>, branches: Vec<(Expr, Expr)>, otherwise: Option<Box<Expr>> },
    /// A scalar function of its arguments.
    Function(Function, Vec<Expr>),
    /// A column found already among the columns of the rows the expression
    /// is evaluated on, at this position: what `sql::eval::locate` makes of
    /// a `Column` when a plan step starts, so that it is not looked up by
    /// name for every row. The parser and planner never make one. It prints
    /// as the column, so a step's columns are named as before.
    Slot(usize, ColumnRef),
}

/// The scalar functions, which take values and give one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Function {
    /// The first argument that is not NULL; the later ones are not evaluated.
    Coalesce,
    /// NULL if its two arguments are equal, else the first.
    Nullif,
    /// The number of characters in a TEXT.
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
    /// (none by default), halves away from zero, as SQLite does.
    Round,
}

impl Function {
    /// The function with this name, in any case.
    pub fn from_name(name: &str) -> Option<Function> {
        Some(match name.to_uppercase().as_str() {
            "COALESCE" => Function::Coalesce,
            "NULLIF" => Function::Nullif,
            "LENGTH" => Function::Length,
            "UPPER" => Function::Upper,
            "LOWER" => Function::Lower,
            "SUBSTR" => Function::Substr,
            "ABS" => Function::Abs,
            "ROUND" => Function::Round,
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
        }
    }

    /// The fewest and most arguments it takes.
    pub fn arity(self) -> (usize, usize) {
        match self {
            Function::Coalesce => (2, usize::MAX),
            Function::Nullif => (2, 2),
            Function::Substr => (2, 3),
            Function::Round => (1, 2),
            Function::Length | Function::Upper | Function::Lower | Function::Abs => (1, 1),
        }
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
            Expr::Aggregate(function, arg, distinct) => {
                Expr::Aggregate(*function, arg.as_deref().map(&mut sub).transpose()?, *distinct)
            }
            Expr::InList(left, items, negated) => {
                let items = items.iter().map(|e| e.replace(f)).collect::<Result<_>>()?;
                Expr::InList(Box::new(left.replace(f)?), items, *negated)
            }
            Expr::Subquery(query) => Expr::Subquery(Box::new(query.try_map(&mut |e| e.replace(f))?)),
            Expr::Exists(query) => Expr::Exists(Box::new(query.try_map(&mut |e| e.replace(f))?)),
            Expr::InSubquery(left, query, negated) => {
                let left = Box::new(left.replace(f)?);
                Expr::InSubquery(left, Box::new(query.try_map(&mut |e| e.replace(f))?), *negated)
            }
            Expr::Case { operand, branches, otherwise } => Expr::Case {
                operand: operand.as_deref().map(&mut sub).transpose()?,
                branches: branches.iter().map(|(w, t)| Ok((w.replace(f)?, t.replace(f)?))).collect::<Result<_>>()?,
                otherwise: otherwise.as_deref().map(|e| e.replace(f).map(Box::new)).transpose()?,
            },
            Expr::Function(function, args) => {
                Expr::Function(*function, args.iter().map(|e| e.replace(f)).collect::<Result<_>>()?)
            }
        })
    }

    /// Calls `f` on this expression and every one inside it, subqueries' included.
    pub fn visit(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Literal(_) | Expr::Column(_) | Expr::Outer(_) | Expr::Slot(..) | Expr::Aggregate(_, None, _) => {}
            Expr::Unary(_, inner) | Expr::IsNull(inner, _) | Expr::Aggregate(_, Some(inner), _) => inner.visit(f),
            Expr::Binary(left, _, right) => {
                left.visit(f);
                right.visit(f);
            }
            Expr::InList(left, items, _) => {
                left.visit(f);
                items.iter().for_each(|e| e.visit(f));
            }
            Expr::Subquery(query) | Expr::Exists(query) => query.exprs().into_iter().for_each(|e| e.visit(f)),
            Expr::InSubquery(left, query, _) => {
                left.visit(f);
                query.exprs().into_iter().for_each(|e| e.visit(f));
            }
            Expr::Case { .. } | Expr::Function(..) => self.parts().into_iter().for_each(|e| e.visit(f)),
        }
    }

    /// The expressions a CASE or a function call is made of, in order; none for others.
    pub fn parts(&self) -> Vec<&Expr> {
        match self {
            Expr::Case { operand, branches, otherwise } => operand
                .as_deref()
                .into_iter()
                .chain(branches.iter().flat_map(|(w, t)| [w, t]))
                .chain(otherwise.as_deref())
                .collect(),
            Expr::Function(_, args) => args.iter().collect(),
            _ => Vec::new(),
        }
    }

    /// A CASE or function call with each of its parts (in the order of
    /// [`Expr::parts`]) replaced by `f`'s result; others as they are.
    pub fn try_map_parts(&self, f: &mut dyn FnMut(&Expr) -> Result<Expr>) -> Result<Expr> {
        Ok(match self {
            Expr::Case { operand, branches, otherwise } => Expr::Case {
                operand: operand.as_deref().map(|e| f(e).map(Box::new)).transpose()?,
                branches: branches.iter().map(|(w, t)| Ok((f(w)?, f(t)?))).collect::<Result<_>>()?,
                otherwise: otherwise.as_deref().map(|e| f(e).map(Box::new)).transpose()?,
            },
            Expr::Function(function, args) => Expr::Function(*function, args.iter().map(f).collect::<Result<_>>()?),
            other => other.clone(),
        })
    }

    /// Whether the expression has a subquery in it.
    pub fn has_subquery(&self) -> bool {
        let mut found = false;
        self.visit(&mut |e| found |= matches!(e, Expr::Subquery(_) | Expr::Exists(_) | Expr::InSubquery(..)));
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
            Expr::Column(column) | Expr::Outer(column) | Expr::Slot(_, column) => write!(f, "{column}"),
            Expr::Unary(UnaryOp::Not, inner) => write!(f, "NOT {}", operand(inner)),
            Expr::Unary(UnaryOp::Neg, inner) => match operand(inner) {
                // Not `--`, which starts a comment.
                inner if inner.starts_with('-') => write!(f, "-({inner})"),
                inner => write!(f, "-{inner}"),
            },
            Expr::Binary(left, op, right) => write!(f, "{} {} {}", operand(left), op.symbol(), operand(right)),
            Expr::IsNull(inner, negated) => {
                write!(f, "{} IS {}NULL", operand(inner), if *negated { "NOT " } else { "" })
            }
            Expr::Aggregate(function, None, _) => write!(f, "{}(*)", function.name().to_uppercase()),
            Expr::Aggregate(function, Some(arg), distinct) => {
                let distinct = if *distinct { "DISTINCT " } else { "" };
                write!(f, "{}({distinct}{arg})", function.name().to_uppercase())
            }
            Expr::InList(left, items, negated) => {
                let items = items.iter().map(Expr::to_string).collect::<Vec<_>>().join(", ");
                write!(f, "{} {}IN ({items})", operand(left), if *negated { "NOT " } else { "" })
            }
            Expr::Subquery(query) => write!(f, "({query})"),
            Expr::Exists(query) => write!(f, "EXISTS ({query})"),
            Expr::InSubquery(left, query, negated) => {
                write!(f, "{} {}IN ({query})", operand(left), if *negated { "NOT " } else { "" })
            }
            Expr::Case { operand, branches, otherwise } => {
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
            Expr::Function(function, args) => {
                let args = args.iter().map(Expr::to_string).collect::<Vec<_>>().join(", ");
                write!(f, "{}({args})", function.name().to_uppercase())
            }
        }
    }
}

impl std::fmt::Display for Select {
    /// Writes the query as SQL that parses back to it.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let list = |exprs: &mut dyn Iterator<Item = String>| exprs.collect::<Vec<_>>().join(", ");
        f.write_str(if self.distinct { "SELECT DISTINCT " } else { "SELECT " })?;
        match &self.projection {
            Projection::All => f.write_str("*")?,
            Projection::Exprs(items) => {
                let items = items.iter().map(|item| match &item.alias {
                    Some(alias) => format!("{} AS {alias}", item.expr),
                    None => item.expr.to_string(),
                });
                f.write_str(&list(&mut items.into_iter()))?
            }
        }
        write!(f, " FROM {}", self.from.table)?;
        for join in &self.from.joins {
            match (join.kind, &join.on) {
                (JoinKind::Inner, None) => write!(f, " CROSS JOIN {}", join.table)?,
                (JoinKind::Inner, Some(on)) => write!(f, " JOIN {} ON {on}", join.table)?,
                (JoinKind::Left, on) => {
                    write!(f, " LEFT JOIN {}", join.table)?;
                    if let Some(on) = on {
                        write!(f, " ON {on}")?;
                    }
                }
            }
        }
        if let Some(filter) = &self.filter {
            write!(f, " WHERE {filter}")?;
        }
        if !self.group_by.is_empty() {
            write!(f, " GROUP BY {}", list(&mut self.group_by.iter().map(Expr::to_string)))?;
        }
        if let Some(having) = &self.having {
            write!(f, " HAVING {having}")?;
        }
        for (all, part) in &self.compound {
            write!(f, " UNION {}{part}", if *all { "ALL " } else { "" })?;
        }
        if !self.order_by.is_empty() {
            let keys = self.order_by.iter().map(|o| format!("{}{}", o.expr, if o.desc { " DESC" } else { "" }));
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
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self.source {
            TableSource::Table(name) => f.write_str(name)?,
            TableSource::Subquery(query) => write!(f, "({query})")?,
        }
        match &self.alias {
            Some(alias) => write!(f, " {alias}"),
            None => Ok(()),
        }
    }
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
        match parse(&format!("SELECT * FROM t WHERE {sql}")).unwrap().pop() {
            Some(Statement::Select(Select { filter: Some(filter), .. })) => filter,
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
        ] {
            let expr = where_clause(sql);
            assert_eq!(where_clause(&expr.to_string()), expr, "{sql} printed as {expr}");
        }
        assert_eq!(where_clause("a + b * c").to_string(), "a + (b * c)");
        assert_eq!(where_clause("count(*) + sum(x)").to_string(), "COUNT(*) + SUM(x)");
    }
}
