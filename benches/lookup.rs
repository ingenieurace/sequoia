//! Times finding rows by key: `cargo bench --bench lookup`, or with group
//! names (`cargo bench --bench lookup -- point`) for only those groups.
//! Each batch runs many queries, each finding different rows, and prints
//! the fastest of a few runs of all of them and the time each took; the
//! plan of its first query must find rows as the batch means to.
//!
//! The same rows are in a heap (`t`), with indexes on `k`, `g` and `name`
//! besides its primary key, in another heap (`h`) with hash indexes on `k`
//! and `name` instead, in an LSM tree (`l`), keyed by `id`, and in a column
//! store (`c`), which has no indexes. `point` looks up one row at a time,
//! by each index and against reading every row, and `range` reads more and
//! more rows by an index, against reading every row.
//!
//! With a `sqlite3` command, the same statements fill the same tables in
//! SQLite, with the same indexes, and every batch is timed there too;
//! `--no-sqlite` leaves it out. SQLite reads `note`, in no index, of each
//! row found: counting the rows an index finds, it would read only the
//! index, where Sequoia reads each row from its table.

mod common;

use common::{Batch, batch, insert};

const ROWS: i64 = 200_000;
/// i * m mod a prime larger than ROWS is different for every row.
const P: i64 = 1_000_003;

fn main() {
    let groups = vec![
        (
            "point",
            vec![
                points(
                    "heap, primary key",
                    1000,
                    |i| format!("SELECT * FROM t WHERE id = {i}"),
                    &["USING INDEX t_pkey"],
                ),
                points(
                    "heap, INT index",
                    1000,
                    |i| format!("SELECT * FROM t WHERE k = {}", k(i)),
                    &["USING INDEX t_k"],
                ),
                points(
                    "heap, TEXT index",
                    1000,
                    |i| format!("SELECT * FROM t WHERE name = '{}'", name(i)),
                    &["t_name"],
                ),
                // SQLite has no hash indexes: B-trees there.
                points(
                    "heap, INT hash",
                    1000,
                    |i| format!("SELECT * FROM h WHERE k = {}", k(i)),
                    &["USING HASH INDEX h_k"],
                ),
                points(
                    "heap, TEXT hash",
                    1000,
                    |i| format!("SELECT * FROM h WHERE name = '{}'", name(i)),
                    &["HASH INDEX h_name"],
                ),
                points(
                    "heap, no such hash",
                    1000,
                    |i| format!("SELECT * FROM h WHERE k = -1 - {i}"),
                    &["HASH INDEX h_k"],
                ),
                points(
                    "heap, no such key",
                    1000,
                    |i| format!("SELECT * FROM t WHERE id = {}", ROWS + i),
                    &["t_pkey"],
                ),
                points(
                    "LSM, primary key",
                    1000,
                    |i| format!("SELECT * FROM l WHERE id = {i}"),
                    &["USING PRIMARY KEY"],
                ),
                // Every SSTable's key range holds the key: its Bloom filter rules it out.
                points(
                    "LSM, no such key",
                    1000,
                    |i| format!("SELECT * FROM l WHERE id = {}", i * 2 + 1),
                    &["PRIMARY KEY"],
                ),
                // `+ 0` hides the key from the planner: every row is read.
                points(
                    "heap, no index",
                    10,
                    |i| format!("SELECT * FROM t WHERE id + 0 = {i}"),
                    &["SCAN t"],
                ),
                points(
                    "LSM, no index",
                    10,
                    |i| format!("SELECT * FROM l WHERE k = {}", k(i)),
                    &["SCAN l"],
                ),
                // SQLite finds these by the index of its primary key: nothing to compare.
                points(
                    "column, pushed down",
                    10,
                    |i| format!("SELECT * FROM c WHERE id = {i}"),
                    &["COMPRESSED CHUNKS"],
                )
                .without_sqlite(),
            ],
        ),
        (
            "range",
            vec![
                // `k` is scattered over the rows: each row found is on a page of its own.
                ranges("0.1%, INT index", 100, 1_000, "k", &["USING INDEX t_k"]),
                ranges("1%, INT index", 20, 10_000, "k", &["USING INDEX t_k"]),
                ranges("10%, INT index", 5, 100_000, "k", &["USING INDEX t_k"]),
                ranges("1%, read every row", 20, 10_000, "k + 0", &["SCAN t"]),
                ranges("10%, read every row", 5, 100_000, "k + 0", &["SCAN t"]),
                // Rows in `id` order are in the heap's order too: pages in a row.
                points(
                    "1%, primary key",
                    20,
                    |i| format!("SELECT * FROM t WHERE id >= {i} AND id < {}", i + 2000),
                    &["t_pkey"],
                ),
                points(
                    "1%, one value of g",
                    20,
                    |i| format!("SELECT * FROM t WHERE g = {}", i % 100),
                    &["USING INDEX t_g"],
                ),
                // An LSM tree reads only the keys in the range, in key order.
                points(
                    "1%, LSM key range",
                    5,
                    |i| format!("SELECT * FROM l WHERE id >= {i} AND id < {}", i + 4000),
                    &["USING PRIMARY KEY"],
                ),
            ],
        ),
    ];

    common::run_queries(
        groups,
        &setup(),
        &format!("{ROWS} rows in each of four tables"),
    );
}

/// A batch of `count` queries made by `query` from row numbers spread out
/// over the first 196,000 rows, which leaves 4,000 rows above each.
fn points(
    name: &str,
    count: i64,
    query: impl Fn(i64) -> String,
    plan: &'static [&'static str],
) -> Batch {
    let statements = (0..count)
        .map(|j| query(j * 7_919 % (ROWS - 4000)))
        .collect();
    batch(name, statements, plan).reading("note")
}

/// A batch of `count` queries, each reading the rows whose `column`, which
/// is `k` or an expression of it, is in a range `width` wide, about one
/// row in five.
fn ranges(
    name: &str,
    count: i64,
    width: i64,
    column: &str,
    plan: &'static [&'static str],
) -> Batch {
    let statements = (0..count)
        .map(|j| {
            let from = j * 104_729 % (P - width);
            format!(
                "SELECT * FROM t WHERE {column} >= {from} AND {column} < {}",
                from + width
            )
        })
        .collect();
    batch(name, statements, plan).reading("note")
}

fn k(i: i64) -> i64 {
    i * 48_271 % P
}

fn name(i: i64) -> String {
    format!("customer {:07}", i * 69_621 % P)
}

/// The statements that fill `t`, `h`, `l` and `c` with the same rows, in
/// `id` order, and index `t` and `h`. `k` and `name` are scattered over the
/// rows, and every one differs; `g` has 100 values, each in every hundredth
/// row. `l` holds only the even ids, so that odd ones fall between its keys.
fn setup() -> Vec<String> {
    let mut sql = Vec::new();
    for (table, engine) in [("t", "HEAP"), ("h", "HEAP"), ("l", "LSM"), ("c", "COLUMN")] {
        sql.push(format!("CREATE TABLE {table} (id INT PRIMARY KEY, k INT, g INT, name TEXT, note TEXT) USING {engine}"));
        let id = |i: i64| if table == "l" { i * 2 } else { i };
        insert(&mut sql, table, ROWS, |i| {
            format!(
                "({}, {}, {}, '{}', 'row {i} of the lookup benchmark')",
                id(i),
                k(i),
                i % 100,
                name(i)
            )
        });
    }
    sql.extend(
        [
            "CREATE UNIQUE INDEX t_k ON t (k)",
            "CREATE INDEX t_g ON t (g)",
            "CREATE INDEX t_name ON t (name)",
        ]
        .map(String::from),
    );
    sql.extend(
        [
            "CREATE UNIQUE INDEX h_k ON h USING HASH (k)",
            "CREATE INDEX h_name ON h USING HASH (name)",
        ]
        .map(String::from),
    );
    sql
}
