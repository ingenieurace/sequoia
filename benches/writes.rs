//! Times writes to a database on disk: `cargo bench --bench writes`, or
//! with group names (`cargo bench --bench writes -- commit update`) for only
//! those groups. Each batch of statements runs on a new database file,
//! filled by its setup first, untimed; it prints the fastest of a few runs.
//!
//! `insert` loads 50,000 rows a thousand at a time into each storage engine
//! and with more and more indexes, `commit` times small transactions, where
//! syncing the log at each commit is most of the cost, `update` and `delete`
//! change a tenth of 50,000 rows, or all of them, and `checkpoint` copies
//! the log's pages into the database file.
//!
//! With a `sqlite3` command, the same statements run in SQLite too, on a
//! file in WAL mode with `synchronous = FULL`: like Sequoia, it syncs its
//! log at each commit and checkpoints it once it reaches 4 MB. `--no-sqlite`
//! leaves it out.

mod common;

use common::{Batch, batch, insert};

const ROWS: i64 = 50_000;
/// Rows changed by the batches that change some of them, as `id < TENTH`.
const TENTH: i64 = ROWS / 10;
/// Rows written by the batches of single-row statements.
const SMALL: i64 = 200;

fn main() {
    let groups = vec![
        (
            "insert",
            vec![
                load("heap, no key", table("HEAP", "id INT")),
                load("heap, primary key", table("HEAP", "id INT PRIMARY KEY")),
                load("heap, key + 2 indexes", [table("HEAP", "id INT PRIMARY KEY"), indexes("")].concat()),
                // SQLite has no hash indexes: B-trees there.
                load("heap, key + 2 hash indexes", [table("HEAP", "id INT PRIMARY KEY"), indexes(" USING HASH")].concat()),
                // Keys in no order: each insert lands in a different leaf of the key's index.
                batch("heap, keys out of order", rows(ROWS, scattered), &[])
                    .setup(table("HEAP", "id INT PRIMARY KEY")),
                load("LSM", table("LSM", "id INT PRIMARY KEY")),
                load("column", table("COLUMN", "id INT PRIMARY KEY")),
                batch("heap, one transaction", transaction(rows(ROWS, row)), &[])
                    .setup(table("HEAP", "id INT PRIMARY KEY")),
            ],
        ),
        (
            "commit",
            vec![
                batch("one row a commit", single_rows(), &[]).setup(table("HEAP", "id INT PRIMARY KEY")),
                batch("one row a commit, LSM", single_rows(), &[]).setup(table("LSM", "id INT PRIMARY KEY")),
                batch("one transaction", transaction(single_rows()), &[]).setup(table("HEAP", "id INT PRIMARY KEY")),
                batch("one statement", rows(SMALL, row), &[]).setup(table("HEAP", "id INT PRIMARY KEY")),
                batch("savepoint a row", savepoints(), &[]).setup(table("HEAP", "id INT PRIMARY KEY")),
            ],
        ),
        (
            "update",
            vec![
                change("a tenth, in place", &format!("UPDATE t SET k = k + 1 WHERE id < {TENTH}"), "HEAP", &["t_pkey"]),
                change("a tenth, indexed column", &format!("UPDATE t SET g = g + 1 WHERE id < {TENTH}"), "HEAP", &[]),
                // Rows grow past the room left in their pages, and move.
                change(
                    "a tenth, rows grow",
                    &format!("UPDATE t SET name = '{}' WHERE id < {TENTH}", "a much longer name ".repeat(5)),
                    "HEAP",
                    &[],
                ),
                change("every row", "UPDATE t SET k = k + 1", "HEAP", &["SCAN t"]),
                change("a tenth, LSM", &format!("UPDATE t SET k = k + 1 WHERE id < {TENTH}"), "LSM", &[]),
                change("a tenth, column", &format!("UPDATE t SET k = k + 1 WHERE id < {TENTH}"), "COLUMN", &[]),
                batch(
                    "one row a statement",
                    transaction((0..SMALL).map(|i| format!("UPDATE t SET k = k + 1 WHERE id = {}", i * 241)).collect()),
                    &["t_pkey"],
                )
                .setup(filled("HEAP")),
            ],
        ),
        (
            "delete",
            vec![
                change("a tenth, by key", &format!("DELETE FROM t WHERE id < {TENTH}"), "HEAP", &["t_pkey"]),
                // Every tenth row: from every page.
                change("a tenth, scattered", "DELETE FROM t WHERE g < 10", "HEAP", &["t_g"]),
                change("every row", "DELETE FROM t", "HEAP", &["SCAN t"]),
                change("a tenth, LSM", &format!("DELETE FROM t WHERE id < {TENTH}"), "LSM", &[]),
                change("a tenth, column", &format!("DELETE FROM t WHERE id < {TENTH}"), "COLUMN", &[]),
            ],
        ),
        (
            "checkpoint",
            vec![
                batch("after 50,000 rows", vec!["CHECKPOINT".into()], &[]).setup(filled("HEAP")),
                batch("after one row a commit", vec!["CHECKPOINT".into()], &[])
                    .setup([table("HEAP", "id INT PRIMARY KEY"), single_rows()].concat()),
            ],
        ),
    ];

    common::run_writes(groups);
}

/// A batch loading ROWS rows, a thousand a statement, into a table made by `setup`.
fn load(name: &str, setup: Vec<String>) -> Batch {
    batch(name, rows(ROWS, row), &[]).setup(setup)
}

/// A batch of one statement on ROWS rows in a table of this engine, with an
/// index on `g` unless it is a column table, which cannot have one.
fn change(name: &str, sql: &str, engine: &str, plan: &'static [&'static str]) -> Batch {
    batch(name, vec![sql.to_string()], plan).setup(filled(engine))
}

/// `t` with ROWS rows, made with this engine.
fn filled(engine: &str) -> Vec<String> {
    let mut sql = table(engine, "id INT PRIMARY KEY");
    if engine != "COLUMN" {
        sql.push("CREATE INDEX t_g ON t (g)".into());
    }
    [sql, rows(ROWS, row)].concat()
}

/// Makes `t`, with this engine and `id` column.
fn table(engine: &str, id: &str) -> Vec<String> {
    vec![format!("CREATE TABLE t ({id}, k INT, g INT, name TEXT) USING {engine}")]
}

/// Indexes on `k` and `name`, `using` B+ trees or hash files.
fn indexes(using: &str) -> Vec<String> {
    vec![format!("CREATE INDEX t_k ON t{using} (k)"), format!("CREATE INDEX t_name ON t{using} (name)")]
}

/// Statements inserting `count` rows made by `row`, a thousand a statement.
fn rows(count: i64, row: fn(i64) -> String) -> Vec<String> {
    let mut sql = Vec::new();
    insert(&mut sql, "t", count, row);
    sql
}

/// Row `i`, in `id` order. `k` and `name` are scattered over the rows, and
/// every one differs; `g` has 100 values, each in every hundredth row.
fn row(i: i64) -> String {
    // i * m mod a prime larger than ROWS is different for every row.
    const P: i64 = 1_000_003;
    format!("({i}, {}, {}, 'customer {:07}')", i * 48_271 % P, i % 100, i * 69_621 % P)
}

/// Row `i` with an `id` that is in no order.
fn scattered(i: i64) -> String {
    const P: i64 = 1_000_003;
    format!("({}, {i}, {}, 'customer {i:07}')", i * 48_271 % P, i % 100)
}

/// SMALL statements inserting a row each.
fn single_rows() -> Vec<String> {
    (0..SMALL).map(|i| format!("INSERT INTO t VALUES {}", row(i))).collect()
}

/// SMALL single-row inserts in one transaction, each after a savepoint that
/// is then released.
fn savepoints() -> Vec<String> {
    let mut sql = vec!["BEGIN".to_string()];
    for i in 0..SMALL {
        sql.extend([format!("SAVEPOINT s{i}"), format!("INSERT INTO t VALUES {}", row(i)), format!("RELEASE s{i}")]);
    }
    sql.push("COMMIT".into());
    sql
}

fn transaction(statements: Vec<String>) -> Vec<String> {
    [vec!["BEGIN".to_string()], statements, vec!["COMMIT".to_string()]].concat()
}
