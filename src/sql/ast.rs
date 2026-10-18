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
        table: String,
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
    Column(String),
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
