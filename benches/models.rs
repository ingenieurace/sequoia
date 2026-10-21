//! Times the same queries in each execution model: `cargo bench --bench
//! models`, or with group names (`cargo bench --bench models -- olap`) for
//! only those groups. Prints the fastest of a few runs in each model, the
//! model the query runs in by default (marked `*` where it is the fastest
//! of the three), and checks that every model returns the same rows.
//!
//! The same 200,000 rows are in a heap table (`th`) and a column table
//! (`tc`). `oltp` looks a few rows up, many times over, where what costs is
//! starting a query and handing its few rows on; `olap` scans and
//! aggregates every row, where the cost per row is all; `stream` returns
//! many rows or stops early at a LIMIT, where holding rows costs.

mod common;

use common::{fastest, insert, shown, wanted};
use sequoia::executor::model::Model;
use sequoia::{Database, QueryResult};

const ROWS: i64 = 200_000;

/// A group's name and its cases: a name, a query (`{i}` is replaced by a
/// row's id), and how many times it runs, with other ids.
type Group = (&'static str, Vec<(&'static str, &'static str, usize)>);

fn main() {
    let groups: Vec<Group> = vec![
        (
            "oltp",
            vec![
                ("1,000 lookups by key", "SELECT name, k FROM th WHERE id = {i}", 1000),
                ("1,000 lookups, joined", "SELECT th.name, d.label FROM th JOIN d ON d.g = th.g WHERE th.id = {i}", 1000),
                ("1,000 index ranges", "SELECT id, k FROM th WHERE id >= {i} AND id < {i} + 20", 1000),
            ],
        ),
        (
            "olap",
            vec![
                ("COUNT, SUM, AVG (heap)", "SELECT COUNT(*), SUM(k), AVG(x) FROM th", 1),
                ("COUNT, SUM, AVG (column)", "SELECT COUNT(*), SUM(k), AVG(x) FROM tc", 1),
                ("filter, then SUM (column)", "SELECT SUM(x) FROM tc WHERE k > 100 AND g < 50", 1),
                ("arithmetic in SUM (column)", "SELECT SUM(k * 2 + g), MIN(x - k) FROM tc", 1),
                ("GROUP BY 100 (column)", "SELECT g, COUNT(*), SUM(k) FROM tc GROUP BY g", 1),
                ("GROUP BY 100 (heap)", "SELECT g, COUNT(*), SUM(k) FROM th GROUP BY g", 1),
                ("filter, project (column)", "SELECT id, k + g, x * 2.0 FROM tc WHERE k < 50000", 1),
            ],
        ),
        (
            "stream",
            vec![
                ("every row (heap)", "SELECT id, name FROM th", 1),
                ("LIMIT 10 of a filter (heap)", "SELECT id FROM th WHERE g = 7 LIMIT 10", 200),
                ("hash join, every row", "SELECT th.id, d.label FROM th JOIN d ON d.g = th.g", 1),
            ],
        ),
    ];

    let mut setup = vec![
        "CREATE TABLE th (id INT PRIMARY KEY, k INT, g INT, x REAL, name TEXT)".to_string(),
        "CREATE TABLE tc (id INT PRIMARY KEY, k INT, g INT, x REAL, name TEXT) USING COLUMN".to_string(),
        "CREATE TABLE d (g INT PRIMARY KEY, label TEXT)".to_string(),
    ];
    let row = |i: i64| format!("({i}, {}, {}, {}.25, 'name{}')", (i * 7919) % ROWS, i % 100, i % 1000, i % 5000);
    insert(&mut setup, "th", ROWS, row);
    insert(&mut setup, "tc", ROWS, row);
    insert(&mut setup, "d", 100, |g| format!("({g}, 'label{g}')"));
    let mut db = Database::in_memory();
    for statement in &setup {
        db.execute(statement).unwrap();
    }

    for (group, cases) in groups.into_iter().filter(|(group, _)| wanted(group)) {
        println!("\n{group:<30} {:>11} {:>15} {:>11}   default", "iterator", "materialization", "vectorized");
        for (name, sql, times) in cases {
            let queries: Vec<String> = (0..times).map(|i| sql.replace("{i}", &((i as i64 * 997) % ROWS).to_string())).collect();
            let mut results = Vec::new();
            let mut line = format!("{name:<30}");
            for (model, width) in Model::ALL.into_iter().zip([11, 15, 11]) {
                db.set_execution_model(Some(model));
                let (time, rows) = fastest(|| run(&mut db, &queries));
                line += &format!(" {:>width$}", shown(time));
                results.push((model, time, rows));
            }
            assert!(results.windows(2).all(|pair| pair[0].2 == pair[1].2), "{name}: the models disagree");
            db.set_execution_model(None);
            let chosen = chosen(&mut db, &queries[0]);
            let best = results.iter().min_by_key(|(_, time, _)| *time).expect("three models").0;
            line += &format!("   {chosen}{}", if chosen == best { " *" } else { "" });
            println!("{line}");
        }
    }
}

/// Runs the queries, and returns the rows of the last one.
fn run(db: &mut Database, queries: &[String]) -> Vec<Vec<sequoia::Value>> {
    let mut last = Vec::new();
    for sql in queries {
        match db.execute(sql).unwrap().pop() {
            Some(QueryResult::Rows { rows, .. }) => last = rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }
    last
}

/// The model a query runs in by default, as EXPLAIN ANALYZE names it.
fn chosen(db: &mut Database, sql: &str) -> Model {
    let Some(QueryResult::Message(plan)) = db.execute(&format!("EXPLAIN ANALYZE {sql}")).unwrap().pop() else {
        panic!("expected a plan");
    };
    let name = plan.lines().next().and_then(|line| line.strip_prefix("MODEL ")).and_then(|rest| rest.split(' ').next());
    name.and_then(Model::from_name).unwrap_or_else(|| panic!("no model in {plan}"))
}
