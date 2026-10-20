//! Times ORDER BY: `cargo bench --bench sort`, or with group names
//! (`cargo bench --bench sort -- spill top`) for only those groups. Prints
//! the fastest of a few runs of each query, to compare changes to the sort
//! by, and checks that each query's plan sorts.
//!
//! `memory` sorts with a budget every row fits in, `spill` with smaller and
//! smaller budgets, so that more and more runs are written out and merged
//! back, over more passes, and `top` with a LIMIT. `+ 0` on `id` hides the
//! primary key's order from the planner, so rows already in order are sorted
//! too.
//!
//! With a `sqlite3` command, the same statements fill the same table in
//! SQLite and every query is timed there too, for a column beside Sequoia's
//! and the ratio of the two; `--no-sqlite` leaves it out. SQLite has its own
//! memory budget for sorting, which the cases' budgets do not change.

mod common;

use common::{case, insert};

const ROWS: i64 = 200_000;
/// A budget no sort here outgrows.
const ALL: usize = 1 << 30;

fn main() {
    let groups = vec![
        (
            "memory",
            vec![
                // What the sorts below cost beyond these is the sort's own. The
                // WHERE is true of every row: without it, SQLite counts the rows
                // without reading them.
                case("scan alone", "SELECT id, k FROM t WHERE k >= 0", &[]),
                case("scan every column", "SELECT * FROM t WHERE k >= 0", &[]),
                case("int key", "SELECT id, k FROM t ORDER BY k", &["SORT BY"]).sort_memory(ALL),
                case("int key, descending", "SELECT id, k FROM t ORDER BY k DESC", &["SORT BY"]).sort_memory(ALL),
                case("text key", "SELECT id, name FROM t ORDER BY name", &["SORT BY"]).sort_memory(ALL),
                // 100 distinct keys: rows tie, and come out in arrival order.
                case("many ties", "SELECT id, g FROM t ORDER BY g", &["SORT BY"]).sort_memory(ALL),
                case("two keys", "SELECT id, g, name FROM t ORDER BY g, name DESC", &["SORT BY"]).sort_memory(ALL),
                case("already in order", "SELECT id, k FROM t ORDER BY id + 0", &["SORT BY"]).sort_memory(ALL),
                case("in reverse order", "SELECT id, k FROM t ORDER BY id + 0 DESC", &["SORT BY"]).sort_memory(ALL),
                case("every column", "SELECT * FROM t ORDER BY k", &["SORT BY"]).sort_memory(ALL),
            ],
        ),
        (
            "spill",
            vec![
                case("int key, 4 MB", "SELECT id, k FROM t ORDER BY k", &["SORT BY"]),
                case("int key, 1 MB", "SELECT id, k FROM t ORDER BY k", &["SORT BY"]).sort_memory(1 << 20),
                case("int key, 256 KB", "SELECT id, k FROM t ORDER BY k", &["SORT BY"]).sort_memory(256 << 10),
                // Hundreds of runs: merged a group at a time, over several passes.
                case("int key, 64 KB", "SELECT id, k FROM t ORDER BY k", &["SORT BY"]).sort_memory(64 << 10),
                case("text key, 4 MB", "SELECT id, name FROM t ORDER BY name", &["SORT BY"]),
                case("already in order, 4 MB", "SELECT id, k FROM t ORDER BY id + 0", &["SORT BY"]),
                case("every column, 4 MB", "SELECT * FROM t ORDER BY k", &["SORT BY"]),
                case("every column, 256 KB", "SELECT * FROM t ORDER BY k", &["SORT BY"]).sort_memory(256 << 10),
            ],
        ),
        (
            "top",
            vec![
                case("LIMIT 10", "SELECT id, k FROM t ORDER BY k LIMIT 10", &["TOP 10"]),
                case("LIMIT 10, text key", "SELECT id, name FROM t ORDER BY name LIMIT 10", &["TOP 10"]),
                case("LIMIT 10,000", "SELECT id, k FROM t ORDER BY k LIMIT 10000", &["TOP 10000"]),
                // The best rows so far outgrow the budget: runs of them are written out.
                case("LIMIT 100,000", "SELECT id, k FROM t ORDER BY k LIMIT 100000", &["TOP 100000"]),
                case("LIMIT 100,000, 256 KB", "SELECT id, k FROM t ORDER BY k LIMIT 100000", &["TOP 100000"])
                    .sort_memory(256 << 10),
            ],
        ),
    ];

    common::run(groups, &setup(), &format!("{ROWS} rows sorted"));
}

/// The statements that fill `t`, in `id` order. `k` and `name` are in other
/// orders, scattered over the rows, and every one differs; `g` has 100
/// values, each in every hundredth row. `note` makes a row wider.
fn setup() -> Vec<String> {
    let mut sql = vec!["CREATE TABLE t (id INT PRIMARY KEY, k INT, g INT, name TEXT, note TEXT)".to_string()];
    // i * m mod a prime larger than ROWS is different for every row.
    const P: i64 = 1_000_003;
    insert(&mut sql, "t", ROWS, |i| {
        let k = i * 48_271 % P;
        let name = format!("customer {:07}", i * 69_621 % P);
        format!("({i}, {k}, {}, '{name}', 'row {i} of the sort benchmark, a note to widen it')", i % 100)
    });
    sql
}
