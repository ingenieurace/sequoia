//! Recursive-descent SQL parser.
//!
//! A name followed by `(` is an aggregate function call, and `(SELECT` starts
//! a subquery.
//!
//! Expression precedence, lowest first:
//! OR, AND, NOT, comparison / IS NULL / IN, + -, * /, unary -, primary.

use super::ast::{
    Aggregate, BinaryOp, ColumnRef, Expr, From, Join, JoinKind, OrderBy, Projection, Select, Statement, TableRef,
    UnaryOp,
};
use crate::error::{Error, Result};
use super::lexer::{Token, tokenize};
use crate::types::{Column, DataType, StorageKind, Value};

/// Parses one or more `;`-separated statements.
pub fn parse(sql: &str) -> Result<Vec<Statement>> {
    let mut parser = Parser { tokens: tokenize(sql)?, pos: 0 };
    let mut statements = Vec::new();
    loop {
        while parser.eat_symbol(";") {}
        if parser.peek().is_none() {
            return Ok(statements);
        }
        statements.push(parser.statement()?);
        if parser.peek().is_some() {
            parser.expect_symbol(";")?;
        }
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    // ---- statements ----

    fn statement(&mut self) -> Result<Statement> {
        let keyword = match self.peek() {
            Some(Token::Keyword(k)) => k.clone(),
            _ => String::new(),
        };
        match keyword.as_str() {
            "CREATE" => self.create(),
            "DROP" => self.drop(),
            "CHECKPOINT" => {
                self.pos += 1;
                Ok(Statement::Checkpoint)
            }
            "EXPLAIN" => {
                self.pos += 1;
                Ok(Statement::Explain(Box::new(self.statement()?)))
            }
            "INSERT" => self.insert(),
            "SELECT" => Ok(Statement::Select(self.select()?)),
            "UPDATE" => self.update(),
            "DELETE" => self.delete(),
            _ => Err(self.unexpected("a statement")),
        }
    }

    fn create(&mut self) -> Result<Statement> {
        self.expect_keyword("CREATE")?;
        match self.peek() {
            Some(Token::Keyword(k)) if k == "INDEX" || k == "UNIQUE" => self.create_index(),
            _ => self.create_table(),
        }
    }

    fn create_table(&mut self) -> Result<Statement> {
        self.expect_keyword("TABLE")?;
        let name = self.ident()?;
        self.expect_symbol("(")?;
        let columns = self.comma_list(Self::column_def)?;
        self.expect_symbol(")")?;
        let mut storage = StorageKind::default();
        if self.eat_keyword("USING") {
            // HEAP, LSM and COLUMN are not keywords, so they stay usable as names.
            storage = match self.ident()?.to_uppercase().as_str() {
                "HEAP" => StorageKind::Heap,
                "LSM" => StorageKind::Lsm,
                "COLUMN" => StorageKind::Column,
                other => return Err(Error(format!("unknown storage: {other}, expected HEAP, LSM or COLUMN"))),
            };
        }
        Ok(Statement::CreateTable { name, columns, storage })
    }

    fn column_def(&mut self) -> Result<Column> {
        let name = self.ident()?;
        let data_type = if self.eat_keyword("INT") || self.eat_keyword("INTEGER") {
            DataType::Int
        } else if self.eat_keyword("TEXT") {
            DataType::Text
        } else if self.eat_keyword("BOOL") || self.eat_keyword("BOOLEAN") {
            DataType::Bool
        } else {
            return Err(self.unexpected("a data type"));
        };
        let primary_key = self.eat_keyword("PRIMARY");
        if primary_key {
            self.expect_keyword("KEY")?;
        }
        Ok(Column { name, data_type, primary_key })
    }

    fn create_index(&mut self) -> Result<Statement> {
        let unique = self.eat_keyword("UNIQUE");
        self.expect_keyword("INDEX")?;
        let name = self.ident()?;
        self.expect_keyword("ON")?;
        let table = self.ident()?;
        self.expect_symbol("(")?;
        let column = self.ident()?;
        self.expect_symbol(")")?;
        Ok(Statement::CreateIndex { name, table, column, unique })
    }

    fn drop(&mut self) -> Result<Statement> {
        self.expect_keyword("DROP")?;
        if self.eat_keyword("INDEX") {
            return Ok(Statement::DropIndex { name: self.ident()? });
        }
        self.expect_keyword("TABLE")?;
        Ok(Statement::DropTable { name: self.ident()? })
    }

    fn insert(&mut self) -> Result<Statement> {
        self.expect_keyword("INSERT")?;
        self.expect_keyword("INTO")?;
        let table = self.ident()?;
        let columns = if self.eat_symbol("(") {
            let columns = self.comma_list(Self::ident)?;
            self.expect_symbol(")")?;
            Some(columns)
        } else {
            None
        };
        self.expect_keyword("VALUES")?;
        let rows = self.comma_list(|p| {
            p.expect_symbol("(")?;
            let values = p.comma_list(Self::expr)?;
            p.expect_symbol(")")?;
            Ok(values)
        })?;
        Ok(Statement::Insert { table, columns, rows })
    }

    fn select(&mut self) -> Result<Select> {
        self.expect_keyword("SELECT")?;
        let distinct = self.eat_keyword("DISTINCT");
        let projection = if self.eat_symbol("*") {
            Projection::All
        } else {
            Projection::Exprs(self.comma_list(Self::expr)?)
        };
        self.expect_keyword("FROM")?;
        let from = self.from()?;
        let filter = self.where_clause()?;

        let mut group_by = Vec::new();
        if self.eat_keyword("GROUP") {
            self.expect_keyword("BY")?;
            group_by = self.comma_list(Self::expr)?;
        }
        let having = match self.eat_keyword("HAVING") {
            true => Some(self.expr()?),
            false => None,
        };

        let mut order_by = Vec::new();
        if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            order_by = self.comma_list(|p| {
                let expr = p.expr()?;
                let desc = p.eat_keyword("DESC");
                if !desc {
                    p.eat_keyword("ASC");
                }
                Ok(OrderBy { expr, desc })
            })?;
        }

        let limit = match self.eat_keyword("LIMIT") {
            true => Some(self.number()? as usize),
            false => None,
        };
        Ok(Select { distinct, from, projection, filter, group_by, having, order_by, limit })
    }

    /// `table [, table | [INNER | LEFT [OUTER] | CROSS] JOIN table [ON expr]]...`
    fn from(&mut self) -> Result<From> {
        let table = self.table_ref()?;
        let mut joins = Vec::new();
        loop {
            let kind = if self.eat_symbol(",") {
                joins.push(Join { kind: JoinKind::Inner, table: self.table_ref()?, on: None });
                continue;
            } else if self.eat_keyword("CROSS") {
                self.expect_keyword("JOIN")?;
                joins.push(Join { kind: JoinKind::Inner, table: self.table_ref()?, on: None });
                continue;
            } else if self.eat_keyword("LEFT") {
                self.eat_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                JoinKind::Left
            } else if self.eat_keyword("INNER") {
                self.expect_keyword("JOIN")?;
                JoinKind::Inner
            } else if self.eat_keyword("JOIN") {
                JoinKind::Inner
            } else {
                return Ok(From { table, joins });
            };
            let table = self.table_ref()?;
            self.expect_keyword("ON")?;
            joins.push(Join { kind, table, on: Some(self.expr()?) });
        }
    }

    /// `name [[AS] alias]`
    fn table_ref(&mut self) -> Result<TableRef> {
        let name = self.ident()?;
        let alias = match (self.eat_keyword("AS"), self.peek()) {
            (true, _) | (false, Some(Token::Ident(_))) => Some(self.ident()?),
            _ => None,
        };
        Ok(TableRef { name, alias })
    }

    fn update(&mut self) -> Result<Statement> {
        self.expect_keyword("UPDATE")?;
        let table = self.ident()?;
        self.expect_keyword("SET")?;
        let assignments = self.comma_list(|p| {
            let column = p.ident()?;
            p.expect_symbol("=")?;
            Ok((column, p.expr()?))
        })?;
        let filter = self.where_clause()?;
        Ok(Statement::Update { table, assignments, filter })
    }

    fn delete(&mut self) -> Result<Statement> {
        self.expect_keyword("DELETE")?;
        self.expect_keyword("FROM")?;
        let table = self.ident()?;
        let filter = self.where_clause()?;
        Ok(Statement::Delete { table, filter })
    }

    fn where_clause(&mut self) -> Result<Option<Expr>> {
        match self.eat_keyword("WHERE") {
            true => Ok(Some(self.expr()?)),
            false => Ok(None),
        }
    }

    // ---- expressions ----

    fn expr(&mut self) -> Result<Expr> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr> {
        let mut left = self.and()?;
        while self.eat_keyword("OR") {
            left = binary(left, BinaryOp::Or, self.and()?);
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut left = self.not()?;
        while self.eat_keyword("AND") {
            left = binary(left, BinaryOp::And, self.not()?);
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr> {
        if self.eat_keyword("NOT") {
            return Ok(Expr::Unary(UnaryOp::Not, Box::new(self.not()?)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr> {
        let left = self.additive()?;
        if self.eat_keyword("IS") {
            let negated = self.eat_keyword("NOT");
            self.expect_keyword("NULL")?;
            return Ok(Expr::IsNull(Box::new(left), negated));
        }
        let negated = self.peek() == Some(&Token::Keyword("NOT".into()))
            && self.tokens.get(self.pos + 1) == Some(&Token::Keyword("IN".into()));
        if negated {
            self.pos += 1;
        }
        if self.eat_keyword("IN") {
            self.expect_symbol("(")?;
            let expr = if self.peek() == Some(&Token::Keyword("SELECT".into())) {
                Expr::InSubquery(Box::new(left), Box::new(self.select()?), negated)
            } else {
                Expr::InList(Box::new(left), self.comma_list(Self::expr)?, negated)
            };
            self.expect_symbol(")")?;
            return Ok(expr);
        }
        let op = match self.peek() {
            Some(Token::Symbol("=")) => BinaryOp::Eq,
            Some(Token::Symbol("!=")) => BinaryOp::NotEq,
            Some(Token::Symbol("<")) => BinaryOp::Lt,
            Some(Token::Symbol("<=")) => BinaryOp::LtEq,
            Some(Token::Symbol(">")) => BinaryOp::Gt,
            Some(Token::Symbol(">=")) => BinaryOp::GtEq,
            _ => return Ok(left),
        };
        self.pos += 1;
        Ok(binary(left, op, self.additive()?))
    }

    fn additive(&mut self) -> Result<Expr> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Symbol("+")) => BinaryOp::Add,
                Some(Token::Symbol("-")) => BinaryOp::Sub,
                _ => return Ok(left),
            };
            self.pos += 1;
            left = binary(left, op, self.multiplicative()?);
        }
    }

    fn multiplicative(&mut self) -> Result<Expr> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Token::Symbol("*")) => BinaryOp::Mul,
                Some(Token::Symbol("/")) => BinaryOp::Div,
                _ => return Ok(left),
            };
            self.pos += 1;
            left = binary(left, op, self.unary()?);
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat_symbol("-") {
            return Ok(Expr::Unary(UnaryOp::Neg, Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr> {
        if self.eat_keyword("EXISTS") {
            self.expect_symbol("(")?;
            let query = self.select()?;
            self.expect_symbol(")")?;
            return Ok(Expr::Exists(Box::new(query)));
        }
        if self.eat_symbol("(") {
            if self.peek() == Some(&Token::Keyword("SELECT".into())) {
                let query = self.select()?;
                self.expect_symbol(")")?;
                return Ok(Expr::Subquery(Box::new(query)));
            }
            let expr = self.expr()?;
            self.expect_symbol(")")?;
            return Ok(expr);
        }
        let expr = match self.peek() {
            Some(Token::Number(n)) => Expr::Literal(Value::Int(*n)),
            Some(Token::Str(s)) => Expr::Literal(Value::Text(s.clone())),
            Some(Token::Ident(name)) if self.tokens.get(self.pos + 1) == Some(&Token::Symbol("(")) => {
                let name = name.clone();
                self.pos += 2;
                return self.aggregate(&name);
            }
            Some(Token::Ident(table)) if self.tokens.get(self.pos + 1) == Some(&Token::Symbol(".")) => {
                let table = table.clone();
                self.pos += 2;
                return Ok(Expr::Column(ColumnRef { table: Some(table), name: self.ident()? }));
            }
            Some(Token::Ident(name)) => Expr::Column(ColumnRef::bare(name.clone())),
            Some(Token::Keyword(k)) if k == "NULL" => Expr::Literal(Value::Null),
            Some(Token::Keyword(k)) if k == "TRUE" => Expr::Literal(Value::Bool(true)),
            Some(Token::Keyword(k)) if k == "FALSE" => Expr::Literal(Value::Bool(false)),
            _ => return Err(self.unexpected("an expression")),
        };
        self.pos += 1;
        Ok(expr)
    }

    /// The rest of `name(...)` after its opening parenthesis.
    fn aggregate(&mut self, name: &str) -> Result<Expr> {
        let Some(function) = Aggregate::from_name(name) else {
            return Err(Error(format!("unknown function: {name}")));
        };
        let distinct = self.eat_keyword("DISTINCT");
        let arg = if function == Aggregate::Count && !distinct && self.eat_symbol("*") {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        self.expect_symbol(")")?;
        Ok(Expr::Aggregate(function, arg, distinct))
    }

    // ---- token helpers ----

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn eat(&mut self, expected: &Token) -> bool {
        let matched = self.peek() == Some(expected);
        if matched {
            self.pos += 1;
        }
        matched
    }

    fn eat_keyword(&mut self, keyword: &str) -> bool {
        self.eat(&Token::Keyword(keyword.into()))
    }

    fn eat_symbol(&mut self, symbol: &'static str) -> bool {
        self.eat(&Token::Symbol(symbol))
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<()> {
        match self.eat_keyword(keyword) {
            true => Ok(()),
            false => Err(self.unexpected(keyword)),
        }
    }

    fn expect_symbol(&mut self, symbol: &'static str) -> Result<()> {
        match self.eat_symbol(symbol) {
            true => Ok(()),
            false => Err(self.unexpected(&format!("'{symbol}'"))),
        }
    }

    fn ident(&mut self) -> Result<String> {
        match self.peek() {
            Some(Token::Ident(name)) => {
                let name = name.clone();
                self.pos += 1;
                Ok(name)
            }
            _ => Err(self.unexpected("an identifier")),
        }
    }

    fn number(&mut self) -> Result<i64> {
        match self.peek() {
            Some(Token::Number(n)) => {
                let n = *n;
                self.pos += 1;
                Ok(n)
            }
            _ => Err(self.unexpected("a number")),
        }
    }

    /// Parses `item (, item)*`.
    fn comma_list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let mut items = vec![item(self)?];
        while self.eat_symbol(",") {
            items.push(item(self)?);
        }
        Ok(items)
    }

    fn unexpected(&self, expected: &str) -> Error {
        match self.peek() {
            Some(token) => Error(format!("expected {expected}, found {token}")),
            None => Error(format!("expected {expected}, found end of input")),
        }
    }
}

fn binary(left: Expr, op: BinaryOp, right: Expr) -> Expr {
    Expr::Binary(Box::new(left), op, Box::new(right))
}
