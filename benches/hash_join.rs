//! Times hash joins: `cargo bench --bench hash_join`. Prints the fastest of
//! a few runs of each query, to compare changes to the join by.

use std::time::{Duration, Instant};

use sequoia::{Database, QueryResult};

const LEFT: i64 = 200_000;
const RIGHT: i64 = 50_000;
const RUNS: usize = 5;

fn main() {
    let mut db = Database::in_memory();
    db.execute(
        "CREATE TABLE a (id INT PRIMARY KEY, k INT, name TEXT);
         CREATE TABLE b (id INT PRIMARY KEY, k INT, name TEXT);",
    )
    .unwrap();
    insert(&mut db, "a", LEFT, |i| format!("({i}, {}, 'customer {}')", i % RIGHT, i % 20_000));
    insert(&mut db, "b", RIGHT, |i| format!("({i}, {i}, 'customer {i}')"));

    let queries = [
        // What the joins cost beyond this is the join's own.
        ("scans alone", "SELECT COUNT(*) FROM a JOIN b ON b.id = 0"),
        ("int key", "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k"),
        ("int key, every row", "SELECT a.id, b.id, b.name FROM a JOIN b ON b.k = a.k"),
        ("text key", "SELECT COUNT(*) FROM a JOIN b ON b.name = a.name"),
        ("two keys", "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k AND b.name = a.name"),
        ("rest of condition", "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k AND b.name > a.name"),
        ("left join, 10% match", "SELECT COUNT(*) FROM a LEFT JOIN b ON b.k = a.k + 45000"),
    ];
    for (name, sql) in queries {
        let plan = explain(&mut db, sql);
        assert!(plan.contains("HASH JOIN") || name == "scans alone", "{sql}\n{plan}");
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
        println!("{name:<22} {:>8.1} ms  ({rows} rows)", best.as_secs_f64() * 1000.0);
    }
}

/// Inserts `count` rows made by `row` into `table`, a thousand per statement.
fn insert(db: &mut Database, table: &str, count: i64, row: impl Fn(i64) -> String) {
    for start in (0..count).step_by(1000) {
        let values: Vec<String> = (start..(start + 1000).min(count)).map(&row).collect();
        db.execute(&format!("INSERT INTO {table} VALUES {}", values.join(", "))).unwrap();
    }
}

fn explain(db: &mut Database, sql: &str) -> String {
    match db.execute(&format!("EXPLAIN {sql}")).unwrap().pop() {
        Some(QueryResult::Message(plan)) => plan,
        other => panic!("expected a plan, got {other:?}"),
    }
}
