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
    /// `CREATE [UNIQUE] INDEX name ON table (column)`
    CreateIndex {
        name: String,
        table: String,
        column: String,
        unique: bool,
    },
    DropIndex {
        name: String,
    },
    /// `EXPLAIN statement`: describes how the statement would read its table.
    Explain(Box<Statement>),
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
    },
    Select(Select),
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
    },
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
}

impl Select {
    /// Every expression of the query, not looking inside its subqueries.
    pub fn exprs(&self) -> impl Iterator<Item = &Expr> {
        let projection = match &self.projection {
            Projection::All => &[][..],
            Projection::Exprs(exprs) => exprs,
        };
        projection
            .iter()
            .chain(self.from.joins.iter().filter_map(|j| j.on.as_ref()))
            .chain(&self.filter)
            .chain(&self.group_by)
            .chain(&self.having)
            .chain(self.order_by.iter().map(|o| &o.expr))
    }

    /// The query with each of its expressions (those `exprs` gives) replaced by `f`'s result.
    pub fn try_map(&self, f: &mut dyn FnMut(&Expr) -> Result<Expr>) -> Result<Select> {
        let mut query = self.clone();
        if let Projection::Exprs(exprs) = &mut query.projection {
            for e in exprs {
                *e = f(e)?;
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
}

/// A table in FROM, and the name it goes by there.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
}

impl TableRef {
    /// What its columns are qualified by: the alias, or else the table's name.
    pub fn qualifier(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Projection {
    /// `SELECT *`
    All,
    Exprs(Vec<Expr>),
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
            Expr::Literal(_) | Expr::Column(_) | Expr::Outer(_) => self.clone(),
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
        })
    }

    /// Calls `f` on this expression and every one inside it, subqueries' included.
    pub fn visit(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Literal(_) | Expr::Column(_) | Expr::Outer(_) | Expr::Aggregate(_, None, _) => {}
            Expr::Unary(_, inner) | Expr::IsNull(inner, _) | Expr::Aggregate(_, Some(inner), _) => inner.visit(f),
            Expr::Binary(left, _, right) => {
                left.visit(f);
                right.visit(f);
            }
            Expr::InList(left, items, _) => {
                left.visit(f);
                items.iter().for_each(|e| e.visit(f));
            }
            Expr::Subquery(query) | Expr::Exists(query) => query.exprs().for_each(|e| e.visit(f)),
            Expr::InSubquery(left, query, _) => {
                left.visit(f);
                query.exprs().for_each(|e| e.visit(f));
            }
        }
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
            Expr::Column(column) | Expr::Outer(column) => write!(f, "{column}"),
            Expr::Unary(UnaryOp::Not, inner) => write!(f, "NOT {}", operand(inner)),
            Expr::Unary(UnaryOp::Neg, inner) => write!(f, "-{}", operand(inner)),
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
            Projection::Exprs(exprs) => f.write_str(&list(&mut exprs.iter().map(Expr::to_string)))?,
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
        if !self.order_by.is_empty() {
            let keys = self.order_by.iter().map(|o| format!("{}{}", o.expr, if o.desc { " DESC" } else { "" }));
            write!(f, " ORDER BY {}", list(&mut keys.into_iter()))?;
        }
        if let Some(limit) = self.limit {
            write!(f, " LIMIT {limit}")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for TableRef {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self.alias {
            Some(alias) => write!(f, "{} {alias}", self.name),
            None => f.write_str(&self.name),
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
            "-(a - b) IS NULL",
            "NOT (NOT a)",
            "a = NULL OR b = TRUE",
        ] {
            let expr = where_clause(sql);
            assert_eq!(where_clause(&expr.to_string()), expr, "{sql} printed as {expr}");
        }
        assert_eq!(where_clause("a + b * c").to_string(), "a + (b * c)");
        assert_eq!(where_clause("count(*) + sum(x)").to_string(), "COUNT(*) + SUM(x)");
    }
}
