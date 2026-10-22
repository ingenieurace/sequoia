//! What the benchmarks share: cases to time, and timing them in Sequoia and,
//! with a `sqlite3` command, in SQLite beside it.
//!
//! A benchmark gives groups of cases and the statements that fill its
//! tables. With group names as arguments, only those groups run; with
//! `--no-sqlite`, SQLite is left out. Each query prints the fastest of a few
//! runs, and its plan must have the words its case names.
//!
//! A case is one query ([`run`]), or a [`Batch`] of statements timed
//! together: many queries on tables filled once ([`run_queries`]), or writes
//! to a database on disk made afresh for each run ([`run_writes`]).
//! Benchmarks of the storage layer, below SQL, time their own work with
//! [`fastest`] and pick groups with [`wanted`].

// Each benchmark uses only some of this.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sequoia::executor::joins::{HASH_MEMORY, JOIN_MEMORY};
use sequoia::executor::sort::SORT_MEMORY;
use sequoia::{Database, QueryResult};

const RUNS: usize = 5;

/// A query to time, the words its plan must have, and the memory budgets to
/// run it with.
pub struct Case {
    name: &'static str,
    sql: &'static str,
    plan: &'static [&'static str],
    join_memory: usize,
    sort_memory: usize,
    hash_memory: usize,
    /// Whether to time it in SQLite too.
    sqlite: bool,
}

pub fn case(name: &'static str, sql: &'static str, plan: &'static [&'static str]) -> Case {
    Case { name, sql, plan, join_memory: JOIN_MEMORY, sort_memory: SORT_MEMORY, hash_memory: HASH_MEMORY, sqlite: true }
}

impl Case {
    pub fn without_sqlite(self) -> Case {
        Case { sqlite: false, ..self }
    }

    pub fn join_memory(self, bytes: usize) -> Case {
        Case { join_memory: bytes, ..self }
    }

    pub fn sort_memory(self, bytes: usize) -> Case {
        Case { sort_memory: bytes, ..self }
    }

    pub fn hash_memory(self, bytes: usize) -> Case {
        Case { hash_memory: bytes, ..self }
    }
}

/// Fills the tables with `setup` and times the cases of the groups the
/// arguments name, or of every group. `rows` says what the tables hold.
pub fn run(groups: Vec<(&'static str, Vec<Case>)>, setup: &[String], rows: &str) {
    let (groups, no_sqlite) = pick(groups);

    let start = Instant::now();
    let cases: Vec<&Case> = groups.iter().flat_map(|(_, cases)| cases).collect();
    let lite = match no_sqlite {
        true => None,
        false => sqlite(setup, &cases),
    };
    if lite.is_some() {
        println!("sqlite3 filled the tables and ran every query in {:.1} s", start.elapsed().as_secs_f64());
    }

    let start = Instant::now();
    let mut db = Database::in_memory();
    for sql in setup {
        db.execute(sql).unwrap();
    }
    println!("{rows}; tables filled in {:.1} s", start.elapsed().as_secs_f64());
    let mut lite = lite.map(Vec::into_iter);
    for (group, cases) in &groups {
        header(group, lite.is_some());
        for case in cases {
            db.set_join_memory(case.join_memory);
            db.set_sort_memory(case.sort_memory);
            db.set_hash_memory(case.hash_memory);
            let plan = explain(&mut db, case.sql);
            for word in case.plan {
                assert!(plan.contains(word), "{}: no {word} in\n{plan}", case.sql);
            }
            let (time, rows) = best_of(&mut db, case.sql);
            let mut line = format!("  {:<28} {}", case.name, shown(time));
            if let Some((lite_time, lite_rows)) = lite.as_mut().and_then(Iterator::next) {
                match lite_time {
                    _ if !case.sqlite => line += "       skipped",
                    Some(t) => line += &format!("  {}  {}", shown(t), ratio(ms(time) / ms(t))),
                    None => line += "        failed",
                }
                if lite_rows.is_some_and(|n| n != rows) {
                    line += &format!("  (sqlite3 returned {} rows)", lite_rows.unwrap());
                }
            }
            println!("{line}  ({rows} rows)");
        }
    }
}

pub fn ms(time: Duration) -> f64 {
    time.as_secs_f64() * 1000.0
}

/// How many times slower Sequoia was, in 6 columns, with more decimals under one.
fn ratio(r: f64) -> String {
    match r {
        r if r < 1.0 => format!("{r:>5.2}x"),
        r => format!("{r:>5.1}x"),
    }
}

/// A time in milliseconds, in 11 columns, with more decimals under one.
pub fn shown(time: Duration) -> String {
    match ms(time) {
        t if t < 1.0 => format!("{t:>8.3} ms"),
        t => format!("{t:>8.1} ms"),
    }
}

/// Adds statements inserting `count` rows made by `row` into `table`, a thousand per statement.
pub fn insert(sql: &mut Vec<String>, table: &str, count: i64, row: impl Fn(i64) -> String) {
    for start in (0..count).step_by(1000) {
        let values: Vec<String> = (start..(start + 1000).min(count)).map(&row).collect();
        sql.push(format!("INSERT INTO {table} VALUES {}", values.join(", ")));
    }
}

/// Runs the same queries on the same rows in SQLite, through the `sqlite3`
/// command, if there is one: for each, the fastest of RUNS runs (or None if
/// it failed), by the shell's `.timer`, which leaves out starting the shell,
/// and how many rows it has. SQLite counts the rows rather than returning
/// them, as printing 200,000 rows would take longer than the query. (It
/// still sorts them: `EXPLAIN QUERY PLAN` shows a temporary B-tree for an
/// ORDER BY in the counted subquery.)
#[allow(clippy::type_complexity)]
fn sqlite(setup: &[String], cases: &[&Case]) -> Option<Vec<(Option<Duration>, Option<usize>)>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let Ok(mut child) = Command::new("sqlite3")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        println!("no sqlite3 command: timing Sequoia alone");
        return None;
    };
    let mut script = String::new();
    for sql in setup {
        script += &format!("{};\n", for_sqlite(sql));
    }
    script += ".timer on\n";
    for (i, case) in cases.iter().enumerate().filter(|(_, case)| case.sqlite) {
        script += &format!(".print @case {i}\n");
        for _ in 0..RUNS {
            script += &format!("SELECT COUNT(*) FROM ({});\n", case.sql);
        }
    }
    child.stdin.take().expect("piped").write_all(script.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        println!("sqlite3 reported: {}", stderr.trim());
    }

    let mut results = vec![(None, None); cases.len()];
    let mut at = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(i) = line.strip_prefix("@case ") {
            at = i.parse::<usize>().ok();
        } else if let (Some(i), Some(rest)) = (at, line.strip_prefix("Run Time: real ")) {
            let time = rest.split_whitespace().next().and_then(|s| s.parse::<f64>().ok());
            let time = Duration::from_secs_f64(time?);
            results[i].0 = Some(results[i].0.map_or(time, |best: Duration| best.min(time)));
        } else if let (Some(i), Ok(rows)) = (at, line.trim().parse::<usize>()) {
            results[i].1 = Some(rows);
        }
    }
    Some(results)
}

/// The fastest of RUNS runs of a query, and how many rows it returned.
fn best_of(db: &mut Database, sql: &str) -> (Duration, usize) {
    let mut best = Duration::MAX;
    let mut rows = 0;
    for _ in 0..RUNS {
        let start = Instant::now();
        rows = count(db, sql);
        best = best.min(start.elapsed());
    }
    (best, rows)
}

fn explain(db: &mut Database, sql: &str) -> String {
    match db.execute(&format!("EXPLAIN {sql}")).unwrap().pop() {
        Some(QueryResult::Message(plan)) => plan,
        other => panic!("expected a plan, got {other:?}"),
    }
}

/// Rewrites a statement for SQLite, which has no storage engines to pick,
/// makes every index a B-tree and checkpoints by a pragma.
fn for_sqlite(sql: &str) -> String {
    if sql == "CHECKPOINT" {
        return "PRAGMA wal_checkpoint(TRUNCATE)".into();
    }
    ["HEAP", "LSM", "COLUMN", "HASH"].iter().fold(sql.to_string(), |sql, using| sql.replace(&format!(" USING {using}"), ""))
}

/// Statements timed together, and the words that the plan of the first
/// query among them (a SELECT, UPDATE or DELETE) must have.
pub struct Batch {
    name: String,
    /// Run before each timing, untimed: for writes only.
    setup: Vec<String>,
    statements: Vec<String>,
    plan: &'static [&'static str],
    /// Whether to time it in SQLite too.
    sqlite: bool,
    /// A column the count around each query in SQLite must read.
    reading: Option<&'static str>,
}

pub fn batch(name: impl Into<String>, statements: Vec<String>, plan: &'static [&'static str]) -> Batch {
    Batch { name: name.into(), setup: Vec::new(), statements, plan, sqlite: true, reading: None }
}

impl Batch {
    /// Statements that make each run's tables, untimed (see [`run_writes`]).
    pub fn setup(self, setup: Vec<String>) -> Batch {
        Batch { setup, ..self }
    }

    pub fn without_sqlite(self) -> Batch {
        Batch { sqlite: false, ..self }
    }

    /// Makes SQLite read this column of each row a query returns, which
    /// must be NULL in none. Otherwise, counting the rows of a query that
    /// finds them by an index, SQLite reads only the index.
    pub fn reading(self, column: &'static str) -> Batch {
        Batch { reading: Some(column), ..self }
    }
}

/// Runs of the same batch made by [`run_writes`].
pub const WRITE_RUNS: usize = 3;

/// Fills the tables with `setup` once, in memory, and times each batch of
/// the groups the arguments name: the fastest of a few runs of all its
/// statements, which must be queries. `rows` says what the tables hold.
pub fn run_queries(groups: Vec<(&'static str, Vec<Batch>)>, setup: &[String], rows: &str) {
    let (groups, no_sqlite) = pick(groups);
    let batches: Vec<&Batch> = groups.iter().flat_map(|(_, batches)| batches).collect();

    let start = Instant::now();
    let lite = match no_sqlite {
        true => None,
        false => {
            let mut script: String = setup.iter().map(|sql| format!("{};\n", for_sqlite(sql))).collect();
            script += ".timer on\n";
            for (i, batch) in batches.iter().enumerate().filter(|(_, batch)| batch.sqlite) {
                for _ in 0..RUNS {
                    script += &format!(".print @case {i}\n");
                    let read = batch.reading.map_or(String::new(), |c| format!(" WHERE {c} IS NOT NULL"));
                    for sql in &batch.statements {
                        script += &format!("SELECT COUNT(*) FROM ({sql}){read};\n");
                    }
                }
            }
            sqlite_script(&script, batches.len())
        }
    };
    if lite.is_some() {
        println!("sqlite3 filled the tables and ran every batch in {:.1} s", start.elapsed().as_secs_f64());
    }

    let start = Instant::now();
    let mut db = Database::in_memory();
    for sql in setup {
        db.execute(sql).unwrap();
    }
    println!("{rows}; tables filled in {:.1} s", start.elapsed().as_secs_f64());
    let mut at = 0;
    for (group, batches) in &groups {
        header(group, lite.is_some());
        for batch in batches {
            check_plan(&mut db, batch);
            let mut best = Duration::MAX;
            let mut rows = 0;
            for _ in 0..RUNS {
                let start = Instant::now();
                rows = batch.statements.iter().map(|sql| count(&mut db, sql)).sum();
                best = best.min(start.elapsed());
            }
            println!("{}", line(batch, best, lite.as_ref().map(|lite| &lite[at][..]), Some(rows)));
            at += 1;
        }
    }
}

/// Times each batch of writes of the groups the arguments name, on a
/// database in a file, made afresh for each of [`WRITE_RUNS`] runs and
/// filled by the batch's setup: the fastest run of all its statements.
/// Every statement outside a transaction commits, and syncs the log to the
/// disk. SQLite runs in WAL mode with `synchronous = FULL`, which syncs its
/// log at each commit too.
pub fn run_writes(groups: Vec<(&'static str, Vec<Batch>)>) {
    let (groups, no_sqlite) = pick(groups);
    let batches: Vec<&Batch> = groups.iter().flat_map(|(_, batches)| batches).collect();
    let dir = std::env::temp_dir().join(format!("sequoia-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let start = Instant::now();
    let lite = match no_sqlite {
        true => None,
        false => {
            let mut script = String::new();
            for (i, batch) in batches.iter().enumerate().filter(|(_, batch)| batch.sqlite) {
                for run in 0..WRITE_RUNS {
                    let path = dir.join(format!("sqlite-{i}-{run}.db"));
                    script += &format!(".timer off\n.open '{}'\n", path.display());
                    script += "PRAGMA journal_mode = WAL;\nPRAGMA synchronous = FULL;\n";
                    for sql in &batch.setup {
                        script += &format!("{};\n", for_sqlite(sql));
                    }
                    script += &format!(".print @case {i}\n.timer on\n");
                    for sql in &batch.statements {
                        script += &format!("{};\n", for_sqlite(sql));
                    }
                }
            }
            script += ".timer off\n.open :memory:\n";
            sqlite_script(&script, batches.len())
        }
    };
    if lite.is_some() {
        println!("sqlite3 ran every batch in {:.1} s", start.elapsed().as_secs_f64());
    }

    println!("each run writes a new database in {}", dir.display());
    let mut at = 0;
    for (group, batches) in &groups {
        header(group, lite.is_some());
        for batch in batches {
            let mut best = Duration::MAX;
            for run in 0..WRITE_RUNS {
                let path = dir.join("sequoia.db");
                remove_database(&path);
                let mut db = Database::open(&path).unwrap();
                for sql in &batch.setup {
                    db.execute(sql).unwrap();
                }
                if run == 0 {
                    check_plan(&mut db, batch);
                }
                let start = Instant::now();
                for sql in &batch.statements {
                    db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
                }
                best = best.min(start.elapsed());
            }
            println!("{}", line(batch, best, lite.as_ref().map(|lite| &lite[at][..]), None));
            at += 1;
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Removes a database file and its logs, if they are there.
fn remove_database(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
}

/// The groups the arguments name, or every group, and whether `--no-sqlite` is given.
fn pick<T>(groups: Vec<(&'static str, T)>) -> (Vec<(&'static str, T)>, bool) {
    let no_sqlite = std::env::args().any(|a| a == "--no-sqlite");
    (groups.into_iter().filter(|(group, _)| wanted(group)).collect(), no_sqlite)
}

/// Whether the arguments name this group, or name no group at all.
pub fn wanted(group: &str) -> bool {
    let names: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    names.is_empty() || names.iter().any(|name| name == group)
}

/// The fastest of a few runs of `f`, and what its last run returned.
pub fn fastest<T>(mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut best = Duration::MAX;
    let mut result = None;
    for _ in 0..RUNS {
        let start = Instant::now();
        result = Some(f());
        best = best.min(start.elapsed());
    }
    (best, result.expect("RUNS > 0"))
}

fn header(group: &str, sqlite: bool) {
    println!("\n{group:<30} {:>11}{}", "sequoia", if sqlite { "      sqlite3" } else { "" });
}

/// A batch's line: its time, SQLite's beside it if there is a column for
/// it, and how long each statement took.
fn line(batch: &Batch, time: Duration, lite: Option<&[LiteRun]>, rows: Option<usize>) -> String {
    let mut line = format!("  {:<28} {}", batch.name, shown(time));
    if let Some(runs) = lite {
        match runs.iter().map(|run| run.time).min() {
            _ if !batch.sqlite => line += "       skipped",
            Some(t) => line += &format!("  {}  {}", shown(t), ratio(ms(time) / ms(t))),
            None => line += "        failed",
        }
        if let (Some(rows), Some(run)) = (rows, runs.first())
            && run.rows != rows
        {
            line += &format!("  (sqlite3 returned {} rows)", run.rows);
        }
    }
    let n = batch.statements.len();
    let each = time.as_secs_f64() * 1e6 / n as f64;
    let mut notes: Vec<String> = Vec::new();
    if n > 1 {
        notes.push(format!("{n} statements, {each:.1} us each"));
    }
    notes.extend(rows.map(|rows| format!("{rows} rows")));
    match notes.is_empty() {
        true => line,
        false => line + &format!("  ({})", notes.join(", ")),
    }
}

/// Checks the plan of the batch's first query against the words it names.
fn check_plan(db: &mut Database, batch: &Batch) {
    if batch.plan.is_empty() {
        return;
    }
    let query = batch.statements.iter().find(|sql| ["SELECT", "UPDATE", "DELETE"].iter().any(|k| sql.starts_with(k)));
    let query = query.unwrap_or_else(|| panic!("{}: no query to check the plan of", batch.name));
    let plan = explain(db, query);
    for word in batch.plan {
        assert!(plan.contains(word), "{query}: no {word} in\n{plan}");
    }
}

/// How many rows a query returns.
fn count(db: &mut Database, sql: &str) -> usize {
    match db.execute(sql).unwrap().pop() {
        Some(QueryResult::Rows { rows, .. }) => rows.len(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// One run of a batch in SQLite: how long its statements took together, by
/// the shell's `.timer`, and how many rows its queries counted.
#[derive(Debug, Clone, Copy, Default)]
struct LiteRun {
    time: Duration,
    rows: usize,
}

/// Runs a script in the `sqlite3` command, if there is one, in which each
/// run of batch `i` starts with `.print @case i`, and returns the runs of
/// each of the `batches`.
fn sqlite_script(script: &str, batches: usize) -> Option<Vec<Vec<LiteRun>>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let Ok(mut child) = Command::new("sqlite3")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        println!("no sqlite3 command: timing Sequoia alone");
        return None;
    };
    // Written from another thread: sqlite3 may fill its output pipe before
    // it has read the whole script, and waits for that to be read.
    let mut stdin = child.stdin.take().expect("piped");
    let script = script.to_owned();
    let writer = std::thread::spawn(move || stdin.write_all(script.as_bytes()));
    let output = child.wait_with_output().ok()?;
    writer.join().ok()?.ok()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        println!("sqlite3 reported: {}", stderr.trim());
    }

    let mut results = vec![Vec::new(); batches];
    let mut at = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(i) = line.strip_prefix("@case ") {
            at = i.parse::<usize>().ok();
            if let Some(i) = at {
                results[i].push(LiteRun::default());
            }
        } else if let (Some(i), Some(rest)) = (at, line.strip_prefix("Run Time: real ")) {
            let time = rest.split_whitespace().next().and_then(|s| s.parse::<f64>().ok());
            let run: &mut LiteRun = results[i].last_mut().expect("a run begun");
            run.time += Duration::from_secs_f64(time?);
        } else if let (Some(i), Ok(rows)) = (at, line.trim().parse::<usize>()) {
            results[i].last_mut().expect("a run begun").rows += rows;
        }
    }
    Some(results)
}
