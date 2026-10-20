//! Times GROUP BY, aggregates and DISTINCT: `cargo bench --bench aggregate`,
//! or with group names (`cargo bench --bench aggregate -- groups distinct`)
//! for only those groups. Prints the fastest of a few runs of each query, to
//! compare changes to the aggregate step by, and checks that each query's
//! plan aggregates as it means to.
//!
//! `groups` counts rows into more and more groups, by int and text keys,
//! `functions` times each aggregate over the same 100 groups, `distinct`
//! times `COUNT(DISTINCT ...)` and `SELECT DISTINCT`, and `around` the steps
//! above an aggregate: HAVING, ORDER BY and a top 10.
//!
//! With a `sqlite3` command, the same statements fill the same table in
//! SQLite and every query is timed there too, for a column beside Sequoia's
//! and the ratio of the two; `--no-sqlite` leaves it out. SQLite groups by
//! sorting where Sequoia hashes, and still computes the aggregates of the
//! counted subquery (`EXPLAIN` shows an `AggStep` for each).

mod common;

use common::{case, insert};

const ROWS: i64 = 200_000;

fn main() {
    let groups = vec![
        (
            "groups",
            vec![
                // What the others cost beyond this is grouping's own. The WHERE
                // is true of every row: without it, SQLite counts the rows
                // without reading them.
                case("one group", "SELECT COUNT(*) FROM t WHERE k >= 0", &["AGGREGATE COUNT(*)"]),
                case("100 groups", "SELECT g, COUNT(*) FROM t GROUP BY g", &["BY g"]),
                case("10,000 groups", "SELECT h, COUNT(*) FROM t GROUP BY h", &["BY h"]),
                // Every row is a group of its own.
                case("200,000 groups", "SELECT k, COUNT(*) FROM t GROUP BY k", &["BY k"]),
                case("10,000 groups, text key", "SELECT city, COUNT(*) FROM t GROUP BY city", &["BY city"]),
                case("200,000 groups, text key", "SELECT name, COUNT(*) FROM t GROUP BY name", &["BY name"]),
                case("10,000 groups, two keys", "SELECT g, h, COUNT(*) FROM t GROUP BY g, h", &["BY g, h"]),
                case("1,001 groups, expression", "SELECT k / 1000, COUNT(*) FROM t GROUP BY k / 1000", &["BY k / 1000"]),
            ],
        ),
        (
            "functions",
            vec![
                case("COUNT(*)", "SELECT g, COUNT(*) FROM t GROUP BY g", &["COUNT(*)"]),
                // A third of the values are NULL, and skipped.
                case("COUNT(maybe)", "SELECT g, COUNT(maybe) FROM t GROUP BY g", &["COUNT(maybe)"]),
                case("SUM(k)", "SELECT g, SUM(k) FROM t GROUP BY g", &["SUM(k)"]),
                case("AVG(k)", "SELECT g, AVG(k) FROM t GROUP BY g", &["AVG(k)"]),
                case("MIN(k)", "SELECT g, MIN(k) FROM t GROUP BY g", &["MIN(k)"]),
                case("MAX(k)", "SELECT g, MAX(k) FROM t GROUP BY g", &["MAX(k)"]),
                case("MIN(name)", "SELECT g, MIN(name) FROM t GROUP BY g", &["MIN(name)"]),
                case(
                    "all five",
                    "SELECT g, COUNT(*), SUM(k), AVG(k), MIN(k), MAX(k) FROM t GROUP BY g",
                    &["COUNT(*), SUM(k), AVG(k), MIN(k), MAX(k) BY g"],
                ),
            ],
        ),
        (
            "distinct",
            vec![
                case("COUNT(DISTINCT g)", "SELECT COUNT(DISTINCT g) FROM t", &["COUNT(DISTINCT g)"]),
                case("COUNT(DISTINCT k)", "SELECT COUNT(DISTINCT k) FROM t", &["COUNT(DISTINCT k)"]),
                // 100 groups of 100 values, each 20 times.
                case("COUNT(DISTINCT h) BY g", "SELECT g, COUNT(DISTINCT h) FROM t GROUP BY g", &["COUNT(DISTINCT h) BY g"]),
                // 100 groups of 2,000 values, each once.
                case("COUNT(DISTINCT k) BY g", "SELECT g, COUNT(DISTINCT k) FROM t GROUP BY g", &["COUNT(DISTINCT k) BY g"]),
                case("SELECT DISTINCT, 100", "SELECT DISTINCT g FROM t", &["DISTINCT"]),
                case("SELECT DISTINCT, 10,000", "SELECT DISTINCT h FROM t", &["DISTINCT"]),
                case("SELECT DISTINCT, 200,000", "SELECT DISTINCT k FROM t", &["DISTINCT"]),
                case("SELECT DISTINCT, text", "SELECT DISTINCT city FROM t", &["DISTINCT"]),
            ],
        ),
        (
            "around",
            vec![
                case("GROUP BY alone", "SELECT h, SUM(k) FROM t GROUP BY h", &["BY h"]),
                case(
                    "HAVING",
                    "SELECT h, SUM(k) FROM t GROUP BY h HAVING SUM(k) > 10000000",
                    &["FILTER SUM(k) > 10000000", "BY h"],
                ),
                case("ORDER BY key", "SELECT h, SUM(k) FROM t GROUP BY h ORDER BY h", &["SORT BY h", "BY h"]),
                case(
                    "ORDER BY SUM LIMIT 10",
                    "SELECT h, SUM(k) FROM t GROUP BY h ORDER BY SUM(k) DESC LIMIT 10",
                    &["TOP 10", "BY h"],
                ),
                // A tenth of the rows reach the aggregate.
                case("after WHERE", "SELECT h, SUM(k) FROM t WHERE k < 100000 GROUP BY h", &["FILTER", "BY h"]),
            ],
        ),
    ];

    common::run(groups, &setup(), &format!("{ROWS} rows aggregated"));
}

/// The statements that fill `t`, in `id` order. `k` and `name` are scattered
/// over the rows, and every one differs; `g` has 100 values, each in every
/// hundredth row; `h` has 10,000, scattered, each in 20 rows, and each `g`
/// goes with 100 of them, so `g, h` makes 10,000 groups too; `city` is `h`
/// as text; `maybe` is `k` or, in every third row, NULL.
fn setup() -> Vec<String> {
    let mut sql =
        vec!["CREATE TABLE t (id INT PRIMARY KEY, k INT, g INT, h INT, city TEXT, name TEXT, maybe INT)".to_string()];
    // i * m mod a prime larger than ROWS is different for every row.
    const P: i64 = 1_000_003;
    insert(&mut sql, "t", ROWS, |i| {
        let k = i * 48_271 % P;
        // 7,919 is prime, so this takes each of 10,000 values every 10,000 rows.
        let h = i * 7_919 % 10_000;
        let name = format!("customer {:07}", i * 69_621 % P);
        let maybe = if i % 3 == 0 { "NULL".to_string() } else { k.to_string() };
        format!("({i}, {k}, {}, {h}, 'city {h:04}', '{name}', {maybe})", i % 100)
    });
    sql
}
