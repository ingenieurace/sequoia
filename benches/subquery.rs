//! Times subqueries: `cargo bench --bench subquery`, or with group names
//! (`cargo bench --bench subquery -- correlated`) for only those groups.
//! Prints the fastest of a few runs of each query, and checks that each
//! query's plan has the subquery it means to time.
//!
//! `o` holds 200,000 orders of 1,000 customers, in `c`. `uncorrelated`
//! times subqueries that refer to nothing around them and run once.
//! `correlated` times subqueries that do: each runs once per distinct set
//! of the outer values it refers to, so the same 200,000 rows cost 10, 1,000
//! or 10,000 runs of it. `derived` times subqueries in FROM.
//!
//! With a `sqlite3` command, the same statements fill the same tables in
//! SQLite and every query is timed there too; `--no-sqlite` leaves it out.
//! SQLite computes no column that the count around the query leaves unread,
//! so a subquery in the select list is timed in Sequoia alone.

mod common;

use common::{case, insert};

const ORDERS: i64 = 200_000;
const CUSTOMERS: i64 = 1_000;

fn main() {
    let groups = vec![
        (
            "uncorrelated",
            vec![
                // What the queries below cost beyond this is the subquery's.
                case(
                    "no subquery",
                    "SELECT id FROM o WHERE amount >= 0",
                    &["SCAN o"],
                ),
                case(
                    "IN, 1% of customers",
                    "SELECT id FROM o WHERE customer IN (SELECT id FROM c WHERE region = 7)",
                    &["IN (SELECT"],
                ),
                case(
                    "NOT IN, 1% of customers",
                    "SELECT id FROM o WHERE customer NOT IN (SELECT id FROM c WHERE region = 7)",
                    &["NOT IN (SELECT"],
                ),
                case(
                    "scalar, an aggregate",
                    "SELECT id FROM o WHERE amount > (SELECT AVG(amount) FROM o)",
                    &["(SELECT AVG"],
                ),
                case(
                    "EXISTS",
                    "SELECT id FROM o WHERE EXISTS (SELECT id FROM c WHERE region = 7)",
                    &["EXISTS (SELECT"],
                ),
            ],
        ),
        (
            "correlated",
            vec![
                // `kind` has 10 values: the subquery reads `c` 10 times.
                case(
                    "10 outer values",
                    "SELECT id FROM o WHERE amount > (SELECT AVG(credit) FROM c WHERE c.region = o.kind)",
                    &["(SELECT AVG(credit)"],
                ),
                // Looks up 1,000 customers by key, once each.
                case(
                    "1,000 outer values",
                    "SELECT id FROM o WHERE amount > (SELECT credit FROM c WHERE c.id = o.customer)",
                    &["(SELECT credit"],
                ),
                case(
                    "1,000 values, 10,000 rows",
                    "SELECT id FROM o WHERE id < 10000 AND amount > (SELECT credit FROM c WHERE c.id = o.customer)",
                    &["(SELECT credit"],
                ),
                // The same lookups, but `o.id` makes every row's outer values differ.
                case(
                    "10,000 outer values",
                    "SELECT id FROM o WHERE id < 10000 AND amount > (SELECT credit FROM c WHERE c.id = o.customer AND o.id >= 0)",
                    &["(SELECT credit"],
                ),
                // Each run looks up a customer's 200 orders by an index.
                case(
                    "EXISTS, by an index",
                    "SELECT id FROM c WHERE EXISTS (SELECT id FROM o WHERE o.customer = c.id AND o.amount > 999000)",
                    &["EXISTS (SELECT"],
                ),
                case(
                    "NOT EXISTS, by an index",
                    "SELECT id FROM c WHERE NOT EXISTS (SELECT id FROM o WHERE o.customer = c.id AND o.amount > 999000)",
                    &["NOT EXISTS (SELECT"],
                ),
                case(
                    "IN, by an index",
                    "SELECT id FROM c WHERE 999 IN (SELECT amount / 1000 FROM o WHERE o.customer = c.id)",
                    &["IN (SELECT"],
                ),
                case(
                    "in the select list",
                    "SELECT id, (SELECT region FROM c WHERE c.id = o.customer) FROM o",
                    &["(SELECT region"],
                )
                .without_sqlite(),
            ],
        ),
        (
            "derived",
            vec![
                case(
                    "aggregate, then filter",
                    "SELECT d.customer FROM (SELECT customer, SUM(amount) AS total FROM o GROUP BY customer) d \
                     WHERE d.total > 100000000",
                    &["SUBQUERY d"],
                ),
                case(
                    "joined to a table",
                    "SELECT c.id, d.n FROM c JOIN (SELECT customer, COUNT(*) AS n FROM o GROUP BY customer) d \
                     ON d.customer = c.id",
                    &["SUBQUERY d", "JOIN"],
                ),
                case(
                    "filter pushed through",
                    "SELECT d.id FROM (SELECT id, amount FROM o) d WHERE d.amount < 10000",
                    &["SUBQUERY d"],
                ),
            ],
        ),
    ];

    common::run(
        groups,
        &setup(),
        &format!("{ORDERS} orders of {CUSTOMERS} customers"),
    );
}

/// The statements that fill `c` and `o`, and index `o` by customer. A
/// customer's `region` is one of 100, each that of 10 customers; `credit`
/// is scattered. An order's `amount` is scattered, and every one differs;
/// `kind` is one of 10. Each customer has 200 orders, one in every 1,000.
fn setup() -> Vec<String> {
    // i * m mod a prime larger than the rows is different for every row.
    const P: i64 = 1_000_003;
    let mut sql = vec![
        "CREATE TABLE c (id INT PRIMARY KEY, region INT, credit INT)".to_string(),
        "CREATE TABLE o (id INT PRIMARY KEY, customer INT, kind INT, amount INT)".to_string(),
    ];
    insert(&mut sql, "c", CUSTOMERS, |i| {
        format!("({i}, {}, {})", i % 100, i * 911 % P)
    });
    insert(&mut sql, "o", ORDERS, |i| {
        format!("({i}, {}, {}, {})", i % CUSTOMERS, i % 10, i * 48_271 % P)
    });
    sql.push("CREATE INDEX o_customer ON o (customer)".into());
    sql
}
