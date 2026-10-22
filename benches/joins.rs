//! Compares the join methods: `cargo bench --bench joins`, or with query
//! names (`cargo bench --bench joins -- int few`) for only those queries.
//!
//! Prints a table: a row for each query, a column for each join method. In
//! a column, every join of the query is made to use that method
//! ([`Database::set_force_join`]), so a row's methods all run the same SQL on
//! the same rows, and must return the same rows. Each cell is the fastest of
//! up to five runs and how many times slower that is than the row's fastest
//! method ("best"); "n/a" where the method cannot run the query (a hash,
//! index or merge join needs an equality between the two sides), "skipped"
//! where it would take too long: a nested loop join tries every pair of
//! rows, ten billion of them for 200,000 left rows by 50,000 right ones.
//! "parallel hash" is the hash join on [`WORKERS`] threads, a parallel grace
//! hash join ([`Database::set_join_workers`]), whose tables are scanned on
//! as many ([`Database::set_scan_workers`]); "parallel index" the index join
//! on as many, whose workers each scan pages of the left side and look its
//! rows up. Below the table, the SQL of each query.

mod common;

use std::time::{Duration, Instant};

use common::insert;
use sequoia::plan::query::ForceJoin;
use sequoia::{Database, QueryResult, Value};

const LEFT: i64 = 200_000;
const RIGHT: i64 = 50_000;

/// Runs of each query, at most: fewer for one that takes over a second.
const RUNS: usize = 5;
const LONG: Duration = Duration::from_secs(1);

/// Threads the parallel hash and index joins run on.
const WORKERS: usize = 8;

/// The join methods, each a column: the method forced, on how many
/// threads, its heading, and what its plan says (one of the `|` ones).
const METHODS: [(ForceJoin, usize, &str, &str); 7] = [
    (ForceJoin::Hash, 1, "hash", "HASH JOIN"),
    (
        ForceJoin::Hash,
        WORKERS,
        "parallel hash",
        // The whole join gathered; or, with a side read otherwise than by
        // a heap's full scan, a parallel hash join.
        "GATHER (8 WORKERS)|PARALLEL HASH JOIN",
    ),
    (ForceJoin::Index, 1, "index", "INDEX JOIN"),
    // Gathered whole, or its left side only, or not at all, as its sides allow.
    (ForceJoin::Index, WORKERS, "parallel index", "INDEX JOIN"),
    (ForceJoin::Merge, 1, "merge", "MERGE JOIN"),
    (
        ForceJoin::Block,
        1,
        "block nested",
        "BLOCK NESTED LOOP JOIN",
    ),
    (
        ForceJoin::Naive,
        1,
        "naive nested",
        "NAIVE NESTED LOOP JOIN",
    ),
];
const NESTED: &[ForceJoin] = &[ForceJoin::Block, ForceJoin::Naive];

/// A row of the table: a query, and how each method runs it.
struct Query {
    /// What picks it on the command line.
    name: &'static str,
    /// What the row is labelled.
    label: &'static str,
    sql: &'static str,
    /// Methods that would take too long.
    skipped: &'static [ForceJoin],
    /// The memory each join and sort may hold rows in, if not the default.
    memory: Option<usize>,
}

const fn query(name: &'static str, label: &'static str, sql: &'static str) -> Query {
    Query {
        name,
        label,
        sql,
        skipped: &[],
        memory: None,
    }
}

impl Query {
    const fn skipping(self, skipped: &'static [ForceJoin]) -> Query {
        Query { skipped, ..self }
    }

    const fn memory(self, bytes: usize) -> Query {
        Query {
            memory: Some(bytes),
            ..self
        }
    }
}

/// Each `a.k` and each `a.name` finds one row of `b`, and each `a.k` one
/// row of `bl` by its `id` (see [`setup`]).
const QUERIES: [Query; 11] = [
    query(
        "int",
        "int key, 200k x 50k",
        "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k",
    )
    .skipping(NESTED),
    query(
        "text",
        "text key, 200k x 50k",
        "SELECT COUNT(*) FROM a JOIN b ON b.name = a.name",
    )
    .skipping(NESTED),
    query(
        "lsm",
        "LSM primary key, 200k x 50k",
        "SELECT COUNT(*) FROM a JOIN bl ON bl.id = a.k",
    )
    .skipping(NESTED),
    query(
        "few",
        "1k left rows x 50k",
        "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k WHERE a.id < 1000",
    ),
    query(
        "small",
        "1k x 2k",
        "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k WHERE a.id < 1000 AND b.id < 2000",
    ),
    query(
        "left",
        "LEFT JOIN, 1 in 10 match",
        "SELECT COUNT(*) FROM a LEFT JOIN b ON b.k = a.k + 45000",
    )
    .skipping(NESTED),
    query(
        "order",
        "ORDER BY the key",
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k ORDER BY a.k",
    )
    .skipping(NESTED),
    query(
        "top",
        "ORDER BY the key LIMIT 10",
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k ORDER BY a.k LIMIT 10",
    )
    .skipping(NESTED),
    query(
        "memory",
        "int key, in 64 KB",
        "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k",
    )
    .skipping(NESTED)
    .memory(64 << 10),
    query(
        "range",
        "b.k < a.k, 1k x 2k",
        "SELECT COUNT(*) FROM a JOIN b ON b.k < a.k WHERE a.id < 1000 AND b.id < 2000",
    ),
    query(
        "range-1kb",
        "b.k < a.k, 1k x 2k, in 1 KB",
        "SELECT COUNT(*) FROM a JOIN b ON b.k < a.k WHERE a.id < 1000 AND b.id < 2000",
    )
    .memory(1 << 10),
];

/// What a cell of the table holds.
enum Cell {
    Ran(Duration),
    NotApplicable,
    Skipped,
}

fn main() {
    let queries: Vec<&Query> = QUERIES.iter().filter(|q| common::wanted(q.name)).collect();
    if queries.is_empty() {
        return;
    }
    println!(
        "Each row is one query; each column, the join method it is made to use. Times are the\n\
         fastest of up to {RUNS} runs, then how many times slower than the row's fastest method.\n\
         n/a: the method cannot run the query. skipped: it would take too long.\n\
         parallel hash, parallel index: each run by {WORKERS} threads at once.\n"
    );

    let start = Instant::now();
    let mut db = Database::in_memory();
    for sql in setup() {
        db.execute(&sql).unwrap();
    }
    let scan = |db: &mut Database, sql| common::shown(best(db, sql).0).trim().to_string();
    println!(
        "a: {LEFT} rows. b: {RIGHT} rows, indexed on k and on name. bl: the same rows, an LSM table keyed by id.\n\
         Filled in {:.1} s. Reading all of a alone takes {}, all of b {}.\n",
        start.elapsed().as_secs_f64(),
        scan(&mut db, "SELECT COUNT(a.k) FROM a"),
        scan(&mut db, "SELECT COUNT(b.k) FROM b"),
    );

    print!("   {:<30}", "query");
    for (_, _, heading, _) in METHODS {
        print!("{heading:>15}");
    }
    println!();
    for (i, query) in queries.iter().enumerate() {
        let cells = row(&mut db, query);
        let best = cells
            .iter()
            .filter_map(|cell| match cell {
                Cell::Ran(time) => Some(*time),
                _ => None,
            })
            .min()
            .expect("a method runs every query");
        print!("{:>2} {:<30}", i + 1, query.label);
        for cell in &cells {
            let shown = match cell {
                Cell::Ran(time) if *time == best => format!("{} best", short(*time)),
                Cell::Ran(time) => match ms(*time) / ms(best) {
                    r if r < 10.0 => format!("{} {r:>4.1}x", short(*time)),
                    r => format!("{} {r:>4.0}x", short(*time)),
                },
                Cell::NotApplicable => "n/a".into(),
                Cell::Skipped => "skipped".into(),
            };
            print!("{shown:>15}");
        }
        println!();
    }

    println!("\nThe queries:");
    for (i, query) in queries.iter().enumerate() {
        let memory = query
            .memory
            .map_or(String::new(), |bytes| format!("  (in {} KB)", bytes >> 10));
        println!("{:>2} {}{memory}", i + 1, query.sql);
    }
}

/// Runs `query` by each method: the cells of its row. Every method that
/// runs it must return the same rows, and its plan must use the method.
fn row(db: &mut Database, query: &Query) -> Vec<Cell> {
    let memory = query.memory;
    db.set_join_memory(memory.unwrap_or(sequoia::executor::joins::JOIN_MEMORY));
    db.set_hash_memory(memory.unwrap_or(sequoia::executor::joins::HASH_MEMORY));
    db.set_sort_memory(memory.unwrap_or(sequoia::executor::sort::SORT_MEMORY));
    let mut first: Option<(&str, Outcome)> = None;
    let cells = METHODS
        .iter()
        .map(|&(method, workers, heading, plan_word)| {
            db.set_force_join(Some(method));
            db.set_join_workers(workers);
            db.set_scan_workers(workers);
            let Some(plan) = explain(db, query.sql) else {
                return Cell::NotApplicable;
            };
            assert!(
                plan_word.split('|').any(|word| plan.contains(word)),
                "{}, by {heading}: no {plan_word} in\n{plan}",
                query.sql
            );
            if query.skipped.contains(&method) {
                return Cell::Skipped;
            }
            let (time, outcome) = best(db, query.sql);
            match &first {
                None => first = Some((heading, outcome)),
                Some((by, expected)) => assert_eq!(
                    &outcome, expected,
                    "{}: {heading} returns other rows than {by}",
                    query.sql
                ),
            }
            Cell::Ran(time)
        })
        .collect();
    db.set_force_join(None);
    db.set_join_workers(1);
    db.set_scan_workers(1);
    cells
}

/// The plan of a query, or None if it cannot be planned so.
fn explain(db: &mut Database, sql: &str) -> Option<String> {
    match db.execute(&format!("EXPLAIN {sql}")).ok()?.pop() {
        Some(QueryResult::Message(plan)) => Some(plan),
        other => panic!("expected a plan, got {other:?}"),
    }
}

/// The fastest of up to [`RUNS`] runs of a query, stopping after one that
/// takes over [`LONG`], and what it returned.
fn best(db: &mut Database, sql: &str) -> (Duration, Outcome) {
    let mut best = Duration::MAX;
    let mut outcome = None;
    for _ in 0..RUNS {
        let start = Instant::now();
        outcome = Some(run(db, sql));
        let time = start.elapsed();
        best = best.min(time);
        if time > LONG {
            break;
        }
    }
    (best, outcome.expect("RUNS > 0"))
}

/// What the methods of a row must agree on: how many rows the query
/// returns, and the row if it returns one only, such as a count.
#[derive(Debug, PartialEq)]
struct Outcome {
    rows: usize,
    lone: Option<Vec<Value>>,
}

fn run(db: &mut Database, sql: &str) -> Outcome {
    match db.execute(sql).unwrap().pop() {
        Some(QueryResult::Rows { rows, .. }) => Outcome {
            rows: rows.len(),
            lone: (rows.len() == 1).then(|| rows[0].clone()),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

fn ms(time: Duration) -> f64 {
    time.as_secs_f64() * 1000.0
}

/// A time in 7 columns or fewer: to two significant digits or so.
fn short(time: Duration) -> String {
    match ms(time) {
        t if t < 10.0 => format!("{t:.1} ms"),
        t if t < 1000.0 => format!("{t:.0} ms"),
        t => format!("{:.1} s", t / 1000.0),
    }
}

/// The statements that fill the tables: `a` (LEFT rows), `b` (RIGHT rows,
/// indexed on `k` and on `name`) and `bl`, an LSM table of the same rows,
/// whose primary key a join looks up. Each `a.k` matches one row of `b` and
/// of `bl`, and each `a.name` one in the first 20,000 of `b`.
fn setup() -> Vec<String> {
    let mut sql = vec![
        "CREATE TABLE a (id INT PRIMARY KEY, k INT, name TEXT)".to_string(),
        "CREATE TABLE b (id INT PRIMARY KEY, k INT, name TEXT)".into(),
        "CREATE TABLE bl (id INT PRIMARY KEY, k INT, name TEXT) USING LSM".into(),
    ];
    insert(&mut sql, "a", LEFT, |i| {
        format!("({i}, {}, 'customer {}')", i % RIGHT, i % 20_000)
    });
    for table in ["b", "bl"] {
        insert(&mut sql, table, RIGHT, |i| {
            format!("({i}, {i}, 'customer {i}')")
        });
    }
    sql.push("CREATE INDEX b_k ON b (k)".into());
    sql.push("CREATE INDEX b_name ON b (name)".into());
    sql
}
