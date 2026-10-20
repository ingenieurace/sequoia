//! What the benchmarks share: cases to time, and timing them in Sequoia and,
//! with a `sqlite3` command, in SQLite beside it.
//!
//! A benchmark gives groups of cases and the statements that fill its
//! tables. With group names as arguments, only those groups run; with
//! `--no-sqlite`, SQLite is left out. Each query prints the fastest of a few
//! runs, and its plan must have the words its case names.

// Each benchmark uses only some of this.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use sequoia::executor::join::JOIN_MEMORY;
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
    /// Whether to time it in SQLite too.
    sqlite: bool,
}

pub fn case(name: &'static str, sql: &'static str, plan: &'static [&'static str]) -> Case {
    Case { name, sql, plan, join_memory: JOIN_MEMORY, sort_memory: SORT_MEMORY, sqlite: true }
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
}

/// Fills the tables with `setup` and times the cases of the groups the
/// arguments name, or of every group. `rows` says what the tables hold.
pub fn run(groups: Vec<(&'static str, Vec<Case>)>, setup: &[String], rows: &str) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let wanted: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let groups: Vec<_> =
        groups.into_iter().filter(|(group, _)| wanted.is_empty() || wanted.iter().any(|w| w == group)).collect();

    let start = Instant::now();
    let cases: Vec<&Case> = groups.iter().flat_map(|(_, cases)| cases).collect();
    let lite = match args.iter().any(|a| a == "--no-sqlite") {
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
        println!("\n{group:<30} {:>11}{}", "sequoia", if lite.is_some() { "      sqlite3" } else { "" });
        for case in cases {
            db.set_join_memory(case.join_memory);
            db.set_sort_memory(case.sort_memory);
            let plan = explain(&mut db, case.sql);
            for word in case.plan {
                assert!(plan.contains(word), "{}: no {word} in\n{plan}", case.sql);
            }
            let (time, rows) = best_of(&mut db, case.sql);
            let mut line = format!("  {:<28} {:>8.1} ms", case.name, ms(time));
            if let Some((lite_time, lite_rows)) = lite.as_mut().and_then(Iterator::next) {
                match lite_time {
                    _ if !case.sqlite => line += "       skipped",
                    Some(t) => line += &format!("  {:>8.1} ms  {:>5.1}x", ms(t), ms(time) / ms(t)),
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

fn ms(time: Duration) -> f64 {
    time.as_secs_f64() * 1000.0
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
        script += &format!("{};\n", sql.replace(" USING LSM", ""));
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
        rows = match db.execute(sql).unwrap().pop() {
            Some(QueryResult::Rows { rows, .. }) => rows.len(),
            other => panic!("expected rows, got {other:?}"),
        };
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
