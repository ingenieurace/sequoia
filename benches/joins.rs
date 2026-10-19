//! Times every join method: `cargo bench --bench joins`, or with group names
//! (`cargo bench --bench joins -- hash merge`) for only those groups. Prints
//! the fastest of a few runs of each query, to compare changes to a join by,
//! and checks that each query's plan uses the join it is meant to time.
//!
//! Most groups pit a method against another on the same rows, so what one
//! wins or loses by shows too: `+ 0` on a key hides it from the planner.

use std::time::{Duration, Instant};

use sequoia::executor::join::JOIN_MEMORY;
use sequoia::executor::sort::SORT_MEMORY;
use sequoia::{Database, QueryResult};

const LEFT: i64 = 200_000;
const RIGHT: i64 = 50_000;
const RUNS: usize = 5;

/// A query to time, the words its plan must have, and the memory budgets to
/// run it with.
struct Case {
    name: &'static str,
    sql: &'static str,
    plan: &'static [&'static str],
    join_memory: usize,
    sort_memory: usize,
}

fn case(name: &'static str, sql: &'static str, plan: &'static [&'static str]) -> Case {
    Case { name, sql, plan, join_memory: JOIN_MEMORY, sort_memory: SORT_MEMORY }
}

impl Case {
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
                // What the joins below cost beyond this is the join's own.
                case("scans alone", "SELECT COUNT(*) FROM a JOIN b ON b.id = 0", &[]),
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
            vec![
                case("index on k", "SELECT COUNT(*) FROM a JOIN bi ON bi.k = a.k", &["INDEX JOIN"]),
                case("  as a hash join", "SELECT COUNT(*) FROM a JOIN bi ON bi.k + 0 = a.k", &["HASH JOIN"]),
                case("LSM primary key", "SELECT COUNT(*) FROM a JOIN bl ON bl.id = a.k", &["INDEX JOIN"]),
                case("  as a hash join", "SELECT COUNT(*) FROM a JOIN bl ON bl.id + 0 = a.k", &["HASH JOIN"]),
                // A hash join reads all of the right side, however few left rows probe it.
                case("1,000 left rows", "SELECT COUNT(*) FROM a JOIN bi ON bi.k = a.k WHERE a.id < 1000", &["INDEX JOIN"]),
                case(
                    "  as a hash join",
                    "SELECT COUNT(*) FROM a JOIN bi ON bi.k + 0 = a.k WHERE a.id < 1000",
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

    let wanted: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    let start = Instant::now();
    let mut db = setup();
    println!("{LEFT} rows joined with {RIGHT}; tables filled in {:.1} s", start.elapsed().as_secs_f64());
    for (group, cases) in groups {
        if !wanted.is_empty() && !wanted.iter().any(|w| w == group) {
            continue;
        }
        println!("\n{group}");
        for case in cases {
            db.set_join_memory(case.join_memory);
            db.set_sort_memory(case.sort_memory);
            let plan = explain(&mut db, case.sql);
            for word in case.plan {
                assert!(plan.contains(word), "{}: no {word} in\n{plan}", case.sql);
            }
            let (time, rows) = best_of(&mut db, case.sql);
            println!("  {:<28} {:>8.1} ms  ({rows} rows)", case.name, time.as_secs_f64() * 1000.0);
        }
    }
}

/// Tables `a` (LEFT rows) and three with the same RIGHT rows: `b`, `bi`
/// with an index on `k`, and `bl`, an LSM table, whose primary key is a
/// lookup key. Each `a.k` matches one right row, and each `a.name` one in
/// the first 20,000. `a.label` is `a.name` again, to order by without
/// ordering by a join key.
fn setup() -> Database {
    let mut db = Database::in_memory();
    db.execute(
        "CREATE TABLE a (id INT PRIMARY KEY, k INT, name TEXT, label TEXT);
         CREATE TABLE b (id INT PRIMARY KEY, k INT, name TEXT);
         CREATE TABLE bi (id INT PRIMARY KEY, k INT, name TEXT);
         CREATE TABLE bl (id INT PRIMARY KEY, k INT, name TEXT) USING LSM;",
    )
    .unwrap();
    insert(&mut db, "a", LEFT, |i| {
        let name = format!("'customer {}'", i % 20_000);
        format!("({i}, {}, {name}, {name})", i % RIGHT)
    });
    for table in ["b", "bi", "bl"] {
        insert(&mut db, table, RIGHT, |i| format!("({i}, {i}, 'customer {i}')"));
    }
    db.execute("CREATE INDEX bi_k ON bi (k)").unwrap();
    db
}

/// Inserts `count` rows made by `row` into `table`, a thousand per statement.
fn insert(db: &mut Database, table: &str, count: i64, row: impl Fn(i64) -> String) {
    for start in (0..count).step_by(1000) {
        let values: Vec<String> = (start..(start + 1000).min(count)).map(&row).collect();
        db.execute(&format!("INSERT INTO {table} VALUES {}", values.join(", "))).unwrap();
    }
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
