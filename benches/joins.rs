//! Compares the join methods: `cargo bench --bench joins`, or with group
//! names (`cargo bench --bench joins -- equi few`) for only those groups.
//!
//! Each group is one query, and each of its cases a join method running it,
//! on the same rows: the fastest of a few runs, and how many times slower
//! that is than the group's fastest method. Every method of a group must
//! return the same rows, and each case's plan must use the method it names.
//!
//! The planner picks one method for a query, so each case makes it pick
//! another, spelled so that what is left to do is the same:
//!
//! - hash: `+ 0` on the right side's key, which an index cannot look up;
//! - index: a WHERE true of every row of `a` ([`ALL_ROWS`]), but which the
//!   planner guesses keeps 1 in 81, so few that looking each up wins. Every
//!   case of a group that has an index join has it, to do the same work;
//! - merge: ORDER BY the key, which a merge join sorts its sides by. For a
//!   join whose order no one asks for, that is in a subquery whose rows are
//!   counted, as every case of the group counts them;
//! - block nested loop: `bi.k - a.k = 0` rather than `bi.k = a.k`, which is
//!   no equality between the two sides. As it pairs every left row with
//!   every right one, it runs only on a few thousand rows (in `small`).
//!
//! The `naive` group times the naive nested loop join, which no plan runs,
//! against the block nested loop join: it builds the joins itself, on rows
//! in memory, rather than running SQL.

mod common;

use common::{case, insert};
use sequoia::Value;
use sequoia::catalog::table::Row;
use sequoia::executor::joins::{BlockJoin, JOIN_MEMORY, NaiveJoin};
use sequoia::executor::operator::Rows;
use sequoia::sql::ast::{BinaryOp, ColumnRef, Expr, JoinKind};
use sequoia::sql::eval::Field;

const LEFT: i64 = 200_000;
const RIGHT: i64 = 50_000;

/// A condition true of every row of `a`, of four comparisons that the
/// planner guesses keep a third of the rows each: 1 in 81 in all.
const ALL_ROWS: &str = "a.k >= 0 AND a.k <= 1000000 AND a.k > -1 AND a.k < 1000001";

const HASH: &[&str] = &["HASH JOIN"];
const INDEX: &[&str] = &["INDEX JOIN"];
const MERGE: &[&str] = &["MERGE JOIN"];
const BLOCK: &[&str] = &["BLOCK NESTED LOOP JOIN"];

/// The rows of a join counted, in a subquery, so that a merge join may sort
/// them inside it.
fn counted(select: String) -> String {
    format!("SELECT COUNT(*) FROM ({select}) t")
}

fn main() {
    // Each of these counts a join of `a` with the right side `join` names,
    // with ORDER BY `order` (or none) inside, for a merge join.
    let every = |join: &str, order: &str| {
        counted(format!(
            "SELECT a.id FROM a JOIN {join} WHERE {ALL_ROWS}{order}"
        ))
    };
    let few = |join: &str, order: &str| {
        counted(format!(
            "SELECT a.id FROM a JOIN {join} WHERE a.id < 1000 AND {ALL_ROWS}{order}"
        ))
    };
    let small = |join: &str, order: &str| {
        counted(format!(
            "SELECT a.id FROM a JOIN {join} WHERE a.id < 1000 AND bi.id < 2000 AND {ALL_ROWS}{order}"
        ))
    };
    let left = |join: &str, order: &str| {
        counted(format!(
            "SELECT a.id FROM a LEFT JOIN {join} WHERE {ALL_ROWS}{order}"
        ))
    };
    // These return the joined rows in a.k's order, or ten of them.
    let ordered = |on: &str, order: &str, limit: &str| {
        format!(
            "SELECT a.id, bi.id FROM a JOIN bi ON {on} WHERE {ALL_ROWS} ORDER BY {order}{limit}"
        )
    };
    let range = "SELECT COUNT(*) FROM a JOIN b ON b.k < a.k WHERE a.id < 1000 AND b.id < 2000";
    let range_left = "SELECT COUNT(*) FROM a LEFT JOIN b ON b.k > a.k + 1500 AND b.id < 2000 \
                      WHERE a.id < 1000";
    let aggregate = "SELECT COUNT(*) FROM a JOIN (SELECT k / 250 AS g, COUNT(*) AS n FROM b GROUP BY k / 250) t \
                     ON t.g * 250 < a.k WHERE a.id < 20000";

    let groups = vec![
        (
            "equi",
            (
                "all 200,000 rows of a, each finding one of 50,000 by an int key",
                vec![
                    case("hash", every("bi ON bi.k + 0 = a.k", ""), HASH),
                    case("index, B+ tree on k", every("bi ON bi.k = a.k", ""), INDEX),
                    case(
                        "index, LSM primary key",
                        every("bl ON bl.id = a.k", ""),
                        INDEX,
                    ),
                    case("merge", every("bi ON bi.k = a.k", " ORDER BY a.k"), MERGE),
                ],
            ),
        ),
        (
            "few",
            (
                "1,000 rows of a, each finding one of 50,000: a hash join reads all of them",
                vec![
                    case("hash", few("bi ON bi.k + 0 = a.k", ""), HASH),
                    case("index", few("bi ON bi.k = a.k", ""), INDEX),
                    case("merge", few("bi ON bi.k = a.k", " ORDER BY a.k"), MERGE),
                ],
            ),
        ),
        (
            "small",
            (
                "1,000 rows of a, each finding one of 2,000: two million pairs",
                vec![
                    case("hash", small("bi ON bi.k + 0 = a.k", ""), HASH),
                    case("index", small("bi ON bi.k = a.k", ""), INDEX),
                    case("merge", small("bi ON bi.k = a.k", " ORDER BY a.k"), MERGE),
                    case(
                        "block nested loop",
                        small("bi ON bi.k - a.k = 0", ""),
                        BLOCK,
                    ),
                ],
            ),
        ),
        (
            "text",
            (
                "all 200,000 rows of a, each finding one of 50,000 by a text key",
                vec![
                    case(
                        "hash",
                        counted("SELECT a.id FROM a JOIN b ON b.name = a.name".into()),
                        HASH,
                    ),
                    case(
                        "merge",
                        counted(
                            "SELECT a.id FROM a JOIN b ON b.name = a.name ORDER BY a.name".into(),
                        ),
                        MERGE,
                    ),
                ],
            ),
        ),
        (
            "left",
            (
                "LEFT JOIN: 1 in 10 of a's 200,000 rows finds one of 50,000",
                vec![
                    case("hash", left("bi ON bi.k + 0 = a.k + 45000", ""), HASH),
                    case("index", left("bi ON bi.k = a.k + 45000", ""), INDEX),
                    case(
                        "merge",
                        left("bi ON bi.k = a.k + 45000", " ORDER BY a.k + 45000"),
                        MERGE,
                    ),
                ],
            ),
        ),
        (
            "ordered",
            (
                "the 200,000 joined rows of `equi`, in a.k's order",
                vec![
                    case("merge", ordered("bi.k = a.k", "a.k", ""), MERGE),
                    case(
                        "hash, then sort",
                        ordered("bi.k + 0 = a.k", "a.k + 0", ""),
                        &["HASH JOIN", "SORT BY"],
                    ),
                    case(
                        "index, then sort",
                        ordered("bi.k = a.k", "a.k + 0", ""),
                        &["INDEX JOIN", "SORT BY"],
                    ),
                ],
            ),
        ),
        (
            "top",
            (
                "the first 10 joined rows of `equi` in a.k's order: a merge join sorts every row",
                vec![
                    case("merge", ordered("bi.k = a.k", "a.k", " LIMIT 10"), MERGE),
                    case(
                        "hash, then top 10",
                        ordered("bi.k + 0 = a.k", "a.k + 0", " LIMIT 10"),
                        &["HASH JOIN", "TOP 10"],
                    ),
                    case(
                        "index, then top 10",
                        ordered("bi.k = a.k", "a.k + 0", " LIMIT 10"),
                        &["INDEX JOIN", "TOP 10"],
                    ),
                ],
            ),
        ),
        (
            "memory",
            (
                "`equi` in little memory: a hash join partitions and writes out, a merge join's sorts write runs",
                vec![
                    case("hash, 1 MB", every("bi ON bi.k + 0 = a.k", ""), HASH)
                        .hash_memory(1 << 20),
                    case("hash, 64 KB", every("bi ON bi.k + 0 = a.k", ""), HASH)
                        .hash_memory(64 << 10),
                    case(
                        "merge, 1 MB",
                        every("bi ON bi.k = a.k", " ORDER BY a.k"),
                        MERGE,
                    )
                    .sort_memory(1 << 20),
                    case(
                        "merge, 64 KB",
                        every("bi ON bi.k = a.k", " ORDER BY a.k"),
                        MERGE,
                    )
                    .sort_memory(64 << 10),
                    case(
                        "index, holding no rows",
                        every("bi ON bi.k = a.k", ""),
                        INDEX,
                    ),
                ],
            ),
        ),
        // Without an equality, only a block nested loop join runs: these
        // compare its budgets, the fewer blocks the fewer reads of the right side.
        (
            "range",
            (
                "b.k < a.k, 1,000 rows of a by 2,000 of b",
                vec![
                    case("block, 1 block", range, BLOCK),
                    case("block, 100 blocks", range, BLOCK).join_memory(1000),
                ],
            ),
        ),
        (
            "range-left",
            (
                "LEFT JOIN on b.k > a.k + 1500, 1,000 rows of a by 2,000 of b",
                vec![
                    case(
                        "block, 1 block",
                        range_left,
                        &["LEFT BLOCK NESTED LOOP JOIN"],
                    ),
                    case(
                        "block, 100 blocks",
                        range_left,
                        &["LEFT BLOCK NESTED LOOP JOIN"],
                    )
                    .join_memory(1000),
                ],
            ),
        ),
        (
            "aggregate",
            (
                "20,000 rows of a by the 200 groups of an aggregate over b, kept rather than grouped again per block",
                vec![
                    case("block, 1 block", aggregate, BLOCK),
                    case("block, 100 blocks", aggregate, BLOCK).join_memory(20_000),
                ],
            ),
        ),
    ];

    // Fill the tables only for a group that queries them.
    if groups.iter().any(|(group, _)| common::wanted(group)) {
        common::compare(
            groups,
            &setup(),
            &format!("{LEFT} rows joined with {RIGHT}"),
            // What the joins cost beyond these is the join's own.
            &[
                ("scanning a alone", "SELECT COUNT(a.k) FROM a"),
                ("scanning bi alone", "SELECT COUNT(bi.k) FROM bi"),
            ],
        );
    }
    if common::wanted("naive") {
        naive();
    }
}

/// The naive nested loop join against the block nested loop join, on the
/// rows the `range` group joins: the first 1,000 of `a` and 2,000 of `b`,
/// their `id` and `k`, in memory. The naive join reads `b` again for each
/// left row, so 1,000 times; the block join with its default budget reads it
/// once and keeps it, and held to 1 KB reads it again for each block of a
/// dozen rows or so. Held to 1 byte, each block is one row and nothing is
/// kept: it reads `b` as often as the naive join, so the two differ only in
/// how they are built. Reading `b` again here copies its rows out of memory;
/// from a table it would scan the table's pages again too.
fn naive() {
    let left: Vec<Row> = (0..1000)
        .map(|i| vec![Value::Int(i), Value::Int(i % RIGHT)])
        .collect();
    let right: Vec<Row> = (0..2000)
        .map(|i| vec![Value::Int(i), Value::Int(i)])
        .collect();
    let fields = |table: &str| {
        ["id", "k"].map(|name| Field {
            table: Some(table.into()),
            name: name.into(),
        })
    };
    let column = |table: &str, name: &str| {
        Box::new(Expr::Column(ColumnRef {
            table: Some(table.into()),
            name: name.into(),
        }))
    };
    // b.k < a.k, and for the LEFT JOIN b.k > a.k + 1500, true of no right
    // row for a.k >= 499: half the left rows are padded.
    let inner = Expr::Binary(column("b", "k"), BinaryOp::Lt, column("a", "k"));
    let plus = Expr::Binary(
        column("a", "k"),
        BinaryOp::Add,
        Box::new(Expr::Literal(Value::Int(1500))),
    );
    let left_join = Expr::Binary(column("b", "k"), BinaryOp::Gt, Box::new(plus));

    let count = |join: &mut dyn Iterator<Item = sequoia::Result<Row>>| {
        let mut rows = 0;
        for row in join {
            row.unwrap();
            rows += 1;
        }
        rows
    };
    let naive = |kind, condition: &Expr| {
        let right = Box::new(|| rows(&right));
        let mut join = NaiveJoin::new(
            rows(&left),
            fields("a").into(),
            right,
            fields("b").into(),
            kind,
            Some(condition.clone()),
        );
        count(&mut join)
    };
    let block = |kind, condition: &Expr, budget| {
        let right = Box::new(|| rows(&right));
        let mut join = BlockJoin::new(
            rows(&left),
            fields("a").into(),
            right,
            fields("b").into(),
            kind,
            Some(condition.clone()),
            budget,
        );
        count(&mut join)
    };

    println!(
        "\nnaive: the naive nested loop join, which no plan runs, against the block one, in memory"
    );
    let cases: [(&str, Run); 6] = [
        ("naive", Box::new(|| naive(JoinKind::Inner, &inner))),
        (
            "  as block, b kept",
            Box::new(|| block(JoinKind::Inner, &inner, JOIN_MEMORY)),
        ),
        (
            "  as block, 1 KB",
            Box::new(|| block(JoinKind::Inner, &inner, 1000)),
        ),
        (
            "  as block, 1 byte",
            Box::new(|| block(JoinKind::Inner, &inner, 1)),
        ),
        (
            "naive, left join",
            Box::new(|| naive(JoinKind::Left, &left_join)),
        ),
        (
            "  as block, b kept",
            Box::new(|| block(JoinKind::Left, &left_join, JOIN_MEMORY)),
        ),
    ];
    // Each method's time against the naive join before it, which returns
    // the same rows.
    let mut expected = None;
    for (i, (name, run)) in cases.iter().enumerate() {
        let (time, rows) = common::fastest(run);
        if name.starts_with("naive") {
            expected = Some((time, rows));
        }
        let (naive_time, naive_rows) = expected.expect("a naive join first");
        assert_eq!(rows, naive_rows, "case {i}, {name}");
        let r = common::ratio(common::ms(time) / common::ms(naive_time));
        println!("  {name:<28} {}  {r}  ({rows} rows)", common::shown(time));
    }
}

/// A case of the `naive` group: runs a join, and returns how many rows it gave.
type Run<'a> = Box<dyn Fn() -> usize + 'a>;

/// Rows from memory, as a join reads them.
fn rows(rows: &[Row]) -> Rows<'_> {
    Box::new(rows.iter().cloned().map(Ok))
}

/// The statements that fill the tables: `a` (LEFT rows) and three with the
/// same RIGHT rows: `b`, `bi` with an index on `k`, and `bl`, an LSM table,
/// whose primary key is a lookup key. Each `a.k` matches one right row, and
/// each `a.name` one in the first 20,000.
fn setup() -> Vec<String> {
    let mut sql = vec![
        "CREATE TABLE a (id INT PRIMARY KEY, k INT, name TEXT)".to_string(),
        "CREATE TABLE b (id INT PRIMARY KEY, k INT, name TEXT)".into(),
        "CREATE TABLE bi (id INT PRIMARY KEY, k INT, name TEXT)".into(),
        "CREATE TABLE bl (id INT PRIMARY KEY, k INT, name TEXT) USING LSM".into(),
    ];
    insert(&mut sql, "a", LEFT, |i| {
        format!("({i}, {}, 'customer {}')", i % RIGHT, i % 20_000)
    });
    for table in ["b", "bi", "bl"] {
        insert(&mut sql, table, RIGHT, |i| {
            format!("({i}, {i}, 'customer {i}')")
        });
    }
    sql.push("CREATE INDEX bi_k ON bi (k)".into());
    sql
}
