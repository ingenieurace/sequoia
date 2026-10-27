//! Recursive-descent SQL parser.
//!
//! A name followed by `(` is an aggregate function call, and `(SELECT` starts
//! a subquery.
//!
//! Expression precedence, lowest first:
//! OR, AND, NOT, comparison / IS NULL / IN, + -, * /, unary -, primary.

use super::ast::{
    Aggregate, BinaryOp, ColumnRef, CreateIndex, Expr, From, Function, IndexKind, Join, JoinKind,
    OrderBy, Projection, Select, SelectItem, Statement, TableRef, TableSource, UnaryOp,
};
use super::lexer::{Token, tokenize};
use crate::datetime;
use crate::decimal::Decimal;
use crate::error::{Error, Result};
use crate::types::{Column, DataType, StorageKind, Value};

/// Parses one or more `;`-separated statements.
pub fn parse(sql: &str) -> Result<Vec<Statement>> {
    let mut parser = Parser {
        tokens: tokenize(sql)?,
        pos: 0,
    };
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
            // Not a keyword, as for EXPLAIN ANALYZE: it stays free as a name.
            Some(Token::Ident(word)) if word.eq_ignore_ascii_case("ANALYZE") => {
                self.pos += 1;
                let table = match self.peek() {
                    Some(Token::Ident(_)) => Some(self.ident()?),
                    _ => None,
                };
                return Ok(Statement::Analyze { table });
            }
            _ => String::new(),
        };
        match keyword.as_str() {
            "CREATE" => self.create(),
            "DROP" => self.drop(),
            "CHECKPOINT" => {
                self.pos += 1;
                Ok(Statement::Checkpoint)
            }
            "BEGIN" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" => self.transaction(),
            "EXPLAIN" => {
                self.pos += 1;
                // Not a keyword, so that it stays free as a name: no statement starts with it.
                let analyze = matches!(self.peek(), Some(Token::Ident(word)) if word.eq_ignore_ascii_case("ANALYZE"));
                self.pos += analyze as usize;
                Ok(Statement::Explain {
                    statement: Box::new(self.statement()?),
                    analyze,
                })
            }
            "INSERT" => self.insert(),
            "SELECT" => Ok(Statement::Select(self.select()?)),
            "UPDATE" => self.update(),
            "DELETE" => self.delete(),
            _ => Err(self.unexpected("a statement")),
        }
    }

    /// BEGIN, COMMIT, ROLLBACK [TO], SAVEPOINT and RELEASE.
    fn transaction(&mut self) -> Result<Statement> {
        let Some(Token::Keyword(keyword)) = self.peek().cloned() else {
            unreachable!("checked by statement")
        };
        self.pos += 1;
        Ok(match keyword.as_str() {
            "SAVEPOINT" => Statement::Savepoint {
                name: self.ident()?,
            },
            "RELEASE" => {
                self.eat_keyword("SAVEPOINT");
                Statement::Release {
                    name: self.ident()?,
                }
            }
            keyword => {
                self.eat_keyword("TRANSACTION");
                match keyword {
                    "BEGIN" => Statement::Begin,
                    "COMMIT" => Statement::Commit,
                    _ if self.eat_keyword("TO") => {
                        self.eat_keyword("SAVEPOINT");
                        Statement::Rollback {
                            savepoint: Some(self.ident()?),
                        }
                    }
                    _ => Statement::Rollback { savepoint: None },
                }
            }
        })
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
                other => {
                    return Err(Error(format!(
                        "unknown storage: {other}, expected HEAP, LSM or COLUMN"
                    )));
                }
            };
        }
        Ok(Statement::CreateTable {
            name,
            columns,
            storage,
        })
    }

    fn column_def(&mut self) -> Result<Column> {
        let name = self.ident()?;
        let data_type = self.data_type()?;
        let primary_key = self.eat_keyword("PRIMARY");
        if primary_key {
            self.expect_keyword("KEY")?;
        }
        Ok(Column {
            name,
            data_type,
            primary_key,
        })
    }

    /// A data type: INT, TEXT, BOOL, REAL, DATE, TIME, TIMESTAMP, BLOB or
    /// `DECIMAL[(precision[, scale])]` (or NUMERIC). The names that are not
    /// keywords stay free as names of columns.
    fn data_type(&mut self) -> Result<DataType> {
        if self.eat_keyword("INT") || self.eat_keyword("INTEGER") {
            return Ok(DataType::Int);
        } else if self.eat_keyword("TEXT") {
            return Ok(DataType::Text);
        } else if self.eat_keyword("BOOL") || self.eat_keyword("BOOLEAN") {
            return Ok(DataType::Bool);
        } else if self.eat_keyword("REAL") || self.eat_keyword("FLOAT") {
            return Ok(DataType::Real);
        }
        let Some(Token::Ident(name)) = self.peek() else {
            return Err(self.unexpected("a data type"));
        };
        let data_type = match name.to_uppercase().as_str() {
            "DATE" => DataType::Date,
            "TIME" => DataType::Time,
            "TIMESTAMP" => DataType::Timestamp,
            "BLOB" => DataType::Blob,
            "DECIMAL" | "NUMERIC" => DataType::Decimal(None),
            _ => return Err(self.unexpected("a data type")),
        };
        self.pos += 1;
        if data_type != DataType::Decimal(None) || !self.eat_symbol("(") {
            return Ok(data_type);
        }
        let precision = self.small_number()?;
        let scale = match self.eat_symbol(",") {
            true => self.small_number()?,
            false => 0,
        };
        self.expect_symbol(")")?;
        if !(1..=crate::decimal::MAX_PRECISION).contains(&precision) || scale > precision {
            return Err(Error(format!(
                "invalid DECIMAL({precision}, {scale}): the precision must be 1 to 38, and the scale at most the precision"
            )));
        }
        Ok(DataType::Decimal(Some((precision, scale))))
    }

    /// A number that fits a byte, as a DECIMAL's precision or scale.
    fn small_number(&mut self) -> Result<u8> {
        match self.peek() {
            Some(Token::Number(n)) if (0..=255).contains(n) => {
                let n = *n as u8;
                self.pos += 1;
                Ok(n)
            }
            _ => Err(self.unexpected("a number from 0 to 255")),
        }
    }

    fn create_index(&mut self) -> Result<Statement> {
        let unique = self.eat_keyword("UNIQUE");
        self.expect_keyword("INDEX")?;
        let name = self.ident()?;
        self.expect_keyword("ON")?;
        let table = self.ident()?;
        let mut using = IndexKind::default();
        if self.eat_keyword("USING") {
            // BTREE and HASH are not keywords, so they stay usable as names.
            using = match self.ident()?.to_uppercase().as_str() {
                "BTREE" => IndexKind::BTree,
                "HASH" => IndexKind::Hash,
                other => {
                    return Err(Error(format!(
                        "unknown index kind: {other}, expected BTREE or HASH"
                    )));
                }
            };
        }
        self.expect_symbol("(")?;
        let keys = self.comma_list(Self::expr)?;
        self.expect_symbol(")")?;
        // Not a keyword, so that it stays free as a name.
        let include = match self.peek() {
            Some(Token::Ident(word)) if word.eq_ignore_ascii_case("INCLUDE") => {
                self.pos += 1;
                self.expect_symbol("(")?;
                let columns = self.comma_list(Self::ident)?;
                self.expect_symbol(")")?;
                columns
            }
            _ => Vec::new(),
        };
        let predicate = if self.eat_keyword("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Statement::CreateIndex(CreateIndex {
            name,
            table,
            using,
            keys,
            include,
            predicate,
            unique,
        }))
    }

    fn drop(&mut self) -> Result<Statement> {
        self.expect_keyword("DROP")?;
        if self.eat_keyword("INDEX") {
            return Ok(Statement::DropIndex {
                name: self.ident()?,
            });
        }
        self.expect_keyword("TABLE")?;
        Ok(Statement::DropTable {
            name: self.ident()?,
        })
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
        let returning = self.returning()?;
        Ok(Statement::Insert {
            table,
            columns,
            rows,
            returning,
        })
    }

    /// `[RETURNING * | expr [AS name], ...]` at the end of an INSERT, UPDATE or DELETE.
    fn returning(&mut self) -> Result<Option<Projection>> {
        // Not a keyword, so that it stays free as a name.
        match self.peek() {
            Some(Token::Ident(word)) if word.eq_ignore_ascii_case("RETURNING") => {
                self.pos += 1;
                Ok(Some(self.projection()?))
            }
            _ => Ok(None),
        }
    }

    /// `* | expr [AS name], ...`: a select list, or a RETURNING one.
    fn projection(&mut self) -> Result<Projection> {
        if self.eat_symbol("*") {
            return Ok(Projection::All);
        }
        Ok(Projection::Exprs(self.comma_list(|p| {
            let expr = p.expr()?;
            let alias = match p.eat_keyword("AS") {
                true => Some(p.ident()?),
                false => None,
            };
            Ok(SelectItem { expr, alias })
        })?))
    }

    /// A query: one SELECT, or several joined by `UNION [ALL]`, then the
    /// ORDER BY, LIMIT and OFFSET of all of them.
    fn select(&mut self) -> Result<Select> {
        let mut query = self.select_core()?;
        while self.eat_keyword("UNION") {
            let all = self.eat_keyword("ALL");
            query.compound.push((all, self.select_core()?));
        }
        if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            query.order_by = self.comma_list(|p| {
                let expr = p.expr()?;
                let desc = p.eat_keyword("DESC");
                if !desc {
                    p.eat_keyword("ASC");
                }
                Ok(OrderBy { expr, desc })
            })?;
        }
        if self.eat_keyword("LIMIT") {
            query.limit = Some(self.number()? as usize);
        }
        if self.eat_keyword("OFFSET") {
            query.offset = Some(self.number()? as usize);
        }
        Ok(query)
    }

    /// `SELECT ... [HAVING ...]`: a query up to its ORDER BY.
    fn select_core(&mut self) -> Result<Select> {
        self.expect_keyword("SELECT")?;
        let distinct = self.eat_keyword("DISTINCT");
        let projection = self.projection()?;
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
        let (order_by, limit, offset, compound) = (Vec::new(), None, None, Vec::new());
        Ok(Select {
            distinct,
            from,
            projection,
            filter,
            group_by,
            having,
            order_by,
            limit,
            offset,
            compound,
        })
    }

    /// `table [, table | [INNER | LEFT [OUTER] | CROSS] JOIN table [ON expr]]...`
    fn from(&mut self) -> Result<From> {
        let table = self.table_ref()?;
        let mut joins = Vec::new();
        loop {
            let kind = if self.eat_symbol(",") {
                joins.push(Join {
                    kind: JoinKind::Inner,
                    table: self.table_ref()?,
                    on: None,
                });
                continue;
            } else if self.eat_keyword("CROSS") {
                self.expect_keyword("JOIN")?;
                joins.push(Join {
                    kind: JoinKind::Inner,
                    table: self.table_ref()?,
                    on: None,
                });
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
            joins.push(Join {
                kind,
                table,
                on: Some(self.expr()?),
            });
        }
    }

    /// `name [[AS] alias]`, or `(SELECT ...) [AS] alias`
    fn table_ref(&mut self) -> Result<TableRef> {
        let source = match self.eat_symbol("(") {
            true => {
                let query = self.select()?;
                self.expect_symbol(")")?;
                TableSource::Subquery(Box::new(query))
            }
            false => TableSource::Table(self.ident()?),
        };
        let alias = match (self.eat_keyword("AS"), self.peek()) {
            (true, _) | (false, Some(Token::Ident(_))) => Some(self.ident()?),
            _ if matches!(source, TableSource::Subquery(_)) => {
                return Err(Error("a subquery in FROM must have an alias".into()));
            }
            _ => None,
        };
        Ok(TableRef { source, alias })
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
        let returning = self.returning()?;
        Ok(Statement::Update {
            table,
            assignments,
            filter,
            returning,
        })
    }

    fn delete(&mut self) -> Result<Statement> {
        self.expect_keyword("DELETE")?;
        self.expect_keyword("FROM")?;
        let table = self.ident()?;
        let filter = self.where_clause()?;
        let returning = self.returning()?;
        Ok(Statement::Delete {
            table,
            filter,
            returning,
        })
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
            && matches!(self.tokens.get(self.pos + 1), Some(Token::Keyword(k)) if ["IN", "BETWEEN", "LIKE"].contains(&k.as_str()));
        if negated {
            self.pos += 1;
        }
        let not = |expr: Expr| {
            if negated {
                Expr::Unary(UnaryOp::Not, Box::new(expr))
            } else {
                expr
            }
        };
        if self.eat_keyword("BETWEEN") {
            // `a BETWEEN x AND y` is `a >= x AND a <= y`, which the planner can push down.
            let low = self.additive()?;
            self.expect_keyword("AND")?;
            let high = self.additive()?;
            return Ok(not(binary(
                binary(left.clone(), BinaryOp::GtEq, low),
                BinaryOp::And,
                binary(left, BinaryOp::LtEq, high),
            )));
        }
        if self.eat_keyword("LIKE") {
            return Ok(not(binary(left, BinaryOp::Like, self.additive()?)));
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
        let mut left = self.concat()?;
        loop {
            let op = match self.peek() {
                Some(Token::Symbol("*")) => BinaryOp::Mul,
                Some(Token::Symbol("/")) => BinaryOp::Div,
                _ => return Ok(left),
            };
            self.pos += 1;
            left = binary(left, op, self.concat()?);
        }
    }

    /// `||` binds tighter than any other operator but a sign, as in SQLite.
    fn concat(&mut self) -> Result<Expr> {
        let mut left = self.unary()?;
        while self.eat_symbol("||") {
            left = binary(left, BinaryOp::Concat, self.unary()?);
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat_symbol("-") {
            return Ok(Expr::Unary(UnaryOp::Neg, Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr> {
        if self.eat_keyword("CASE") {
            return self.case();
        }
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
        // `DATE '2024-01-31'` and the like: a literal of a type written as text.
        if let (Some(Token::Ident(name)), Some(Token::Str(text))) =
            (self.peek(), self.tokens.get(self.pos + 1))
        {
            let value = match name.to_uppercase().as_str() {
                "DATE" => Some(datetime::parse_date(text).map(Value::Date)),
                "TIME" => Some(datetime::parse_time(text).map(Value::Time)),
                "TIMESTAMP" => Some(datetime::parse_timestamp(text).map(Value::Timestamp)),
                "DECIMAL" | "NUMERIC" => Some(Decimal::parse(text).map(Value::Decimal)),
                _ => None,
            };
            if let Some(value) = value {
                self.pos += 2;
                return Ok(Expr::Literal(value?));
            }
        }
        let expr = match self.peek() {
            Some(Token::Number(n)) => Expr::Literal(Value::Int(*n)),
            Some(Token::Real(r)) => Expr::Literal(Value::Real(*r)),
            Some(Token::Str(s)) => Expr::Literal(Value::Text(s.clone())),
            Some(Token::Blob(b)) => Expr::Literal(Value::Blob(b.clone())),
            Some(Token::Ident(name))
                if self.tokens.get(self.pos + 1) == Some(&Token::Symbol("(")) =>
            {
                let name = name.clone();
                self.pos += 2;
                return self.aggregate(&name);
            }
            Some(Token::Ident(table))
                if self.tokens.get(self.pos + 1) == Some(&Token::Symbol(".")) =>
            {
                let table = table.clone();
                self.pos += 2;
                return Ok(Expr::Column(ColumnRef {
                    table: Some(table),
                    name: self.ident()?,
                }));
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

    /// The rest of `CASE ... END` after `CASE`.
    fn case(&mut self) -> Result<Expr> {
        let operand = match self.peek() == Some(&Token::Keyword("WHEN".into())) {
            true => None,
            false => Some(Box::new(self.expr()?)),
        };
        let mut branches = Vec::new();
        while self.eat_keyword("WHEN") {
            let when = self.expr()?;
            self.expect_keyword("THEN")?;
            branches.push((when, self.expr()?));
        }
        if branches.is_empty() {
            return Err(self.unexpected("WHEN"));
        }
        let otherwise = match self.eat_keyword("ELSE") {
            true => Some(Box::new(self.expr()?)),
            false => None,
        };
        self.expect_keyword("END")?;
        Ok(Expr::Case {
            operand,
            branches,
            otherwise,
        })
    }

    /// The rest of `name(...)` after its opening parenthesis: an aggregate or a scalar function.
    fn aggregate(&mut self, name: &str) -> Result<Expr> {
        // `CAST(value AS type)` and `EXTRACT(field FROM value)`, which take
        // more than values.
        match name.to_uppercase().as_str() {
            "CAST" => {
                let value = self.expr()?;
                self.expect_keyword("AS")?;
                let data_type = self.data_type()?;
                self.expect_symbol(")")?;
                return Ok(Expr::Function(Function::Cast(data_type), vec![value]));
            }
            "EXTRACT" => {
                let field = match self.peek() {
                    Some(Token::Ident(name)) => datetime::Field::from_name(name),
                    _ => None,
                };
                let Some(field) = field else {
                    return Err(self.unexpected("YEAR, MONTH, DAY, HOUR, MINUTE or SECOND"));
                };
                self.pos += 1;
                self.expect_keyword("FROM")?;
                let value = self.expr()?;
                self.expect_symbol(")")?;
                return Ok(Expr::Function(Function::Extract(field), vec![value]));
            }
            _ => {}
        }
        let Some(function) = Aggregate::from_name(name) else {
            let Some(function) = Function::from_name(name) else {
                return Err(Error(format!("unknown function: {name}")));
            };
            let args = match self.peek() == Some(&Token::Symbol(")")) {
                true => Vec::new(),
                false => self.comma_list(Self::expr)?,
            };
            self.expect_symbol(")")?;
            let (fewest, most) = function.arity();
            if args.len() < fewest || args.len() > most {
                let name = function.name().to_uppercase();
                return Err(Error(format!(
                    "wrong number of arguments to {name}: {}",
                    args.len()
                )));
            }
            return Ok(Expr::Function(function, args));
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
