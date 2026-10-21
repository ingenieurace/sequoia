//! Times reading rows: `cargo bench --bench scan`, or with group names
//! (`cargo bench --bench scan -- engines limit`) for only those groups.
//! Prints the fastest of a few runs of each query, and checks that each
//! query's plan reads its table as it means to.
//!
//! The same rows are stored three times: in a heap (`th`), an LSM tree
//! (`tl`) and a column store (`tc`). `engines` reads each in the same ways,
//! `zones` the column store's WHERE clauses that its zone maps let it skip
//! row groups for, or not, `filter` WHERE clauses that keep more and more rows, `project`
//! more and more columns and expressions, and `limit` a LIMIT without ORDER
//! BY, which stops the scan once it has its rows. Every other step costs
//! something on top of these.
//!
//! With a `sqlite3` command, the same statements fill the same tables in
//! SQLite (with no storage engines to choose: all three are B-trees) and
//! every query but those of `project` is timed there too; `--no-sqlite`
//! leaves it out. A query with no WHERE gets one true of every row,
//! `k >= 0`: SQLite counts the rows of a bare table without reading them.

mod common;

use common::{case, insert};

const ROWS: i64 = 200_000;

fn main() {
    let groups = vec![
        (
            "engines",
            vec![
                case("heap, every column", "SELECT * FROM th WHERE k >= 0", &["SCAN th"]),
                case("heap, two columns", "SELECT id, k FROM th WHERE k >= 0", &["SCAN th"]),
                case("heap, 1% of rows", "SELECT * FROM th WHERE k < 10000", &["SCAN th"]),
                case("LSM, every column", "SELECT * FROM tl WHERE k >= 0", &["SCAN tl"]),
                case("LSM, two columns", "SELECT id, k FROM tl WHERE k >= 0", &["SCAN tl"]),
                case("LSM, 1% of rows", "SELECT * FROM tl WHERE k < 10000", &["SCAN tl"]),
                // `k >= 0` is pushed down into the chunks too, and keeps every row.
                case("column, every column", "SELECT * FROM tc WHERE k >= 0", &["COMPRESSED CHUNKS ON k >= 0"]),
                case("column, two columns", "SELECT id, k FROM tc WHERE k >= 0", &["COMPRESSED CHUNKS ON k >= 0"]),
                // Only the rows that match are decoded from the other chunks.
                case("column, 1% of rows", "SELECT * FROM tc WHERE k < 10000", &["COMPRESSED CHUNKS ON k < 10000"]),
            ],
        ),
        (
            // The column table's zone maps: rows come in `id` order, so each
            // row group holds a narrow range of ids, and a scan skips the
            // groups whose range a predicate rules out without reading them.
            // `k` is scattered: every group holds nearly all of its range.
            "zones",
            vec![
                case("column, 1% by id, in order", "SELECT * FROM tc WHERE id < 2000", &["CHUNKS ON id < 2000"]),
                case("column, 1% by k, scattered", "SELECT * FROM tc WHERE k < 10000", &["CHUNKS ON k < 10000"]),
                case("column, one id", "SELECT * FROM tc WHERE id = 123456", &["CHUNKS ON id = 123456"]),
                case("column, 10% by id", "SELECT COUNT(*) FROM tc WHERE id >= 50000 AND id < 70000", &["CHUNKS ON id >="]),
                case("column, no id", "SELECT * FROM tc WHERE id > 500000", &["CHUNKS ON id > 500000"]),
            ],
        ),
        (
            "filter",
            vec![
                case("1% of rows", "SELECT id FROM th WHERE k < 10000", &["FILTER k < 10000"]),
                case("10% of rows", "SELECT id FROM th WHERE k < 100000", &["FILTER k < 100000"]),
                case("50% of rows", "SELECT id FROM th WHERE k < 500000", &["FILTER k < 500000"]),
                case("every row", "SELECT id FROM th WHERE k >= 0", &["FILTER k >= 0"]),
                case("text, 10% of rows", "SELECT id FROM th WHERE name < 'customer 0100000'", &["FILTER name <"]),
                case("AND, 1% of rows", "SELECT id FROM th WHERE k < 100000 AND g < 10", &["FILTER"]),
                case("OR, 2% of rows", "SELECT id FROM th WHERE k < 10000 OR g = 7", &["FILTER"]),
                case("IN list, 3% of rows", "SELECT id FROM th WHERE g IN (3, 7, 11)", &["FILTER"]),
                // Not of the form `column op literal`: evaluated on each row.
                case("expression, 1% of rows", "SELECT id FROM th WHERE k + 1 < 10001", &["FILTER (k + 1) < 10001"]),
                case("column, text, 10%", "SELECT id FROM tc WHERE name < 'customer 0100000'", &["COMPRESSED CHUNKS"]),
                case("column, AND, 1%", "SELECT id FROM tc WHERE k < 100000 AND g < 10", &["COMPRESSED CHUNKS"]),
                case("column, expression, 1%", "SELECT id FROM tc WHERE k + 1 < 10001", &["SCAN tc"]),
            ],
        ),
        (
            "project",
            // SQLite computes no column the count around the query leaves
            // unread, so it has nothing to compare here.
            vec![
                case("one column", "SELECT id FROM th WHERE k >= 0", &["PROJECT id"]).without_sqlite(),
                case("three columns", "SELECT id, k, g FROM th WHERE k >= 0", &["PROJECT id, k, g"]).without_sqlite(),
                case("every column", "SELECT * FROM th WHERE k >= 0", &["SCAN th"]).without_sqlite(),
                case("one sum", "SELECT k + 1 FROM th WHERE k >= 0", &["PROJECT k + 1"]).without_sqlite(),
                case("four operations", "SELECT (k * 2 + g - id) / 3 FROM th WHERE k >= 0", &["PROJECT"]).without_sqlite(),
                case("five expressions", "SELECT k + 1, k - 1, g * 2, id / 2, k + g FROM th WHERE k >= 0", &["PROJECT"]).without_sqlite(),
                case("column, one column", "SELECT id FROM tc WHERE k >= 0", &["PROJECT id"]).without_sqlite(),
                case("column, text column", "SELECT name FROM tc WHERE k >= 0", &["PROJECT name"]).without_sqlite(),
            ],
        ),
        (
            "limit",
            vec![
                case("heap, LIMIT 10", "SELECT * FROM th LIMIT 10", &["LIMIT 10"]),
                case("LSM, LIMIT 10", "SELECT * FROM tl LIMIT 10", &["LIMIT 10"]),
                case("column, LIMIT 10", "SELECT * FROM tc LIMIT 10", &["LIMIT 10"]),
                // About a thousand rows are read before ten match.
                case("heap, 1% then LIMIT 10", "SELECT * FROM th WHERE k < 10000 LIMIT 10", &["LIMIT 10"]),
                // No row matches: the scan reads them all.
                case("heap, none, LIMIT 10", "SELECT * FROM th WHERE k < 0 LIMIT 10", &["LIMIT 10"]),
                case("heap, LIMIT 100,000", "SELECT * FROM th LIMIT 100000", &["LIMIT 100000"]),
            ],
        ),
    ];

    common::run(groups, &setup(), &format!("{ROWS} rows in each of three tables"));
}

/// The statements that fill `th`, `tl` and `tc` with the same rows, in `id`
/// order. `k` and `name` are scattered over the rows, and every one differs;
/// `g` has 100 values, each in every hundredth row. `note` makes a row
/// wider.
fn setup() -> Vec<String> {
    let mut sql = Vec::new();
    for (table, engine) in [("th", "HEAP"), ("tl", "LSM"), ("tc", "COLUMN")] {
        sql.push(format!("CREATE TABLE {table} (id INT PRIMARY KEY, k INT, g INT, name TEXT, note TEXT) USING {engine}"));
        // i * m mod a prime larger than ROWS is different for every row.
        const P: i64 = 1_000_003;
        insert(&mut sql, table, ROWS, |i| {
            let k = i * 48_271 % P;
            let name = format!("customer {:07}", i * 69_621 % P);
            format!("({i}, {k}, {}, '{name}', 'row {i} of the scan benchmark, a note to widen it')", i % 100)
        });
    }
    sql
}
