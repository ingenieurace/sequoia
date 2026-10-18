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
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    },
    Select {
        from: From,
        projection: Projection,
        filter: Option<Expr>,
        /// `GROUP BY`: empty for none.
        group_by: Vec<Expr>,
        having: Option<Expr>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
    },
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

/// `FROM t1 [JOIN t2 ON ...]...`: a table, then the tables joined to it in order.
#[derive(Debug, Clone, PartialEq)]
pub struct From {
    pub table: TableRef,
    pub joins: Vec<Join>,
}

/// A table in FROM, and the name it goes by there.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    pub on: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    /// `SELECT *`
    All,
    Exprs(Vec<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub expr: Expr,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Column(ColumnRef),
    Unary(UnaryOp, Box<Expr>),
    Binary(Box<Expr>, BinaryOp, Box<Expr>),
    /// `expr IS NULL`, or `expr IS NOT NULL` when the flag is true.
    IsNull(Box<Expr>, bool),
    /// An aggregate function over a group of rows; no argument is `COUNT(*)`.
    Aggregate(Aggregate, Option<Box<Expr>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnaryOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, Copy, PartialEq)]
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
            Expr::Binary(..) | Expr::IsNull(..) | Expr::Unary(UnaryOp::Not, _) => format!("({e})"),
            e => e.to_string(),
        };
        match self {
            Expr::Literal(value) => f.write_str(&value.to_sql()),
            Expr::Column(column) => write!(f, "{column}"),
            Expr::Unary(UnaryOp::Not, inner) => write!(f, "NOT {}", operand(inner)),
            Expr::Unary(UnaryOp::Neg, inner) => write!(f, "-{}", operand(inner)),
            Expr::Binary(left, op, right) => write!(f, "{} {} {}", operand(left), op.symbol(), operand(right)),
            Expr::IsNull(inner, negated) => {
                write!(f, "{} IS {}NULL", operand(inner), if *negated { "NOT " } else { "" })
            }
            Expr::Aggregate(function, None) => write!(f, "{}(*)", function.name().to_uppercase()),
            Expr::Aggregate(function, Some(arg)) => write!(f, "{}({arg})", function.name().to_uppercase()),
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
    use crate::sql::ast::{Expr, Statement};
    use crate::sql::parse;

    fn where_clause(sql: &str) -> Expr {
        match parse(&format!("SELECT * FROM t WHERE {sql}")).unwrap().pop() {
            Some(Statement::Select { filter: Some(filter), .. }) => filter,
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
