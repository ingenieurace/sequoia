//! Recursive-descent SQL parser.
//!
//! A name followed by `(` is an aggregate function call, and `(SELECT` (or
//! `(WITH`) starts a subquery.
//!
//! Expression precedence, lowest first:
//! OR, AND, NOT, comparison / IS NULL / IN, + -, * /, unary -, primary.
//! Of the set operations, INTERSECT binds tighter than UNION and EXCEPT,
//! which go left to right.
//!
//! **Common table expressions.** `WITH name AS (query)` names a query for
//! the statement after it, and the parser puts the query in wherever FROM
//! names it, as a derived table: a common table expression means no more
//! than that, so nothing after the parser sees one. Names are found in the
//! innermost WITH that has them first, then in the database. `WITH
//! RECURSIVE` lets an expression's query name the expression itself; if it
//! does, the expression becomes a [`RecursiveCte`] wherever it is named,
//! and its own name inside it a [`TableSource::WorkTable`].

use super::ast::{
    Aggregate, BinaryOp, ColumnRef, Compound, CreateIndex, Expr, Frame, FrameBound, FrameUnits,
    From, Function, IndexKind, InsertSource, Join, JoinKind, OrderBy, Projection, RecursiveCte,
    Select, SelectItem, SetOp, Statement, TableRef, TableSource, UnaryOp, Window, WindowFunction,
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
        ctes: Vec::new(),
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
    /// The common table expressions of the WITHs around the query being
    /// parsed, innermost last.
    ctes: Vec<Cte>,
}

/// A common table expression, and what its name stands for.
struct Cte {
    name: String,
    kind: CteKind,
}

enum CteKind {
    /// A query, read as a derived table, its columns renamed by `columns`.
    Derived {
        query: Box<Select>,
        columns: Vec<String>,
    },
    Recursive(Box<RecursiveCte>),
    /// A recursive expression's own name, in its query being parsed:
    /// `refs` counts how often it is named there.
    Work { columns: Vec<String>, refs: usize },
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
            "SELECT" | "WITH" => Ok(Statement::Select(Box::new(self.select()?))),
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
        let source = if self.query_follows() {
            InsertSource::Select(Box::new(self.select()?))
        } else {
            self.expect_keyword("VALUES")?;
            InsertSource::Values(self.comma_list(|p| {
                p.expect_symbol("(")?;
                let values = p.comma_list(Self::expr)?;
                p.expect_symbol(")")?;
                Ok(values)
            })?)
        };
        let returning = self.returning()?;
        Ok(Statement::Insert {
            table,
            columns,
            source,
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

    /// A query: `[WITH ...]` one SELECT, or several combined by set
    /// operations, then the ORDER BY, LIMIT and OFFSET of all of them.
    fn select(&mut self) -> Result<Select> {
        let scope = self.ctes.len();
        let query = match self.eat_keyword("WITH") {
            true => self.with().and_then(|()| self.set_operations()),
            false => self.set_operations(),
        };
        self.ctes.truncate(scope);
        query
    }

    /// Whether a query starts here: `SELECT` or `WITH`.
    fn query_follows(&self) -> bool {
        matches!(self.peek(), Some(Token::Keyword(k)) if k == "SELECT" || k == "WITH")
    }

    /// `intersection [{UNION | EXCEPT} [ALL] intersection]... [ORDER BY ...] [LIMIT n] [OFFSET m]`
    fn set_operations(&mut self) -> Result<Select> {
        let mut query = self.intersection()?;
        loop {
            let op = if self.eat_keyword("UNION") {
                SetOp::Union
            } else if self.eat_keyword("EXCEPT") {
                SetOp::Except
            } else {
                break;
            };
            let all = self.eat_keyword("ALL");
            let part = self.intersection()?;
            query.compound.push(Compound {
                op,
                all,
                query: part,
            });
        }
        if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            query.order_by = self.order_list()?;
        }
        if self.eat_keyword("LIMIT") {
            query.limit = Some(self.number()? as usize);
        }
        if self.eat_keyword("OFFSET") {
            query.offset = Some(self.number()? as usize);
        }
        Ok(query)
    }

    /// `select_core [INTERSECT [ALL] select_core]...`
    fn intersection(&mut self) -> Result<Select> {
        let mut query = self.select_core()?;
        while self.eat_keyword("INTERSECT") {
            let all = self.eat_keyword("ALL");
            let part = self.select_core()?;
            query.compound.push(Compound {
                op: SetOp::Intersect,
                all,
                query: part,
            });
        }
        Ok(query)
    }

    /// `expr [ASC | DESC], ...`
    fn order_list(&mut self) -> Result<Vec<OrderBy>> {
        self.comma_list(|p| {
            let expr = p.expr()?;
            let desc = p.eat_keyword("DESC");
            if !desc {
                p.eat_keyword("ASC");
            }
            Ok(OrderBy { expr, desc })
        })
    }

    /// The rest of `WITH [RECURSIVE] name [(column, ...)] AS (query), ...`
    /// after WITH: each expression is in scope for those after it, and for
    /// the query after them all, until [`Parser::select`] ends the scope.
    fn with(&mut self) -> Result<()> {
        // Not a keyword, so that it stays free as a name.
        let recursive = self.eat_word("RECURSIVE");
        let scope = self.ctes.len();
        loop {
            let name = self.ident()?;
            let columns = self.column_list()?;
            self.expect_keyword("AS")?;
            self.expect_symbol("(")?;
            let kind = if recursive {
                self.ctes.push(Cte {
                    name: name.clone(),
                    kind: CteKind::Work {
                        columns: columns.clone(),
                        refs: 0,
                    },
                });
                let body = self.select();
                let Some(Cte {
                    kind: CteKind::Work { refs, .. },
                    ..
                }) = self.ctes.pop()
                else {
                    unreachable!("pushed above, below the scope of the query")
                };
                match refs {
                    0 => CteKind::Derived {
                        query: Box::new(body?),
                        columns,
                    },
                    _ => CteKind::Recursive(Box::new(recursive_cte(&name, columns, body?)?)),
                }
            } else {
                CteKind::Derived {
                    query: Box::new(self.select()?),
                    columns,
                }
            };
            self.expect_symbol(")")?;
            if self.ctes[scope..].iter().any(|cte| cte.name == name) {
                return Err(Error(format!(
                    "WITH query name {name} is given more than once"
                )));
            }
            self.ctes.push(Cte { name, kind });
            if !self.eat_symbol(",") {
                return Ok(());
            }
        }
    }

    /// `[(name, ...)]`: the names a common table expression or a derived
    /// table gives its columns, if any.
    fn column_list(&mut self) -> Result<Vec<String>> {
        if !self.eat_symbol("(") {
            return Ok(Vec::new());
        }
        let names = self.comma_list(Self::ident)?;
        self.expect_symbol(")")?;
        Ok(names)
    }

    /// `SELECT ... [FROM ...] [WHERE ...] [GROUP BY ...] [HAVING ...]`: a
    /// query up to its set operations and ORDER BY. Without FROM, it reads
    /// a single row of no columns.
    fn select_core(&mut self) -> Result<Select> {
        self.expect_keyword("SELECT")?;
        let distinct = self.eat_keyword("DISTINCT");
        let projection = self.projection()?;
        let from = match self.eat_keyword("FROM") {
            true => self.from()?,
            false => From {
                table: TableRef::of(TableSource::Dual, None),
                joins: Vec::new(),
            },
        };
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

    /// `table [, table | [INNER | {LEFT | RIGHT | FULL} [OUTER] | CROSS] JOIN table [ON expr]]...`
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
            } else if self.eat_keyword("RIGHT") {
                self.eat_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                JoinKind::Right
            } else if self.eat_keyword("FULL") {
                self.eat_keyword("OUTER");
                self.expect_keyword("JOIN")?;
                JoinKind::Full
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

    /// `name [[AS] alias [(column, ...)]]`, or `[LATERAL] (SELECT ...) [AS]
    /// alias [(column, ...)]`. A name of a common table expression in scope
    /// reads it, under that name if not an alias.
    fn table_ref(&mut self) -> Result<TableRef> {
        // Not a keyword, so that it stays free as a name: a table's is never followed by `(`.
        let lateral = matches!(self.peek(), Some(Token::Ident(word)) if word.eq_ignore_ascii_case("LATERAL"))
            && self.tokens.get(self.pos + 1) == Some(&Token::Symbol("("));
        self.pos += lateral as usize;
        let (source, mut columns, cte) = match self.eat_symbol("(") {
            true => {
                let query = self.select()?;
                self.expect_symbol(")")?;
                (TableSource::Subquery(Box::new(query)), Vec::new(), None)
            }
            false => {
                let name = self.ident()?;
                match self.cte(&name) {
                    Some((source, columns)) => (source, columns, Some(name)),
                    None => (TableSource::Table(name), Vec::new(), None),
                }
            }
        };
        // RETURNING may end the query of an INSERT: it is no alias without AS.
        let returning = matches!(self.peek(), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("RETURNING"));
        let alias = match (self.eat_keyword("AS"), self.peek()) {
            (true, _) => Some(self.ident()?),
            (false, Some(Token::Ident(_))) if !returning => Some(self.ident()?),
            _ if matches!(source, TableSource::Subquery(_)) && cte.is_none() => {
                return Err(Error("a subquery in FROM must have an alias".into()));
            }
            _ => None,
        };
        if alias.is_some() {
            // The reference's names for the columns, then the expression's for the rest.
            let renamed = self.column_list()?;
            let rest = columns.split_off(renamed.len().min(columns.len()));
            columns = renamed.into_iter().chain(rest).collect();
        }
        // A common table expression's derived table goes by its name.
        let alias = alias.or(cte.filter(|_| matches!(source, TableSource::Subquery(_))));
        Ok(TableRef {
            source,
            alias,
            columns,
            lateral,
        })
    }

    /// What a name in FROM reads if it is a common table expression's in
    /// scope, and the names it gives its columns.
    fn cte(&mut self, name: &str) -> Option<(TableSource, Vec<String>)> {
        let cte = self.ctes.iter_mut().rev().find(|cte| cte.name == name)?;
        Some(match &mut cte.kind {
            CteKind::Derived { query, columns } => {
                (TableSource::Subquery(query.clone()), columns.clone())
            }
            CteKind::Recursive(cte) => (TableSource::Recursive(cte.clone()), Vec::new()),
            CteKind::Work { columns, refs } => {
                *refs += 1;
                let source = TableSource::WorkTable {
                    name: name.into(),
                    columns: columns.clone(),
                    cell: None,
                };
                (source, Vec::new())
            }
        })
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
            let expr = if self.query_follows() {
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
            if self.query_follows() {
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
        let window_only = Aggregate::from_name(name).is_none()
            && Function::from_name(name).is_none()
            && WindowFunction::from_name(name).is_some();
        if window_only {
            let function = WindowFunction::from_name(name).expect("checked above");
            let args = match self.peek() == Some(&Token::Symbol(")")) {
                true => Vec::new(),
                false => self.comma_list(Self::expr)?,
            };
            self.expect_symbol(")")?;
            if !self.over_follows() {
                let name = function.name().to_uppercase();
                return Err(Error(format!("{name} needs an OVER clause")));
            }
            return self.window(function, args);
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
            if self.over_follows() {
                let name = function.name().to_uppercase();
                return Err(Error(format!("{name} is not a window function")));
            }
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
        if self.over_follows() {
            if distinct {
                return Err(Error(
                    "DISTINCT is not supported in an aggregate over a window".into(),
                ));
            }
            let args = arg.map(|arg| *arg).into_iter().collect();
            return self.window(WindowFunction::Aggregate(function), args);
        }
        Ok(Expr::Aggregate(function, arg, distinct))
    }

    /// The rest of a window function's call after its arguments: `OVER
    /// ([PARTITION BY expr, ...] [ORDER BY ...] [frame])`.
    fn window(&mut self, function: WindowFunction, args: Vec<Expr>) -> Result<Expr> {
        let (fewest, most) = function.arity();
        if args.len() < fewest || args.len() > most {
            let name = function.name().to_uppercase();
            return Err(Error(format!(
                "wrong number of arguments to {name}: {}",
                args.len()
            )));
        }
        self.expect_word("OVER")?;
        self.expect_symbol("(")?;
        // PARTITION, ROWS, RANGE and the words of frames are not keywords,
        // so that they stay free as names.
        let mut partition_by = Vec::new();
        if self.eat_word("PARTITION") {
            self.expect_keyword("BY")?;
            partition_by = self.comma_list(Self::expr)?;
        }
        let mut order_by = Vec::new();
        if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            order_by = self.order_list()?;
        }
        let units = if self.eat_word("ROWS") {
            Some(FrameUnits::Rows)
        } else if self.eat_word("RANGE") {
            Some(FrameUnits::Range)
        } else {
            None
        };
        let frame = match units {
            None => None,
            Some(units) => {
                let (start, end) = match self.eat_keyword("BETWEEN") {
                    true => {
                        let start = self.frame_bound()?;
                        self.expect_keyword("AND")?;
                        (start, self.frame_bound()?)
                    }
                    false => (self.frame_bound()?, FrameBound::CurrentRow),
                };
                Some(frame(units, start, end)?)
            }
        };
        self.expect_symbol(")")?;
        Ok(Expr::Window(Box::new(Window {
            function,
            args,
            partition_by,
            order_by,
            frame,
        })))
    }

    /// `UNBOUNDED {PRECEDING | FOLLOWING}`, `n {PRECEDING | FOLLOWING}` or `CURRENT ROW`.
    fn frame_bound(&mut self) -> Result<FrameBound> {
        if self.eat_word("CURRENT") {
            self.expect_word("ROW")?;
            return Ok(FrameBound::CurrentRow);
        }
        let offset = match self.eat_word("UNBOUNDED") {
            true => None,
            false => match self.peek() {
                Some(Token::Number(n)) if *n >= 0 => {
                    let n = *n as u64;
                    self.pos += 1;
                    Some(n)
                }
                _ => return Err(self.unexpected("UNBOUNDED, CURRENT ROW or a number")),
            },
        };
        if self.eat_word("PRECEDING") {
            Ok(offset.map_or(FrameBound::UnboundedPreceding, FrameBound::Preceding))
        } else if self.eat_word("FOLLOWING") {
            Ok(offset.map_or(FrameBound::UnboundedFollowing, FrameBound::Following))
        } else {
            Err(self.unexpected("PRECEDING or FOLLOWING"))
        }
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

    /// Eats a word that is not a keyword, in any case: one that means
    /// something only where it stands, and is a name elsewhere.
    fn eat_word(&mut self, word: &str) -> bool {
        let matched = matches!(self.peek(), Some(Token::Ident(w)) if w.eq_ignore_ascii_case(word));
        self.pos += matched as usize;
        matched
    }

    fn expect_word(&mut self, word: &str) -> Result<()> {
        match self.eat_word(word) {
            true => Ok(()),
            false => Err(self.unexpected(word)),
        }
    }

    /// Whether `OVER (` follows: a window for the function call before it.
    fn over_follows(&self) -> bool {
        matches!(self.peek(), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("OVER"))
            && self.tokens.get(self.pos + 1) == Some(&Token::Symbol("("))
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

/// A frame from `start` to `end`, if it can be one: it may not start after
/// it ends, nor at the end of a partition or end at its start, and RANGE
/// takes no offsets.
fn frame(units: FrameUnits, start: FrameBound, end: FrameBound) -> Result<Frame> {
    // Where each bound stands, from the start of the partition to its end.
    let place = |bound: FrameBound| match bound {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::Preceding(_) => 1,
        FrameBound::CurrentRow => 2,
        FrameBound::Following(_) => 3,
        FrameBound::UnboundedFollowing => 4,
    };
    if start == FrameBound::UnboundedFollowing {
        return Err(Error("a frame cannot start at UNBOUNDED FOLLOWING".into()));
    }
    if end == FrameBound::UnboundedPreceding {
        return Err(Error("a frame cannot end at UNBOUNDED PRECEDING".into()));
    }
    if place(start) > place(end) {
        return Err(Error(format!(
            "a frame cannot start at {start} and end at {end}, before it"
        )));
    }
    let offset = |bound| matches!(bound, FrameBound::Preceding(_) | FrameBound::Following(_));
    if units == FrameUnits::Range && (offset(start) || offset(end)) {
        return Err(Error(
            "RANGE frames take only UNBOUNDED and CURRENT ROW bounds; ROWS takes offsets".into(),
        ));
    }
    Ok(Frame { units, start, end })
}

/// A recursive common table expression from its query, `body`: the part
/// after its last UNION is its step, and the rest its anchor. Only the step
/// may name the expression, once, in its FROM, and not on a side of an
/// outer join that may be padded with NULLs: else a round could return
/// rows when the last one returned none, and the recursion would not end.
fn recursive_cte(name: &str, columns: Vec<String>, mut body: Select) -> Result<RecursiveCte> {
    if !body.order_by.is_empty() || body.limit.is_some() || body.offset.is_some() {
        return Err(Error(format!(
            "ORDER BY, LIMIT and OFFSET are not supported in the recursive query {name}"
        )));
    }
    let form = || {
        Error(format!(
            "the recursive query {name} must be a query that does not name {name}, then UNION [ALL], then one that does"
        ))
    };
    let (all, step) = match body.compound.pop() {
        Some(Compound {
            op: SetOp::Union,
            all,
            query,
        }) => (all, query),
        _ => return Err(form()),
    };
    if work_refs(&body) > 0 {
        return Err(form());
    }
    let tables: Vec<&TableRef> = step.from.tables().collect();
    let direct: Vec<usize> = (0..tables.len())
        .filter(|&i| matches!(tables[i].source, TableSource::WorkTable { .. }))
        .collect();
    if direct.len() != 1 || work_refs(&step) != 1 {
        return Err(Error(format!(
            "the recursive part of {name} must name {name} once, in its FROM, not in a subquery"
        )));
    }
    let at = direct[0];
    let kinds: Vec<JoinKind> = std::iter::once(JoinKind::Inner)
        .chain(step.from.joins.iter().map(|j| j.kind))
        .collect();
    if kinds[at].keeps_left() || kinds[at + 1..].iter().any(|k| k.keeps_right()) {
        return Err(Error(format!(
            "the recursive part of {name} cannot read {name} on a side of an outer join padded with NULLs"
        )));
    }
    Ok(RecursiveCte {
        name: name.into(),
        columns,
        anchor: body,
        step,
        all,
    })
}

/// How many times the query names a recursive expression's own rows (as
/// [`TableSource::WorkTable`]), in its FROM, its subqueries, and the
/// queries combined with it.
fn work_refs(query: &Select) -> usize {
    // Those in the FROMs of the query, of its derived tables and of the
    // queries combined with it.
    fn direct(query: &Select) -> usize {
        let from: usize = query
            .from
            .tables()
            .map(|table| match &table.source {
                TableSource::WorkTable { .. } => 1,
                TableSource::Subquery(query) => direct(query),
                _ => 0,
            })
            .sum();
        from + query.compound.iter().map(|c| direct(&c.query)).sum::<usize>()
    }
    // Every subquery of an expression, at any depth, is visited once.
    let mut count = direct(query);
    for expr in query.exprs() {
        expr.visit(&mut |e| {
            if let Expr::Subquery(q) | Expr::Exists(q) | Expr::InSubquery(_, q, _) = e {
                count += direct(q);
            }
        });
    }
    count
}
