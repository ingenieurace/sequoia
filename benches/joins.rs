//! Times every join method: `cargo bench --bench joins`, or with group names
//! (`cargo bench --bench joins -- hash merge`) for only those groups. Prints
//! the fastest of a few runs of each query, to compare changes to a join by,
//! and checks that each query's plan uses the join it is meant to time.
//!
//! Most groups pit a method against another on the same rows, so what one
//! wins or loses by shows too: `+ 0` on a key hides it from the planner.
//!
//! With a `sqlite3` command, the same statements fill the same tables in
//! SQLite and every query is timed there too, for a column beside Sequoia's
//! and the ratio of the two; `--no-sqlite` leaves it out.

use std::time::{Duration, Instant};

use sequoia::executor::join::JOIN_MEMORY;
use sequoia::executor::sort::SORT_MEMORY;
use sequoia::{Database, QueryResult};

const LEFT: i64 = 200_000;
const RIGHT: i64 = 50_000;
const RUNS: usize = 5;

/// A condition true of every row of `a`, of four comparisons that the
/// planner guesses keep a third of the rows each: 1 in 81 in all.
macro_rules! all_rows {
    () => {
        "a.k >= 0 AND a.k <= 1000000 AND a.k > -1 AND a.k < 1000001"
    };
}

/// A query to time, the words its plan must have, and the memory budgets to
/// run it with.
struct Case {
    name: &'static str,
    sql: &'static str,
    plan: &'static [&'static str],
    join_memory: usize,
    sort_memory: usize,
    /// Whether to time it in SQLite too.
    sqlite: bool,
}

fn case(name: &'static str, sql: &'static str, plan: &'static [&'static str]) -> Case {
    Case { name, sql, plan, join_memory: JOIN_MEMORY, sort_memory: SORT_MEMORY, sqlite: true }
}

impl Case {
    fn without_sqlite(self) -> Case {
        Case { sqlite: false, ..self }
    }

    fn join_memory(self, bytes: usize) -> Case {
        Case { join_memory: bytes, ..self }
    }

    fn sort_memory(self, bytes: usize) -> Case {
        Case { sort_memory: bytes, ..self }
    }
}

fn main() {
    let groups = [
        (
            "hash",
            vec![
                // What the joins below cost beyond these is the join's own.
                // COUNT(k), as SQLite counts the rows of COUNT(*) without reading them.
                case("scan a alone", "SELECT COUNT(a.k) FROM a", &[]),
                case("scan b alone", "SELECT COUNT(b.k) FROM b", &[]),
                case("int key", "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k", &["HASH JOIN"]),
                case("int key, every row", "SELECT a.id, b.id, b.name FROM a JOIN b ON b.k = a.k", &["HASH JOIN"]),
                case("text key", "SELECT COUNT(*) FROM a JOIN b ON b.name = a.name", &["HASH JOIN"]),
                case("two keys", "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k AND b.name = a.name", &["HASH JOIN"]),
                case("rest of condition", "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k AND b.name > a.name", &["HASH JOIN"]),
                case("left join, 10% match", "SELECT COUNT(*) FROM a LEFT JOIN b ON b.k = a.k + 45000", &["HASH JOIN"]),
            ],
        ),
        (
            "index",
            // all_rows!() keeps every row of `a`, but the planner guesses that
            // each of its four comparisons keeps a third of them: so few that
            // it plans an index join. With `+ 0` hiding the key, the same
            // query makes a hash join.
            vec![
                case(
                    "index on k",
                    concat!("SELECT COUNT(*) FROM a JOIN bi ON bi.k = a.k WHERE ", all_rows!()),
                    &["INDEX JOIN"],
                ),
                case(
                    "  as a hash join",
                    concat!("SELECT COUNT(*) FROM a JOIN bi ON bi.k + 0 = a.k WHERE ", all_rows!()),
                    &["HASH JOIN"],
                ),
                case(
                    "LSM primary key",
                    concat!("SELECT COUNT(*) FROM a JOIN bl ON bl.id = a.k WHERE ", all_rows!()),
                    &["INDEX JOIN"],
                ),
                case(
                    "  as a hash join",
                    concat!("SELECT COUNT(*) FROM a JOIN bl ON bl.id + 0 = a.k WHERE ", all_rows!()),
                    &["HASH JOIN"],
                ),
                // A hash join reads all of the right side, however few left rows probe it.
                case(
                    "1,000 left rows",
                    concat!("SELECT COUNT(*) FROM a JOIN bi ON bi.k = a.k WHERE a.id < 1000 AND ", all_rows!()),
                    &["INDEX JOIN"],
                ),
                case(
                    "  as a hash join",
                    concat!("SELECT COUNT(*) FROM a JOIN bi ON bi.k + 0 = a.k WHERE a.id < 1000 AND ", all_rows!()),
                    &["HASH JOIN"],
                ),
                // What the planner picks on its own. For `a.id < 1000` it
                // guesses a third of `a`, and so a hash join, though lookups
                // would be faster.
                case("200,000 left rows, planned", "SELECT COUNT(*) FROM a JOIN bi ON bi.k = a.k", &["HASH JOIN"]),
                case(
                    "1,000 left rows, planned",
                    "SELECT COUNT(*) FROM a JOIN bi ON bi.k = a.k WHERE a.id < 1000",
                    &["HASH JOIN"],
                ),
            ],
        ),
        (
            "block",
            vec![
                // 1,000 left rows and 2,000 right ones: two million pairs.
                case(
                    "1 block",
                    "SELECT COUNT(*) FROM a JOIN b ON b.k < a.k WHERE a.id < 1000 AND b.id < 2000",
                    &["BLOCK NESTED LOOP JOIN"],
                ),
                case(
                    "100 blocks",
                    "SELECT COUNT(*) FROM a JOIN b ON b.k < a.k WHERE a.id < 1000 AND b.id < 2000",
                    &["BLOCK NESTED LOOP JOIN"],
                )
                .join_memory(1000),
                // 20,000 left rows in about 100 blocks of 20 KB, and on the
                // right the 200 groups of an aggregate over all of `b`, which
                // fit in 20 KB too: kept, rather than grouped again per block.
                // SQLite takes over a minute: it groups again per left row.
                case(
                    "100 blocks, aggregate kept",
                    "SELECT COUNT(*) FROM a JOIN (SELECT k / 250 AS g, COUNT(*) AS n FROM b GROUP BY k / 250) t
                     ON t.g * 250 < a.k WHERE a.id < 20000",
                    &["BLOCK NESTED LOOP JOIN"],
                )
                .join_memory(20_000)
                .without_sqlite(),
                case(
                    "left join, 1 block",
                    // In the ON: WHERE would filter the right side only after the join.
                    "SELECT COUNT(*) FROM a LEFT JOIN b ON b.k > a.k + 1500 AND b.id < 2000 WHERE a.id < 1000",
                    &["LEFT BLOCK NESTED LOOP JOIN"],
                ),
            ],
        ),
        (
            "merge",
            vec![
                case(
                    "ORDER BY int key",
                    "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k ORDER BY a.k",
                    &["MERGE JOIN"],
                ),
                case(
                    "  as hash join + sort",
                    "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k ORDER BY a.k + 0",
                    &["HASH JOIN", "SORT BY"],
                ),
                case(
                    "ORDER BY text key",
                    "SELECT a.id, b.id FROM a JOIN b ON b.name = a.name ORDER BY a.name DESC",
                    &["MERGE JOIN"],
                ),
                case(
                    "  as hash join + sort",
                    "SELECT a.id, b.id FROM a JOIN b ON b.name = a.name ORDER BY a.label DESC",
                    &["HASH JOIN", "SORT BY"],
                ),
                // Both sides spill: runs written out and merged back.
                case(
                    "ORDER BY int key, 256 KB",
                    "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k ORDER BY a.k",
                    &["MERGE JOIN"],
                )
                .sort_memory(256 << 10),
                case(
                    "  as hash join + sort",
                    "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k ORDER BY a.k + 0",
                    &["HASH JOIN", "SORT BY"],
                )
                .sort_memory(256 << 10),
                // A merge join sorts every row; a top-N sort keeps only ten.
                case(
                    "ORDER BY key LIMIT 10",
                    "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k ORDER BY a.k LIMIT 10",
                    &["MERGE JOIN"],
                ),
                case(
                    "  as hash join + top 10",
                    "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k ORDER BY a.k + 0 LIMIT 10",
                    &["HASH JOIN", "TOP 10"],
                ),
            ],
        ),
    ];

    let args: Vec<String> = std::env::args().skip(1).collect();
    let wanted: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let groups: Vec<_> =
        groups.into_iter().filter(|(group, _)| wanted.is_empty() || wanted.iter().any(|w| w == group)).collect();
    let setup = setup();

    let start = Instant::now();
    let cases: Vec<&Case> = groups.iter().flat_map(|(_, cases)| cases).collect();
    let lite = match args.iter().any(|a| a == "--no-sqlite") {
        true => None,
        false => sqlite(&setup, &cases),
    };
    if lite.is_some() {
        println!("sqlite3 filled the tables and ran every query in {:.1} s", start.elapsed().as_secs_f64());
    }

    let start = Instant::now();
    let mut db = Database::in_memory();
    for sql in &setup {
        db.execute(sql).unwrap();
    }
    println!("{LEFT} rows joined with {RIGHT}; tables filled in {:.1} s", start.elapsed().as_secs_f64());
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

/// The statements that fill the tables: `a` (LEFT rows) and three with the
/// same RIGHT rows: `b`, `bi` with an index on `k`, and `bl`, an LSM table,
/// whose primary key is a lookup key. Each `a.k` matches one right row, and
/// each `a.name` one in the first 20,000. `a.label` is `a.name` again, to
/// order by without ordering by a join key.
fn setup() -> Vec<String> {
    let mut sql = vec![
        "CREATE TABLE a (id INT PRIMARY KEY, k INT, name TEXT, label TEXT)".to_string(),
        "CREATE TABLE b (id INT PRIMARY KEY, k INT, name TEXT)".into(),
        "CREATE TABLE bi (id INT PRIMARY KEY, k INT, name TEXT)".into(),
        "CREATE TABLE bl (id INT PRIMARY KEY, k INT, name TEXT) USING LSM".into(),
    ];
    insert(&mut sql, "a", LEFT, |i| {
        let name = format!("'customer {}'", i % 20_000);
        format!("({i}, {}, {name}, {name})", i % RIGHT)
    });
    for table in ["b", "bi", "bl"] {
        insert(&mut sql, table, RIGHT, |i| format!("({i}, {i}, 'customer {i}')"));
    }
    sql.push("CREATE INDEX bi_k ON bi (k)".into());
    sql
}

/// Adds statements inserting `count` rows made by `row` into `table`, a thousand per statement.
fn insert(sql: &mut Vec<String>, table: &str, count: i64, row: impl Fn(i64) -> String) {
    for start in (0..count).step_by(1000) {
        let values: Vec<String> = (start..(start + 1000).min(count)).map(&row).collect();
        sql.push(format!("INSERT INTO {table} VALUES {}", values.join(", ")));
    }
}

/// Runs the same queries on the same rows in SQLite, through the `sqlite3`
/// command, if there is one: for each, the fastest of RUNS runs (or None if
/// it failed), by the shell's `.timer`, which leaves out starting the shell,
/// and how many rows it has. SQLite counts the rows rather than returning
/// them, as printing 200,000 rows would take longer than the query.
///
/// SQLite has no hash join: for an equality with no index it builds a
/// temporary index instead, and looks each left row up in it.
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
