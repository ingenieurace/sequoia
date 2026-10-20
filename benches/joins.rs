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

mod common;

use common::{case, insert};

const LEFT: i64 = 200_000;
const RIGHT: i64 = 50_000;

/// A condition true of every row of `a`, of four comparisons that the
/// planner guesses keep a third of the rows each: 1 in 81 in all.
macro_rules! all_rows {
    () => {
        "a.k >= 0 AND a.k <= 1000000 AND a.k > -1 AND a.k < 1000001"
    };
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
                // Past its budget the right side is partitioned: most of b and of a go to disk and back.
                case("int key, 1 MB of b held", "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k", &["HASH JOIN"])
                    .hash_memory(1 << 20),
                case("int key, 64 KB of b held", "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k", &["HASH JOIN"])
                    .hash_memory(64 << 10),
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

    common::run(groups.into(), &setup(), &format!("{LEFT} rows joined with {RIGHT}"));
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
