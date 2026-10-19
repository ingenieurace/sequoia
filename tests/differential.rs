//! Differential testing against SQLite: random schemas, rows, queries,
//! changes and transactions run on both, which must agree.
//!
//! Needs the `sqlite3` command, and is skipped without it. Runs
//! `SEQUOIA_DIFF_CASES` cases (default 40) from seed `SEQUOIA_DIFF_SEED`
//! (default 1). A mismatch prints the case as a script for Sequoia's shell.
//!
//! The generator writes only SQL whose meaning the two databases share, so
//! any difference is a bug in one of them. In particular, it keeps
//! expressions well typed (SQLite would convert where Sequoia reports an
//! error), divides only by nonzero constants, never sorts by a constant
//! (SQLite reads `ORDER BY 2` as the second column), lets aggregates in a
//! subquery read only the subquery's own tables (SQLite would compute one
//! that reads only outer columns in the outer query), compares rows in
//! order only when ORDER BY fixes it, and uses LIMIT only then. SQLite's
//! `AVG` is cast to an integer, as Sequoia's truncates.

use std::io::Write;
use std::process::{Command, Stdio};

use sequoia::{Database, QueryResult, Value};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Type {
    Int,
    Text,
    Bool,
}

const TYPES: [Type; 3] = [Type::Int, Type::Text, Type::Bool];
const TEXTS: [&str; 8] = ["a", "b", "ab", "ba", "B", "", " a", "it''s"];

/// A xorshift64* generator: small, and the same everywhere.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    fn range(&mut self, low: i64, high: i64) -> i64 {
        low + self.below((high - low + 1) as usize) as i64
    }
}

struct Table {
    name: String,
    /// The first is `id INT PRIMARY KEY`.
    columns: Vec<(String, Type)>,
    storage: &'static str,
}

/// What an expression may refer to.
#[derive(Clone, Default)]
struct Scope {
    /// Row columns, as the expression refers to them, and their types.
    cols: Vec<(String, Type)>,
    /// The same columns as a subquery refers to them (qualified).
    outer: Vec<(String, Type)>,
    /// In a grouped query, its GROUP BY expressions: other than inside
    /// aggregates, columns may only be read through these.
    groups: Option<Vec<(String, Type)>>,
    subqueries: bool,
    /// The tables subqueries may read.
    sub_tables: Vec<usize>,
    depth: usize,
}

/// A statement for both databases.
struct Stmt {
    sequoia: String,
    sqlite: String,
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// A query whose rows must be equal as a list.
    Ordered,
    /// A query whose rows must be equal as a multiset.
    Unordered,
    /// Anything else: both must succeed or both fail.
    Other,
}

struct Gen {
    rng: Rng,
    tables: Vec<Table>,
    indexes: Vec<String>,
}

impl Gen {
    // ---- expressions ----

    fn literal(&mut self, ty: Type) -> String {
        if self.rng.chance(10) {
            return "NULL".into();
        }
        match ty {
            Type::Int => self.rng.range(-5, 5).to_string(),
            Type::Text => format!("'{}'", self.rng.pick(&TEXTS)),
            Type::Bool => self.rng.pick(&["TRUE", "FALSE"]).to_string(),
        }
    }

    fn leaf(&mut self, ty: Type, s: &Scope) -> String {
        let choices: Vec<String> = match &s.groups {
            Some(groups) => groups.iter().filter(|(_, t)| *t == ty).map(|(e, _)| e.clone()).collect(),
            None => s.cols.iter().filter(|(_, t)| *t == ty).map(|(c, _)| c.clone()).collect(),
        };
        if s.groups.is_some() && self.rng.chance(35) {
            return self.aggregate(ty, s);
        }
        if !choices.is_empty() && self.rng.chance(70) {
            return self.rng.pick(&choices).clone();
        }
        self.literal(ty)
    }

    fn expr(&mut self, ty: Type, s: &Scope, budget: u32) -> String {
        if budget == 0 || self.rng.chance(25) {
            return self.leaf(ty, s);
        }
        let b = budget - 1;
        let subquery = s.subqueries && s.depth < 2;
        match ty {
            Type::Int => match self.rng.below(8) {
                0 => format!("({} + {})", self.expr(ty, s, b), self.expr(ty, s, b)),
                1 => format!("({} - {})", self.expr(ty, s, b), self.expr(ty, s, b)),
                2 => format!("({} * {})", self.expr(ty, s, b), self.expr(ty, s, b)),
                3 => format!("({} / {})", self.expr(ty, s, b), self.rng.range(1, 3)),
                4 => format!("(- {})", self.expr(ty, s, b)),
                5 if subquery => self.scalar_subquery(ty, s),
                _ => self.leaf(ty, s),
            },
            Type::Text => match self.rng.below(4) {
                0 if subquery => self.scalar_subquery(ty, s),
                _ => self.leaf(ty, s),
            },
            Type::Bool => match self.rng.below(12) {
                0..=2 => {
                    let t = *self.rng.pick(&TYPES);
                    let op = self.rng.pick(&["=", "!=", "<>", "<", "<=", ">", ">="]);
                    format!("({} {op} {})", self.expr(t, s, b), self.expr(t, s, b))
                }
                3 => format!("({} AND {})", self.expr(ty, s, b), self.expr(ty, s, b)),
                4 => format!("({} OR {})", self.expr(ty, s, b), self.expr(ty, s, b)),
                5 => format!("(NOT {})", self.expr(ty, s, b)),
                6 => {
                    let t = *self.rng.pick(&TYPES);
                    let not = if self.rng.chance(50) { "NOT " } else { "" };
                    format!("({} IS {not}NULL)", self.expr(t, s, b))
                }
                7 => {
                    let t = *self.rng.pick(&TYPES);
                    let items: Vec<String> = (0..1 + self.rng.below(3)).map(|_| self.expr(t, s, 0)).collect();
                    let not = if self.rng.chance(30) { "NOT " } else { "" };
                    format!("({} {not}IN ({}))", self.expr(t, s, b), items.join(", "))
                }
                8 if subquery => {
                    let t = *self.rng.pick(&TYPES);
                    let not = if self.rng.chance(30) { "NOT " } else { "" };
                    format!("({} {not}IN ({}))", self.expr(t, s, b), self.column_subquery(t, s))
                }
                9 if subquery => {
                    let not = if self.rng.chance(30) { "NOT " } else { "" };
                    format!("({not}EXISTS ({}))", self.exists_subquery(s))
                }
                _ => self.leaf(ty, s),
            },
        }
    }

    /// An expression that reads a column: SQLite reads a constant integer in
    /// ORDER BY, even `(- 1)`, as a column number.
    fn key(&mut self, ty: Type, s: &Scope, budget: u32) -> String {
        loop {
            let e = self.expr(ty, s, budget);
            if reads_column(&e) {
                return e;
            }
        }
    }

    /// An aggregate over the rows of `s`, which must have row columns.
    fn aggregate(&mut self, ty: Type, s: &Scope) -> String {
        let rows = Scope { cols: s.cols.clone(), ..Scope::default() };
        let distinct = if self.rng.chance(25) { "DISTINCT " } else { "" };
        match ty {
            Type::Int => match self.rng.below(6) {
                0 => "COUNT(*)".into(),
                1 => {
                    let t = *self.rng.pick(&TYPES);
                    format!("COUNT({distinct}{})", self.expr(t, &rows, 1))
                }
                2 => format!("SUM({distinct}{})", self.expr(ty, &rows, 1)),
                3 => format!("AVG({distinct}{})", self.expr(ty, &rows, 1)),
                4 => format!("MIN({})", self.expr(ty, &rows, 1)),
                _ => format!("MAX({distinct}{})", self.expr(ty, &rows, 1)),
            },
            _ => format!("{}({distinct}{})", self.rng.pick(&["MIN", "MAX"]), self.expr(ty, &rows, 1)),
        }
    }

    // ---- subqueries ----

    /// The FROM of a subquery, and the scope of its expressions: its table,
    /// and (unless the query around it is grouped) the columns around it.
    fn sub_from(&mut self, s: &Scope) -> (String, Scope, Scope) {
        let t = *self.rng.pick(&s.sub_tables);
        let alias = format!("s{}", s.depth + 1);
        let own: Vec<(String, Type)> =
            self.tables[t].columns.iter().map(|(c, ty)| (format!("{alias}.{c}"), *ty)).collect();
        let mut cols = own.clone();
        if s.groups.is_none() {
            cols.extend(s.outer.iter().cloned());
        }
        let inner = Scope {
            outer: cols.clone(),
            cols,
            groups: None,
            subqueries: true,
            sub_tables: s.sub_tables.clone(),
            depth: s.depth + 1,
        };
        // Aggregates read only the subquery's own table.
        let own = Scope { cols: own, ..Scope::default() };
        (format!("{} {alias}", self.tables[t].name), inner, own)
    }

    fn where_clause(&mut self, s: &Scope, percent: usize) -> String {
        match self.rng.chance(percent) {
            true => format!(" WHERE {}", self.expr(Type::Bool, s, 3)),
            false => String::new(),
        }
    }

    fn scalar_subquery(&mut self, ty: Type, s: &Scope) -> String {
        let (from, inner, own) = self.sub_from(s);
        let own = Scope { groups: Some(Vec::new()), ..own };
        let value = self.aggregate(ty, &own);
        format!("(SELECT {value} FROM {from}{})", self.where_clause(&inner, 70))
    }

    fn column_subquery(&mut self, ty: Type, s: &Scope) -> String {
        let (from, inner, _) = self.sub_from(s);
        let distinct = if self.rng.chance(20) { "DISTINCT " } else { "" };
        let value = self.expr(ty, &Scope { subqueries: false, ..inner.clone() }, 1);
        format!("SELECT {distinct}{value} FROM {from}{}", self.where_clause(&inner, 70))
    }

    fn exists_subquery(&mut self, s: &Scope) -> String {
        let (from, inner, _) = self.sub_from(s);
        format!("SELECT * FROM {from}{}", self.where_clause(&inner, 85))
    }

    // ---- queries ----

    fn select(&mut self) -> Stmt {
        let all: Vec<usize> = (0..self.tables.len()).collect();
        let n = *self.rng.pick(&[1, 1, 1, 2, 2, 3]);
        let mut cols: Vec<(String, Type)> = Vec::new();
        let mut outer: Vec<(String, Type)> = Vec::new();
        let mut from = String::new();
        for i in 0..n {
            // A table, or a subquery (a derived table) with columns x0, x1...
            let (name, columns, derived) = match self.rng.chance(20) {
                true => {
                    let (sql, columns) = self.derived();
                    (sql, columns, true)
                }
                false => {
                    let t = self.rng.below(self.tables.len());
                    (self.tables[t].name.clone(), self.tables[t].columns.clone(), false)
                }
            };
            let bare = n == 1 && !derived && self.rng.chance(30);
            let alias = format!("a{i}");
            let own: Vec<(String, Type)> = columns
                .iter()
                .map(|(c, ty)| (if bare { c.clone() } else { format!("{alias}.{c}") }, *ty))
                .collect();
            if bare {
                from = name.clone();
                outer = columns.iter().map(|(c, ty)| (format!("{name}.{c}"), *ty)).collect();
            } else if i == 0 {
                from = format!("{name} {alias}");
            } else {
                let on_scope = Scope { cols: [cols.clone(), own.clone()].concat(), ..Scope::default() };
                let condition = |g: &mut Gen| {
                    // Mostly an equality between the new table and one before it, for hash and index joins.
                    let pairs: Vec<(String, String)> = own
                        .iter()
                        .flat_map(|(a, ta)| cols.iter().filter(move |(_, tb)| tb == ta).map(|(b, _)| (a.clone(), b.clone())))
                        .collect();
                    let mut on = match g.rng.chance(75) && !pairs.is_empty() {
                        true => {
                            let (a, b) = g.rng.pick(&pairs).clone();
                            format!("{a} = {b}")
                        }
                        false => g.expr(Type::Bool, &on_scope, 2),
                    };
                    if g.rng.chance(25) {
                        on = format!("{on} AND {}", g.expr(Type::Bool, &on_scope, 2));
                    }
                    on
                };
                from += &match self.rng.below(5) {
                    0 => format!(" CROSS JOIN {name} {alias}"),
                    1 => format!(", {name} {alias}"),
                    2 => format!(" LEFT JOIN {name} {alias} ON {}", condition(self)),
                    _ => format!(" JOIN {name} {alias} ON {}", condition(self)),
                };
            }
            cols.extend(own);
        }
        if outer.is_empty() {
            outer = cols.clone();
        }
        let rows = Scope { cols: cols.clone(), outer, groups: None, subqueries: true, sub_tables: all, depth: 0 };
        let filter = self.where_clause(&rows, 70);

        let grouped = self.rng.chance(30);
        let (scope, group_by, having) = if grouped {
            let mut groups = Vec::new();
            if self.rng.chance(85) {
                for _ in 0..1 + self.rng.below(2) {
                    let (c, ty) = self.rng.pick(&cols).clone();
                    let ints: Vec<&(String, Type)> = cols.iter().filter(|(_, t)| *t == Type::Int).collect();
                    groups.push(match self.rng.chance(30) && !ints.is_empty() {
                        true => (format!("({} + {})", self.rng.pick(&ints).0, self.rng.range(1, 3)), Type::Int),
                        false => (c, ty),
                    });
                }
            }
            let group_by = match groups.is_empty() {
                true => String::new(),
                false => format!(" GROUP BY {}", groups.iter().map(|(e, _)| e.as_str()).collect::<Vec<_>>().join(", ")),
            };
            let scope = Scope { groups: Some(groups), outer: Vec::new(), ..rows.clone() };
            let having = match self.rng.chance(40) {
                true => format!(" HAVING {}", self.expr(Type::Bool, &scope, 2)),
                false => String::new(),
            };
            (scope, group_by, having)
        } else {
            (rows.clone(), String::new(), String::new())
        };

        let distinct = self.rng.chance(20);
        let star = !grouped && self.rng.chance(15);
        let mut projection: Vec<String> = match star {
            true => Vec::new(),
            false => (0..1 + self.rng.below(3))
                .map(|_| match &scope.groups {
                    Some(groups) if !groups.is_empty() && self.rng.chance(40) => self.rng.pick(groups).0.clone(),
                    _ => {
                        let ty = *self.rng.pick(&TYPES);
                        self.key(ty, &scope, 2)
                    }
                })
                .collect(),
        };
        // Without GROUP BY, SQLite groups only if the select list has an aggregate.
        if scope.groups.as_ref().is_some_and(Vec::is_empty) {
            let ty = *self.rng.pick(&TYPES);
            projection[0] = self.aggregate(ty, &scope);
        }
        // Some get an AS name, which ORDER BY may use in their place.
        let names: Vec<Option<String>> =
            (0..projection.len()).map(|i| self.rng.chance(30).then(|| format!("o{i}"))).collect();
        let by_name = |g: &mut Gen, i: usize| match &names[i] {
            Some(name) if g.rng.chance(50) => name.clone(),
            _ => projection[i].clone(),
        };

        // ORDER BY: one that fixes the order (it sorts by every selected
        // expression), one that does not, or none.
        let direction = |g: &mut Gen, e: String| if g.rng.chance(40) { format!("{e} DESC") } else { e };
        let mut kind = Kind::Unordered;
        let mut keys: Vec<String> = Vec::new();
        if !star && self.rng.chance(60) {
            if self.rng.chance(60) {
                if !distinct && self.rng.chance(30) {
                    let ty = *self.rng.pick(&TYPES);
                    let extra = self.key(ty, &scope, 1);
                    keys.push(direction(self, extra));
                }
                let mut order: Vec<usize> = (0..projection.len()).collect();
                for i in (1..order.len()).rev() {
                    order.swap(i, self.rng.below(i + 1));
                }
                for i in order {
                    let e = by_name(self, i);
                    keys.push(direction(self, e));
                }
                kind = Kind::Ordered;
            } else {
                for _ in 0..1 + self.rng.below(2) {
                    let e = match distinct {
                        true => {
                            let i = self.rng.below(projection.len());
                            by_name(self, i)
                        }
                        false => {
                            let ty = *self.rng.pick(&TYPES);
                            self.key(ty, &scope, 1)
                        }
                    };
                    keys.push(direction(self, e));
                }
            }
        }
        let order_by = match keys.is_empty() {
            true => String::new(),
            false => format!(" ORDER BY {}", keys.join(", ")),
        };
        let limit = match kind == Kind::Ordered && self.rng.chance(50) {
            true => format!(" LIMIT {}", self.rng.below(8)),
            false => String::new(),
        };
        let projection = match star {
            true => "*".into(),
            false => {
                let items = projection.iter().zip(&names).map(|(e, name)| match name {
                    Some(name) => format!("{e} AS {name}"),
                    None => e.clone(),
                });
                items.collect::<Vec<_>>().join(", ")
            }
        };
        let sql = format!(
            "SELECT {}{projection} FROM {from}{filter}{group_by}{having}{order_by}{limit}",
            if distinct { "DISTINCT " } else { "" }
        );
        stmt(sql, kind)
    }

    /// A derived table: its SQL, and its columns' names and types. Its own
    /// table goes by `s1`, so that subqueries inside it start from `s2`.
    fn derived(&mut self) -> (String, Vec<(String, Type)>) {
        let all: Vec<usize> = (0..self.tables.len()).collect();
        let s = Scope { depth: 0, sub_tables: all, subqueries: true, ..Scope::default() };
        let (from, inner, _) = self.sub_from(&s);
        let filter = self.where_clause(&inner, 50);
        let mut columns = Vec::new();
        let mut items = Vec::new();
        let distinct = if self.rng.chance(20) { "DISTINCT " } else { "" };
        if self.rng.chance(35) {
            // Grouped by one column, with aggregates.
            // Its scope has only its own table's columns: a derived table sees no other.
            let (group, ty) = self.rng.pick(&inner.cols).clone();
            let grouped = Scope { groups: Some(vec![(group.clone(), ty)]), ..inner.clone() };
            items.push(group.clone());
            columns.push(ty);
            for _ in 0..1 + self.rng.below(2) {
                let ty = *self.rng.pick(&TYPES);
                items.push(self.aggregate(ty, &grouped));
                columns.push(ty);
            }
            let names: Vec<(String, Type)> = columns.iter().enumerate().map(|(i, ty)| (format!("x{i}"), *ty)).collect();
            let list: Vec<String> = items.iter().enumerate().map(|(i, e)| format!("{e} AS x{i}")).collect();
            return (format!("(SELECT {distinct}{} FROM {from}{filter} GROUP BY {group})", list.join(", ")), names);
        }
        for _ in 0..1 + self.rng.below(3) {
            let ty = *self.rng.pick(&TYPES);
            items.push(self.expr(ty, &inner, 2));
            columns.push(ty);
        }
        let names: Vec<(String, Type)> = columns.iter().enumerate().map(|(i, ty)| (format!("x{i}"), *ty)).collect();
        let list: Vec<String> = items.iter().enumerate().map(|(i, e)| format!("{e} AS x{i}")).collect();
        (format!("(SELECT {distinct}{} FROM {from}{filter})", list.join(", ")), names)
    }

    // ---- changes ----

    fn table_scope(&mut self, t: usize, sub_tables: Vec<usize>) -> Scope {
        let table = &self.tables[t];
        let qualified = self.rng.chance(30);
        let cols: Vec<(String, Type)> = table
            .columns
            .iter()
            .map(|(c, ty)| (if qualified { format!("{}.{c}", table.name) } else { c.clone() }, *ty))
            .collect();
        let outer = table.columns.iter().map(|(c, ty)| (format!("{}.{c}", table.name), *ty)).collect();
        Scope { cols, outer, groups: None, subqueries: !sub_tables.is_empty(), sub_tables, depth: 0 }
    }

    fn insert(&mut self) -> Stmt {
        let t = self.rng.below(self.tables.len());
        let columns = self.tables[t].columns.clone();
        let named: Vec<usize> = match self.rng.chance(30) {
            true => (0..columns.len()).filter(|&i| i == 0 || self.rng.chance(50)).collect(),
            false => (0..columns.len()).collect(),
        };
        let rows: Vec<String> = (0..1 + self.rng.below(3))
            .map(|_| {
                let values: Vec<String> = named
                    .iter()
                    .map(|&i| match i {
                        0 if self.rng.chance(10) => {
                            let other = self.rng.pick(&self.tables).name.clone();
                            format!("((SELECT MAX(s1.id) FROM {other} s1) + 1)")
                        }
                        0 if self.rng.chance(3) => "NULL".into(),
                        0 => self.rng.range(0, 40).to_string(),
                        _ => self.literal(columns[i].1),
                    })
                    .collect();
                format!("({})", values.join(", "))
            })
            .collect();
        let list = match named.len() == columns.len() {
            true => String::new(),
            false => format!(" ({})", named.iter().map(|&i| columns[i].0.as_str()).collect::<Vec<_>>().join(", ")),
        };
        stmt(format!("INSERT INTO {}{list} VALUES {}", self.tables[t].name, rows.join(", ")), Kind::Other)
    }

    fn update(&mut self) -> Stmt {
        let t = self.rng.below(self.tables.len());
        // Subqueries read other tables only: SQLite may show them rows the UPDATE already changed.
        let others: Vec<usize> = (0..self.tables.len()).filter(|&i| i != t).collect();
        let s = self.table_scope(t, others);
        let columns = self.tables[t].columns.clone();
        let mut sets = Vec::new();
        for (c, ty) in &columns[1..] {
            if sets.is_empty() || self.rng.chance(30) {
                sets.push(format!("{c} = {}", self.expr(*ty, &s, 2)));
            }
        }
        let filter = self.where_clause(&s, 75);
        stmt(format!("UPDATE {} SET {}{filter}", self.tables[t].name, sets.join(", ")), Kind::Other)
    }

    fn delete(&mut self) -> Stmt {
        let t = self.rng.below(self.tables.len());
        let all = (0..self.tables.len()).collect();
        let s = self.table_scope(t, all);
        let filter = self.where_clause(&s, 85);
        stmt(format!("DELETE FROM {}{filter}", self.tables[t].name), Kind::Other)
    }

    fn contents(&self, t: usize) -> Stmt {
        stmt(format!("SELECT * FROM {} ORDER BY id", self.tables[t].name), Kind::Ordered)
    }

    /// A change, then each table's rows.
    fn change(&mut self, out: &mut Vec<Stmt>) {
        let change = match self.rng.below(10) {
            0..=3 => self.insert(),
            4..=6 => self.update(),
            7 | 8 => self.delete(),
            _ => self.index(),
        };
        out.push(change);
        for t in 0..self.tables.len() {
            out.push(self.contents(t));
        }
    }

    fn index(&mut self) -> Stmt {
        if !self.indexes.is_empty() && self.rng.chance(30) {
            let i = self.rng.below(self.indexes.len());
            return stmt(format!("DROP INDEX {}", self.indexes.swap_remove(i)), Kind::Other);
        }
        let indexable: Vec<usize> = (0..self.tables.len()).filter(|&t| self.tables[t].storage != "COLUMN").collect();
        if indexable.is_empty() {
            return self.insert();
        }
        let t = *self.rng.pick(&indexable);
        let column = self.rng.pick(&self.tables[t].columns[1..]).0.clone();
        let name = format!("ix{}", self.rng.next() % 1_000_000);
        self.indexes.push(name.clone());
        stmt(format!("CREATE INDEX {name} ON {} ({column})", self.tables[t].name), Kind::Other)
    }

    /// A transaction of changes, queries and savepoints, committed or rolled back.
    fn transaction(&mut self, out: &mut Vec<Stmt>) {
        out.push(stmt("BEGIN".into(), Kind::Other));
        for _ in 0..2 + self.rng.below(6) {
            let name = self.rng.pick(&["sp1", "sp2", "sp3"]);
            match self.rng.below(10) {
                0 => out.push(stmt(format!("SAVEPOINT {name}"), Kind::Other)),
                1 => out.push(stmt(format!("ROLLBACK TO {name}"), Kind::Other)),
                2 => out.push(stmt(format!("RELEASE {name}"), Kind::Other)),
                3 | 4 => out.push(self.select()),
                _ => self.change(out),
            }
        }
        let end = if self.rng.chance(60) { "COMMIT" } else { "ROLLBACK" };
        out.push(stmt(end.into(), Kind::Other));
        for t in 0..self.tables.len() {
            out.push(self.contents(t));
        }
    }

    // ---- a case ----

    fn case(seed: u64) -> Vec<Stmt> {
        let mut g = Gen { rng: Rng::new(seed), tables: Vec::new(), indexes: Vec::new() };
        let mut out = Vec::new();
        for t in 0..2 + g.rng.below(2) {
            let mut columns = vec![("id".to_string(), Type::Int)];
            for c in 1..=1 + g.rng.below(4) {
                columns.push((format!("c{c}"), *g.rng.pick(&TYPES)));
            }
            let storage = *g.rng.pick(&["HEAP", "LSM", "COLUMN"]);
            let name = format!("t{t}");
            let ddl = |types: &dyn Fn(Type) -> &'static str, pk: &str| {
                let rest: Vec<String> = columns[1..].iter().map(|(c, ty)| format!("{c} {}", types(*ty))).collect();
                format!("CREATE TABLE {name} (id INT {pk}, {})", rest.join(", "))
            };
            let sequoia = format!(
                "{} USING {storage}",
                ddl(&|ty| match ty {
                    Type::Int => "INT",
                    Type::Text => "TEXT",
                    Type::Bool => "BOOL",
                }, "PRIMARY KEY")
            );
            // STRICT, so that SQLite checks types; it has no BOOL, and lets a PRIMARY KEY be NULL unless told not to.
            let sqlite = format!(
                "{} STRICT",
                ddl(&|ty| if ty == Type::Text { "TEXT" } else { "INT" }, "NOT NULL PRIMARY KEY")
            );
            out.push(Stmt { sequoia, sqlite, kind: Kind::Other });
            g.tables.push(Table { name, columns, storage });
        }
        for t in 0..g.tables.len() {
            let mut ids: Vec<i64> = (0..40).collect();
            for i in (1..ids.len()).rev() {
                ids.swap(i, g.rng.below(i + 1));
            }
            let rows: Vec<String> = ids[..g.rng.below(16)]
                .iter()
                .map(|id| {
                    let columns = g.tables[t].columns.clone();
                    let values: Vec<String> = columns[1..].iter().map(|(_, ty)| g.literal(*ty)).collect();
                    format!("({id}, {})", values.join(", "))
                })
                .collect();
            if !rows.is_empty() {
                out.push(stmt(format!("INSERT INTO {} VALUES {}", g.tables[t].name, rows.join(", ")), Kind::Other));
            }
            if g.rng.chance(50) {
                let index = g.index();
                out.push(index);
            }
        }
        for _ in 0..40 {
            match g.rng.below(20) {
                0..=13 => out.push(g.select()),
                14..=17 => g.change(&mut out),
                _ => g.transaction(&mut out),
            }
        }
        out
    }
}

fn stmt(sequoia: String, kind: Kind) -> Stmt {
    let sqlite = cast_averages(&sequoia);
    Stmt { sequoia, sqlite, kind }
}

/// Rewrites each `AVG(...)` as `CAST(AVG(...) AS INTEGER)`.
fn cast_averages(sql: &str) -> String {
    let mut out = String::new();
    let mut rest = sql;
    while let Some(at) = rest.find("AVG(") {
        out.push_str(&rest[..at]);
        let start = at + 4;
        let mut depth = 1;
        let mut end = start;
        for (i, c) in rest[start..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = start + i;
                break;
            }
        }
        out.push_str(&format!("CAST(AVG({}) AS INTEGER)", &rest[start..end]));
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Whether an expression names a column (`id`, `c1` or a derived table's
/// `x1`, maybe qualified) outside its string literals.
fn reads_column(e: &str) -> bool {
    let mut words = Vec::new();
    let mut quoted = false;
    let mut word = String::new();
    for c in e.chars().chain([' ']) {
        if c == '\'' {
            quoted = !quoted;
        } else if !quoted && (c.is_ascii_alphanumeric() || c == '_') {
            word.push(c);
            continue;
        }
        words.push(std::mem::take(&mut word));
    }
    let numbered = |w: &str| w.len() > 1 && w[1..].bytes().all(|b| b.is_ascii_digit());
    words.iter().any(|w| w == "id" || ((w.starts_with('c') || w.starts_with('x')) && numbered(w)))
}

/// What a statement did: its rows as SQLite's quote mode prints them, or an error.
#[derive(Debug, PartialEq)]
enum Outcome {
    Rows(Vec<String>),
    Error(String),
}

fn quote(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Int(n) => n.to_string(),
        Value::Bool(b) => (*b as i64).to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
    }
}

fn run_sequoia(script: &[Stmt]) -> Vec<Outcome> {
    let mut db = Database::in_memory();
    script
        .iter()
        .map(|s| match db.execute(&s.sequoia) {
            Ok(mut results) => match results.pop() {
                Some(QueryResult::Rows { rows, .. }) => Outcome::Rows(
                    rows.iter().map(|row| row.iter().map(quote).collect::<Vec<_>>().join(",")).collect(),
                ),
                _ => Outcome::Rows(Vec::new()),
            },
            Err(e) => Outcome::Error(e.0),
        })
        .collect()
}

/// Runs the script in one `sqlite3` process, or returns None if there is no
/// such command. Each statement is followed by a marker row, and errors name
/// the line of their statement.
fn run_sqlite(script: &[Stmt]) -> Option<Vec<Outcome>> {
    let mut input = String::from(".mode quote\n");
    for s in script {
        input += &format!("{};\nSELECT '@@';\n", s.sqlite);
    }
    let mut child = Command::new("sqlite3")
        .arg("-batch")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut rows = Vec::new();
    for line in stdout.lines() {
        if line == "'@@'" {
            outcomes.push(Outcome::Rows(std::mem::take(&mut rows)));
        } else {
            rows.push(line.to_string());
        }
    }
    assert_eq!(outcomes.len(), script.len(), "sqlite3 output: {stdout}");
    for line in String::from_utf8(output.stderr).unwrap().lines() {
        let Some(at) = line.find("near line ") else { continue };
        let digits: String = line[at + 10..].chars().take_while(char::is_ascii_digit).collect();
        let line_number: usize = digits.parse().unwrap();
        outcomes[(line_number - 2) / 2] = Outcome::Error(line.to_string());
    }
    Some(outcomes)
}

/// Whether two outcomes agree, as the statement's kind asks.
fn agree(kind: Kind, a: &Outcome, b: &Outcome) -> bool {
    match (a, b) {
        (Outcome::Error(_), Outcome::Error(_)) => true,
        (Outcome::Rows(a), Outcome::Rows(b)) => match kind {
            Kind::Ordered | Kind::Other => a == b,
            Kind::Unordered => {
                let (mut a, mut b) = (a.clone(), b.clone());
                a.sort();
                b.sort();
                a == b
            }
        },
        _ => false,
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[test]
fn sequoia_agrees_with_sqlite() {
    let (cases, first) = (env("SEQUOIA_DIFF_CASES", 40), env("SEQUOIA_DIFF_SEED", 1));
    let mut mismatches = Vec::new();
    let (mut statements, mut queries, mut failed) = (0, 0, 0);
    for seed in first..first + cases {
        let script = Gen::case(seed);
        let Some(expected) = run_sqlite(&script) else {
            eprintln!("skipped: no sqlite3 command");
            return;
        };
        let found = run_sequoia(&script);
        statements += script.len();
        queries += script.iter().filter(|s| s.kind != Kind::Other).count();
        failed += found.iter().zip(&expected).filter(|(a, b)| matches!((a, b), (Outcome::Error(_), Outcome::Error(_)))).count();
        if let Some(i) = (0..script.len()).find(|&i| !agree(script[i].kind, &found[i], &expected[i])) {
            let setup: Vec<String> = script[..i].iter().map(|s| format!("{};", s.sequoia)).collect();
            mismatches.push(format!(
                "seed {seed}, statement {i}:\n  {}\n  sequoia: {:?}\n  sqlite:  {:?}\n  sqlite statement: {}\n  script:\n{}\n{};",
                script[i].sequoia,
                found[i],
                expected[i],
                script[i].sqlite,
                setup.join("\n"),
                script[i].sequoia
            ));
        }
    }
    eprintln!(
        "{cases} cases, {statements} statements ({queries} queries; {failed} failed in both), {} mismatches",
        mismatches.len()
    );
    if let Some(first) = mismatches.first() {
        // Each one's first five lines: where, the statement, and both outcomes.
        let all: Vec<String> = mismatches.iter().map(|m| m.lines().take(5).collect::<Vec<_>>().join("\n")).collect();
        panic!(
            "{} of {cases} cases disagree:\n\n{}\n\nThe first in full:\n{first}",
            mismatches.len(),
            all.join("\n\n")
        );
    }
}
