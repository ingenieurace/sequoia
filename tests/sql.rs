//! SQL statements end to end. Every query runs in the execution model each
//! chooses for itself, or with `SEQUOIA_MODEL` set (to `iterator`,
//! `materialization` or `vectorized`), all in that one.

use sequoia::executor::model::Model;
use sequoia::{Database, QueryResult, Value};

/// The model `SEQUOIA_MODEL` names, if it is set.
fn model() -> Option<Model> {
    let name = std::env::var("SEQUOIA_MODEL").ok()?;
    Some(Model::from_name(&name).unwrap_or_else(|| panic!("SEQUOIA_MODEL={name} names no model")))
}

/// A database in memory, running queries in [`model`].
fn in_memory() -> Database {
    let mut db = Database::in_memory();
    db.set_execution_model(model());
    db
}

/// A database stored in a file, running queries in [`model`].
fn open(path: impl AsRef<std::path::Path>) -> sequoia::Result<Database> {
    let mut db = Database::open(path)?;
    db.set_execution_model(model());
    Ok(db)
}

fn setup() -> Database {
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT, active BOOL);
         INSERT INTO users VALUES (1, 'alice', 30, TRUE), (2, 'bob', 25, FALSE);
         INSERT INTO users (id, name) VALUES (3, 'carol');",
    )
    .unwrap();
    db
}

/// Runs a single SELECT and returns its rows.
fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap().pop() {
        Some(QueryResult::Rows { rows, .. }) => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn int(n: i64) -> Value {
    Value::Int(n)
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

#[test]
fn select_with_filter_order_and_limit() {
    let mut db = setup();
    let rows = query(
        &mut db,
        "SELECT name, age + 1 FROM users WHERE age >= 25 ORDER BY age DESC LIMIT 1",
    );
    assert_eq!(rows, vec![vec![text("alice"), int(31)]]);
}

#[test]
fn null_handling() {
    let mut db = setup();
    assert_eq!(
        query(&mut db, "SELECT id FROM users WHERE age IS NULL"),
        vec![vec![int(3)]]
    );
    // Comparisons with NULL are unknown, so carol is excluded either way.
    assert_eq!(
        query(&mut db, "SELECT id FROM users WHERE age > 0 OR age <= 0").len(),
        2
    );
    // NULL sorts first.
    assert_eq!(
        query(&mut db, "SELECT id FROM users ORDER BY age")[0],
        vec![int(3)]
    );
    assert_eq!(
        query(&mut db, "SELECT id FROM users WHERE id = 1 OR id = 2").len(),
        2
    );
}

#[test]
fn and_or_follow_three_valued_logic() {
    let mut db = setup();
    let (t, f, n) = (Value::Bool(true), Value::Bool(false), Value::Null);
    for (a, b, and, or) in [
        ("TRUE", "TRUE", &t, &t),
        ("TRUE", "FALSE", &f, &t),
        ("FALSE", "TRUE", &f, &t),
        ("FALSE", "FALSE", &f, &f),
        ("TRUE", "NULL", &n, &t),
        ("NULL", "TRUE", &n, &t),
        ("FALSE", "NULL", &f, &n),
        ("NULL", "FALSE", &f, &n),
        ("NULL", "NULL", &n, &n),
    ] {
        let sql = format!("SELECT {a} AND {b}, {a} OR {b} FROM users WHERE id = 1");
        assert_eq!(
            query(&mut db, &sql),
            [vec![and.clone(), or.clone()]],
            "{a}, {b}"
        );
    }
}

#[test]
fn update_and_delete() {
    let mut db = setup();
    db.execute("UPDATE users SET age = age * 2, active = NOT active WHERE name = 'bob'")
        .unwrap();
    assert_eq!(
        query(&mut db, "SELECT age, active FROM users WHERE id = 2"),
        vec![vec![int(50), Value::Bool(true)]]
    );

    db.execute("DELETE FROM users WHERE id != 2").unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM users"), vec![vec![int(2)]]);
}

#[test]
fn constraints_are_enforced() {
    let mut db = setup();
    assert!(
        db.execute("INSERT INTO users VALUES (1, 'dup', 1, TRUE)")
            .is_err()
    );
    assert!(
        db.execute("INSERT INTO users (name) VALUES ('no id')")
            .is_err()
    );
    assert!(
        db.execute("INSERT INTO users VALUES (9, 42, 1, TRUE)")
            .is_err()
    );
    // A failing UPDATE changes nothing.
    assert!(db.execute("UPDATE users SET id = 1").is_err());
    assert_eq!(query(&mut db, "SELECT id FROM users ORDER BY id").len(), 3);
}

#[test]
fn errors_are_reported() {
    let mut db = setup();
    assert!(db.execute("SELECT * FROM missing").is_err());
    assert!(db.execute("SELECT nope FROM users").is_err());
    assert!(db.execute("SELECT * FROM users WHERE").is_err());
    assert!(db.execute("SELECT 1 / 0 FROM users").is_err());
    assert!(db.execute("CREATE TABLE users (x INT)").is_err());
}

#[test]
fn persists_to_disk() {
    let path = std::env::temp_dir().join(format!("sequoia-test-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = open(&path).unwrap();
        db.execute("CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)")
            .unwrap();
        db.execute("INSERT INTO notes VALUES (1, 'it''s here'), (2, NULL)")
            .unwrap();
    }
    let mut db = open(&path).unwrap();
    assert_eq!(
        query(&mut db, "SELECT * FROM notes ORDER BY id"),
        vec![vec![int(1), text("it's here")], vec![int(2), Value::Null]]
    );
    std::fs::remove_file(&path).unwrap();
}

/// A temp database path that is removed when dropped.
struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("sequoia-sql-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        TempDb(path)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(format!("{}-wal", self.0.display()));
    }
}

/// The size of the database file once every committed page is checkpointed into it.
fn file_size(db: &mut Database, path: &std::path::Path) -> u64 {
    db.execute("CHECKPOINT").unwrap();
    std::fs::metadata(path).unwrap().len()
}

#[test]
fn large_tables_span_pages_and_survive_reopen() {
    let path = TempDb::new("large");
    {
        let mut db = open(&path.0).unwrap();
        db.execute("CREATE TABLE logs (id INT PRIMARY KEY, msg TEXT)")
            .unwrap();
        for i in 0..500 {
            db.execute(&format!(
                "INSERT INTO logs VALUES ({i}, '{}')",
                "x".repeat(100)
            ))
            .unwrap();
        }
        db.execute("DELETE FROM logs WHERE id >= 400").unwrap();
        db.execute("UPDATE logs SET msg = 'short' WHERE id < 10")
            .unwrap();
    }
    let mut db = open(&path.0).unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM logs").len(), 400);
    assert_eq!(
        query(&mut db, "SELECT msg FROM logs WHERE id = 5"),
        vec![vec![text("short")]]
    );
    // Inserts and key checks still work after reopening.
    db.execute("INSERT INTO logs VALUES (1000, 'new')").unwrap();
    assert!(db.execute("INSERT INTO logs VALUES (1, 'dup')").is_err());
}

#[test]
fn dropped_tables_free_their_pages() {
    let path = TempDb::new("drop");
    let mut db = open(&path.0).unwrap();
    let fill = |db: &mut Database| {
        db.execute("CREATE TABLE t (body TEXT)").unwrap();
        for _ in 0..100 {
            db.execute(&format!("INSERT INTO t VALUES ('{}')", "y".repeat(200)))
                .unwrap();
        }
    };
    fill(&mut db);
    let size = file_size(&mut db, &path.0);
    db.execute("DROP TABLE t").unwrap();
    fill(&mut db);
    assert_eq!(file_size(&mut db, &path.0), size, "freed pages are reused");

    drop(db);
    let mut db = open(&path.0).unwrap();
    assert_eq!(db.table_names(), vec!["t"]);
    assert_eq!(query(&mut db, "SELECT * FROM t").len(), 100);
}

#[test]
fn rows_larger_than_a_page_are_rejected() {
    let mut db = setup();
    let huge = "z".repeat(5000);
    assert!(
        db.execute(&format!(
            "INSERT INTO users (id, name) VALUES (9, '{huge}')"
        ))
        .is_err()
    );
    assert_eq!(query(&mut db, "SELECT id FROM users").len(), 3);
}

#[test]
fn deleted_space_is_reused_after_reopening() {
    let path = TempDb::new("reuse");
    let size = {
        let mut db = open(&path.0).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, body TEXT)")
            .unwrap();
        for i in 0..200 {
            db.execute(&format!(
                "INSERT INTO t VALUES ({i}, '{}')",
                "x".repeat(100)
            ))
            .unwrap();
        }
        db.execute("DELETE FROM t WHERE id < 100").unwrap();
        file_size(&mut db, &path.0)
    };

    let mut db = open(&path.0).unwrap();
    for i in 0..100 {
        db.execute(&format!(
            "INSERT INTO t VALUES ({i}, '{}')",
            "y".repeat(100)
        ))
        .unwrap();
    }
    assert_eq!(file_size(&mut db, &path.0), size, "the file did not grow");
    assert_eq!(query(&mut db, "SELECT id FROM t").len(), 200);
}

#[test]
fn updates_that_grow_rows_keep_every_row() {
    let path = TempDb::new("grow");
    let mut db = open(&path.0).unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, body TEXT)")
        .unwrap();
    for i in 0..100 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'x')"))
            .unwrap();
    }
    let size = file_size(&mut db, &path.0);
    // Shrinking or same-size updates stay in place.
    db.execute("UPDATE t SET body = 'y'").unwrap();
    assert_eq!(file_size(&mut db, &path.0), size);

    // Rows too big for their page move elsewhere.
    db.execute(&format!(
        "UPDATE t SET body = '{}' WHERE id < 50",
        "z".repeat(500)
    ))
    .unwrap();
    drop(db);
    let mut db = open(&path.0).unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM t").len(), 100);
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE body = 'y'").len(),
        50
    );
    assert_eq!(
        query(
            &mut db,
            &format!("SELECT id FROM t WHERE body = '{}'", "z".repeat(500))
        )
        .len(),
        50
    );
}

fn setup_lsm() -> Database {
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT, active BOOL) USING LSM;
         INSERT INTO users VALUES (1, 'alice', 30, TRUE), (2, 'bob', 25, FALSE);
         INSERT INTO users (id, name) VALUES (3, 'carol');",
    )
    .unwrap();
    db
}

#[test]
fn lsm_tables_run_every_statement() {
    let mut db = setup_lsm();
    assert_eq!(
        query(&mut db, "SELECT name FROM users WHERE age IS NULL"),
        vec![vec![text("carol")]]
    );

    db.execute("UPDATE users SET age = age * 2, active = NOT active WHERE name = 'bob'")
        .unwrap();
    assert_eq!(
        query(&mut db, "SELECT age, active FROM users WHERE id = 2"),
        vec![vec![int(50), Value::Bool(true)]]
    );

    // Changing the primary key moves the row to its new key.
    db.execute("UPDATE users SET id = id + 10 WHERE id >= 2")
        .unwrap();
    assert_eq!(
        query(&mut db, "SELECT id FROM users"),
        vec![vec![int(1)], vec![int(12)], vec![int(13)]]
    );

    db.execute("DELETE FROM users WHERE id != 12").unwrap();
    assert_eq!(
        query(&mut db, "SELECT name FROM users"),
        vec![vec![text("bob")]]
    );
    db.execute("INSERT INTO users (id, name) VALUES (1, 'alice again')")
        .unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM users").len(), 2);
}

#[test]
fn lsm_tables_enforce_constraints() {
    let mut db = setup_lsm();
    assert!(
        db.execute("INSERT INTO users VALUES (1, 'dup', 1, TRUE)")
            .is_err()
    );
    assert!(
        db.execute("INSERT INTO users VALUES (9, 'x', 1, TRUE), (9, 'y', 1, TRUE)")
            .is_err()
    );
    assert!(
        db.execute("INSERT INTO users (name) VALUES ('no id')")
            .is_err()
    );
    assert!(db.execute("UPDATE users SET id = 1").is_err());
    assert!(
        db.execute(&format!(
            "INSERT INTO users (id, name) VALUES (9, '{}')",
            "z".repeat(5000)
        ))
        .is_err()
    );
    assert_eq!(
        query(&mut db, "SELECT id FROM users").len(),
        3,
        "failed statements change nothing"
    );

    // Swapping two keys in one statement is fine.
    db.execute("UPDATE users SET id = 3 - id WHERE id <= 2")
        .unwrap();
    assert_eq!(
        query(&mut db, "SELECT name FROM users WHERE id = 1"),
        vec![vec![text("bob")]]
    );
    assert_eq!(
        query(&mut db, "SELECT name FROM users WHERE id = 2"),
        vec![vec![text("alice")]]
    );
}

#[test]
fn storage_clause_is_parsed_and_checked() {
    let mut db = in_memory();
    db.execute("CREATE TABLE a (x INT) using lsm").unwrap();
    db.execute("CREATE TABLE b (x INT) USING heap").unwrap();
    db.execute("CREATE TABLE d (x INT) USING Column").unwrap();
    assert!(db.execute("CREATE TABLE c (x INT) USING btree").is_err());
    // HEAP, LSM and COLUMN are not reserved words.
    db.execute("CREATE TABLE lsm (heap INT, column INT)")
        .unwrap();
}

#[test]
fn lsm_tables_survive_flushes_compactions_and_reopening() {
    let path = TempDb::new("lsm");
    let row = |i: i64, tag: &str| vec![int(i), text(&format!("{tag}{}", "x".repeat(100)))];
    {
        let mut db = open(&path.0).unwrap();
        db.execute("CREATE TABLE logs (id INT PRIMARY KEY, msg TEXT) USING LSM")
            .unwrap();
        // About 1 MB of writes: many MemTable flushes and several compactions.
        for batch in 0..30 {
            let values: Vec<String> = (0..250)
                .map(|i| format!("({}, 'v{batch}{}')", batch * 250 + i, "x".repeat(100)))
                .collect();
            db.execute(&format!("INSERT INTO logs VALUES {}", values.join(", ")))
                .unwrap();
        }
        db.execute(&format!(
            "UPDATE logs SET msg = 'new{}' WHERE id < 1000",
            "x".repeat(100)
        ))
        .unwrap();
        db.execute("DELETE FROM logs WHERE id >= 7000").unwrap();
    }

    let mut db = open(&path.0).unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM logs").len(), 7000);
    assert_eq!(
        query(&mut db, "SELECT * FROM logs WHERE id = 5"),
        vec![row(5, "new")]
    );
    assert_eq!(
        query(&mut db, "SELECT * FROM logs WHERE id = 6999"),
        vec![row(6999, "v27")]
    );
    assert!(query(&mut db, "SELECT id FROM logs WHERE id = 7000").is_empty());
    assert!(db.execute("INSERT INTO logs VALUES (6999, 'dup')").is_err());
    db.execute("INSERT INTO logs VALUES (7000, 'back')")
        .unwrap();
}

#[test]
fn lsm_tables_without_a_primary_key_use_rowids() {
    let path = TempDb::new("rowid");
    {
        let mut db = open(&path.0).unwrap();
        db.execute("CREATE TABLE events (name TEXT) USING LSM")
            .unwrap();
        db.execute("INSERT INTO events VALUES ('a'), ('a'), ('b')")
            .unwrap();
        db.execute("UPDATE events SET name = 'c' WHERE name = 'b'")
            .unwrap();
    }
    let mut db = open(&path.0).unwrap();
    // New rows after reopening must not overwrite old ones.
    db.execute("INSERT INTO events VALUES ('d')").unwrap();
    assert_eq!(
        query(&mut db, "SELECT name FROM events"),
        vec![
            vec![text("a")],
            vec![text("a")],
            vec![text("c")],
            vec![text("d")]
        ]
    );
}

#[test]
fn dropped_lsm_tables_free_their_pages() {
    let path = TempDb::new("lsm-drop");
    let mut db = open(&path.0).unwrap();
    let fill = |db: &mut Database| {
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, body TEXT) USING LSM")
            .unwrap();
        for i in 0..1000 {
            db.execute(&format!(
                "INSERT INTO t VALUES ({i}, '{}')",
                "y".repeat(200)
            ))
            .unwrap();
        }
    };
    fill(&mut db);
    let size = file_size(&mut db, &path.0);
    db.execute("DROP TABLE t").unwrap();
    fill(&mut db);
    assert_eq!(file_size(&mut db, &path.0), size, "freed pages are reused");
}

fn setup_column() -> Database {
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT, active BOOL) USING COLUMN;
         INSERT INTO users VALUES (1, 'alice', 30, TRUE), (2, 'bob', 25, FALSE);
         INSERT INTO users (id, name) VALUES (3, 'carol');",
    )
    .unwrap();
    db
}

#[test]
fn column_tables_run_every_statement() {
    let mut db = setup_column();
    assert_eq!(
        query(&mut db, "SELECT name FROM users WHERE age IS NULL"),
        vec![vec![text("carol")]]
    );

    db.execute("UPDATE users SET age = age * 2, active = NOT active WHERE name = 'bob'")
        .unwrap();
    assert_eq!(
        query(&mut db, "SELECT age, active FROM users WHERE id = 2"),
        vec![vec![int(50), Value::Bool(true)]]
    );
    db.execute("UPDATE users SET id = id + 10 WHERE id >= 2")
        .unwrap();
    assert_eq!(
        query(&mut db, "SELECT id FROM users ORDER BY id"),
        vec![vec![int(1)], vec![int(12)], vec![int(13)]]
    );

    db.execute("DELETE FROM users WHERE id != 12").unwrap();
    assert_eq!(
        query(&mut db, "SELECT * FROM users"),
        vec![vec![int(12), text("bob"), int(50), Value::Bool(true)]]
    );
}

#[test]
fn column_tables_enforce_constraints() {
    let mut db = setup_column();
    assert!(
        db.execute("INSERT INTO users VALUES (1, 'dup', 1, TRUE)")
            .is_err()
    );
    assert!(
        db.execute("INSERT INTO users VALUES (9, 'x', 1, TRUE), (9, 'y', 1, TRUE)")
            .is_err()
    );
    assert!(
        db.execute("INSERT INTO users (name) VALUES ('no id')")
            .is_err()
    );
    assert!(db.execute("UPDATE users SET id = 1").is_err());
    assert!(
        db.execute("INSERT INTO users VALUES ('one', 'x', 1, TRUE)")
            .is_err()
    );
    assert!(
        db.execute(&format!(
            "INSERT INTO users (id, name) VALUES (9, '{}')",
            "z".repeat(5000)
        ))
        .is_err()
    );
    assert_eq!(
        query(&mut db, "SELECT id FROM users").len(),
        3,
        "failed statements change nothing"
    );

    db.execute("UPDATE users SET id = 3 - id WHERE id <= 2")
        .unwrap();
    assert_eq!(
        query(&mut db, "SELECT name FROM users WHERE id = 1"),
        vec![vec![text("bob")]]
    );
}

#[test]
fn column_tables_survive_migrations_and_reopening() {
    let path = TempDb::new("column");
    let cities = ["lima", "oslo", "paris", "tokyo"];
    {
        let mut db = open(&path.0).unwrap();
        db.execute("CREATE TABLE visits (id INT PRIMARY KEY, city TEXT, nights INT, paid BOOL) USING COLUMN").unwrap();
        // Several row groups of 1024 rows, plus rows left in the delta store.
        for batch in 0..5 {
            let values: Vec<String> = (0..1000)
                .map(|i| {
                    let id = batch * 1000 + i;
                    let nights = if id % 7 == 0 {
                        "NULL".to_string()
                    } else {
                        (id % 10).to_string()
                    };
                    format!(
                        "({id}, '{}', {nights}, {})",
                        cities[id as usize % 4],
                        id % 2 == 0
                    )
                })
                .collect();
            db.execute(&format!("INSERT INTO visits VALUES {}", values.join(", ")))
                .unwrap();
        }
        // Touches rows in row groups and in the delta store.
        db.execute("UPDATE visits SET city = 'rome' WHERE id / 100 * 100 = id")
            .unwrap();
        db.execute("DELETE FROM visits WHERE city = 'oslo'")
            .unwrap();
    }

    let mut db = open(&path.0).unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM visits").len(), 3750);
    assert_eq!(
        query(&mut db, "SELECT city FROM visits WHERE city = 'rome'").len(),
        50
    );
    assert!(query(&mut db, "SELECT id FROM visits WHERE city = 'oslo'").is_empty());
    assert_eq!(
        query(&mut db, "SELECT id FROM visits WHERE nights IS NULL").len(),
        537
    );
    assert_eq!(
        query(&mut db, "SELECT * FROM visits WHERE id = 4994"),
        vec![vec![int(4994), text("paris"), int(4), Value::Bool(true)]]
    );
    // Selecting only some columns still filters and sorts on others.
    assert_eq!(
        query(
            &mut db,
            "SELECT id FROM visits WHERE paid AND nights = 9 ORDER BY city DESC, id LIMIT 2"
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT city FROM visits WHERE nights = 8 ORDER BY id DESC LIMIT 1"
        ),
        vec![vec![text("lima")]]
    );
    assert!(
        db.execute("INSERT INTO visits VALUES (6, 'dup', 1, TRUE)")
            .is_err()
    );
    db.execute("INSERT INTO visits VALUES (5, 'back', 1, TRUE)")
        .unwrap(); // its old row was deleted
}

#[test]
fn dropped_column_tables_free_their_pages() {
    let path = TempDb::new("column-drop");
    let mut db = open(&path.0).unwrap();
    let fill = |db: &mut Database| {
        db.execute("CREATE TABLE t (id INT, body TEXT) USING COLUMN")
            .unwrap();
        for batch in 0..3 {
            let values: Vec<String> = (0..1000)
                .map(|i| format!("({}, '{}')", batch * 1000 + i, "y".repeat(50)))
                .collect();
            db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
                .unwrap();
        }
    };
    fill(&mut db);
    let size = file_size(&mut db, &path.0);
    db.execute("DROP TABLE t").unwrap();
    fill(&mut db);
    assert_eq!(file_size(&mut db, &path.0), size, "freed pages are reused");
}

#[test]
fn column_tables_filter_compressed_chunks_like_heap_tables() {
    let mut db = in_memory();
    let cities = ["lima", "oslo", "paris", "tokyo"];
    for storage in ["HEAP", "COLUMN"] {
        let schema = "(id INT PRIMARY KEY, city TEXT, nights INT, paid BOOL)";
        db.execute(&format!(
            "CREATE TABLE {storage}_t {schema} USING {storage}"
        ))
        .unwrap();
        // Row groups of sorted ids, runs of cities and few distinct nights, plus rows left in the delta store.
        for batch in 0..3 {
            let values: Vec<String> = (0..1000)
                .map(|i| {
                    let id = batch * 1000 + i;
                    let nights = if id % 11 == 0 {
                        "NULL".to_string()
                    } else {
                        (id % 6 - 2).to_string()
                    };
                    let paid = if id % 13 == 0 {
                        "NULL".to_string()
                    } else {
                        (id % 3 == 0).to_string()
                    };
                    format!(
                        "({id}, '{}', {nights}, {paid})",
                        cities[id as usize / 300 % 4]
                    )
                })
                .collect();
            db.execute(&format!(
                "INSERT INTO {storage}_t VALUES {}",
                values.join(", ")
            ))
            .unwrap();
        }
    }
    let same = |db: &mut Database, sql: &str| {
        let mut heap = query(db, &sql.replace("{t}", "HEAP_t"));
        let mut column = query(db, &sql.replace("{t}", "COLUMN_t"));
        heap.sort_by(|a, b| a[0].sort_cmp(&b[0]));
        column.sort_by(|a, b| a[0].sort_cmp(&b[0]));
        assert_eq!(column, heap, "{sql}");
        heap.len()
    };
    let wheres = [
        "city = 'oslo'",
        "city != 'oslo'",
        "city < 'm'",
        "'paris' <= city",
        "city > 'rome'",
        "city = 'rome'",
        "nights = -1",
        "nights < 0 AND paid",
        "nights >= 2 AND city = 'tokyo' AND id < 2500",
        "-2 = nights",
        "nights != 1 + 1",
        "paid = FALSE",
        "paid != TRUE AND id > 2950",
        "id >= 1020 AND id < 1030",
        "id = 2999 OR nights = 3",
        "nights = NULL",
        "nights IS NULL AND city = 'lima'",
        "NOT city = 'lima' AND id <= 10",
    ];
    for filter in wheres {
        same(&mut db, &format!("SELECT * FROM {{t}} WHERE {filter}"));
        same(
            &mut db,
            &format!("SELECT id, nights FROM {{t}} WHERE {filter}"),
        );
    }
    assert_eq!(
        same(
            &mut db,
            "SELECT id FROM {t} WHERE city = 'oslo' AND nights = 3"
        ),
        136
    );
    // Type errors are still reported.
    assert!(
        db.execute("SELECT id FROM COLUMN_t WHERE city = 1")
            .is_err()
    );

    let statements = [
        "UPDATE {t} SET nights = 9 WHERE city = 'paris' AND id < 1500",
        "DELETE FROM {t} WHERE nights = 0",
    ];
    for statement in statements {
        for table in ["HEAP_t", "COLUMN_t"] {
            db.execute(&statement.replace("{t}", table)).unwrap();
        }
        same(&mut db, "SELECT * FROM {t}");
    }
    same(
        &mut db,
        "SELECT * FROM {t} WHERE nights = 9 AND city > 'lima'",
    );
}

/// Runs EXPLAIN on a statement and returns its whole plan.
fn explain(db: &mut Database, sql: &str) -> String {
    match db.execute(&format!("EXPLAIN {sql}")).unwrap().pop() {
        Some(QueryResult::Message(plan)) => plan,
        other => panic!("expected a plan, got {other:?}"),
    }
}

/// How a statement reads its table: the last line of its plan.
fn plan(db: &mut Database, sql: &str) -> String {
    explain(db, sql).lines().last().unwrap().trim().to_string()
}

#[test]
fn heap_primary_keys_and_created_indexes_are_used_by_where() {
    let mut db = setup();
    assert_eq!(
        plan(&mut db, "SELECT * FROM users WHERE id = 2"),
        "SEARCH users USING INDEX users_pkey (id = 2)"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM users WHERE name = 'bob'"),
        "SCAN users"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM users WHERE id != 2 OR id = 3"),
        "SCAN users",
        "no conjunct to use"
    );

    db.execute("CREATE INDEX users_name ON users (name)")
        .unwrap();
    db.execute("CREATE INDEX users_age ON users (age)").unwrap();
    assert_eq!(
        plan(&mut db, "UPDATE users SET age = 1 WHERE 'bob' = name"),
        "SEARCH users USING INDEX users_name (name = 'bob')"
    );
    assert_eq!(
        plan(
            &mut db,
            "DELETE FROM users WHERE age > 20 AND age <= 30 AND age >= 21 AND age < 99"
        ),
        "SEARCH users USING INDEX users_age (age >= 21 AND age <= 30)",
        "the tightest bounds"
    );
    assert_eq!(
        plan(
            &mut db,
            "SELECT * FROM users WHERE age > 20 AND name = 'bob'"
        ),
        "SEARCH users USING INDEX users_name (name = 'bob')",
        "an equality beats a range"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM users WHERE age = 'x'"),
        "SCAN users",
        "a literal of the wrong type"
    );

    assert_eq!(
        query(&mut db, "SELECT id FROM users WHERE name = 'bob'"),
        vec![vec![int(2)]]
    );
    // Rows come back in index order, and a range leaves out NULL ages.
    assert_eq!(
        query(&mut db, "SELECT name FROM users WHERE age < 100"),
        vec![vec![text("bob")], vec![text("alice")]]
    );
    assert!(query(&mut db, "SELECT id FROM users WHERE age > 30").is_empty());
    assert!(
        db.execute("EXPLAIN INSERT INTO users VALUES (4, 'x', 1, TRUE)")
            .is_err()
    );
}

#[test]
fn indexed_tables_answer_like_unindexed_ones() {
    let mut db = in_memory();
    for storage in ["HEAP", "LSM"] {
        for indexed in [false, true] {
            let t = format!("{storage}_{indexed}");
            let schema = "(id INT PRIMARY KEY, city TEXT, nights INT)";
            db.execute(&format!("CREATE TABLE {t} {schema} USING {storage}"))
                .unwrap();
            if indexed {
                db.execute(&format!("CREATE INDEX {t}_city ON {t} (city)"))
                    .unwrap();
                db.execute(&format!("CREATE INDEX {t}_nights ON {t} (nights)"))
                    .unwrap();
            }
            let values: Vec<String> = (0..2000i64)
                .map(|id| {
                    let nights = if id % 11 == 0 {
                        "NULL".to_string()
                    } else {
                        (id % 7 - 3).to_string()
                    };
                    format!(
                        "({id}, '{}', {nights})",
                        ["lima", "oslo", "paris", "tokyo", "a\0b"][id as usize % 5]
                    )
                })
                .collect();
            db.execute(&format!("INSERT INTO {t} VALUES {}", values.join(", ")))
                .unwrap();
        }
    }
    let tables = ["HEAP_false", "HEAP_true", "LSM_false", "LSM_true"];
    let same = |db: &mut Database, sql: &str| {
        let results: Vec<Vec<Vec<Value>>> = tables
            .iter()
            .map(|t| {
                let mut rows = query(db, &sql.replace("{t}", t));
                rows.sort_by(|a, b| a[0].sort_cmp(&b[0]));
                rows
            })
            .collect();
        for (t, rows) in tables.iter().zip(&results) {
            assert_eq!(rows, &results[0], "{sql} on {t}");
        }
    };
    let wheres = [
        "city = 'oslo'",
        "city = 'a\0b' AND nights > 0",
        "city > 'lima' AND city < 'tokyo'",
        "city >= 'oslo'",
        "city <= 'a'",
        "nights = -3",
        "nights >= 2",
        "nights < 0 AND nights > -3",
        "nights > 5",
        "nights = NULL",
        "nights IS NULL",
        "id = 1500",
        "id >= 1990 AND city = 'paris'",
    ];
    let changes = [
        // Keys and indexed values that stay the same keep their entries.
        "UPDATE {t} SET id = id, nights = nights WHERE id < 500",
        "UPDATE {t} SET city = 'rome' WHERE city = 'lima' AND id < 1000",
        "UPDATE {t} SET nights = nights + 1 WHERE nights >= 0",
        // Grown rows move to other heap pages; changed keys move in an LSM tree.
        &format!(
            "UPDATE {{t}} SET city = '{}' WHERE id / 3 * 3 = id",
            "z".repeat(300)
        ),
        "UPDATE {t} SET id = id + 10000 WHERE nights = 2",
        "DELETE FROM {t} WHERE city = 'tokyo' OR nights IS NULL",
    ];
    for change in [""].into_iter().chain(changes) {
        if !change.is_empty() {
            for t in tables {
                db.execute(&change.replace("{t}", t)).unwrap();
            }
        }
        same(&mut db, "SELECT * FROM {t}");
        for filter in wheres {
            same(&mut db, &format!("SELECT * FROM {{t}} WHERE {filter}"));
        }
    }
    assert!(
        plan(&mut db, "SELECT * FROM HEAP_true WHERE city = 'rome'")
            .contains("USING INDEX HEAP_true_city")
    );
    assert!(
        plan(&mut db, "SELECT * FROM LSM_true WHERE nights < 0")
            .contains("USING INDEX LSM_true_nights")
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM LSM_true WHERE id = 7"),
        "SEARCH LSM_true USING PRIMARY KEY (id = 7)"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM LSM_false WHERE city = 'oslo'"),
        "SCAN LSM_false"
    );
    assert_eq!(
        plan(
            &mut db,
            "SELECT * FROM LSM_false WHERE id >= 1990 AND city = 'paris'"
        ),
        "SEARCH LSM_false USING PRIMARY KEY (id >= 1990)"
    );
}

#[test]
fn lsm_primary_key_ranges_read_only_their_keys() {
    let mut db = in_memory();
    for (t, key) in [("ints", "INT"), ("texts", "TEXT")] {
        db.execute(&format!(
            "CREATE TABLE {t} (k {key} PRIMARY KEY, v INT) USING LSM"
        ))
        .unwrap();
        // Enough rows to fill SSTables, then deletes and new rows still in the MemTable.
        let values: Vec<String> = (0..3000)
            .map(|i| format!("({}, {i})", key_of(t, i)))
            .collect();
        db.execute(&format!("INSERT INTO {t} VALUES {}", values.join(", ")))
            .unwrap();
        db.execute(&format!("DELETE FROM {t} WHERE v >= 1000 AND v < 1010"))
            .unwrap();
        db.execute(&format!(
            "UPDATE {t} SET v = v + 10000 WHERE v >= 2000 AND v < 2005"
        ))
        .unwrap();
    }
    let keys = |db: &mut Database, sql: &str| -> Vec<i64> {
        query(db, sql)
            .into_iter()
            .map(|row| match row[1] {
                Value::Int(v) => v % 10000,
                _ => unreachable!(),
            })
            .collect()
    };
    for t in ["ints", "texts"] {
        let k = |i| key_of(t, i);
        let range = |db: &mut Database, condition: String| {
            let sql = format!("SELECT * FROM {t} WHERE {condition}");
            assert!(plan(db, &sql).contains("USING PRIMARY KEY"), "{sql}");
            keys(db, &sql)
        };
        assert_eq!(
            range(&mut db, format!("k >= {} AND k < {}", k(5), k(9))),
            [5, 6, 7, 8]
        );
        assert_eq!(
            range(&mut db, format!("k > {} AND k <= {}", k(5), k(9))),
            [6, 7, 8, 9]
        );
        assert_eq!(
            range(&mut db, format!("k > {} AND k < {}", k(5), k(6))),
            Vec::<i64>::new()
        );
        assert_eq!(
            range(&mut db, format!("k >= {} AND k < {}", k(9), k(5))),
            Vec::<i64>::new()
        );
        assert_eq!(
            range(&mut db, format!("k >= {} AND k <= {}", k(998), k(1011))),
            [998, 999, 1010, 1011]
        );
        assert_eq!(
            range(&mut db, format!("k >= {} AND k < {}", k(1998), k(2007))),
            (1998..2007).collect::<Vec<_>>()
        );
        assert_eq!(
            range(&mut db, format!("k > {}", k(2996))),
            [2997, 2998, 2999]
        );
        assert_eq!(range(&mut db, format!("k < {}", k(3))), [0, 1, 2]);
        // Other conditions are still checked on the rows read.
        assert_eq!(
            range(
                &mut db,
                format!("k >= {} AND k < {} AND v != 7", k(5), k(9))
            ),
            [5, 6, 8]
        );
    }
}

/// The key of row `i` of table `t`: a number, or text that sorts the same way.
fn key_of(t: &str, i: i64) -> String {
    if t == "ints" {
        i.to_string()
    } else {
        format!("'key {i:05}'")
    }
}

#[test]
fn unique_indexes_reject_duplicates() {
    let mut db = setup();
    db.execute("CREATE UNIQUE INDEX users_name ON users (name)")
        .unwrap();
    assert!(
        db.execute("INSERT INTO users VALUES (4, 'bob', 1, TRUE)")
            .is_err()
    );
    assert!(
        db.execute("INSERT INTO users VALUES (4, 'dave', 1, TRUE), (5, 'dave', 1, TRUE)")
            .is_err()
    );
    assert!(
        db.execute("UPDATE users SET name = 'alice' WHERE id = 2")
            .is_err()
    );
    // NULLs do not clash, and rows may swap values in one statement.
    db.execute("INSERT INTO users (id) VALUES (4), (5)")
        .unwrap();
    db.execute("UPDATE users SET id = id + 1").unwrap();
    db.execute("UPDATE users SET id = 3 - id WHERE id < 3")
        .unwrap();
    assert_eq!(
        query(&mut db, "SELECT id FROM users WHERE name = 'alice'"),
        vec![vec![int(1)]]
    );
    assert_eq!(
        query(&mut db, "SELECT name FROM users WHERE id = 3"),
        vec![vec![text("bob")]]
    );
    assert_eq!(
        query(&mut db, "SELECT id FROM users WHERE id >= 1 ORDER BY id").len(),
        5
    );

    // A unique index cannot be built over duplicates, and none is left behind.
    db.execute("INSERT INTO users VALUES (9, 'zed', 30, TRUE)")
        .unwrap();
    assert!(
        db.execute("CREATE UNIQUE INDEX users_age ON users (age)")
            .is_err()
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM users WHERE age = 30"),
        "SCAN users"
    );
    db.execute("CREATE INDEX users_age ON users (age)").unwrap();
    assert_eq!(
        query(&mut db, "SELECT id FROM users WHERE age = 30").len(),
        2
    );
}

#[test]
fn index_statements_are_checked() {
    let mut db = setup();
    assert!(db.execute("CREATE INDEX i ON missing (id)").is_err());
    assert!(db.execute("CREATE INDEX i ON users (missing)").is_err());
    assert!(
        db.execute("CREATE INDEX users_pkey ON users (name)")
            .is_err(),
        "name taken"
    );
    assert!(
        db.execute("DROP INDEX users_pkey").is_err(),
        "the primary key needs it"
    );
    assert!(db.execute("DROP INDEX missing").is_err());
    assert!(db.execute("CREATE INDEX i ON users").is_err());

    // Values too long for an index entry are rejected, leaving the table unchanged.
    db.execute("CREATE INDEX users_name ON users (name)")
        .unwrap();
    let long = "n".repeat(2000);
    assert!(
        db.execute(&format!(
            "INSERT INTO users (id, name) VALUES (9, '{long}')"
        ))
        .is_err()
    );
    assert!(
        db.execute(&format!("UPDATE users SET name = '{long}' WHERE id = 1"))
            .is_err()
    );
    assert_eq!(
        query(&mut db, "SELECT name FROM users WHERE id = 1"),
        vec![vec![text("alice")]]
    );
    assert_eq!(query(&mut db, "SELECT id FROM users").len(), 3);

    db.execute("DROP INDEX users_name").unwrap();
    db.execute(&format!(
        "INSERT INTO users (id, name) VALUES (9, '{long}')"
    ))
    .unwrap();
    assert!(
        db.execute("CREATE INDEX users_name ON users (name)")
            .is_err()
    );

    db.execute("CREATE TABLE c (id INT PRIMARY KEY, x INT) USING COLUMN")
        .unwrap();
    assert!(db.execute("CREATE INDEX c_x ON c (x)").is_err());
    assert_eq!(
        plan(&mut db, "SELECT * FROM c WHERE x > 1 AND id = 2"),
        "SCAN c FILTERING COMPRESSED CHUNKS ON x > 1 AND id = 2"
    );
}

#[test]
fn indexes_survive_reopening_and_are_dropped_with_their_table() {
    let path = TempDb::new("index");
    let fill = |db: &mut Database| {
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, tag TEXT, n INT)")
            .unwrap();
        db.execute("CREATE INDEX t_tag ON t (tag)").unwrap();
        for batch in 0..10 {
            let values: Vec<String> = (0..200)
                .map(|i| batch * 200 + i)
                .map(|id| format!("({id}, 'tag{}', {id})", id % 50))
                .collect();
            db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
                .unwrap();
        }
    };
    {
        let mut db = open(&path.0).unwrap();
        fill(&mut db);
        db.execute("CREATE INDEX t_n ON t (n)").unwrap();
        db.execute("DELETE FROM t WHERE n >= 1000").unwrap();
    }
    let mut db = open(&path.0).unwrap();
    assert_eq!(
        plan(&mut db, "SELECT * FROM t WHERE tag = 'tag7'"),
        "SEARCH t USING INDEX t_tag (tag = 'tag7')"
    );
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE tag = 'tag7'").len(),
        20
    );
    assert_eq!(query(&mut db, "SELECT id FROM t WHERE n >= 990").len(), 10);
    assert!(
        db.execute("INSERT INTO t VALUES (5, 'dup', 0)").is_err(),
        "the primary key index was reopened"
    );
    db.execute("INSERT INTO t VALUES (5000, 'tag7', 5000)")
        .unwrap();
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE tag = 'tag7'").len(),
        21
    );

    db.execute("DROP INDEX t_n").unwrap();
    assert_eq!(plan(&mut db, "SELECT * FROM t WHERE n = 3"), "SCAN t");
    db.execute("DROP TABLE t").unwrap();
    let size = file_size(&mut db, &path.0);
    fill(&mut db);
    assert_eq!(
        file_size(&mut db, &path.0),
        size,
        "the indexes' pages were freed and reused"
    );
    drop(db);

    let mut db = open(&path.0).unwrap();
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE tag = 'tag1'").len(),
        40
    );
}

#[test]
fn several_keys_expressions_partial_and_covering_indexes_are_used() {
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT, city TEXT, nights INT, status TEXT)",
    )
    .unwrap();
    let values: Vec<String> = (0..200)
        .map(|id| {
            let nights = if id % 9 == 0 {
                "NULL".to_string()
            } else {
                (id % 7).to_string()
            };
            let (city, status) = (
                ["oslo", "lima", "rome"][id % 3],
                ["open", "closed"][(id / 3) % 2],
            );
            format!(
                "({id}, '{}', '{city}', {nights}, '{status}')",
                ["Bob", "ALICE", "carol"][id % 3]
            )
        })
        .collect();
    db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
        .unwrap();
    db.execute("CREATE INDEX t_city_nights ON t (city, nights)")
        .unwrap();
    db.execute("CREATE INDEX t_lower ON t (LOWER(name))")
        .unwrap();
    db.execute("CREATE INDEX t_open ON t (nights) WHERE status = 'open'")
        .unwrap();
    db.execute("CREATE INDEX t_status ON t (status) INCLUDE (name)")
        .unwrap();

    // Equalities on the first keys, then a range on the next.
    assert_eq!(
        plan(
            &mut db,
            "SELECT * FROM t WHERE city = 'oslo' AND nights > 2 AND nights <= 5"
        ),
        "SEARCH t USING INDEX t_city_nights (city = 'oslo' AND nights > 2 AND nights <= 5)"
    );
    assert_eq!(
        plan(
            &mut db,
            "SELECT * FROM t WHERE nights = 3 AND city = 'lima'"
        ),
        "SEARCH t USING INDEX t_city_nights (city = 'lima' AND nights = 3)"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM t WHERE city = 'rome'"),
        "SEARCH t USING INDEX t_city_nights (city = 'rome')"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM t WHERE nights = 3"),
        "SCAN t",
        "not the first key, and not a partial index's rows"
    );
    // An expression, matched however its columns are qualified.
    assert_eq!(
        plan(&mut db, "SELECT * FROM t WHERE lower(name) = 'bob'"),
        "SEARCH t USING INDEX t_lower (LOWER(name) = 'bob')"
    );
    assert_eq!(
        plan(
            &mut db,
            "SELECT x.id FROM t x WHERE 'alice' < LOWER(x.name)"
        ),
        "SEARCH t x USING INDEX t_lower (LOWER(name) > 'alice')"
    );
    // A partial index serves only a WHERE that implies its condition.
    assert_eq!(
        plan(
            &mut db,
            "SELECT id FROM t WHERE status = 'open' AND nights < 2"
        ),
        "SEARCH t USING INDEX t_open (nights < 2)"
    );
    assert_eq!(
        plan(
            &mut db,
            "SELECT id FROM t WHERE nights = 4 AND status = 'open'"
        ),
        "SEARCH t USING INDEX t_open (nights = 4)"
    );
    assert_eq!(
        plan(&mut db, "SELECT nights FROM t WHERE 'open' = status"),
        "SCAN t USING INDEX t_open"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM t WHERE 'open' = status"),
        "SCAN t USING INDEX t_open",
        "the earlier index"
    );
    db.execute("CREATE INDEX t_long ON t (name) WHERE nights > 3")
        .unwrap();
    for (filter, uses) in [
        ("nights > 5", true),
        ("nights >= 3", false),
        ("nights >= 4", true),
        ("nights = 6", true),
        ("nights < 9", false),
    ] {
        let sql = format!("SELECT * FROM t WHERE name = 'Bob' AND {filter}");
        assert_eq!(
            plan(&mut db, &sql).contains("t_long"),
            uses,
            "{filter} implies nights > 3: {uses}"
        );
    }
    db.execute("DROP INDEX t_long").unwrap();
    // An index holding every column needed is read alone.
    assert_eq!(
        plan(&mut db, "SELECT name FROM t WHERE status = 'closed'"),
        "SEARCH t USING COVERING INDEX t_status (status = 'closed')"
    );
    assert_eq!(
        plan(
            &mut db,
            "SELECT city, nights FROM t WHERE city = 'oslo' AND nights IS NULL"
        ),
        "SEARCH t USING COVERING INDEX t_city_nights (city = 'oslo')"
    );

    // The rows as inserted above: (city, nights, open), and the name's case aside.
    type Row = (usize, Option<usize>, bool);
    let rows: Vec<Row> = (0..200)
        .map(|id| (id % 3, (id % 9 != 0).then_some(id % 7), (id / 3) % 2 == 0))
        .collect();
    let expect = |f: &dyn Fn(&Row) -> bool| rows.iter().filter(|r| f(r)).count();
    let count = |db: &mut Database, sql: &str| query(db, sql).len();
    assert_eq!(
        count(
            &mut db,
            "SELECT * FROM t WHERE city = 'oslo' AND nights > 2 AND nights <= 5"
        ),
        expect(&|r| r.0 == 0 && r.1.is_some_and(|n| n > 2 && n <= 5))
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT * FROM t WHERE city = 'oslo' AND nights IS NULL"
        ),
        expect(&|r| r.0 == 0 && r.1.is_none()),
        "NULL keys are indexed"
    );
    assert_eq!(
        count(&mut db, "SELECT * FROM t WHERE lower(name) = 'bob'"),
        expect(&|r| r.0 == 0)
    );
    assert_eq!(
        count(
            &mut db,
            "SELECT id FROM t WHERE status = 'open' AND nights < 2"
        ),
        expect(&|r| r.2 && r.1.is_some_and(|n| n < 2))
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT name, id FROM t WHERE status = 'closed' AND id < 6"
        ),
        vec![
            vec![text("Bob"), int(3)],
            vec![text("ALICE"), int(4)],
            vec![text("carol"), int(5)]
        ]
    );
}

#[test]
fn indexes_of_every_kind_answer_like_no_index() {
    let mut db = in_memory();
    let indexes = [
        "CREATE INDEX {t}_a ON {t} (city, nights)",
        "CREATE INDEX {t}_b ON {t} (LOWER(city), nights * 2 + 1)",
        "CREATE INDEX {t}_c ON {t} (nights) INCLUDE (city, score) WHERE score > 0.5",
        "CREATE INDEX {t}_d ON {t} (score, city) WHERE nights IS NOT NULL",
        "CREATE INDEX {t}_e ON {t} (vip) INCLUDE (id)",
    ];
    let tables = [
        "heap_plain",
        "heap_indexed",
        "heap_hashed",
        "lsm_plain",
        "lsm_indexed",
        "lsm_hashed",
    ];
    for t in tables {
        let storage = if t.starts_with("lsm") { "LSM" } else { "HEAP" };
        db.execute(&format!(
            "CREATE TABLE {t} (id INT PRIMARY KEY, city TEXT, nights INT, score REAL, vip BOOL) USING {storage}"
        ))
        .unwrap();
        if !t.ends_with("plain") {
            for index in indexes {
                let index = index.replace("{t}", t);
                let index = match t.ends_with("hashed") {
                    true => index.replacen(&format!("ON {t} "), &format!("ON {t} USING HASH "), 1),
                    false => index,
                };
                db.execute(&index).unwrap();
            }
        }
        let values: Vec<String> = (0..1500i64)
            .map(|id| {
                let nights = if id % 11 == 0 {
                    "NULL".to_string()
                } else {
                    (id % 7 - 3).to_string()
                };
                let score = if id % 13 == 0 {
                    "NULL".to_string()
                } else {
                    format!("{}.25", id % 4)
                };
                let city = ["Lima", "oslo", "OSLO", "rome", "a\0b", ""][id as usize % 6];
                format!("({id}, '{city}', {nights}, {score}, {})", id % 5 == 0)
            })
            .collect();
        db.execute(&format!("INSERT INTO {t} VALUES {}", values.join(", ")))
            .unwrap();
    }
    let queries = [
        "SELECT * FROM {t} WHERE city = 'oslo' AND nights = 1",
        "SELECT id, nights FROM {t} WHERE city = 'oslo' AND nights >= -1",
        "SELECT city, nights FROM {t} WHERE city = 'rome'",
        "SELECT * FROM {t} WHERE LOWER(city) = 'oslo' AND nights * 2 + 1 > 1",
        "SELECT * FROM {t} WHERE lower(city) >= 'oslo'",
        "SELECT city, score FROM {t} WHERE score > 0.5 AND nights = 2",
        "SELECT * FROM {t} WHERE nights < 0 AND score > 0.5",
        "SELECT nights FROM {t} WHERE score > 0.5",
        "SELECT * FROM {t} WHERE score = 2.25 AND nights > 0",
        "SELECT city FROM {t} WHERE score >= 1 AND city > 'a' AND nights != 3",
        "SELECT id FROM {t} WHERE vip = TRUE",
        "SELECT * FROM {t} WHERE vip = FALSE AND id > 1400",
        // Every key of each index fixed, for hash indexes.
        "SELECT * FROM {t} WHERE LOWER(city) = 'oslo' AND nights * 2 + 1 = 3",
        "SELECT city, score FROM {t} WHERE nights = 1 AND score > 0.5",
        "SELECT * FROM {t} WHERE score = 1.25 AND city = 'rome' AND nights IS NOT NULL",
        "SELECT * FROM {t} WHERE score = 1 AND city = 'rome' AND nights IS NOT NULL",
        "SELECT * FROM {t} WHERE city = 'a\0b' AND nights = -3.0",
    ];
    let changes = [
        "UPDATE {t} SET nights = nights + 1 WHERE city = 'oslo'",
        // In and out of partial indexes, and changes to included columns alone.
        "UPDATE {t} SET score = 0.0 WHERE id - id / 4 * 4 = 3",
        "UPDATE {t} SET score = 3.25, nights = NULL WHERE id - id / 10 * 10 = 1",
        "UPDATE {t} SET city = 'OSLO' WHERE city = 'Lima' AND id < 700",
        "UPDATE {t} SET vip = NOT vip WHERE id > 1000",
        "UPDATE {t} SET id = id + 5000 WHERE nights = 2",
        "DELETE FROM {t} WHERE score < 1 OR city = 'rome'",
        "INSERT INTO {t} VALUES (9000, 'oslo', 1, 0.75, TRUE), (9001, NULL, NULL, NULL, NULL)",
    ];
    for change in [""].into_iter().chain(changes) {
        if !change.is_empty() {
            for t in tables {
                db.execute(&change.replace("{t}", t))
                    .unwrap_or_else(|e| panic!("{change} on {t}: {e:?}"));
            }
        }
        for sql in queries {
            let results: Vec<Vec<Vec<Value>>> = tables
                .iter()
                .map(|t| {
                    let mut rows = query(&mut db, &sql.replace("{t}", t));
                    rows.sort_by(|a, b| {
                        a.iter()
                            .zip(b)
                            .map(|(a, b)| a.sort_cmp(b))
                            .find(|o| o.is_ne())
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    rows
                })
                .collect();
            for (t, rows) in tables.iter().zip(&results) {
                assert_eq!(rows, &results[0], "{sql} on {t} after {change}");
            }
        }
    }
    for t in ["heap_indexed", "lsm_indexed", "heap_hashed", "lsm_hashed"] {
        let plans: Vec<String> = queries
            .iter()
            .map(|sql| plan(&mut db, &sql.replace("{t}", t)))
            .collect();
        for index in ["a", "b", "c", "d", "e"] {
            assert!(
                plans
                    .iter()
                    .any(|p| p.contains(&format!("INDEX {t}_{index} "))),
                "{t}_{index} unused: {plans:#?}"
            );
        }
        assert!(plans.iter().any(|p| p.contains("COVERING ")));
        assert_eq!(
            plans.iter().any(|p| p.contains("HASH INDEX")),
            t.ends_with("hashed")
        );
    }
}

#[test]
fn hash_indexes_find_rows_whose_every_key_is_fixed() {
    let file = TempDb::new("hash-index");
    let mut db = open(&file.0).unwrap();
    db.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT, n INT, x REAL);
         CREATE INDEX t_name ON t USING HASH (name);
         CREATE INDEX t_name_n ON t USING hash (name, n) INCLUDE (x);
         CREATE INDEX t_n_tree ON t (n);
         CREATE INDEX t_n ON t USING HASH (n);",
    )
    .unwrap();
    let values: Vec<String> = (0..3000)
        .map(|i| {
            let name = if i % 50 == 0 {
                "NULL".to_string()
            } else {
                format!("'name {}'", i % 400)
            };
            format!("({i}, {name}, {}, {}.5)", i % 9, i % 4)
        })
        .collect();
    db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
        .unwrap();

    assert_eq!(
        plan(&mut db, "SELECT * FROM t WHERE name = 'name 7'"),
        "SEARCH t USING HASH INDEX t_name (name = 'name 7')"
    );
    assert_eq!(
        plan(&mut db, "SELECT x FROM t WHERE n = 3 AND name = 'name 7'"),
        "SEARCH t USING COVERING HASH INDEX t_name_n (name = 'name 7' AND n = 3)"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM t WHERE n = 3"),
        "SEARCH t USING HASH INDEX t_n (n = 3)",
        "before the B+ tree"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM t WHERE n > 3"),
        "SEARCH t USING INDEX t_n_tree (n > 3)",
        "no ranges"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM t WHERE name > 'name 7'"),
        "SCAN t"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM t WHERE name IS NULL"),
        "SCAN t"
    );
    assert_eq!(
        query(&mut db, "SELECT COUNT(*) FROM t WHERE name = 'name 7'"),
        [[int(8)]]
    );
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE name = 'name 7' AND n = 7"),
        [[int(7)]]
    );
    assert_eq!(
        query(&mut db, "SELECT COUNT(*) FROM t WHERE n = 3.0"),
        [[int(333)]],
        "a REAL finds the INT it equals"
    );
    assert_eq!(
        query(&mut db, "SELECT COUNT(*) FROM t WHERE n = 3.5"),
        [[int(0)]]
    );

    // Lookups for an index join go through the hash index.
    db.execute("CREATE TABLE l (k INT); INSERT INTO l VALUES (1), (2), (NULL)")
        .unwrap();
    let join = "SELECT COUNT(*) FROM l JOIN t ON t.n = l.k";
    assert!(
        explain(&mut db, join).contains("LOOKUP t USING HASH INDEX t_n (t.n = l.k)"),
        "{}",
        explain(&mut db, join)
    );
    assert_eq!(query(&mut db, join), [[int(334 + 334)]]);

    // Changes, and a failed statement, leave the indexes matching the rows.
    db.execute("UPDATE t SET name = 'renamed', n = n + 100 WHERE id < 1000")
        .unwrap();
    db.execute("DELETE FROM t WHERE id >= 2500").unwrap();
    assert!(
        db.execute("INSERT INTO t VALUES (5000, 'new', 1, 1.0), (1, 'dup', 1, 1.0)")
            .is_err()
    );
    db.execute("BEGIN; DELETE FROM t WHERE n = 103; ROLLBACK")
        .unwrap();
    drop(db);
    let mut db = open(&file.0).unwrap();
    for (sql, count) in [
        ("SELECT COUNT(*) FROM t WHERE name = 'renamed'", 1000),
        ("SELECT COUNT(*) FROM t WHERE name = 'name 207'", 4),
        ("SELECT COUNT(*) FROM t WHERE name = 'name 7'", 4),
        ("SELECT COUNT(*) FROM t WHERE name = 'new'", 0),
        ("SELECT COUNT(*) FROM t WHERE n = 103", 111),
        ("SELECT COUNT(*) FROM t WHERE n = 3", 167),
        (
            "SELECT COUNT(*) FROM t WHERE name = 'renamed' AND n = 105",
            111,
        ),
    ] {
        assert!(plan(&mut db, sql).contains("HASH INDEX"), "{sql}");
        let scanned = query(&mut db, &(sql.replace("WHERE", "WHERE id < 0 OR (") + ")"));
        assert_eq!(query(&mut db, sql), scanned, "{sql}");
        assert_eq!(query(&mut db, sql), [[int(count)]], "{sql}");
    }

    // A unique hash index rejects duplicates, NULLs aside.
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, email TEXT); CREATE UNIQUE INDEX u_email ON u USING HASH (email)").unwrap();
    db.execute("INSERT INTO u VALUES (1, 'a'), (2, 'b'), (3, NULL), (4, NULL)")
        .unwrap();
    let error = db.execute("INSERT INTO u VALUES (5, 'a')").unwrap_err();
    assert!(
        format!("{error:?}").contains("duplicate value in unique index u_email"),
        "{error:?}"
    );
    db.execute("UPDATE u SET email = 'c' WHERE id = 1; INSERT INTO u VALUES (5, 'a')")
        .unwrap();
    assert!(
        db.execute("CREATE UNIQUE INDEX u_bad ON u USING HASH (id - id)")
            .is_err()
    );
    assert!(
        db.execute("CREATE INDEX u_bad ON u USING ROPE (email)")
            .is_err()
    );

    // Dropping a hash index frees its pages for the next one.
    let size = file_size(&mut db, &file.0);
    db.execute(
        "DROP INDEX t_name; DROP INDEX t_name_n; CREATE INDEX t_name ON t USING HASH (name)",
    )
    .unwrap();
    db.execute("CREATE INDEX t_name_n ON t USING HASH (name, n) INCLUDE (x)")
        .unwrap();
    assert_eq!(file_size(&mut db, &file.0), size);
}

#[test]
fn unique_indexes_of_several_keys_or_partial_ones_reject_duplicates() {
    let mut db = in_memory();
    db.execute("CREATE TABLE r (id INT PRIMARY KEY, room INT, day INT, active BOOL)")
        .unwrap();
    db.execute("CREATE UNIQUE INDEX r_booking ON r (room, day) WHERE active")
        .unwrap();
    db.execute(
        "INSERT INTO r VALUES (1, 1, 1, TRUE), (2, 1, 2, TRUE), (3, 2, 1, TRUE), (4, 1, 1, FALSE)",
    )
    .unwrap();
    // Inactive rows, and rows with a NULL key, are not checked.
    db.execute("INSERT INTO r VALUES (5, 1, 1, FALSE), (6, 1, NULL, TRUE), (7, 1, NULL, TRUE)")
        .unwrap();
    let duplicate = db
        .execute("INSERT INTO r VALUES (8, 1, 2, TRUE)")
        .unwrap_err()
        .0;
    assert_eq!(
        duplicate,
        "duplicate value in unique index r_booking: (1, 2)"
    );
    assert!(
        db.execute("INSERT INTO r VALUES (8, 3, 3, TRUE), (9, 3, 3, TRUE)")
            .is_err(),
        "among the new rows"
    );
    assert!(
        db.execute("UPDATE r SET active = TRUE WHERE id = 4")
            .is_err(),
        "a row coming into the index"
    );
    // Rows may swap keys in one statement, and leave the index to free theirs.
    db.execute("UPDATE r SET day = 3 - day WHERE room = 1 AND id < 3")
        .unwrap();
    db.execute("UPDATE r SET active = FALSE WHERE id = 2")
        .unwrap();
    db.execute("UPDATE r SET active = TRUE WHERE id = 4")
        .unwrap();
    assert_eq!(
        query(
            &mut db,
            "SELECT id FROM r WHERE room = 1 AND day = 2 AND active"
        ),
        vec![vec![int(1)]]
    );

    assert!(
        db.execute("CREATE UNIQUE INDEX r_day ON r (room, day)")
            .is_err(),
        "inactive rows clash"
    );
    db.execute("CREATE UNIQUE INDEX r_room_id ON r (room, id)")
        .unwrap();
    let message = db
        .execute("CREATE UNIQUE INDEX r_lower ON r (room * 0)")
        .unwrap_err()
        .0;
    assert_eq!(
        message,
        "cannot create unique index r_lower: duplicate value 0"
    );
}

#[test]
fn index_definitions_are_checked() {
    let mut db = setup();
    for (sql, error) in [
        (
            "CREATE INDEX i ON users (missing)",
            "unknown column: missing",
        ),
        (
            "CREATE INDEX i ON users (LOWER(missing))",
            "unknown column: missing",
        ),
        (
            "CREATE INDEX i ON users (other.name)",
            "unknown column: other.name",
        ),
        (
            "CREATE INDEX i ON users (name) INCLUDE (missing)",
            "unknown column: missing",
        ),
        (
            "CREATE INDEX i ON users (name) WHERE missing > 1",
            "unknown column: missing",
        ),
        (
            "CREATE INDEX i ON users (COUNT(*))",
            "an index key cannot use an aggregate",
        ),
        (
            "CREATE INDEX i ON users (name) WHERE id IN (SELECT id FROM users)",
            "an index's WHERE cannot use a subquery",
        ),
        (
            "CREATE INDEX i ON users (name) WHERE age",
            "WHERE clause must be boolean, got 30",
        ),
        ("CREATE INDEX i ON users (age / 0)", "division by zero"),
    ] {
        assert_eq!(db.execute(sql).unwrap_err().0, error, "{sql}");
    }
    assert!(
        db.execute("CREATE INDEX i ON users (name) INCLUDE ()")
            .is_err()
    );
    assert!(db.execute("CREATE INDEX i ON users (name) WHERE").is_err());
    assert_eq!(
        plan(&mut db, "SELECT * FROM users WHERE name = 'bob'"),
        "SCAN users",
        "no index was left behind"
    );

    // A key or condition that fails on a new row fails the write.
    db.execute("CREATE INDEX users_ratio ON users (100 / age) WHERE age IS NOT NULL")
        .unwrap();
    assert!(
        db.execute("INSERT INTO users VALUES (4, 'dan', 0, TRUE)")
            .is_err()
    );
    db.execute("INSERT INTO users VALUES (4, 'dan', NULL, TRUE)")
        .unwrap();
    assert_eq!(
        query(&mut db, "SELECT id FROM users WHERE 100 / age = 4"),
        vec![vec![int(2)]]
    );
}

#[test]
fn indexes_of_every_kind_survive_reopening() {
    let path = TempDb::new("index-kinds");
    {
        let mut db = open(&path.0).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT, n INT, x REAL)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a', -1, 1.0), (2, 'B', 2, 2.5), (3, 'c', NULL, -0.5), (4, NULL, 4, 3.0)")
            .unwrap();
        db.execute("CREATE INDEX t_k ON t (LOWER(name), - -n) INCLUDE (x, name) WHERE x > 0 AND name IS NOT NULL").unwrap();
        db.execute("CREATE UNIQUE INDEX t_u ON t (x, n)").unwrap();
    }
    let mut db = open(&path.0).unwrap();
    assert_eq!(
        plan(
            &mut db,
            "SELECT x FROM t WHERE lower(name) = 'b' AND x > 0 AND name IS NOT NULL"
        ),
        "SEARCH t USING COVERING INDEX t_k (LOWER(name) = 'b')"
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT x FROM t WHERE lower(name) = 'b' AND x > 0 AND name IS NOT NULL"
        ),
        vec![vec![Value::Real(2.5)]]
    );
    assert_eq!(
        query(&mut db, "SELECT n, x FROM t WHERE x = 3"),
        vec![vec![int(4), Value::Real(3.0)]]
    );
    assert!(
        db.execute("INSERT INTO t VALUES (5, 'e', 2, 2.5)").is_err(),
        "the unique index was reopened"
    );
    db.execute("ROLLBACK").unwrap_err();
    db.execute("BEGIN; INSERT INTO t VALUES (6, 'b', 9, 9.0); ROLLBACK")
        .unwrap();
    assert_eq!(
        query(
            &mut db,
            "SELECT id FROM t WHERE lower(name) = 'b' AND x > 0 AND name IS NOT NULL"
        )
        .len(),
        1
    );
}

#[test]
fn order_by_gives_the_same_rows_when_sorts_spill_to_disk() {
    let mut db = in_memory();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, k INT, name TEXT)")
        .unwrap();
    let values: Vec<String> = (0..3000i64)
        .map(|id| {
            let k = if id % 13 == 0 {
                "NULL".to_string()
            } else {
                (id * 7 % 10).to_string()
            };
            format!(
                "({id}, {k}, '{}')",
                ["ann", "bo", "cy", ""][id as usize % 4].repeat(id as usize % 5)
            )
        })
        .collect();
    db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
        .unwrap();

    let queries = [
        "SELECT * FROM t ORDER BY k",
        "SELECT * FROM t ORDER BY k DESC, name",
        "SELECT id, name FROM t ORDER BY name DESC, k * -1",
        "SELECT * FROM t WHERE k > 3 ORDER BY name, id DESC",
        "SELECT * FROM t ORDER BY k LIMIT 25",
        "SELECT * FROM t ORDER BY name DESC LIMIT 2999",
        "SELECT * FROM t ORDER BY k LIMIT 0",
        "SELECT id FROM t WHERE id >= 100 ORDER BY k DESC LIMIT 5000",
    ];
    let in_memory: Vec<Vec<Vec<Value>>> = queries.iter().map(|sql| query(&mut db, sql)).collect();
    db.set_sort_memory(1000); // a few rows per run
    for (sql, expected) in queries.iter().zip(&in_memory) {
        assert_eq!(&query(&mut db, sql), expected, "{sql}");
    }

    // Equal keys keep the order the rows are read in, as a stable sort would.
    let mut expected = query(&mut db, "SELECT k, id FROM t");
    expected.sort_by(|a, b| a[0].sort_cmp(&b[0]));
    assert_eq!(query(&mut db, "SELECT k, id FROM t ORDER BY k"), expected);
    assert!(expected[0][0].is_null(), "NULL sorts first");
    expected.sort_by(|a, b| b[0].sort_cmp(&a[0]));
    assert_eq!(
        query(&mut db, "SELECT k, id FROM t ORDER BY k DESC LIMIT 3"),
        expected[..3]
    );
}

#[test]
fn limit_without_order_by_stops_reading_rows() {
    let mut db = setup();
    assert_eq!(
        query(&mut db, "SELECT id FROM users LIMIT 2"),
        vec![vec![int(1)], vec![int(2)]]
    );
    assert!(query(&mut db, "SELECT id FROM users LIMIT 0").is_empty());
    // The third row would divide by zero, but two rows are found before it.
    let sql = "SELECT id FROM users WHERE 10 / (id - 3) < 0";
    assert!(db.execute(sql).is_err());
    assert_eq!(
        query(&mut db, &format!("{sql} LIMIT 2")),
        vec![vec![int(1)], vec![int(2)]]
    );
    // With an order, every row is read, so the error is still reported.
    assert!(db.execute(&format!("{sql} ORDER BY id LIMIT 2")).is_err());
}

fn setup_people(storage: &str) -> Database {
    let mut db = in_memory();
    db.execute(&format!(
        "CREATE TABLE p (id INT PRIMARY KEY, city TEXT, age INT, vip BOOL) USING {storage}"
    ))
    .unwrap();
    db.execute(
        "INSERT INTO p VALUES (1, 'oslo', 30, TRUE), (2, 'lima', NULL, FALSE), (3, 'oslo', 21, FALSE),
         (4, NULL, 40, TRUE), (5, 'lima', 25, NULL), (6, 'oslo', 45, TRUE), (7, NULL, NULL, FALSE)",
    )
    .unwrap();
    db
}

#[test]
fn aggregates_over_the_whole_table() {
    let mut db = setup_people("HEAP");
    assert_eq!(
        query(
            &mut db,
            "SELECT COUNT(*), COUNT(age), COUNT(city), SUM(age), MIN(age), MAX(age), AVG(age) FROM p"
        ),
        vec![vec![
            int(7),
            int(5),
            int(5),
            int(161),
            int(21),
            int(45),
            Value::Real(32.2)
        ]]
    );
    // Over no rows there is still one row: COUNT is 0, the others NULL.
    assert_eq!(
        query(
            &mut db,
            "SELECT COUNT(*), SUM(age), MIN(city), AVG(age) FROM p WHERE id > 100"
        ),
        vec![vec![int(0), Value::Null, Value::Null, Value::Null]]
    );
    assert_eq!(
        query(&mut db, "SELECT MIN(city), MAX(city), MAX(vip) FROM p"),
        vec![vec![text("lima"), text("oslo"), Value::Bool(true)]]
    );
    // Expressions over aggregates, and the same aggregate twice.
    assert_eq!(
        query(
            &mut db,
            "SELECT SUM(age) - MIN(age) * 2, COUNT(*) + COUNT(*) FROM p WHERE vip"
        ),
        vec![vec![int(115 - 60), int(6)]]
    );
    // AVG is a REAL, while / of INTs truncates toward zero.
    db.execute("CREATE TABLE n (x INT); INSERT INTO n VALUES (-3), (-4), (NULL)")
        .unwrap();
    assert_eq!(
        query(&mut db, "SELECT AVG(x), SUM(x) / COUNT(x) FROM n"),
        vec![vec![Value::Real(-3.5), int(-3)]]
    );
}

#[test]
fn group_by_with_having_order_by_and_limit() {
    let mut db = setup_people("HEAP");
    // NULL keys form one group; groups come out in the order they were first seen.
    assert_eq!(
        query(
            &mut db,
            "SELECT city, COUNT(*), MAX(age) FROM p GROUP BY city"
        ),
        vec![
            vec![text("oslo"), int(3), int(45)],
            vec![text("lima"), int(2), int(25)],
            vec![Value::Null, int(2), int(40)],
        ]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT city FROM p GROUP BY city HAVING MIN(age) < 25 OR COUNT(age) = 0 ORDER BY city"
        ),
        vec![vec![text("oslo")]]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT city, SUM(age) FROM p WHERE age > 0 GROUP BY city ORDER BY SUM(age) DESC LIMIT 2"
        ),
        vec![vec![text("oslo"), int(96)], vec![Value::Null, int(40)]]
    );
    // Grouping by an expression, and using it in the select list and ORDER BY.
    assert_eq!(
        query(
            &mut db,
            "SELECT age / 10 * 10, COUNT(*) FROM p WHERE age > 0 GROUP BY age / 10 ORDER BY age / 10"
        ),
        vec![
            vec![int(20), int(2)],
            vec![int(30), int(1)],
            vec![int(40), int(2)]
        ]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT city, vip, COUNT(*) FROM p GROUP BY city, vip HAVING COUNT(*) > 1"
        ),
        vec![vec![text("oslo"), Value::Bool(true), int(2)]]
    );
    // A repeated GROUP BY expression changes nothing.
    assert_eq!(
        query(
            &mut db,
            "SELECT city, COUNT(*) FROM p GROUP BY city, city ORDER BY city"
        ),
        vec![
            vec![Value::Null, int(2)],
            vec![text("lima"), int(2)],
            vec![text("oslo"), int(3)]
        ]
    );
    // HAVING without GROUP BY makes one group, which it may filter out.
    assert_eq!(
        query(&mut db, "SELECT COUNT(*) FROM p HAVING COUNT(*) > 100"),
        Vec::<Vec<Value>>::new()
    );
    assert!(
        query(&mut db, "SELECT city FROM p WHERE id > 100 GROUP BY city").is_empty(),
        "no rows, no groups"
    );
    match db
        .execute("SELECT city, COUNT(*), SUM(age) * 2 FROM p GROUP BY city")
        .unwrap()
        .pop()
    {
        Some(QueryResult::Rows { columns, .. }) => {
            assert_eq!(columns, ["city", "count", "?column?"])
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn every_storage_and_access_path_aggregates_alike() {
    let queries = [
        "SELECT city, COUNT(*), COUNT(age), SUM(age), MIN(age), MAX(age), AVG(age) FROM p GROUP BY city ORDER BY city",
        "SELECT vip, MIN(city), MAX(id) FROM p GROUP BY vip ORDER BY vip",
        "SELECT COUNT(*), SUM(id) FROM p WHERE id >= 3 AND id < 7",
        "SELECT city, COUNT(*) FROM p WHERE city = 'oslo' GROUP BY city",
        "SELECT age, COUNT(*) FROM p WHERE age > 22 GROUP BY age ORDER BY COUNT(*) DESC, age LIMIT 3",
        "SELECT DISTINCT city, vip FROM p ORDER BY city, vip",
        "SELECT DISTINCT city FROM p WHERE city = 'oslo'",
        "SELECT vip, COUNT(DISTINCT city), SUM(DISTINCT age) FROM p GROUP BY vip ORDER BY vip",
    ];
    let mut results = Vec::new();
    for storage in ["HEAP", "LSM", "COLUMN"] {
        let mut db = setup_people(storage);
        if storage != "COLUMN" {
            db.execute("CREATE INDEX p_city ON p (city); CREATE INDEX p_age ON p (age)")
                .unwrap();
        }
        results.push(
            queries
                .iter()
                .map(|sql| query(&mut db, sql))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(results[0], results[1], "heap and LSM");
    assert_eq!(results[0], results[2], "heap and column");
}

#[test]
fn misplaced_aggregates_and_ungrouped_columns_are_rejected() {
    let mut db = setup_people("HEAP");
    let errors = [
        ("SELECT * FROM p GROUP BY city", "SELECT *"),
        (
            "SELECT age FROM p GROUP BY city",
            "column age must appear in GROUP BY",
        ),
        (
            "SELECT COUNT(*), age FROM p",
            "column age must appear in GROUP BY",
        ),
        (
            "SELECT city FROM p GROUP BY city ORDER BY age",
            "column age must appear in GROUP BY",
        ),
        ("SELECT COUNT(SUM(age)) FROM p", "cannot be nested"),
        (
            "SELECT city FROM p WHERE COUNT(*) > 1",
            "not allowed in WHERE",
        ),
        (
            "SELECT COUNT(*) FROM p GROUP BY COUNT(*)",
            "not allowed in GROUP BY",
        ),
        ("UPDATE p SET age = MAX(age)", "not allowed in UPDATE"),
        ("DELETE FROM p WHERE age = MIN(age)", "not allowed in WHERE"),
        (
            "INSERT INTO p (id) VALUES (COUNT(*))",
            "not allowed in VALUES",
        ),
        ("SELECT SUM(city) FROM p", "SUM expects a number"),
        ("SELECT AVG(vip) FROM p", "AVG expects a number"),
        ("SELECT MEDIAN(age) FROM p", "unknown function: MEDIAN"),
        (
            "SELECT COUNT(*) FROM p GROUP BY nope",
            "unknown column: nope",
        ),
        ("SELECT nope FROM p GROUP BY city", "unknown column: nope"),
    ];
    for (sql, expected) in errors {
        let e = db.execute(sql).expect_err(sql);
        assert!(e.0.contains(expected), "{sql}: {e}");
    }
    db.execute(&format!(
        "INSERT INTO p VALUES (8, 'x', {}, TRUE), (9, 'x', 1, TRUE)",
        i64::MAX
    ))
    .unwrap();
    assert!(
        db.execute("SELECT SUM(age) FROM p WHERE city = 'x'")
            .unwrap_err()
            .0
            .contains("overflow")
    );
    assert_eq!(
        query(&mut db, "SELECT AVG(age) FROM p WHERE city = 'x'"),
        vec![vec![int(i64::MAX / 2 + 1)]]
    );
}

#[test]
fn explain_shows_every_step_of_the_plan() {
    let mut db = setup_people("HEAP");
    db.execute("CREATE INDEX p_city ON p (city)").unwrap();
    let sql = "SELECT city, COUNT(*), MAX(age) + 1 FROM p WHERE vip AND id > 1
               GROUP BY city HAVING COUNT(*) > 1 ORDER BY COUNT(*) DESC LIMIT 5";
    assert_eq!(
        explain(&mut db, sql),
        "PROJECT city, COUNT(*), MAX(age) + 1
  SORT BY COUNT(*) DESC (TOP 5)
    FILTER COUNT(*) > 1
      AGGREGATE COUNT(*), MAX(age) BY city
        FILTER vip AND (id > 1)
          SEARCH p USING INDEX p_pkey (id > 1)"
    );
    assert_eq!(
        explain(&mut db, "SELECT * FROM p WHERE city = 'oslo' LIMIT 3"),
        "LIMIT 3\n  FILTER city = 'oslo'\n    SEARCH p USING INDEX p_city (city = 'oslo')"
    );
    assert_eq!(
        explain(&mut db, "SELECT COUNT(*) FROM p"),
        "PROJECT COUNT(*)\n  AGGREGATE COUNT(*)\n    SCAN p"
    );
    assert_eq!(
        explain(&mut db, "UPDATE p SET age = 1 WHERE age IS NULL"),
        "FILTER age IS NULL\n  SCAN p"
    );
    assert_eq!(explain(&mut db, "DELETE FROM p"), "SCAN p");
}

#[test]
fn select_distinct_drops_repeated_rows() {
    let mut db = setup_people("HEAP");
    let column = |rows: Vec<Vec<Value>>| {
        rows.into_iter()
            .map(|mut r| r.remove(0))
            .collect::<Vec<_>>()
    };
    // NULLs are equal to each other here; rows come out in the order first seen.
    assert_eq!(
        column(query(&mut db, "SELECT DISTINCT city FROM p")),
        [text("oslo"), text("lima"), Value::Null]
    );
    assert_eq!(
        column(query(&mut db, "SELECT DISTINCT city FROM p ORDER BY city")),
        [Value::Null, text("lima"), text("oslo")]
    );
    // LIMIT counts distinct rows, with or without ORDER BY.
    assert_eq!(
        column(query(
            &mut db,
            "SELECT DISTINCT city FROM p ORDER BY city DESC LIMIT 2"
        )),
        [text("oslo"), text("lima")]
    );
    assert_eq!(
        column(query(&mut db, "SELECT DISTINCT city FROM p LIMIT 2")),
        [text("oslo"), text("lima")]
    );
    // Rows are distinct as a whole.
    let pairs = query(
        &mut db,
        "SELECT DISTINCT city, vip FROM p ORDER BY city, vip",
    );
    let (t, f) = (Value::Bool(true), Value::Bool(false));
    assert_eq!(
        pairs,
        [
            vec![Value::Null, f.clone()],
            vec![Value::Null, t.clone()],
            vec![text("lima"), Value::Null],
            vec![text("lima"), f.clone()],
            vec![text("oslo"), f],
            vec![text("oslo"), t],
        ]
    );
    assert_eq!(query(&mut db, "SELECT DISTINCT * FROM p").len(), 7);
    assert_eq!(
        column(query(
            &mut db,
            "SELECT DISTINCT age / 10 FROM p WHERE age > 0 ORDER BY age / 10"
        )),
        [int(2), int(3), int(4)]
    );
    // After grouping: the groups' counts are 3, 2 and 2.
    assert_eq!(
        column(query(
            &mut db,
            "SELECT DISTINCT COUNT(*) FROM p GROUP BY city"
        )),
        [int(3), int(2)]
    );

    let mut shop = setup_shop();
    let sql = "SELECT DISTINCT u.city FROM users u JOIN orders o ON o.user_id = u.id ORDER BY city";
    assert_eq!(column(query(&mut shop, sql)), [text("lima"), text("oslo")]);

    let e = db
        .execute("SELECT DISTINCT city FROM p ORDER BY age")
        .unwrap_err();
    assert!(
        e.0.contains("ORDER BY age must appear in the select list"),
        "{e}"
    );
    assert_eq!(
        explain(&mut db, "SELECT DISTINCT city FROM p ORDER BY city LIMIT 2"),
        "LIMIT 2\n  DISTINCT\n    PROJECT city\n      SORT BY city\n        SCAN p"
    );
}

#[test]
fn aggregates_over_distinct_values() {
    let mut db = setup_people("HEAP");
    // age / 10 is 3, 2, 4, 2, 4 (and NULL twice): distinct, 2, 3 and 4.
    assert_eq!(
        query(
            &mut db,
            "SELECT COUNT(DISTINCT city), COUNT(vip), COUNT(DISTINCT vip), SUM(age / 10), SUM(DISTINCT age / 10),
                    AVG(DISTINCT age / 10), MIN(DISTINCT age) FROM p"
        ),
        vec![vec![int(2), int(6), int(2), int(15), int(9), int(3), int(21)]]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT city, COUNT(DISTINCT vip) FROM p GROUP BY city ORDER BY city"
        ),
        [
            vec![Value::Null, int(2)],
            vec![text("lima"), int(1)],
            vec![text("oslo"), int(2)],
        ]
    );
    // In HAVING and ORDER BY too, and over no rows.
    assert_eq!(
        query(
            &mut db,
            "SELECT city FROM p GROUP BY city HAVING COUNT(DISTINCT vip) > 1 ORDER BY COUNT(DISTINCT age), city"
        ),
        [vec![Value::Null], vec![text("oslo")]]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT COUNT(DISTINCT age), SUM(DISTINCT age) FROM p WHERE id > 100"
        ),
        [vec![int(0), Value::Null]]
    );
    match db
        .execute("SELECT COUNT(DISTINCT city) FROM p")
        .unwrap()
        .pop()
    {
        Some(QueryResult::Rows { columns, .. }) => assert_eq!(columns, ["count"]),
        other => panic!("expected rows, got {other:?}"),
    }
    assert_eq!(
        explain(&mut db, "SELECT COUNT(DISTINCT city), COUNT(city) FROM p"),
        "PROJECT COUNT(DISTINCT city), COUNT(city)\n  AGGREGATE COUNT(DISTINCT city), COUNT(city)\n    SCAN p"
    );
    assert!(db.execute("SELECT COUNT(DISTINCT *) FROM p").is_err());
}

#[test]
fn query_streams_rows_as_they_are_produced() {
    let mut db = setup();
    {
        let mut rows = db
            .query("SELECT name, id * 10 FROM users WHERE id >= 2")
            .unwrap();
        assert_eq!(rows.columns, ["name", "?column?"]);
        assert_eq!(rows.next().unwrap().unwrap(), vec![text("bob"), int(20)]);
        assert_eq!(rows.next().unwrap().unwrap(), vec![text("carol"), int(30)]);
        assert!(rows.next().is_none());
    }
    {
        // Rows are made only when asked for: the third would divide by zero.
        let sql = "SELECT id FROM users WHERE 10 / (id - 3) < 0";
        let mut rows = db.query(sql).unwrap();
        assert_eq!(rows.next().unwrap().unwrap(), vec![int(1)]);
        assert_eq!(rows.next().unwrap().unwrap(), vec![int(2)]);
        assert!(rows.next().unwrap().is_err());
        // A sort needs every row before its first one.
        assert!(
            db.query(&format!("{sql} ORDER BY id"))
                .unwrap()
                .next()
                .unwrap()
                .is_err()
        );
    }
    assert!(db.query("DELETE FROM users").is_err(), "only a SELECT");
    assert!(
        db.query("SELECT * FROM users; SELECT * FROM users")
            .is_err(),
        "only one"
    );
    assert!(db.query("SELECT nope FROM missing").is_err());
    // The same rows as execute gives.
    let sql = "SELECT * FROM users ORDER BY name DESC";
    let streamed: Vec<Vec<Value>> = db.query(sql).unwrap().map(|row| row.unwrap()).collect();
    assert_eq!(streamed, query(&mut db, sql));
}

fn setup_shop() -> Database {
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, city TEXT);
         CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, total INT);
         CREATE INDEX orders_user ON orders (user_id);
         INSERT INTO users VALUES (1, 'ann', 'oslo'), (2, 'bo', 'lima'), (3, 'cy', 'oslo');
         INSERT INTO orders VALUES (10, 1, 5), (11, 1, 7), (12, 2, 3), (13, NULL, 9), (14, 99, 1);",
    )
    .unwrap();
    db
}

#[test]
fn inner_and_left_joins() {
    let mut db = setup_shop();
    let mut pairs = |sql: &str| {
        let rows = query(&mut db, sql);
        rows.into_iter()
            .map(|r| (r[0].clone(), r[1].clone()))
            .collect::<Vec<_>>()
    };
    let join = "FROM users u LEFT JOIN orders o ON o.user_id = u.id";
    assert_eq!(
        pairs("SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id ORDER BY o.id"),
        [
            (text("ann"), int(10)),
            (text("ann"), int(11)),
            (text("bo"), int(12))
        ]
    );
    // Every user, with NULLs where there is no order.
    assert_eq!(
        pairs(&format!("SELECT u.name, o.id {join} ORDER BY u.id, o.id")),
        [
            (text("ann"), int(10)),
            (text("ann"), int(11)),
            (text("bo"), int(12)),
            (text("cy"), Value::Null)
        ]
    );
    // Every order; NULL and dangling user ids match no user.
    assert_eq!(
        pairs(
            "SELECT o.id, u.name FROM orders o LEFT OUTER JOIN users u ON u.id = o.user_id ORDER BY o.id"
        ),
        [
            (int(10), text("ann")),
            (int(11), text("ann")),
            (int(12), text("bo")),
            (int(13), Value::Null),
            (int(14), Value::Null)
        ]
    );
    // ON filters which rows match; WHERE filters the result, padded rows included.
    assert_eq!(
        pairs(&format!(
            "SELECT u.name, o.id {join} AND o.total > 6 ORDER BY u.id"
        )),
        [
            (text("ann"), int(11)),
            (text("bo"), Value::Null),
            (text("cy"), Value::Null)
        ]
    );
    assert_eq!(
        pairs(&format!("SELECT u.name, o.id {join} WHERE o.id IS NULL")),
        [(text("cy"), Value::Null)],
        "users without orders"
    );
    // Commas and CROSS JOIN pair every row, and WHERE can join them.
    assert_eq!(
        pairs("SELECT COUNT(*), 0 FROM users, orders"),
        [(int(15), int(0))]
    );
    let sql = "SELECT COUNT(*), 0 FROM users CROSS JOIN orders WHERE orders.user_id = users.id";
    assert_eq!(pairs(sql), [(int(3), int(0))]);
    // Without aliases, tables qualify their own columns; unambiguous names need no table.
    assert_eq!(
        pairs(
            "SELECT name, total FROM users INNER JOIN orders ON user_id = users.id WHERE total < 6 ORDER BY total"
        ),
        [(text("bo"), int(3)), (text("ann"), int(5))]
    );
    // SELECT * gives every column of every table, in FROM order.
    match db
        .execute("SELECT * FROM users u JOIN orders o ON o.user_id = u.id WHERE o.id = 12")
        .unwrap()
        .pop()
    {
        Some(QueryResult::Rows { columns, rows }) => {
            assert_eq!(columns, ["id", "name", "city", "id", "user_id", "total"]);
            assert_eq!(
                rows,
                vec![vec![
                    int(2),
                    text("bo"),
                    text("lima"),
                    int(12),
                    int(2),
                    int(3)
                ]]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn joins_of_three_tables_self_joins_and_grouping() {
    let mut db = setup_shop();
    db.execute(
        "CREATE TABLE cities (name TEXT PRIMARY KEY, country TEXT);
         INSERT INTO cities VALUES ('oslo', 'no'), ('lima', 'pe');",
    )
    .unwrap();
    assert_eq!(
        query(
            &mut db,
            "SELECT c.country, COUNT(o.id), SUM(o.total) FROM cities c JOIN users u ON u.city = c.name
             LEFT JOIN orders o ON o.user_id = u.id GROUP BY c.country ORDER BY c.country"
        ),
        vec![vec![text("no"), int(2), int(12)], vec![text("pe"), int(1), int(3)]]
    );
    // A table joined to itself, under two aliases.
    assert_eq!(
        query(
            &mut db,
            "SELECT a.name, b.name FROM users AS a JOIN users AS b ON a.city = b.city AND a.id < b.id"
        ),
        vec![vec![text("ann"), text("cy")]]
    );
    // GROUP BY a qualified column, selected unqualified or qualified.
    assert_eq!(
        query(
            &mut db,
            "SELECT city, COUNT(*) FROM users u JOIN orders o ON o.user_id = u.id GROUP BY u.city ORDER BY city"
        ),
        vec![vec![text("lima"), int(1)], vec![text("oslo"), int(2)]]
    );
    assert_eq!(
        explain(
            &mut db,
            "SELECT u.name, c.country FROM users u JOIN cities c ON c.name = u.city
             LEFT JOIN orders o ON o.user_id = u.id AND o.total > 4 WHERE c.country = 'no' AND o.id IS NULL"
        ),
        "PROJECT u.name, c.country
  FILTER o.id IS NULL
    LEFT HASH JOIN ON u.id = o.user_id
      HASH JOIN ON u.city = c.name
        SCAN users u
        FILTER c.country = 'no'
          SCAN cities c
      FILTER o.total > 4
        SCAN orders o"
    );
}

#[test]
fn every_join_method_and_storage_agrees_with_a_nested_loop() {
    // Conditions, each tried as written and as `(c) OR FALSE`: the same, but
    // hiding its equalities and single-table parts from the planner.
    let conditions = [
        ("b.a_k = a.k", "JOIN"),
        ("b.id = a.k", "JOIN"),
        ("b.a_k = a.k AND b.v > a.id", "JOIN"),
        ("a.k + 1 = b.v AND a.tag = 'x'", "JOIN"),
        ("b.v < a.k", "BLOCK NESTED LOOP JOIN"),
        ("b.a_k = a.k AND b.v > 5", "JOIN"),
    ];
    for storage in ["HEAP", "LSM", "COLUMN"] {
        for indexed in [false, true] {
            let mut db = in_memory();
            db.execute(&format!(
                "CREATE TABLE a (id INT PRIMARY KEY, k INT, tag TEXT) USING {storage}"
            ))
            .unwrap();
            db.execute(&format!(
                "CREATE TABLE b (id INT PRIMARY KEY, a_k INT, v INT) USING {storage}"
            ))
            .unwrap();
            if indexed && storage != "COLUMN" {
                db.execute("CREATE INDEX b_a_k ON b (a_k)").unwrap();
            }
            // Few left rows and many right ones, so that an index makes for an index join.
            let a: Vec<String> = (0..20i64)
                .map(|i| {
                    let k = if i % 9 == 0 {
                        "NULL".into()
                    } else {
                        (i % 13).to_string()
                    };
                    format!("({i}, {k}, '{}')", ["x", "y"][i as usize % 2])
                })
                .collect();
            let b: Vec<String> = (0..300i64)
                .map(|i| {
                    let a_k = if i % 7 == 0 {
                        "NULL".into()
                    } else {
                        (i % 17).to_string()
                    };
                    format!("({i}, {a_k}, {})", i % 11)
                })
                .collect();
            db.execute(&format!("INSERT INTO a VALUES {}", a.join(", ")))
                .unwrap();
            db.execute(&format!("INSERT INTO b VALUES {}", b.join(", ")))
                .unwrap();

            for (condition, method) in conditions {
                for kind in ["JOIN", "LEFT JOIN"] {
                    for filter in ["", " WHERE a.id < 12", " WHERE b.id IS NULL OR b.v != 3"] {
                        let sql = |on: &str| {
                            format!("SELECT a.id, b.id, b.v FROM a {kind} b ON {on}{filter}")
                        };
                        let mut planned = query(&mut db, &sql(condition));
                        let mut looped = query(&mut db, &sql(&format!("({condition}) OR FALSE")));
                        planned.sort_by(|x, y| x[0].sort_cmp(&y[0]).then(x[1].sort_cmp(&y[1])));
                        looped.sort_by(|x, y| x[0].sort_cmp(&y[0]).then(x[1].sort_cmp(&y[1])));
                        let label = format!("{storage}, indexed {indexed}: {}", sql(condition));
                        assert_eq!(planned, looped, "{label}");
                        assert!(
                            explain(&mut db, &sql(condition)).contains(method),
                            "{label}"
                        );
                    }
                }
            }
            let plan = explain(&mut db, "SELECT * FROM a JOIN b ON b.a_k = a.k");
            let expected = if indexed && storage != "COLUMN" {
                "INDEX JOIN"
            } else {
                "HASH JOIN"
            };
            assert!(
                plan.contains(expected),
                "{storage}, indexed {indexed}: {plan}"
            );
            let plan = explain(&mut db, "SELECT * FROM a JOIN b ON b.id = a.k");
            assert!(
                plan.contains(if storage == "COLUMN" {
                    "HASH JOIN"
                } else {
                    "INDEX JOIN"
                }),
                "{plan}"
            );
        }
    }
}

#[test]
fn every_forced_join_method_returns_what_the_planner_does() {
    use sequoia::plan::query::ForceJoin;
    let methods = [
        (ForceJoin::Hash, "HASH JOIN", "a hash join"),
        (ForceJoin::Index, "INDEX JOIN", "an index join"),
        (ForceJoin::Merge, "MERGE JOIN", "a merge join"),
        (
            ForceJoin::Block,
            "BLOCK NESTED LOOP JOIN",
            "a block nested loop join",
        ),
        (
            ForceJoin::Naive,
            "NAIVE NESTED LOOP JOIN",
            "a naive nested loop join",
        ),
    ];
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE a (id INT PRIMARY KEY, k INT);
         CREATE TABLE b (id INT PRIMARY KEY, k INT, v INT);
         CREATE TABLE c (id INT PRIMARY KEY, w INT) USING LSM;
         CREATE INDEX b_k ON b (k);",
    )
    .unwrap();
    let a: Vec<String> = (0..40i64)
        .map(|i| match i % 9 {
            0 => format!("({i}, NULL)"),
            _ => format!("({i}, {})", i % 13),
        })
        .collect();
    let b: Vec<String> = (0..60i64)
        .map(|i| format!("({i}, {}, {})", i % 17, i % 5))
        .collect();
    let c: Vec<String> = (0..30i64).map(|i| format!("({i}, {})", i % 4)).collect();
    db.execute(&format!(
        "INSERT INTO a VALUES {}; INSERT INTO b VALUES {}; INSERT INTO c VALUES {};",
        a.join(", "),
        b.join(", "),
        c.join(", ")
    ))
    .unwrap();

    // Each sorted, but the last, whose ORDER BY orders every row.
    let queries = [
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON b.k = a.k AND b.v > 2",
        "SELECT a.id, b.id, c.w FROM a JOIN b ON b.k = a.k JOIN c ON c.id = b.id WHERE c.w < 3",
        "SELECT t.id, COUNT(*) FROM (SELECT a.id FROM a JOIN b ON b.k = a.k) t GROUP BY t.id",
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k ORDER BY a.k DESC, a.id, b.id",
    ];
    for (i, sql) in queries.iter().enumerate() {
        let sorted = |mut rows: Vec<Vec<Value>>| {
            if i < queries.len() - 1 {
                rows.sort_by(|x, y| {
                    x.iter()
                        .zip(y)
                        .map(|(x, y)| x.sort_cmp(y))
                        .find(|o| o.is_ne())
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            rows
        };
        db.set_force_join(None);
        let expected = sorted(query(&mut db, sql));
        assert!(!expected.is_empty(), "{sql}");
        for (method, word, _) in methods {
            db.set_force_join(Some(method));
            let plan = explain(&mut db, sql);
            assert!(plan.contains(word), "{method:?}: {sql}\n{plan}");
            assert_eq!(sorted(query(&mut db, sql)), expected, "{method:?}: {sql}");
        }
    }

    // Without an equality, only the nested loop joins can.
    let sql = "SELECT a.id, b.id FROM a JOIN b ON b.v < a.k";
    db.set_force_join(None);
    let expected = query(&mut db, sql).len();
    for (method, word, name) in methods {
        db.set_force_join(Some(method));
        match method {
            ForceJoin::Block | ForceJoin::Naive => {
                assert!(explain(&mut db, sql).contains(word));
                assert_eq!(query(&mut db, sql).len(), expected, "{method:?}");
            }
            _ => {
                let e = db.execute(sql).unwrap_err().to_string();
                assert!(
                    e.contains(&format!("the join of b cannot be {name}")),
                    "{e}"
                );
            }
        }
    }
    // Nor an index join without an index on the right column.
    db.set_force_join(Some(ForceJoin::Index));
    let e = db
        .execute("SELECT a.id FROM a JOIN b ON b.v = a.k")
        .unwrap_err()
        .to_string();
    assert!(e.contains("cannot be an index join"), "{e}");
}

#[test]
fn parallel_hash_join_returns_what_the_serial_one_does() {
    use sequoia::plan::query::ForceJoin;
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE a (id INT PRIMARY KEY, k INT, tag TEXT);
         CREATE TABLE b (id INT PRIMARY KEY, k INT, tag TEXT, v INT);
         CREATE TABLE c (id INT PRIMARY KEY, w INT);",
    )
    .unwrap();
    // Keys repeated, NULL, and on one side only; text keys too.
    let a: Vec<String> = (0..3000i64)
        .map(|i| match i % 11 {
            0 => format!("({i}, NULL, 't{}')", i % 40),
            _ => format!("({i}, {}, 't{}')", i % 700, i % 40),
        })
        .collect();
    let b: Vec<String> = (0..2000i64)
        .map(|i| match i % 13 {
            0 => format!("({i}, NULL, NULL, {})", i % 7),
            _ => format!("({i}, {}, 't{}', {})", i % 900, i % 50, i % 7),
        })
        .collect();
    let c: Vec<String> = (0..500i64).map(|i| format!("({i}, {})", i % 3)).collect();
    db.execute(&format!(
        "INSERT INTO a VALUES {}; INSERT INTO b VALUES {}; INSERT INTO c VALUES {};",
        a.join(", "),
        b.join(", "),
        c.join(", ")
    ))
    .unwrap();
    let queries = [
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON b.k = a.k AND b.v > 2",
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k AND b.tag = a.tag",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON b.tag = a.tag WHERE a.id < 400",
        "SELECT a.id, b.id, c.w FROM a JOIN b ON b.k = a.k JOIN c ON c.id = b.v + a.k",
        "SELECT a.tag, COUNT(*), SUM(b.v) FROM a JOIN b ON b.k = a.k GROUP BY a.tag",
        "SELECT a.id FROM a JOIN b ON b.k = a.k WHERE b.id > 5000",
    ];
    let sorted = |mut rows: Vec<Vec<Value>>| {
        rows.sort_by(|x, y| {
            x.iter()
                .zip(y)
                .map(|(x, y)| x.sort_cmp(y))
                .find(|o| o.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        rows
    };
    db.set_force_join(Some(ForceJoin::Hash));
    for sql in queries {
        db.set_join_workers(1);
        db.set_hash_memory(sequoia::executor::joins::HASH_MEMORY);
        let expected = sorted(query(&mut db, sql));
        // Plenty of memory; and so little that partitions go to files, and
        // the workers' tables split again.
        for memory in [sequoia::executor::joins::HASH_MEMORY, 4096] {
            for workers in [2, 4, 16] {
                db.set_join_workers(workers);
                db.set_hash_memory(memory);
                let plan = explain(&mut db, sql);
                assert!(
                    plan.contains("PARALLEL HASH JOIN")
                        && plan.contains(&format!("({workers} WORKERS)")),
                    "{sql}\n{plan}"
                );
                let found = sorted(query(&mut db, sql));
                assert!(
                    found == expected,
                    "{sql}, {workers} workers in {memory} bytes"
                );
            }
        }
    }
    db.set_hash_memory(sequoia::executor::joins::HASH_MEMORY);
    db.set_force_join(None);
    db.set_join_workers(4);

    // A LIMIT stops reading early, and the workers stop with it.
    let rows = query(&mut db, "SELECT a.id FROM a JOIN b ON b.k = a.k LIMIT 7");
    assert_eq!(rows.len(), 7);
    // An error in a worker comes back as the query's.
    let e = db
        .execute("SELECT a.id FROM a JOIN b ON b.k = a.k AND b.v / (a.id - a.id) > 0")
        .unwrap_err()
        .to_string();
    assert!(e.contains("division by zero"), "{e}");
    // And so does one in a partitioner, working out a key.
    let e = db
        .execute("SELECT a.id FROM a JOIN b ON b.k = a.k / (a.id - a.id)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("division by zero"), "{e}");
    // A hash join after a merge join keeps its order, so it runs alone.
    let sql =
        "SELECT a.id, b.id, c.w FROM a JOIN b ON b.k = a.k JOIN c ON c.id = b.id ORDER BY a.k";
    let plan = explain(&mut db, sql);
    assert!(
        plan.contains("MERGE JOIN") && !plan.contains("PARALLEL"),
        "{plan}"
    );
    // With one worker, it is the hash join it always was.
    db.set_join_workers(1);
    let plan = explain(&mut db, "SELECT a.id FROM a JOIN b ON b.k = a.k");
    assert!(
        plan.contains("HASH JOIN") && !plan.contains("PARALLEL"),
        "{plan}"
    );
}

#[test]
fn gathered_hash_joins_return_what_one_thread_does() {
    use sequoia::plan::query::ForceJoin;
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE a (id INT PRIMARY KEY, k INT, tag TEXT);
         CREATE TABLE b (id INT PRIMARY KEY, k INT, tag TEXT, v INT);
         CREATE TABLE c (id INT PRIMARY KEY, w INT);",
    )
    .unwrap();
    // Keys repeated, NULL, and on one side only; text keys too.
    let a: Vec<String> = (0..4000i64)
        .map(|i| match i % 11 {
            0 => format!("({i}, NULL, 't{}')", i % 40),
            _ => format!("({i}, {}, 't{}')", i % 700, i % 40),
        })
        .collect();
    let b: Vec<String> = (0..2500i64)
        .map(|i| match i % 13 {
            0 => format!("({i}, NULL, NULL, {})", i % 7),
            _ => format!("({i}, {}, 't{}', {})", i % 900, i % 50, i % 7),
        })
        .collect();
    let c: Vec<String> = (0..500i64).map(|i| format!("({i}, {})", i % 3)).collect();
    db.execute(&format!(
        "INSERT INTO a VALUES {}; INSERT INTO b VALUES {}; INSERT INTO c VALUES {};",
        a.join(", "),
        b.join(", "),
        c.join(", ")
    ))
    .unwrap();
    let queries = [
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k",
        "SELECT a.id, b.v * 2 FROM a JOIN b ON b.k = a.k AND b.v > a.id / 1000 WHERE a.tag < 't20'",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON b.k = a.k AND b.v > 2",
        "SELECT a.id, b.v FROM a LEFT JOIN b ON b.k = a.k WHERE b.v IS NULL OR b.v < 3",
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k AND b.tag = a.tag",
        "SELECT a.id, b.id FROM a JOIN b ON b.tag = a.tag WHERE a.id < 300",
        "SELECT a.tag, COUNT(*), SUM(b.v) FROM a JOIN b ON b.k = a.k GROUP BY a.tag",
        "SELECT a.id, b.id, c.w FROM a JOIN b ON b.k = a.k JOIN c ON c.id = b.v + a.k",
        "SELECT a.id FROM a JOIN b ON b.k = a.k WHERE b.id > 5000",
    ];
    let sorted = |mut rows: Vec<Vec<Value>>| {
        rows.sort_by(|x, y| {
            x.iter()
                .zip(y)
                .map(|(x, y)| x.sort_cmp(y))
                .find(|o| o.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        rows
    };
    db.set_force_join(Some(ForceJoin::Hash));
    let default = sequoia::executor::joins::HASH_MEMORY;
    for sql in queries {
        db.set_scan_workers(1);
        db.set_hash_memory(default);
        let expected = sorted(query(&mut db, sql));
        // Plenty of memory; and so little that the build writes partitions
        // out, and the left rows of theirs wait to be joined after.
        for memory in [default, 4096] {
            for workers in [2, 4, 16] {
                db.set_scan_workers(workers);
                db.set_hash_memory(memory);
                let plan = explain(&mut db, sql);
                assert!(
                    plan.contains(&format!("GATHER ({workers} WORKERS)"))
                        && plan.contains("HASH JOIN")
                        && !plan.contains("PARALLEL"),
                    "{sql}\n{plan}"
                );
                let found = sorted(query(&mut db, sql));
                assert!(
                    found == expected,
                    "{sql}, {workers} workers in {memory} bytes\n{plan}"
                );
            }
        }
    }
    db.set_hash_memory(default);
    db.set_scan_workers(4);
    // The whole join in the workers, as the slides draw it.
    assert_eq!(
        explain(
            &mut db,
            "SELECT a.id, b.v FROM a JOIN b ON b.k = a.k WHERE b.v > 1"
        ),
        "GATHER (4 WORKERS)
  PROJECT a.id, b.v
    HASH JOIN ON a.k = b.k
      SCAN a
      FILTER b.v > 1
        SCAN b"
    );
    // A LIMIT stops the workers early.
    assert_eq!(
        query(&mut db, "SELECT a.id FROM a JOIN b ON b.k = a.k LIMIT 7").len(),
        7
    );
    // An error in any phase is the query's: building, probing, and joining
    // the partitions that waited.
    for (sql, memory) in [
        (
            "SELECT a.id FROM a JOIN b ON b.k / (b.id - b.id) = a.k",
            default,
        ),
        (
            "SELECT a.id FROM a JOIN b ON b.k = a.k / (a.id - a.id)",
            default,
        ),
        (
            "SELECT a.id FROM a JOIN b ON b.k = a.k AND b.v / (a.id - a.id) > 0",
            default,
        ),
        (
            "SELECT a.id FROM a JOIN b ON b.k = a.k AND b.v / (a.id - a.id) > 0",
            4096,
        ),
    ] {
        db.set_hash_memory(memory);
        let e = db.execute(sql).unwrap_err().to_string();
        assert!(e.contains("division by zero"), "{sql}: {e}");
    }
}

#[test]
fn partial_aggregates_return_what_one_thread_does() {
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE h (id INT PRIMARY KEY, g INT, k INT, r REAL, big INT, tag TEXT);
         CREATE TABLE b (id INT PRIMARY KEY, k INT, v INT);",
    )
    .unwrap();
    // NULLs in every column; REALs that add up exactly, in any order; INTs
    // near the largest, whose sums only an average's wider sum holds.
    let h: Vec<String> = (0..6000i64)
        .map(|i| {
            let null = |v: String| if i % 13 == 0 { "NULL".into() } else { v };
            let big = if i % 2 == 0 {
                i64::MAX - i
            } else {
                i64::MIN + i
            };
            format!(
                "({i}, {}, {}, {}, {}, {})",
                null((i % 7).to_string()),
                null((i % 500).to_string()),
                null(format!("{}.5", i % 40)),
                null(big.to_string()),
                null(format!("'t{}'", i % 30)),
            )
        })
        .collect();
    let b: Vec<String> = (0..1500i64)
        .map(|i| format!("({i}, {}, {})", i % 700, i % 9))
        .collect();
    db.execute(&format!(
        "INSERT INTO h VALUES {}; INSERT INTO b VALUES {};",
        h.join(", "),
        b.join(", ")
    ))
    .unwrap();
    let queries = [
        "SELECT COUNT(*), COUNT(k), SUM(k), AVG(k), MIN(tag), MAX(tag) FROM h",
        "SELECT SUM(r), AVG(r), SUM(k + r), MIN(r), MAX(r) FROM h WHERE g < 5",
        "SELECT AVG(big), COUNT(big), MIN(big), MAX(big) FROM h",
        "SELECT g, COUNT(*), SUM(k), AVG(r), MAX(tag) FROM h GROUP BY g",
        "SELECT g, tag, COUNT(*), MIN(k) FROM h GROUP BY g, tag",
        "SELECT k / 100, COUNT(*) FROM h GROUP BY k / 100 HAVING COUNT(*) > 1000",
        "SELECT id, SUM(k), AVG(big) FROM h GROUP BY id",
        "SELECT COUNT(*), SUM(k) FROM h WHERE k > 100000",
        "SELECT g, COUNT(*) FROM h WHERE k > 100000 GROUP BY g",
        "SELECT h.g, COUNT(*), SUM(b.v), AVG(b.v) FROM h JOIN b ON b.k = h.k GROUP BY h.g",
        "SELECT h.g, COUNT(*), COUNT(b.v), MAX(b.id) FROM h LEFT JOIN b ON b.k = h.k AND b.v > 4 GROUP BY h.g",
        "SELECT COUNT(*) FROM h JOIN b ON b.k = h.k",
    ];
    let sorted = |mut rows: Vec<Vec<Value>>| {
        rows.sort_by(|x, y| {
            x.iter()
                .zip(y)
                .map(|(x, y)| x.sort_cmp(y))
                .find(|o| o.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        rows
    };
    let default = sequoia::executor::joins::HASH_MEMORY;
    for sql in queries {
        db.set_scan_workers(1);
        db.set_hash_memory(default);
        let expected = sorted(query(&mut db, sql));
        // Plenty of memory; and so little that the workers' groups, and the
        // merged ones, are written out and folded again.
        for memory in [default, 4096] {
            for workers in [2, 4, 16] {
                db.set_scan_workers(workers);
                db.set_hash_memory(memory);
                let plan = explain(&mut db, sql);
                assert!(
                    plan.contains("FINALIZE AGGREGATE") && plan.contains("PARTIAL AGGREGATE"),
                    "{sql}\n{plan}"
                );
                let found = sorted(query(&mut db, sql));
                assert!(
                    found == expected,
                    "{sql}, {workers} workers in {memory} bytes\n{plan}"
                );
            }
        }
    }
    db.set_hash_memory(default);
    db.set_scan_workers(4);
    assert_eq!(
        explain(&mut db, "SELECT g, COUNT(*) FROM h WHERE k > 3 GROUP BY g"),
        "PROJECT g, COUNT(*)
  FINALIZE AGGREGATE COUNT(*) BY g
    GATHER (4 WORKERS)
      PARTIAL AGGREGATE COUNT(*) BY g
        FILTER k > 3
          SCAN h"
    );
    // A DISTINCT aggregate is folded in the query's thread, from a gather.
    let plan = explain(&mut db, "SELECT g, COUNT(DISTINCT k) FROM h GROUP BY g");
    assert!(
        !plan.contains("PARTIAL") && plan.contains("GATHER"),
        "{plan}"
    );
    let distinct = "SELECT g, COUNT(DISTINCT k), SUM(k) FROM h GROUP BY g";
    let found = sorted(query(&mut db, distinct));
    db.set_scan_workers(1);
    assert!(found == sorted(query(&mut db, distinct)));
    db.set_scan_workers(4);
    // Errors in the workers are the query's: an argument's, and a sum's.
    let e = db
        .execute("SELECT g, SUM(k / (k - k)) FROM h GROUP BY g")
        .unwrap_err()
        .to_string();
    assert!(e.contains("division by zero"), "{e}");
    let e = db
        .execute("SELECT SUM(big) FROM h WHERE big > 0")
        .unwrap_err()
        .to_string();
    assert!(e.contains("overflow"), "{e}");
}

#[test]
fn gathered_scans_return_what_one_thread_does() {
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE h (id INT PRIMARY KEY, k INT, tag TEXT, body TEXT);
         CREATE TABLE l (id INT PRIMARY KEY, k INT) USING LSM;
         CREATE TABLE c (id INT PRIMARY KEY, k INT) USING COLUMN;
         CREATE INDEX h_k ON h (k);",
    )
    .unwrap();
    // Enough rows for many pages, some with NULLs.
    let h: Vec<String> = (0..6000i64)
        .map(|i| match i % 17 {
            0 => format!("({i}, NULL, NULL, '{}')", "x".repeat(i as usize % 50)),
            _ => format!(
                "({i}, {}, 't{}', '{}')",
                i % 300,
                i % 9,
                "x".repeat(i as usize % 50)
            ),
        })
        .collect();
    let small: Vec<String> = (0..400i64).map(|i| format!("({i}, {})", i % 30)).collect();
    db.execute(&format!(
        "INSERT INTO h VALUES {}; INSERT INTO l VALUES {}; INSERT INTO c VALUES {};",
        h.join(", "),
        small.join(", "),
        small.join(", ")
    ))
    .unwrap();
    // Each, but the ordered ones, compared as sorted rows.
    let queries = [
        ("SELECT * FROM h", false),
        (
            "SELECT id, k * 2, tag FROM h WHERE k > 100 AND tag <> 't3'",
            false,
        ),
        (
            "SELECT id FROM h WHERE k IS NULL OR body LIKE 'xxxx%'",
            false,
        ),
        ("SELECT id, k FROM h WHERE k < 50 ORDER BY k DESC, id", true),
        (
            "SELECT tag, COUNT(*), SUM(k), MAX(body) FROM h GROUP BY tag",
            false,
        ),
        ("SELECT DISTINCT k FROM h WHERE id > 100", false),
        (
            "SELECT a.id, b.id FROM h a JOIN h b ON b.id = a.k WHERE a.id < 2000",
            false,
        ),
        (
            "SELECT a.id, l.id FROM h a LEFT JOIN l ON l.k = a.k WHERE a.id < 500",
            false,
        ),
        (
            "SELECT a.id, c.id FROM h a JOIN c ON c.k < a.k WHERE a.id < 40",
            false,
        ),
        (
            "SELECT t.k, COUNT(*) FROM (SELECT k FROM h WHERE id >= 0) t GROUP BY t.k",
            false,
        ),
        (
            "SELECT id FROM h WHERE id < 10 UNION SELECT k FROM h WHERE k < 20",
            false,
        ),
        ("SELECT id FROM h WHERE k > (SELECT AVG(k) FROM h)", false),
        (
            "SELECT id, (SELECT COUNT(*) FROM l WHERE l.k = h.k) FROM h WHERE id < 300",
            false,
        ),
        ("SELECT id FROM h WHERE k = 7", false),
    ];
    let sorted = |mut rows: Vec<Vec<Value>>| {
        rows.sort_by(|x, y| {
            x.iter()
                .zip(y)
                .map(|(x, y)| x.sort_cmp(y))
                .find(|o| o.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        rows
    };
    for (sql, ordered) in queries {
        db.set_scan_workers(1);
        let expected = query(&mut db, sql);
        for workers in [2, 4, 16] {
            db.set_scan_workers(workers);
            // A tiny block, so that the block join reads its gathered right side again and again.
            db.set_join_memory(64);
            let found = query(&mut db, sql);
            db.set_join_memory(sequoia::executor::joins::JOIN_MEMORY);
            let label = format!("{sql}, {workers} workers\n{}", explain(&mut db, sql));
            match ordered {
                true => assert!(found == expected, "{label}"),
                false => assert!(sorted(found) == sorted(expected.clone()), "{label}"),
            }
        }
    }

    db.set_scan_workers(4);
    let plan = |db: &mut Database, sql| explain(db, sql);
    // The scan, its filters and its projection, all in the workers.
    assert_eq!(
        plan(&mut db, "SELECT id, k + 1 FROM h WHERE tag <> 't3'"),
        "GATHER (4 WORKERS)\n  PROJECT id, k + 1\n    FILTER tag != 't3'\n      SCAN h"
    );
    // A filter with a subquery stays in the query's thread; so does an
    // index's scan, and the scans of LSM and column tables.
    let p = plan(&mut db, "SELECT id FROM h WHERE k > (SELECT AVG(k) FROM h)");
    assert!(
        p.contains("FILTER k > (SELECT") && p.contains("GATHER"),
        "{p}"
    );
    for sql in [
        "SELECT id FROM h WHERE k = 7",
        "SELECT * FROM l",
        "SELECT * FROM c",
    ] {
        let p = plan(&mut db, sql);
        assert!(!p.contains("GATHER"), "{p}");
    }
    // A LIMIT stops the workers early.
    assert_eq!(query(&mut db, "SELECT id FROM h LIMIT 3").len(), 3);
    // An error in a worker is the query's.
    let e = db
        .execute("SELECT id FROM h WHERE id / (k - k) > 1")
        .unwrap_err()
        .to_string();
    assert!(e.contains("division by zero"), "{e}");
    // Writes see what gathered reads saw.
    db.execute("UPDATE h SET k = k + 1 WHERE id IN (SELECT id FROM h WHERE k = 5)")
        .unwrap();
    db.set_scan_workers(1);
    let one = sorted(query(&mut db, "SELECT id, k FROM h WHERE k = 6"));
    db.set_scan_workers(8);
    assert!(sorted(query(&mut db, "SELECT id, k FROM h WHERE k = 6")) == one);
}

#[test]
fn index_join_is_picked_for_few_left_rows_by_row_counts() {
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE l (id INT PRIMARY KEY, k INT);
         CREATE TABLE r (id INT PRIMARY KEY, k INT, v INT);
         CREATE INDEX r_k ON r (k);
         CREATE TABLE p (id INT PRIMARY KEY, v INT) USING LSM;",
    )
    .unwrap();
    let rows = |count: i64, row: &dyn Fn(i64) -> String| {
        (0..count).map(row).collect::<Vec<_>>().join(", ")
    };
    db.execute(&format!(
        "INSERT INTO l VALUES {}",
        rows(10, &|i| format!("({i}, {i})"))
    ))
    .unwrap();
    db.execute(&format!(
        "INSERT INTO r VALUES {}",
        rows(1000, &|i| format!("({i}, {}, {})", i % 100, i % 7))
    ))
    .unwrap();
    db.execute(&format!(
        "INSERT INTO p VALUES {}",
        rows(100, &|i| format!("({i}, {i})"))
    ))
    .unwrap();

    // Ten lookups cost less than reading a thousand rows into a hash table.
    let sql = "SELECT * FROM l JOIN r ON r.k = l.k AND r.v > 5";
    assert_eq!(
        explain(&mut db, sql),
        "INDEX JOIN
  SCAN l
  FILTER r.v > 5
    LOOKUP r USING INDEX r_k (r.k = l.k)"
    );
    let method = |db: &mut Database, sql: &str| {
        let plan = explain(db, sql);
        ["INDEX JOIN", "HASH JOIN"]
            .into_iter()
            .find(|m| plan.contains(m))
            .unwrap_or_else(|| panic!("{plan}"))
    };
    // Thousands of lookups do not: the counts follow inserts and deletes.
    let more = |from: i64| {
        format!(
            "INSERT INTO l VALUES {}",
            rows(2000, &|i| format!("({}, {i})", from + i))
        )
    };
    db.execute(&more(100)).unwrap();
    assert_eq!(method(&mut db, sql), "HASH JOIN");
    db.execute("DELETE FROM l WHERE id >= 100").unwrap();
    assert_eq!(method(&mut db, sql), "INDEX JOIN");
    // A rollback forgets the count, and the next plan counts again.
    db.execute(&format!("BEGIN; {};", more(100))).unwrap();
    assert_eq!(method(&mut db, sql), "HASH JOIN");
    db.execute("ROLLBACK").unwrap();
    assert_eq!(method(&mut db, sql), "INDEX JOIN");
    // A filter on the left makes for fewer lookups: a third of the rows, as guessed.
    db.execute(&more(100)).unwrap();
    assert_eq!(
        method(&mut db, &format!("{sql} WHERE l.id > 1500")),
        "HASH JOIN"
    );
    assert_eq!(
        method(
            &mut db,
            &format!("{sql} WHERE l.id > 1500 AND l.k < 9 AND l.k > 1")
        ),
        "INDEX JOIN"
    );
    // An LSM primary key lookup is cheaper: worth it for as many left rows as right ones.
    db.execute("DELETE FROM l WHERE id >= 100").unwrap();
    assert_eq!(
        method(&mut db, "SELECT * FROM l JOIN p ON p.id = l.k"),
        "INDEX JOIN"
    );
    db.execute(&format!(
        "INSERT INTO l VALUES {}",
        rows(200, &|i| format!("({}, {i})", 100 + i))
    ))
    .unwrap();
    assert_eq!(
        method(&mut db, "SELECT * FROM l JOIN p ON p.id = l.k"),
        "HASH JOIN"
    );
}

#[test]
fn merge_join_stands_in_for_an_order_by_on_its_keys() {
    let mut db = setup_shop();
    let sql = "SELECT u.name, o.total FROM users u LEFT JOIN orders o ON o.user_id = u.id ORDER BY u.id DESC LIMIT 4";
    assert_eq!(
        explain(&mut db, sql),
        "PROJECT u.name, o.total
  LIMIT 4
    LEFT MERGE JOIN ON u.id = o.user_id (SORTED BY u.id DESC)
      SCAN users u
      SCAN orders o"
    );
    let names: Vec<Value> = query(&mut db, sql)
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    assert_eq!(names, [text("cy"), text("bo"), text("ann"), text("ann")]);

    // Not the join's keys, or a right key of a LEFT JOIN: sorted after the join.
    for sql in [
        "SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id ORDER BY u.name",
        "SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id ORDER BY o.total, u.id",
        "SELECT u.name FROM users u LEFT JOIN orders o ON o.user_id = u.id ORDER BY o.user_id",
        "SELECT u.city FROM users u JOIN orders o ON o.user_id = u.id GROUP BY u.city ORDER BY u.city",
    ] {
        let plan = explain(&mut db, sql);
        assert!(
            plan.contains("SORT BY") && !plan.contains("MERGE"),
            "{sql}\n{plan}"
        );
    }
}

#[test]
fn merge_join_agrees_with_a_hash_join_and_a_sort() {
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE a (id INT PRIMARY KEY, k INT, g INT);
         CREATE TABLE b (id INT PRIMARY KEY, k INT, t TEXT, h INT);
         CREATE TABLE c (id INT PRIMARY KEY, v INT);",
    )
    .unwrap();
    let a: Vec<String> = (0..60i64)
        .map(|i| {
            format!(
                "({i}, {}, {})",
                if i % 7 == 0 {
                    "NULL".into()
                } else {
                    (i % 9).to_string()
                },
                i % 4
            )
        })
        .collect();
    let b: Vec<String> = (0..50i64)
        .map(|i| {
            let k = if i % 6 == 0 {
                "NULL".into()
            } else {
                (i % 11).to_string()
            };
            format!("({i}, {k}, '{}', {})", i % 5, i % 4)
        })
        .collect();
    let c: Vec<String> = (0..40i64).map(|i| format!("({i}, {})", i % 3)).collect();
    db.execute(&format!("INSERT INTO a VALUES {}", a.join(", ")))
        .unwrap();
    db.execute(&format!("INSERT INTO b VALUES {}", b.join(", ")))
        .unwrap();
    db.execute(&format!("INSERT INTO c VALUES {}", c.join(", ")))
        .unwrap();

    // (FROM, ORDER BY, other columns, whether a merge join does the sort)
    let cases = [
        ("a JOIN b ON b.k = a.k", "a.k", "a.id, b.id", true),
        ("a LEFT JOIN b ON b.k = a.k", "a.k DESC", "a.id, b.id", true),
        ("a JOIN b ON b.k = a.k", "b.k DESC", "a.id, b.id", true),
        ("a LEFT JOIN b ON b.k = a.k", "b.k", "a.id, b.id", false),
        (
            "a JOIN b ON b.k = a.k AND b.id > a.g",
            "a.k, a.g DESC",
            "a.id, b.id",
            true,
        ),
        (
            "a LEFT JOIN b ON b.k = a.k AND b.h = a.g",
            "a.k DESC",
            "a.id, b.id",
            true,
        ),
        (
            "a JOIN b ON b.k = a.k AND b.h = a.g",
            "a.k, a.id",
            "b.id",
            false,
        ),
        (
            "a JOIN b ON b.k = a.k WHERE a.id < 30 AND b.id > 5",
            "a.k",
            "a.id, b.id",
            true,
        ),
        (
            "a JOIN b ON b.k = a.k JOIN c ON c.id = b.id",
            "a.k DESC",
            "a.id, b.id, c.v",
            true,
        ),
        (
            "a JOIN b ON b.k = a.k LEFT JOIN c ON c.v = a.g",
            "a.k",
            "a.id, b.id, c.id",
            true,
        ),
        (
            "a JOIN b ON b.k = a.k JOIN c ON c.v < a.g",
            "a.k",
            "a.id, b.id, c.id",
            false,
        ),
        (
            "a JOIN c ON c.id = a.k JOIN b ON b.k = c.v",
            "c.v DESC, a.id",
            "b.id",
            true,
        ),
        // Keys of different types are never equal, as in a hash join.
        ("a JOIN b ON b.t = a.k", "a.k", "a.id, b.id", true),
    ];
    let hidden = |order: &str| {
        order
            .split(", ")
            .map(|key| match key.strip_suffix(" DESC") {
                Some(expr) => format!("{expr} + 0 DESC"),
                None => format!("{key} + 0"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    // With little memory, sorts spill, and hash joins after a merge join
    // spill too, and must keep its order.
    for memory in [usize::MAX, 300] {
        db.set_sort_memory(memory);
        db.set_hash_memory(memory);
        for (from, order, others, merged) in cases {
            let keys: Vec<&str> = order
                .split(", ")
                .map(|k| k.trim_end_matches(" DESC"))
                .collect();
            let sql = |order: &str| {
                format!(
                    "SELECT {}, {others} FROM {from} ORDER BY {order}",
                    keys.join(", ")
                )
            };
            let plan = explain(&mut db, &sql(order));
            assert_eq!(
                plan.contains("MERGE JOIN") && !plan.contains("SORT BY"),
                merged,
                "{}\n{plan}",
                sql(order)
            );
            let mut rows = query(&mut db, &sql(order));
            let mut expected = query(&mut db, &sql(&hidden(order)));
            assert!(explain(&mut db, &sql(&hidden(order))).contains("SORT BY"));
            // The same keys in the same order; rows with equal keys may come in any order.
            let prefix = |rows: &[Vec<Value>]| {
                rows.iter()
                    .map(|r| r[..keys.len()].to_vec())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                prefix(&rows),
                prefix(&expected),
                "{}, sort memory {memory}",
                sql(order)
            );
            let by_all = |x: &Vec<Value>, y: &Vec<Value>| {
                x.iter()
                    .zip(y)
                    .map(|(a, b)| a.sort_cmp(b))
                    .find(|o| o.is_ne())
                    .unwrap_or(std::cmp::Ordering::Equal)
            };
            rows.sort_by(by_all);
            expected.sort_by(by_all);
            assert_eq!(rows, expected, "{}, sort memory {memory}", sql(order));
        }
    }
}

#[test]
fn block_nested_loop_join_agrees_for_every_block_size() {
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE a (id INT PRIMARY KEY, k INT);
         CREATE TABLE b (id INT PRIMARY KEY, v INT);
         CREATE TABLE empty (id INT PRIMARY KEY, v INT);",
    )
    .unwrap();
    let a: Vec<String> = (0..50i64)
        .map(|i| {
            format!(
                "({i}, {})",
                if i % 7 == 0 {
                    "NULL".into()
                } else {
                    (i % 10).to_string()
                }
            )
        })
        .collect();
    let b: Vec<String> = (0..30i64).map(|i| format!("({i}, {})", i % 8)).collect();
    db.execute(&format!("INSERT INTO a VALUES {}", a.join(", ")))
        .unwrap();
    db.execute(&format!("INSERT INTO b VALUES {}", b.join(", ")))
        .unwrap();

    let queries = [
        "SELECT a.id, b.id FROM a JOIN b ON b.v < a.k",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON b.v > a.k",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON b.v < a.k WHERE a.id < 20 AND b.id IS NULL",
        // The right side is a sort, which runs again for each block unless its rows are kept.
        "SELECT a.id, t.id FROM a JOIN (SELECT id, v FROM b ORDER BY v DESC LIMIT 5) t ON t.v < a.k",
        "SELECT a.id, e.id FROM a LEFT JOIN empty e ON e.v < a.k",
        "SELECT e.id, a.id FROM empty e LEFT JOIN a ON a.k < e.v",
    ];
    for sql in queries {
        assert!(
            explain(&mut db, sql).contains("BLOCK NESTED LOOP JOIN"),
            "{sql}"
        );
        db.set_join_memory(usize::MAX);
        let mut whole = query(&mut db, sql);
        whole.sort_by(|x, y| x[0].sort_cmp(&y[0]).then(x[1].sort_cmp(&y[1])));
        // A row of `a` or `b` takes about 96 bytes: blocks of 1, 3, 11 and
        // 32 rows. With 3000 bytes the 30 rows of `b` are read once and kept
        // for the second block; with less they are read again for each one.
        for memory in [0, 200, 1000, 3000] {
            db.set_join_memory(memory);
            let mut rows = query(&mut db, sql);
            rows.sort_by(|x, y| x[0].sort_cmp(&y[0]).then(x[1].sort_cmp(&y[1])));
            assert_eq!(rows, whole, "{sql}, {memory} bytes");
        }
    }
}

#[test]
fn hash_joins_and_aggregates_spill_partitions_past_their_budget() {
    let mut db = in_memory();
    db.execute(
        "CREATE TABLE a (id INT PRIMARY KEY, k INT, s TEXT);
         CREATE TABLE b (id INT PRIMARY KEY, k REAL, one INT, t TEXT);",
    )
    .unwrap();
    let a: Vec<String> = (0..3000i64)
        .map(|i| {
            let k = if i % 13 == 0 {
                "NULL".to_string()
            } else {
                (i * 7 % 1100).to_string()
            };
            format!("({i}, {k}, 'a{}')", i % 37)
        })
        .collect();
    // Keys as REALs, which match the INTs they equal; one key of many rows.
    let b: Vec<String> = (0..2500i64)
        .map(|i| {
            let k = match i % 10 {
                0 => "NULL".to_string(),
                1 => "500.0".to_string(),
                _ => format!("{}.0", i * 3 % 1300),
            };
            format!("({i}, {k}, 1, '{}')", "x".repeat(i as usize % 40))
        })
        .collect();
    db.execute(&format!("INSERT INTO a VALUES {}", a.join(", ")))
        .unwrap();
    db.execute(&format!("INSERT INTO b VALUES {}", b.join(", ")))
        .unwrap();

    let queries = [
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k",
        "SELECT a.id, b.id, b.t FROM a LEFT JOIN b ON b.k = a.k AND b.id > a.id",
        "SELECT a.s, COUNT(*), SUM(b.id) FROM a JOIN b ON b.k = a.k GROUP BY a.s",
        "SELECT k, COUNT(*), COUNT(DISTINCT t), MIN(t), AVG(id) FROM b GROUP BY k",
        "SELECT a.k, a.s, MAX(a.id), COUNT(DISTINCT a.id - a.id / 3 * 3) FROM a GROUP BY a.k, a.s",
        "SELECT one, COUNT(*) FROM b GROUP BY one",
        "SELECT DISTINCT a.k FROM a JOIN b ON b.one = 1 AND b.k = a.k",
    ];
    let sorted = |mut rows: Vec<Vec<Value>>| {
        rows.sort_by(|x, y| {
            x.iter()
                .zip(y)
                .map(|(a, b)| a.sort_cmp(b))
                .find(|o| o.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        rows
    };
    for sql in queries {
        db.set_hash_memory(usize::MAX);
        let whole = sorted(query(&mut db, sql));
        assert!(!whole.is_empty(), "{sql}");
        // From partitions of a few rows, split again and again, to more than fit.
        for memory in [0, 2000, 50_000, 300_000] {
            db.set_hash_memory(memory);
            assert_eq!(sorted(query(&mut db, sql)), whole, "{sql}, {memory} bytes");
        }
    }
    db.set_hash_memory(0);
    // Every row of one key: a table that cannot be split is held whole.
    assert_eq!(
        query(
            &mut db,
            "SELECT COUNT(*) FROM b x JOIN b y ON y.one = x.one WHERE x.id < 300"
        ),
        [[int(300 * 2500)]]
    );
    // An error in a partition read back ends the query.
    let error = db
        .execute("SELECT a.id / (b.id - 2402) FROM a JOIN b ON b.k = a.k")
        .unwrap_err();
    assert!(error.0.contains("division by zero"), "{error:?}");
}

#[test]
fn join_names_and_conditions_are_checked() {
    let mut db = setup_shop();
    let errors = [
        (
            "SELECT id FROM users u JOIN orders o ON o.user_id = u.id",
            "column reference id is ambiguous",
        ),
        (
            "SELECT x.id FROM users u JOIN orders o ON o.user_id = u.id",
            "unknown column: x.id",
        ),
        ("SELECT users.id FROM users u", "unknown column: users.id"),
        (
            "SELECT * FROM users u JOIN orders u ON u.id = 1",
            "appears more than once",
        ),
        (
            "SELECT * FROM users JOIN users ON users.id = 1",
            "appears more than once",
        ),
        (
            "SELECT * FROM users u JOIN orders o ON c.name = u.city JOIN cities c ON c.name = u.city",
            "unknown",
        ),
        (
            "SELECT * FROM users u JOIN orders o ON COUNT(*) > 1",
            "not allowed in JOIN ... ON",
        ),
        ("SELECT * FROM users u JOIN orders o", "expected ON"),
        (
            "SELECT * FROM missing m JOIN users u ON u.id = m.id",
            "unknown table: missing",
        ),
        (
            "SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id GROUP BY u.city",
            "u.name must appear",
        ),
    ];
    for (sql, expected) in errors {
        let e = db.execute(sql).expect_err(sql);
        assert!(e.0.contains(expected), "{sql}: {e}");
    }
    db.execute("CREATE TABLE cities (name TEXT PRIMARY KEY)")
        .unwrap();
    let e = db.execute(
        "SELECT * FROM users u JOIN orders o ON c.name = u.city JOIN cities c ON c.name = u.city",
    );
    assert!(e.unwrap_err().0.contains("joined after it"));
    // A single table may still be referred to by its name.
    assert_eq!(
        query(&mut db, "SELECT users.name FROM users WHERE users.id = 2"),
        vec![vec![text("bo")]]
    );
}

fn ints(rows: Vec<Vec<Value>>) -> Vec<Value> {
    rows.into_iter().map(|mut r| r.remove(0)).collect()
}

#[test]
fn subqueries_as_values_with_exists_and_in() {
    let mut db = setup_shop();
    // Uncorrelated: the average total is 25 / 5 = 5.
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT id FROM orders WHERE total > (SELECT AVG(total) FROM orders) ORDER BY id"
        )),
        [int(11), int(13)]
    );
    // Correlated, in the select list; inner columns win over outer ones of the same name.
    assert_eq!(
        query(
            &mut db,
            "SELECT name, (SELECT SUM(total) FROM orders WHERE user_id = users.id) FROM users ORDER BY id"
        ),
        [
            vec![text("ann"), int(12)],
            vec![text("bo"), int(3)],
            vec![text("cy"), Value::Null]
        ]
    );
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) FROM users u ORDER BY u.id"
        )),
        [int(2), int(1), int(0)]
    );
    let names = |db: &mut Database, sql: &str| ints(query(db, sql));
    assert_eq!(
        names(
            &mut db,
            "SELECT name FROM users u WHERE EXISTS (SELECT * FROM orders o WHERE o.user_id = u.id) ORDER BY name"
        ),
        [text("ann"), text("bo")]
    );
    assert_eq!(
        names(
            &mut db,
            "SELECT name FROM users u WHERE NOT EXISTS (SELECT 1 FROM orders WHERE user_id = u.id)"
        ),
        [text("cy")]
    );
    assert_eq!(
        names(
            &mut db,
            "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders) ORDER BY id"
        ),
        [text("ann"), text("bo")]
    );
    // NOT IN is never true when the subquery returns a NULL, nor for a NULL value.
    assert!(
        query(
            &mut db,
            "SELECT name FROM users WHERE id NOT IN (SELECT user_id FROM orders)"
        )
        .is_empty()
    );
    assert_eq!(
        names(
            &mut db,
            "SELECT name FROM users WHERE id NOT IN (SELECT user_id FROM orders WHERE user_id IS NOT NULL)"
        ),
        [text("cy")]
    );
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT id FROM orders WHERE user_id NOT IN (SELECT id FROM users)"
        )),
        [int(14)]
    );
    // Over no rows: IN is false, even for NULL, and a value is NULL.
    assert_eq!(
        query(
            &mut db,
            "SELECT COUNT(*) FROM users WHERE NULL NOT IN (SELECT id FROM users WHERE id > 100)"
        ),
        [vec![int(3)]]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT (SELECT id FROM users WHERE id > 100) FROM users WHERE id = 1"
        ),
        [vec![Value::Null]]
    );
    // Values of another type match nothing.
    assert_eq!(
        query(
            &mut db,
            "SELECT COUNT(*) FROM users WHERE name IN (SELECT id FROM users)"
        ),
        [vec![int(0)]]
    );

    // IN with a list.
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT id FROM orders WHERE user_id IN (1, 99) ORDER BY id"
        )),
        [int(10), int(11), int(14)]
    );
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT id FROM orders WHERE total NOT IN (3, 5 + 4) ORDER BY id"
        )),
        [int(10), int(11), int(14)]
    );
    assert!(
        query(
            &mut db,
            "SELECT id FROM orders WHERE total NOT IN (3, NULL)"
        )
        .is_empty()
    );
    assert!(
        db.execute("SELECT id FROM orders WHERE total IN ('x')")
            .is_err(),
        "compared like ="
    );
}

#[test]
fn nested_correlated_and_grouped_subqueries() {
    let mut db = setup_shop();
    // The same table inside and out.
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT id FROM orders WHERE total = (SELECT MAX(total) FROM orders)"
        )),
        [int(13)]
    );
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT o.id FROM orders o WHERE o.total = (SELECT MAX(total) FROM orders i WHERE i.user_id = o.user_id)
             ORDER BY o.id"
        )),
        [int(11), int(12), int(14)]
    );
    // A subquery two levels in may refer to either query around it.
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT name FROM users u WHERE EXISTS (SELECT * FROM orders o WHERE o.user_id = u.id
               AND o.total > (SELECT MIN(total) FROM orders p WHERE p.user_id = u.id))"
        )),
        [text("ann")]
    );
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT name FROM users u WHERE EXISTS (SELECT * FROM orders o WHERE o.user_id = u.id
               AND EXISTS (SELECT * FROM orders p WHERE p.user_id = o.user_id AND p.total < o.total))"
        )),
        [text("ann")]
    );
    // In a join's WHERE, reading both tables: checked after the join.
    let sql = "SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id
               WHERE o.total = (SELECT MAX(total) FROM orders x WHERE x.user_id = u.id) ORDER BY o.id";
    assert_eq!(
        query(&mut db, sql),
        [vec![text("ann"), int(11)], vec![text("bo"), int(12)]]
    );
    assert!(explain(&mut db, sql).contains(
        "FILTER o.total = (SELECT MAX(total) FROM orders x WHERE x.user_id = u.id)\n      HASH JOIN"
    ));
    let sql = "SELECT u.name, o.id FROM users u LEFT JOIN orders o ON o.user_id = u.id
               WHERE NOT EXISTS (SELECT * FROM orders x WHERE x.user_id = u.id AND x.total > 6) ORDER BY u.id";
    assert_eq!(
        query(&mut db, sql),
        [vec![text("bo"), int(12)], vec![text("cy"), Value::Null]]
    );

    // Grouped: outer references must be grouped columns.
    assert_eq!(
        query(
            &mut db,
            "SELECT city, COUNT(*) FROM users GROUP BY city HAVING COUNT(*) > (SELECT COUNT(*) FROM users WHERE city = 'lima')"
        ),
        [vec![text("oslo"), int(2)]]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT u.city, (SELECT COUNT(*) FROM users v WHERE v.city = u.city) FROM users u GROUP BY u.city
             ORDER BY u.city"
        ),
        [vec![text("lima"), int(1)], vec![text("oslo"), int(2)]]
    );
    // Aggregates inside a subquery group its rows, not the outer query's.
    assert_eq!(
        query(
            &mut db,
            "SELECT name FROM users WHERE id = (SELECT MIN(user_id) FROM orders)"
        ),
        [vec![text("ann")]]
    );
    assert_eq!(
        explain(
            &mut db,
            "SELECT name FROM users u WHERE EXISTS (SELECT * FROM orders o WHERE o.user_id = u.id)"
        ),
        "PROJECT name\n  FILTER EXISTS (SELECT * FROM orders o WHERE o.user_id = u.id)\n    SCAN users u"
    );
}

#[test]
fn subqueries_in_insert_update_and_delete() {
    let mut db = setup_shop();
    db.execute(
        "UPDATE users SET city = (SELECT city FROM users WHERE id = 2) WHERE id IN (SELECT user_id FROM orders WHERE total > 6)",
    )
    .unwrap();
    assert_eq!(
        ints(query(&mut db, "SELECT city FROM users ORDER BY id")),
        [text("lima"), text("lima"), text("oslo")]
    );
    // Correlated, by a column only the outer table has.
    db.execute(
        "CREATE TABLE stats (uid INT, n INT); INSERT INTO stats VALUES (1, 0), (2, 0), (3, 0)",
    )
    .unwrap();
    db.execute("UPDATE stats SET n = (SELECT COUNT(*) FROM orders WHERE user_id = uid)")
        .unwrap();
    assert_eq!(
        ints(query(&mut db, "SELECT n FROM stats ORDER BY uid")),
        [int(2), int(1), int(0)]
    );

    db.execute("DELETE FROM orders WHERE user_id NOT IN (SELECT id FROM users)")
        .unwrap();
    assert_eq!(
        ints(query(&mut db, "SELECT id FROM orders ORDER BY id")),
        [int(10), int(11), int(12), int(13)]
    );
    // The subquery sees the table as it was before the statement: the average is 6.
    db.execute("DELETE FROM orders WHERE total < (SELECT AVG(total) FROM orders)")
        .unwrap();
    assert_eq!(
        ints(query(&mut db, "SELECT id FROM orders ORDER BY id")),
        [int(11), int(13)]
    );
    db.execute("INSERT INTO orders VALUES ((SELECT MAX(id) FROM orders) + 1, 3, (SELECT COUNT(*) FROM users))").unwrap();
    assert_eq!(
        query(&mut db, "SELECT * FROM orders WHERE user_id = 3"),
        [vec![int(14), int(3), int(3)]]
    );
    assert_eq!(
        explain(
            &mut db,
            "DELETE FROM orders WHERE id IN (SELECT id FROM orders WHERE total > 5)"
        ),
        "FILTER id IN (SELECT id FROM orders WHERE total > 5)\n  SCAN orders"
    );
    // A failing subquery rolls the statement back.
    assert!(
        db.execute("UPDATE orders SET total = (SELECT id FROM users)")
            .is_err()
    );
    assert_eq!(
        ints(query(&mut db, "SELECT total FROM orders ORDER BY id")),
        [int(7), int(9), int(3)]
    );
}

#[test]
fn subquery_mistakes_are_reported() {
    let mut db = setup_shop();
    let errors = [
        (
            "SELECT (SELECT id FROM users) FROM users",
            "more than one row",
        ),
        (
            "SELECT * FROM users WHERE id IN (SELECT id, name FROM users)",
            "only one column",
        ),
        ("SELECT (SELECT * FROM users) FROM users", "only one column"),
        // Planned even where it would never run.
        (
            "SELECT * FROM users WHERE id > 100 AND EXISTS (SELECT nope FROM orders)",
            "unknown column: nope",
        ),
        (
            "SELECT * FROM users WHERE EXISTS (SELECT * FROM missing)",
            "missing",
        ),
        (
            "SELECT * FROM users u JOIN orders o ON o.user_id IN (SELECT id FROM users)",
            "not allowed in JOIN ... ON",
        ),
        (
            "SELECT COUNT(*) FROM users GROUP BY (SELECT 1 FROM orders LIMIT 1)",
            "not allowed in GROUP BY",
        ),
        (
            "SELECT SUM((SELECT 1 FROM orders LIMIT 1)) FROM users",
            "not allowed in aggregate functions",
        ),
        (
            "SELECT city, (SELECT COUNT(*) FROM orders WHERE user_id = users.id) FROM users GROUP BY city",
            "column users.id must appear in GROUP BY",
        ),
        (
            "SELECT * FROM users WHERE id IN (SELECT id FROM orders",
            "expected",
        ),
        (
            "SELECT * FROM users WHERE EXISTS SELECT 1 FROM users",
            "expected",
        ),
    ];
    for (sql, expected) in errors {
        let e = db.execute(sql).expect_err(sql);
        assert!(e.0.contains(expected), "{sql}: {e}");
    }
}

#[test]
fn every_storage_answers_subqueries_alike() {
    let queries = [
        "SELECT name, (SELECT SUM(total) FROM orders o WHERE o.user_id = u.id) FROM users u ORDER BY id",
        "SELECT name FROM users u WHERE EXISTS (SELECT * FROM orders o WHERE o.user_id = u.id) ORDER BY id",
        "SELECT id FROM orders WHERE user_id NOT IN (SELECT id FROM users WHERE city = 'oslo') ORDER BY id",
        "SELECT id FROM orders o WHERE total > (SELECT MIN(total) FROM orders i WHERE i.user_id = o.user_id) ORDER BY id",
    ];
    let mut results = Vec::new();
    for storage in ["HEAP", "LSM", "COLUMN"] {
        let mut db = in_memory();
        db.execute(&format!(
            "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, city TEXT) USING {storage};
             CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, total INT) USING {storage};
             INSERT INTO users VALUES (1, 'ann', 'oslo'), (2, 'bo', 'lima'), (3, 'cy', 'oslo');
             INSERT INTO orders VALUES (10, 1, 5), (11, 1, 7), (12, 2, 3), (13, NULL, 9), (14, 99, 1);"
        ))
        .unwrap();
        if storage != "COLUMN" {
            db.execute("CREATE INDEX orders_user ON orders (user_id)")
                .unwrap();
        }
        results.push(
            queries
                .iter()
                .map(|sql| query(&mut db, sql))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(results[0], results[1], "heap and LSM");
    assert_eq!(results[0], results[2], "heap and column");
}

#[test]
fn transactions_commit_or_roll_back_together() {
    let mut db = setup();
    let ids = |db: &mut Database| ints(query(db, "SELECT id FROM users ORDER BY id"));
    db.execute("BEGIN; INSERT INTO users (id, name) VALUES (4, 'dan'); UPDATE users SET age = 99 WHERE id = 1").unwrap();
    assert!(db.in_transaction());
    // Later statements see the earlier ones.
    assert_eq!(ids(&mut db), [int(1), int(2), int(3), int(4)]);
    assert_eq!(
        query(&mut db, "SELECT age FROM users WHERE id = 1"),
        [vec![int(99)]]
    );
    db.execute("ROLLBACK").unwrap();
    assert!(!db.in_transaction());
    assert_eq!(ids(&mut db), [int(1), int(2), int(3)]);
    assert_eq!(
        query(&mut db, "SELECT age FROM users WHERE id = 1"),
        [vec![int(30)]]
    );

    db.execute("BEGIN TRANSACTION; DELETE FROM users WHERE id = 2; INSERT INTO users (id) VALUES (5); COMMIT").unwrap();
    assert_eq!(ids(&mut db), [int(1), int(3), int(5)]);

    // Tables and indexes of every kind come and go with the transaction.
    db.execute(
        "BEGIN;
         CREATE TABLE l (id INT PRIMARY KEY, v TEXT) USING LSM;
         CREATE TABLE c (id INT, v TEXT) USING COLUMN;
         CREATE INDEX users_name ON users (name);
         DROP TABLE users;",
    )
    .unwrap();
    let rows = (0..1500)
        .map(|i| format!("({i}, '{}')", "x".repeat(i % 40)))
        .collect::<Vec<_>>()
        .join(", ");
    db.execute(&format!(
        "INSERT INTO l VALUES {rows}; INSERT INTO c VALUES {rows}"
    ))
    .unwrap();
    assert_eq!(db.table_names(), ["c", "l"]);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(db.table_names(), ["users"]);
    assert_eq!(ids(&mut db), [int(1), int(3), int(5)]);
    assert!(
        explain(&mut db, "SELECT * FROM users WHERE name = 'x'").contains("SCAN users"),
        "no index"
    );

    db.execute("BEGIN; CREATE TABLE l (id INT PRIMARY KEY, v TEXT) USING LSM")
        .unwrap();
    db.execute(&format!(
        "INSERT INTO l VALUES {rows}; CREATE INDEX users_name ON users (name); COMMIT"
    ))
    .unwrap();
    assert_eq!(query(&mut db, "SELECT COUNT(*) FROM l"), [vec![int(1500)]]);
    assert!(
        explain(&mut db, "SELECT * FROM users WHERE name = 'x'").contains("USING INDEX users_name")
    );
}

#[test]
fn a_failing_statement_in_a_transaction_is_undone_alone() {
    let mut db = setup();
    db.execute("BEGIN; INSERT INTO users (id) VALUES (4)")
        .unwrap();
    // The third row is a duplicate, after two rows went in.
    assert!(
        db.execute("INSERT INTO users (id) VALUES (5), (6), (1), (7)")
            .is_err()
    );
    assert!(db.execute("UPDATE users SET id = 4").is_err());
    assert!(db.in_transaction(), "the transaction goes on");
    db.execute("INSERT INTO users (id) VALUES (8); COMMIT")
        .unwrap();
    assert_eq!(
        ints(query(&mut db, "SELECT id FROM users ORDER BY id")),
        [int(1), int(2), int(3), int(4), int(8)]
    );

    // execute stops at the first error, and the transaction stays open.
    assert!(
        db.execute("BEGIN; INSERT INTO users (id) VALUES (9); SELECT nope FROM users; COMMIT")
            .is_err()
    );
    assert!(db.in_transaction());
    db.execute("ROLLBACK").unwrap();
    assert_eq!(
        query(&mut db, "SELECT COUNT(*) FROM users WHERE id = 9"),
        [vec![int(0)]]
    );
}

#[test]
fn savepoints_roll_back_part_of_a_transaction() {
    let mut db = setup();
    let ids = |db: &mut Database| ints(query(db, "SELECT id FROM users WHERE id > 3 ORDER BY id"));
    db.execute(
        "BEGIN;
         INSERT INTO users (id) VALUES (4);
         SAVEPOINT a;
         INSERT INTO users (id) VALUES (5);
         SAVEPOINT b;
         INSERT INTO users (id) VALUES (6);
         CREATE TABLE t (x INT);",
    )
    .unwrap();
    db.execute("ROLLBACK TO SAVEPOINT a").unwrap();
    assert_eq!(ids(&mut db), [int(4)]);
    assert_eq!(db.table_names(), ["users"]);
    // b was taken after a, so it is gone; a stays.
    assert!(
        db.execute("ROLLBACK TO b")
            .unwrap_err()
            .0
            .contains("no such savepoint: b")
    );
    db.execute("INSERT INTO users (id) VALUES (7); ROLLBACK TRANSACTION TO a")
        .unwrap();
    assert_eq!(ids(&mut db), [int(4)]);
    // The newest savepoint of a name wins; RELEASE forgets it and those after it.
    db.execute("INSERT INTO users (id) VALUES (8); SAVEPOINT a; INSERT INTO users (id) VALUES (9); SAVEPOINT c").unwrap();
    db.execute("ROLLBACK TO a").unwrap();
    assert_eq!(ids(&mut db), [int(4), int(8)]);
    db.execute("RELEASE SAVEPOINT a").unwrap();
    db.execute("ROLLBACK TO a").unwrap();
    assert_eq!(ids(&mut db), [int(4)]);
    db.execute("RELEASE a").unwrap();
    assert!(db.execute("ROLLBACK TO a").is_err());
    db.execute("COMMIT").unwrap();
    assert_eq!(ids(&mut db), [int(4)]);
}

#[test]
fn transaction_statements_are_checked() {
    let mut db = setup();
    let errors = [
        ("COMMIT", "no transaction is in progress"),
        ("ROLLBACK", "no transaction is in progress"),
        ("SAVEPOINT a", "SAVEPOINT can only be used in a transaction"),
        ("RELEASE a", "RELEASE can only be used in a transaction"),
        (
            "ROLLBACK TO a",
            "ROLLBACK TO can only be used in a transaction",
        ),
        ("BEGIN; BEGIN", "already in progress"),
        ("CHECKPOINT", "cannot run inside a transaction"),
        ("RELEASE nope", "no such savepoint: nope"),
        ("EXPLAIN COMMIT", "EXPLAIN supports only"),
    ];
    for (sql, expected) in errors {
        let e = db.execute(sql).expect_err(sql);
        assert!(e.0.contains(expected), "{sql}: {e}");
    }
    assert!(db.in_transaction());
    db.execute("COMMIT; CHECKPOINT").unwrap();
}

#[test]
fn closing_the_database_rolls_back_an_open_transaction() {
    let path = TempDb::new("tx");
    {
        let mut db = open(&path.0).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY); INSERT INTO t VALUES (1)")
            .unwrap();
        db.execute("BEGIN; INSERT INTO t VALUES (2); COMMIT")
            .unwrap();
        db.execute("BEGIN; INSERT INTO t VALUES (3); DELETE FROM t WHERE id = 1")
            .unwrap();
    }
    let mut db = open(&path.0).unwrap();
    assert_eq!(
        ints(query(&mut db, "SELECT id FROM t ORDER BY id")),
        [int(1), int(2)]
    );
    assert!(!db.in_transaction());
}

fn columns(db: &mut Database, sql: &str) -> Vec<String> {
    match db.execute(sql).unwrap().pop() {
        Some(QueryResult::Rows { columns, .. }) => columns,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn subqueries_in_from() {
    let mut db = setup_shop();
    // Joined to a table, by a column named with AS; hash joined, as it has no index.
    let sql = "SELECT u.name, s.spent FROM users u JOIN (SELECT user_id, SUM(total) AS spent FROM orders GROUP BY user_id) s
               ON s.user_id = u.id ORDER BY spent DESC";
    assert_eq!(
        query(&mut db, sql),
        [vec![text("ann"), int(12)], vec![text("bo"), int(3)]]
    );
    assert!(
        explain(&mut db, sql).contains("HASH JOIN ON u.id = s.user_id"),
        "{}",
        explain(&mut db, sql)
    );
    assert!(explain(&mut db, sql).contains("SUBQUERY s\n"));
    // LEFT JOIN pads it with NULLs.
    assert_eq!(
        query(
            &mut db,
            "SELECT u.name, c.n FROM users u LEFT JOIN (SELECT user_id, COUNT(*) AS n FROM orders GROUP BY user_id) c
             ON c.user_id = u.id ORDER BY u.id"
        ),
        [vec![text("ann"), int(2)], vec![text("bo"), int(1)], vec![text("cy"), Value::Null]]
    );
    // Nested, with its own ORDER BY and LIMIT, and grouped over.
    assert_eq!(
        query(
            &mut db,
            "SELECT COUNT(*) FROM (SELECT DISTINCT city FROM (SELECT * FROM users) a) b"
        ),
        [vec![int(2)]]
    );
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT * FROM (SELECT id FROM orders ORDER BY total DESC LIMIT 2) t ORDER BY id"
        )),
        [int(11), int(13)]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT city, MAX(n) FROM (SELECT u.city, (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) AS n
             FROM users u) t GROUP BY city ORDER BY city"
        ),
        [vec![text("lima"), int(1)], vec![text("oslo"), int(2)]]
    );
    // Its columns are named as its select list names them.
    assert_eq!(
        columns(
            &mut db,
            "SELECT * FROM (SELECT id + 1 AS next, name, COUNT(*) FROM users GROUP BY id, name) d"
        ),
        ["next", "name", "count"]
    );
    assert_eq!(
        columns(
            &mut db,
            "SELECT * FROM (SELECT u.id, o.id FROM users u, orders o) d"
        ),
        ["id", "id"]
    );
    // Inside a subquery it may refer to the queries around it.
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT name FROM users u WHERE EXISTS (SELECT * FROM (SELECT * FROM orders o WHERE o.user_id = u.id) x
             WHERE x.total > 6)"
        )),
        [text("ann")]
    );
    db.execute(
        "UPDATE users SET city = 'rich' WHERE id IN
         (SELECT t.user_id FROM (SELECT user_id, SUM(total) AS s FROM orders GROUP BY user_id) t WHERE t.s > 10)",
    )
    .unwrap();
    assert_eq!(
        ints(query(&mut db, "SELECT city FROM users ORDER BY id")),
        [text("rich"), text("lima"), text("oslo")]
    );

    let errors = [
        ("SELECT * FROM (SELECT * FROM users)", "must have an alias"),
        (
            "SELECT * FROM users u, (SELECT * FROM orders WHERE user_id = u.id) o",
            "unknown column: u.id",
        ),
        (
            "SELECT d.id FROM (SELECT u.id, o.id FROM users u, orders o) d",
            "ambiguous",
        ),
        (
            "SELECT d.nope FROM (SELECT id FROM users) d",
            "unknown column: d.nope",
        ),
        (
            "SELECT * FROM (SELECT id FROM users) u, users u",
            "appears more than once",
        ),
    ];
    for (sql, expected) in errors {
        let e = db.execute(sql).expect_err(sql);
        assert!(e.0.contains(expected), "{sql}: {e}");
    }
}

#[test]
fn select_list_names_and_order_by_them() {
    let mut db = setup_shop();
    assert_eq!(
        columns(
            &mut db,
            "SELECT name AS who, u.id * 2 AS twice, total FROM users u, orders LIMIT 1"
        ),
        ["who", "twice", "total"]
    );
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT name AS n FROM users ORDER BY n DESC"
        )),
        [text("cy"), text("bo"), text("ann")]
    );
    // A name of the select list wins over a column's, as in PostgreSQL and SQLite.
    assert_eq!(
        ints(query(&mut db, "SELECT - id AS id FROM users ORDER BY id")),
        [int(-3), int(-2), int(-1)]
    );
    assert_eq!(
        ints(query(
            &mut db,
            "SELECT DISTINCT city AS c FROM users ORDER BY c"
        )),
        [text("lima"), text("oslo")]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT user_id AS u, COUNT(*) AS n FROM orders GROUP BY user_id ORDER BY n DESC, u LIMIT 2"
        ),
        [vec![int(1), int(2)], vec![Value::Null, int(1)]]
    );
}

/// What EXPLAIN ANALYZE says each step did, without its times: each line's
/// step, and what follows it in parentheses, then the query's rows. The
/// first line, the execution model, is left out.
fn analyzed(db: &mut Database, sql: &str) -> Vec<(String, String)> {
    let plan = explain(db, &format!("ANALYZE {sql}"));
    let mut lines: Vec<&str> = plan.lines().skip(1).collect();
    let total = lines.pop().unwrap();
    let without_time = |did: &str| match did.rsplit_once(", ") {
        Some((did, time)) if time.ends_with(" ms") => did.to_string(),
        _ => did.to_string(),
    };
    let mut steps: Vec<(String, String)> = lines
        .into_iter()
        .map(|line| {
            let (step, did) = line.rsplit_once("  (").unwrap();
            (
                step.trim().to_string(),
                without_time(did.strip_suffix(')').unwrap()),
            )
        })
        .collect();
    let (rows, time) = total.split_once(" in ").unwrap();
    assert!(time.ends_with(" ms"), "{total}");
    steps.push(("total".into(), rows.into()));
    steps
}

#[test]
fn column_loops_return_and_fail_as_rows_do_in_every_model() {
    let mut db = Database::in_memory();
    // Batches of the vectorized model: a column table's row groups, and a
    // heap's rows put in batches.
    for (table, storage) in [("tc", "COLUMN"), ("th", "HEAP")] {
        db.execute(&format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY, k INT, big INT, x REAL, gone INT) USING {storage}"
        ))
        .unwrap();
        let rows: Vec<String> = (0..5000i64)
            .map(|i| {
                let k = if i % 11 == 0 {
                    "NULL".into()
                } else {
                    ((i * 37) % 2000 - 1000).to_string()
                };
                let big = match i % 4 {
                    0 => (i64::MAX - i).to_string(),
                    1 => (i64::MIN + i).to_string(),
                    2 => "NULL".into(),
                    _ => i.to_string(),
                };
                format!("({i}, {k}, {big}, {}.25, NULL)", (i * 13) % 300)
            })
            .collect();
        db.execute(&format!("INSERT INTO {table} VALUES {}", rows.join(", ")))
            .unwrap();
    }
    // A running sum that overflows on the way, though the last would fit.
    db.execute("CREATE TABLE s (id INT PRIMARY KEY, v INT) USING COLUMN")
        .unwrap();
    db.execute(&format!(
        "INSERT INTO s VALUES (1, {}), (2, 1), (3, -1)",
        i64::MAX
    ))
    .unwrap();

    let mut queries = vec![
        "SELECT SUM(v) FROM s".to_string(),
        "SELECT AVG(v), MIN(v), MAX(v), COUNT(v) FROM s".to_string(),
    ];
    for t in ["tc", "th"] {
        queries.extend([
            format!("SELECT COUNT(*), COUNT(k), SUM(k), AVG(k), MIN(k), MAX(k) FROM {t}"),
            format!("SELECT AVG(big), MIN(big), MAX(big), COUNT(big) FROM {t}"),
            format!("SELECT SUM(big) FROM {t}"),
            format!("SELECT SUM(big) FROM {t} WHERE big > 0"),
            format!("SELECT COUNT(gone), SUM(gone), AVG(gone), MIN(gone), MAX(gone) FROM {t}"),
            format!("SELECT SUM(x), AVG(x), MIN(x), MAX(x), COUNT(x) FROM {t}"),
            format!("SELECT COUNT(*), SUM(k) FROM {t} WHERE k > id - 3000 AND x < k + 100.0"),
            format!("SELECT SUM(k * 2 + id), MIN(x * 2.0 - k), MAX(k - x / 4.0) FROM {t}"),
            format!("SELECT SUM(k * 3), SUM(big * 2) FROM {t} WHERE id < 10"),
            format!("SELECT COUNT(*) FROM {t} WHERE x / (k - k) > 1.0"),
            format!("SELECT id, k * id, x - k FROM {t} WHERE k <> id AND (k < 0 OR x > 100.0)"),
        ]);
    }
    for sql in &queries {
        let mut outcomes = Vec::new();
        for model in Model::ALL {
            db.set_execution_model(Some(model));
            let outcome = match db.execute(sql) {
                Ok(mut results) => match results.pop() {
                    Some(QueryResult::Rows { rows, .. }) => Ok(rows),
                    other => panic!("expected rows, got {other:?}"),
                },
                Err(e) => Err(e.to_string()),
            };
            outcomes.push((model, outcome));
        }
        let (first, expected) = &outcomes[0];
        for (model, outcome) in &outcomes[1..] {
            assert!(
                outcome == expected,
                "{sql}: {model} gives {outcome:?}, {first} {expected:?}"
            );
        }
    }
    // What the column loops must keep: an overflow on the way is one, as
    // value by value, and the sums an average takes are exact.
    db.set_execution_model(Some(Model::Vectorized));
    let e = db.execute("SELECT SUM(v) FROM s").unwrap_err().to_string();
    assert!(e.contains("overflow"), "{e}");
    let rows = query(&mut db, "SELECT AVG(v) FROM s");
    assert_eq!(rows, vec![vec![Value::Real(i64::MAX as f64 / 3.0)]]);
}

#[test]
fn explain_analyze_tells_what_each_step_did() {
    // What the models chosen per query do.
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, g INT)")
        .unwrap();
    db.execute("CREATE TABLE u (id INT PRIMARY KEY, g INT)")
        .unwrap();
    db.execute("CREATE INDEX u_g ON u (g)").unwrap();
    let values: Vec<String> = (0..100).map(|i| format!("({i}, {})", i % 10)).collect();
    db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
        .unwrap();
    db.execute(&format!("INSERT INTO u VALUES {}", values.join(", ")))
        .unwrap();
    let steps = |steps: &[(&str, &str)]| -> Vec<(String, String)> {
        steps
            .iter()
            .map(|(step, did)| (step.to_string(), did.to_string()))
            .collect()
    };

    assert_eq!(
        analyzed(
            &mut db,
            "SELECT g, COUNT(*) FROM t WHERE id < 50 AND g > 6 GROUP BY g ORDER BY g DESC"
        ),
        steps(&[
            ("PROJECT g, COUNT(*)", "3 rows"),
            ("SORT BY g DESC", "3 rows"),
            ("AGGREGATE COUNT(*) BY g", "3 rows"),
            ("FILTER (id < 50) AND (g > 6)", "15 rows"),
            ("SEARCH t USING INDEX t_pkey (id < 50)", "50 rows"),
            ("total", "3 rows"),
        ])
    );
    // A LIMIT stops the scan below it.
    assert_eq!(
        analyzed(&mut db, "SELECT * FROM t LIMIT 2"),
        steps(&[
            ("LIMIT 2", "2 rows"),
            ("SCAN t", "2 rows"),
            ("total", "2 rows")
        ])
    );
    // An index join looks rows up through its right side, which is not a step of its own.
    assert_eq!(
        analyzed(
            &mut db,
            "SELECT t.id, u.id FROM t JOIN u ON u.g = t.g WHERE t.id = 3"
        ),
        steps(&[
            ("PROJECT t.id, u.id", "10 rows"),
            ("INDEX JOIN", "10 rows"),
            ("FILTER t.id = 3", "1 row"),
            ("SEARCH t USING INDEX t_pkey (id = 3)", "1 row"),
            (
                "LOOKUP u USING INDEX u_g (u.g = t.g)",
                "counted with the step above"
            ),
            ("total", "10 rows"),
        ])
    );
    // With a small budget, a block nested loop join reads its right side once per block.
    db.set_join_memory(1000);
    let joined = analyzed(&mut db, "SELECT COUNT(*) FROM t JOIN u ON u.g < t.g");
    assert_eq!(
        joined[2],
        (
            "BLOCK NESTED LOOP JOIN ON u.g < t.g".to_string(),
            "4500 rows".to_string()
        )
    );
    assert_eq!(joined[3], ("SCAN t".to_string(), "100 rows".to_string()));
    // Its rows are counted over all of its runs.
    let (step, did) = &joined[4];
    let (rows, runs) = did
        .strip_suffix(" runs")
        .and_then(|d| d.split_once(" rows in "))
        .unwrap_or_else(|| panic!("{did}"));
    let (rows, runs): (u32, u32) = (rows.parse().unwrap(), runs.parse().unwrap());
    assert!(
        step == "SCAN u" && runs > 1 && rows == 100 * runs,
        "{joined:?}"
    );

    let error = db
        .execute("EXPLAIN ANALYZE UPDATE t SET g = 0")
        .unwrap_err();
    assert_eq!(error.to_string(), "EXPLAIN ANALYZE supports only SELECT");
    // ANALYZE is no keyword: it can still name a column.
    db.execute("CREATE TABLE stats (analyze INT)").unwrap();
    db.execute("INSERT INTO stats VALUES (1)").unwrap();
    assert_eq!(query(&mut db, "SELECT analyze FROM stats"), [vec![int(1)]]);
    assert_eq!(
        analyzed(&mut db, "SELECT analyze FROM stats")[2],
        ("total".to_string(), "1 row".to_string())
    );
}

fn real(r: f64) -> Value {
    Value::Real(r)
}

/// The first column of rows of INTs.
fn numbers(rows: Vec<Vec<Value>>) -> Vec<i64> {
    rows.into_iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            ref other => panic!("expected an INT, got {other:?}"),
        })
        .collect()
}

#[test]
fn case_coalesce_and_scalar_functions() {
    let mut db = setup();
    assert_eq!(
        query(
            &mut db,
            "SELECT id, CASE WHEN age IS NULL THEN 'unknown' WHEN age >= 30 THEN 'older' ELSE 'younger' END, \
             CASE id WHEN 1 THEN 'one' WHEN 2 THEN 'two' END FROM users ORDER BY id"
        ),
        [
            vec![int(1), text("older"), text("one")],
            vec![int(2), text("younger"), text("two")],
            vec![int(3), text("unknown"), Value::Null],
        ]
    );
    // CASE and COALESCE evaluate only what they need.
    assert_eq!(
        query(
            &mut db,
            "SELECT CASE WHEN id > 0 THEN 1 ELSE 1 / 0 END FROM users WHERE id = 1"
        ),
        [vec![int(1)]]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT COALESCE(age, 1 / 0) FROM users WHERE id = 1"
        ),
        [vec![int(30)]]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT COALESCE(age, -1), NULLIF(age, 30), name || '!' FROM users ORDER BY id"
        ),
        [
            vec![int(30), Value::Null, text("alice!")],
            vec![int(25), int(25), text("bob!")],
            vec![int(-1), Value::Null, text("carol!")],
        ]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT LENGTH(name), UPPER(name), LOWER('\u{c0}BC'), SUBSTR(name, 2, 3), SUBSTR(name, -2), ABS(-age) \
             FROM users WHERE id = 1"
        ),
        [vec![
            int(5),
            text("ALICE"),
            text("\u{c0}bc"),
            text("lic"),
            text("ce"),
            int(30)
        ]]
    );
    // SQLite's rules for SUBSTR's edges.
    for (args, want) in [
        ("'hello', 0, 2", "h"),
        ("'hello', 3, -2", "he"),
        ("'hello', -10, 7", "he"),
        ("'hello', 9", ""),
    ] {
        assert_eq!(
            query(
                &mut db,
                &format!("SELECT SUBSTR({args}) FROM users WHERE id = 1")
            ),
            [vec![text(want)]],
            "{args}"
        );
    }
    for (sql, error) in [
        (
            "SELECT COALESCE(age) FROM users",
            "wrong number of arguments to COALESCE: 1",
        ),
        (
            "SELECT LENGTH(age) FROM users",
            "LENGTH expects TEXT, got 30",
        ),
        ("SELECT name || age FROM users", "cannot apply Concat"),
        (
            "SELECT CASE WHEN age THEN 1 END FROM users",
            "CASE WHEN must be boolean, got 30",
        ),
        ("SELECT NOPE(age) FROM users", "unknown function: NOPE"),
    ] {
        assert!(db.execute(sql).unwrap_err().0.contains(error), "{sql}");
    }
}

#[test]
fn between_and_like_filter_rows() {
    let mut db = setup();
    let ids = |db: &mut Database, filter: &str| {
        numbers(query(
            db,
            &format!("SELECT id FROM users WHERE {filter} ORDER BY id"),
        ))
    };
    assert_eq!(ids(&mut db, "age BETWEEN 25 AND 29"), [2]);
    assert_eq!(ids(&mut db, "age NOT BETWEEN 25 AND 29"), [1]);
    // ASCII letters match in either case, as in SQLite.
    assert_eq!(ids(&mut db, "name LIKE 'A%'"), [1]);
    assert_eq!(ids(&mut db, "name LIKE '_o_'"), [2]);
    assert_eq!(ids(&mut db, "name LIKE '%r%l'"), [3]);
    assert_eq!(ids(&mut db, "name NOT LIKE '%o%'"), [1]);
    // BETWEEN is a range the planner can read through an index.
    assert_eq!(
        plan(&mut db, "SELECT * FROM users WHERE id BETWEEN 2 AND 3"),
        "SEARCH users USING INDEX users_pkey (id >= 2 AND id <= 3)"
    );
}

#[test]
fn offset_and_union() {
    let mut db = setup();
    db.execute("CREATE TABLE staff (id INT PRIMARY KEY, name TEXT); INSERT INTO staff VALUES (7, 'bob'), (8, 'dave')")
        .unwrap();
    assert_eq!(
        numbers(query(
            &mut db,
            "SELECT id FROM users ORDER BY id LIMIT 2 OFFSET 1"
        )),
        [2, 3]
    );
    assert_eq!(
        numbers(query(
            &mut db,
            "SELECT id FROM users ORDER BY id DESC LIMIT 5 OFFSET 2"
        )),
        [1]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT name FROM users UNION SELECT name FROM staff ORDER BY name"
        ),
        [
            vec![text("alice")],
            vec![text("bob")],
            vec![text("carol")],
            vec![text("dave")]
        ]
    );
    assert_eq!(
        query(
            &mut db,
            "SELECT name FROM users UNION ALL SELECT name FROM staff"
        )
        .len(),
        5
    );
    assert_eq!(
        numbers(query(
            &mut db,
            "SELECT id AS n FROM users UNION ALL SELECT id FROM staff ORDER BY n DESC LIMIT 2 OFFSET 1"
        )),
        [7, 3]
    );
    assert_eq!(
        numbers(query(
            &mut db,
            "SELECT id FROM staff WHERE name IN (SELECT name FROM users UNION SELECT 'dave' FROM users)"
        )),
        [7, 8]
    );
    assert_eq!(
        explain(
            &mut db,
            "SELECT name FROM users UNION SELECT name FROM staff"
        ),
        "DISTINCT\n  UNION ALL\n    PROJECT name\n      SCAN users\n    PROJECT name\n      SCAN staff"
    );
    let error = db
        .execute("SELECT id, name FROM users UNION SELECT id FROM staff")
        .unwrap_err();
    assert_eq!(
        error.0,
        "each query of a UNION must return as many columns: 2 and 1"
    );
}

#[test]
fn real_numbers() {
    let mut db = in_memory();
    for storage in ["HEAP", "LSM", "COLUMN"] {
        let t = format!("prices_{storage}");
        db.execute(&format!(
            "CREATE TABLE {t} (id INT PRIMARY KEY, price REAL, qty INT) USING {storage}"
        ))
        .unwrap();
        // An INT stored in a REAL column becomes a REAL; 2.0 in an INT column an INT.
        db.execute(&format!("INSERT INTO {t} VALUES (1, 2.5, 4), (2, 10, 1), (3, -0.75, 2.0), (4, NULL, 3), (5, 1e20, 1)"))
            .unwrap();
        let error = db
            .execute(&format!("INSERT INTO {t} VALUES (6, 1.0, 2.5)"))
            .unwrap_err();
        assert!(
            error.0.contains("expects INT, got 2.5"),
            "{storage}: {error}"
        );
        let rows = query(
            &mut db,
            &format!("SELECT price, price * qty, qty / 2, qty / 2.0 FROM {t} ORDER BY id"),
        );
        assert!(
            matches!(rows[1][0], Value::Real(r) if r == 10.0),
            "{storage}: {:?}",
            rows[1]
        );
        assert_eq!(
            rows[0],
            [real(2.5), real(10.0), int(2), real(2.0)],
            "{storage}"
        );
        assert_eq!(
            rows[2],
            [real(-0.75), real(-1.5), int(1), real(1.0)],
            "{storage}"
        );
        // INT 10 equals REAL 10.0, in a filter and in a pushed-down condition.
        assert_eq!(
            numbers(query(
                &mut db,
                &format!("SELECT id FROM {t} WHERE price = 10 ORDER BY id")
            )),
            [2],
            "{storage}"
        );
        assert_eq!(
            numbers(query(
                &mut db,
                &format!("SELECT id FROM {t} WHERE price > 2 ORDER BY price")
            )),
            [1, 2, 5],
            "{storage}"
        );
        assert_eq!(
            query(
                &mut db,
                &format!(
                    "SELECT SUM(price), AVG(qty), MIN(price), MAX(price), ROUND(AVG(price), 2) FROM {t} WHERE id < 4"
                )
            ),
            [vec![
                real(11.75),
                real(7.0 / 3.0),
                real(-0.75),
                real(10.0),
                real(3.92)
            ]],
            "{storage}"
        );
    }
    // An index on a REAL column, looked up with an INT.
    db.execute("CREATE INDEX prices_heap_price ON prices_HEAP (price)")
        .unwrap();
    assert_eq!(
        plan(&mut db, "SELECT * FROM prices_HEAP WHERE price >= 2"),
        "SEARCH prices_HEAP USING INDEX prices_heap_price (price >= 2.0)"
    );
    assert_eq!(
        numbers(query(
            &mut db,
            "SELECT id FROM prices_HEAP WHERE price >= 2"
        )),
        [1, 2, 5]
    );
    // INTs and REALs sort, group and join by value.
    assert_eq!(
        query(
            &mut db,
            "SELECT x FROM (SELECT qty AS x FROM prices_HEAP UNION ALL SELECT price FROM prices_HEAP) d ORDER BY x"
        ),
        [
            Value::Null,
            real(-0.75),
            int(1),
            int(1),
            int(2),
            real(2.5),
            int(3),
            int(4),
            real(10.0),
            real(1e20)
        ]
        .map(|v| vec![v])
    );
    // 1, 2, 3, 4, then 2.0 four times, which is 2, and NULL.
    assert_eq!(
        query(
            &mut db,
            "SELECT COUNT(*) FROM (SELECT qty FROM prices_HEAP UNION SELECT price * 0 + 2 FROM prices_HEAP) d"
        ),
        [vec![int(5)]]
    );
    // A hash join of an INT with a REAL: qty 4 + 6 is price 10.0.
    assert_eq!(
        numbers(query(
            &mut db,
            "SELECT a.id FROM prices_HEAP a JOIN prices_LSM b ON b.price = a.qty + 6"
        )),
        [1]
    );
    for (sql, want) in [
        ("ROUND(2.5)", 3.0),
        ("ROUND(-2.5)", -3.0),
        ("ROUND(0.125, 2)", 0.13),
        ("ROUND(2.675, 2)", 2.67),
    ] {
        assert_eq!(
            query(
                &mut db,
                &format!("SELECT {sql} FROM prices_HEAP WHERE id = 1")
            ),
            [vec![real(want)]],
            "{sql}"
        );
    }
    for (sql, error) in [
        ("1.5 / 0", "division by zero"),
        ("1e300 * 1e300", "REAL overflow"),
        ("ROUND('a')", "ROUND expects a number"),
    ] {
        assert!(
            db.execute(&format!("SELECT {sql} FROM prices_HEAP"))
                .unwrap_err()
                .0
                .contains(error),
            "{sql}"
        );
    }
    // A REAL is printed so that it reads back as the same value.
    assert_eq!(
        explain(&mut db, "SELECT * FROM prices_LSM WHERE price < 0.1"),
        "FILTER price < 0.1\n  SCAN prices_LSM"
    );

    let file = TempDb::new("reals");
    {
        let mut db = open(&file.0).unwrap();
        db.execute("CREATE TABLE r (x REAL PRIMARY KEY) USING LSM; INSERT INTO r VALUES (0.5), (-1e-7), (3)").unwrap();
    }
    let mut db = open(&file.0).unwrap();
    assert_eq!(
        query(&mut db, "SELECT x FROM r WHERE x > -1 ORDER BY x DESC"),
        [vec![real(3.0)], vec![real(0.5)], vec![real(-1e-7)]]
    );
}

#[test]
fn zone_maps_skip_row_groups_and_change_no_result() {
    let mut db = Database::in_memory();
    // `id` grows with each row, `k` is scattered, `x` has NULLs and -0.0.
    for (table, storage) in [("h", "HEAP"), ("c", "COLUMN")] {
        db.execute(&format!(
            "CREATE TABLE {table} (id INT, k INT, x REAL, name TEXT, flag BOOL) USING {storage}"
        ))
        .unwrap();
        for start in (0..10_000).step_by(1000) {
            let rows: Vec<String> = (start..start + 1000)
                .map(|i: i64| {
                    let x = match i % 9 {
                        0 => "NULL".to_string(),
                        1 => "-0.0".to_string(),
                        _ => format!("{}.5", i / 10),
                    };
                    let flag = if i < 3000 {
                        "NULL"
                    } else if i % 2 == 0 {
                        "TRUE"
                    } else {
                        "FALSE"
                    };
                    format!(
                        "({i}, {}, {x}, 'n{:05}', {flag})",
                        (i * 7919) % 10_000,
                        i / 3
                    )
                })
                .collect();
            db.execute(&format!("INSERT INTO {table} VALUES {}", rows.join(", ")))
                .unwrap();
        }
    }
    let changes = [
        "DELETE FROM {t} WHERE id % 13 = 0",
        "UPDATE {t} SET k = -1 WHERE id >= 4000 AND id < 4100",
    ];
    for (round, change) in ["", changes[0], changes[1]].into_iter().enumerate() {
        for table in ["h", "c"] {
            if !change.is_empty() {
                db.execute(
                    &change
                        .replace("{t}", table)
                        .replace("id % 13 = 0", "id - id / 13 * 13 = 0"),
                )
                .unwrap();
            }
        }
        for filter in [
            "id < 500",
            "id >= 2500 AND id < 2600",
            "id = 7777",
            "id > 20000",
            "id <> 5",
            "k < 100",
            "k = -1",
            "x < 0.0",
            "x <= -0.0",
            "x >= 999.5",
            "x IS NULL AND id < 2000",
            "name >= 'n03000' AND name < 'n03010'",
            "flag = TRUE AND id < 3500",
            "flag <> FALSE",
            "id >= 0",
        ] {
            let sql = |t: &str| {
                format!("SELECT id, k, x, name, flag FROM {t} WHERE {filter} ORDER BY id")
            };
            let heap = query(&mut db, &sql("h"));
            for model in Model::ALL {
                db.set_execution_model(Some(model));
                assert_eq!(
                    query(&mut db, &sql("c")),
                    heap,
                    "round {round}, {filter}, {model}"
                );
            }
            db.set_execution_model(None);
        }
    }
    // What EXPLAIN ANALYZE says of the zone maps, after the changes above.
    let skipped = |db: &mut Database, filter: &str| -> (u64, u64) {
        let plan = explain(
            db,
            &format!("ANALYZE SELECT COUNT(*) FROM c WHERE {filter}"),
        );
        let line = plan
            .lines()
            .find(|line| line.starts_with("ZONE MAPS c  ("))
            .unwrap_or_else(|| panic!("{plan}"));
        let (skipped, total) = line["ZONE MAPS c  (".len()..].split_once(" of ").unwrap();
        (
            skipped.parse().unwrap(),
            total.split(' ').next().unwrap().parse().unwrap(),
        )
    };
    let (skipped_low, groups) = skipped(&mut db, "id < 500");
    assert!(
        groups >= 8 && skipped_low == groups - 1,
        "{skipped_low} of {groups}"
    );
    assert_eq!(skipped(&mut db, "id > 20000"), (groups, groups));
    assert_eq!(
        skipped(&mut db, "k < 100").0,
        0,
        "k is scattered over every group"
    );
    assert_eq!(
        skipped(&mut db, "flag = TRUE AND id < 2000").0,
        groups,
        "flag is all NULL where id < 3000"
    );
    // Heap tables have no zone maps.
    assert!(
        !explain(&mut db, "ANALYZE SELECT COUNT(*) FROM h WHERE id < 500").contains("ZONE MAPS")
    );
}

/// A heap table and a column table of the same `n` rows, with NULLs, many
/// batches and row groups of them, and a small table to join them with.
fn models_setup(n: i64) -> Database {
    let mut db = Database::in_memory();
    for (table, storage) in [("h", "HEAP"), ("c", "COLUMN")] {
        db.execute(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, g INT, x REAL, name TEXT, flag BOOL) USING {storage}"))
            .unwrap();
        for start in (0..n).step_by(1000) {
            let rows: Vec<String> = (start..n.min(start + 1000))
                .map(|i| {
                    let g = if i % 11 == 0 {
                        "NULL".to_string()
                    } else {
                        (i % 7).to_string()
                    };
                    let x = if i % 13 == 0 {
                        "NULL".to_string()
                    } else {
                        format!("{}.5", i % 100)
                    };
                    format!("({i}, {g}, {x}, 'n{}', {})", i % 50, i % 3 == 0)
                })
                .collect();
            db.execute(&format!("INSERT INTO {table} VALUES {}", rows.join(", ")))
                .unwrap();
        }
    }
    db.execute("CREATE TABLE k (g INT PRIMARY KEY, label TEXT)")
        .unwrap();
    db.execute(
        "INSERT INTO k VALUES (0, 'zero'), (1, 'one'), (2, 'two'), (3, 'three'), (5, 'five')",
    )
    .unwrap();
    db
}

/// What a statement returns in `model`: its rows, or its error.
fn outcome(db: &mut Database, model: Option<Model>, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    db.set_execution_model(model);
    match db.execute(sql) {
        Ok(mut results) => match results.pop() {
            Some(QueryResult::Rows { rows, .. }) => Ok(rows),
            other => panic!("expected rows, got {other:?}"),
        },
        Err(e) => Err(e.0),
    }
}

/// The model EXPLAIN ANALYZE says a query ran in, and why.
fn model_of(db: &mut Database, sql: &str) -> String {
    explain(db, &format!("ANALYZE {sql}"))
        .lines()
        .next()
        .unwrap()
        .to_string()
}

#[test]
fn every_execution_model_returns_the_same_rows() {
    let mut db = models_setup(5000);
    for table in ["h", "c"] {
        let queries = [
            "SELECT * FROM {t}",
            "SELECT id, g * 2 + 1, x / 2, name || '!', NOT flag FROM {t} WHERE g > 2 AND x < 40.0",
            "SELECT id FROM {t} WHERE g IS NULL OR name LIKE 'n1%'",
            "SELECT id, g IN (1, 3, NULL), g NOT IN (2, 4) FROM {t} WHERE id < 100",
            "SELECT id, CASE WHEN g > 3 THEN 'high' WHEN g IS NULL THEN 'none' ELSE 'low' END, COALESCE(g, -1) FROM {t}",
            "SELECT ABS(g - 3), UPPER(name), LENGTH(name), ROUND(x, 1), NULLIF(g, 0) FROM {t} WHERE id < 60",
            "SELECT COUNT(*), COUNT(g), SUM(g), SUM(x), AVG(g), AVG(x), MIN(x), MAX(name), MIN(id), MAX(id) FROM {t}",
            "SELECT COUNT(*), SUM(id) FROM {t} WHERE flag",
            "SELECT g, COUNT(*), SUM(x), MIN(name), COUNT(DISTINCT name) FROM {t} GROUP BY g",
            "SELECT g, flag, AVG(x) FROM {t} GROUP BY g, flag HAVING COUNT(*) > 200 ORDER BY g, flag",
            "SELECT COUNT(DISTINCT g), SUM(DISTINCT g) FROM {t}",
            "SELECT DISTINCT g, flag FROM {t}",
            "SELECT id, name FROM {t} ORDER BY x DESC, id LIMIT 25 OFFSET 10",
            "SELECT id FROM {t} LIMIT 1500 OFFSET 1000",
            "SELECT id FROM {t} WHERE g = 3 LIMIT 3",
            "SELECT t.id, k.label FROM {t} t JOIN k ON k.g = t.g WHERE t.id < 300",
            "SELECT t.id, k.label FROM {t} t LEFT JOIN k ON k.g = t.g AND k.label <> 'one' WHERE t.id < 300",
            "SELECT k.label, COUNT(*) FROM {t} t JOIN k ON k.g = t.g GROUP BY k.label ORDER BY k.label",
            "SELECT a.id, b.id FROM {t} a JOIN {t} b ON b.id = a.id + 1 WHERE a.id < 50",
            "SELECT id FROM {t} WHERE id < 10 UNION SELECT g FROM {t} WHERE g IS NOT NULL",
            "SELECT id FROM {t} WHERE id < 5 UNION ALL SELECT id FROM {t} WHERE id > 4995",
            "SELECT d.g, d.n FROM (SELECT g, COUNT(*) AS n FROM {t} GROUP BY g) d WHERE d.n > 600",
            "SELECT id, (SELECT label FROM k WHERE k.g = t.g) FROM {t} t WHERE id < 40",
            "SELECT id FROM {t} t WHERE EXISTS (SELECT 1 FROM k WHERE k.g = t.g) AND id < 100",
            "SELECT COUNT(*) FROM {t} WHERE g IN (SELECT g FROM k)",
            "SELECT id FROM {t} WHERE id > 100000",
            "SELECT SUM(g) FROM {t} WHERE id > 100000",
        ];
        for sql in queries {
            let sql = sql.replace("{t}", table);
            let expected = outcome(&mut db, Some(Model::Iterator), &sql);
            assert!(expected.is_ok(), "{sql}: {expected:?}");
            for model in [Some(Model::Materialization), Some(Model::Vectorized), None] {
                assert_eq!(
                    outcome(&mut db, model, &sql),
                    expected,
                    "{sql} in {model:?}"
                );
            }
        }
    }
}

#[test]
fn every_execution_model_fails_on_the_row_the_iterator_model_does() {
    let mut db = models_setup(3000);
    // Row 500 divides by zero, row 2000 overflows: the first a row at a time is the error.
    db.execute("UPDATE h SET g = 1 WHERE g = 0").unwrap();
    db.execute("UPDATE h SET g = 0 WHERE id = 500").unwrap();
    db.execute("UPDATE h SET g = 9223372036854775807 WHERE id = 2000")
        .unwrap();
    for (sql, fails) in [
        // A LIMIT that has its rows before the error ends the query without it,
        // though a batch of 1024 rows holds it.
        ("SELECT id / g FROM h WHERE g IS NOT NULL LIMIT 10", false),
        ("SELECT id / g FROM h WHERE g IS NOT NULL LIMIT 600", true),
        ("SELECT id FROM h WHERE id / g > 0 LIMIT 100", false),
        ("SELECT id FROM h WHERE id / g >= 0", true),
        ("SELECT id, g + g FROM h WHERE id > 1000 LIMIT 500", false),
        ("SELECT id, g + g FROM h WHERE id > 1000", true),
        ("SELECT SUM(g) FROM h WHERE id < 1500", false),
        ("SELECT SUM(g) FROM h", true),
        ("SELECT COUNT(*), MAX(g), SUM(x) FROM h", false),
        ("SELECT SUM(g + g) FROM h", true),
        ("SELECT SUM(g * 1000000000000) FROM h", true),
        ("SELECT g, SUM(id / g) FROM h GROUP BY g", true),
        // The select list is evaluated on the 3 rows the sort keeps, its keys on all.
        ("SELECT id, id / g FROM h ORDER BY id LIMIT 3", false),
        ("SELECT id FROM h ORDER BY id / g LIMIT 3", true),
        ("SELECT id FROM h WHERE g = 'text' LIMIT 1", true),
    ] {
        let expected = outcome(&mut db, Some(Model::Iterator), sql);
        assert_eq!(expected.is_err(), fails, "{sql}: {expected:?}");
        for model in [Some(Model::Materialization), Some(Model::Vectorized), None] {
            assert_eq!(outcome(&mut db, model, sql), expected, "{sql} in {model:?}");
        }
    }
}

#[test]
fn each_query_runs_in_the_model_that_suits_it() {
    let mut db = models_setup(20_000);
    // A lookup of a few rows: each step hands on all of its rows at once.
    assert!(
        model_of(&mut db, "SELECT * FROM h WHERE id = 7")
            .starts_with("MODEL materialization  (at most about 1 row ")
    );
    assert!(model_of(&mut db, "SELECT label FROM k").starts_with("MODEL materialization"));
    // Scanning and aggregating many rows: batches of columns.
    assert!(
        model_of(&mut db, "SELECT g, COUNT(*) FROM h GROUP BY g")
            .starts_with("MODEL vectorized  (about 20000 rows at a step")
    );
    // A whole column table is read, however few rows its filter keeps.
    assert!(model_of(&mut db, "SELECT id, x FROM c WHERE g = 1").starts_with("MODEL vectorized"));
    assert!(
        model_of(&mut db, "SELECT * FROM h WHERE g = 1 LIMIT 5").starts_with("MODEL vectorized")
    );
    // Joins work on rows: streamed a row at a time.
    let joined = model_of(&mut db, "SELECT t.id, k.label FROM h t JOIN k ON k.g = t.g");
    assert!(
        joined.starts_with("MODEL iterator  (up to about 20000 rows at a step, through a join"),
        "{joined}"
    );
    // Subqueries run a row at a time anyway.
    assert!(
        model_of(
            &mut db,
            "SELECT COUNT(*) FROM h t WHERE EXISTS (SELECT 1 FROM k WHERE k.g = t.g)"
        )
        .starts_with("MODEL iterator")
    );
    // Too many rows to hold, too few for batches.
    db.execute("DELETE FROM h WHERE id >= 1000").unwrap();
    let some = model_of(&mut db, "SELECT * FROM h WHERE g = 1");
    assert!(
        some.starts_with("MODEL iterator  (up to about 1000 rows at a step, too many"),
        "{some}"
    );
    // A model the session sets runs every query.
    db.set_execution_model(Some(Model::Vectorized));
    assert_eq!(
        model_of(&mut db, "SELECT * FROM h WHERE id = 7"),
        "MODEL vectorized  (set for the session)"
    );
}

#[test]
fn explain_analyze_shows_how_each_model_reads() {
    let mut db = models_setup(5000);
    let steps = |db: &mut Database, model, sql| {
        db.set_execution_model(Some(model));
        analyzed(db, sql)
    };
    let limited = "SELECT id FROM h WHERE g = 1 LIMIT 3";
    // The iterator model stops the scan at the row the LIMIT needs, the
    // vectorized one at the end of that row's batch.
    let scanned = |steps: Vec<(String, String)>| steps[3].1.clone();
    assert_eq!(scanned(steps(&mut db, Model::Iterator, limited)), "16 rows");
    assert_eq!(
        scanned(steps(&mut db, Model::Vectorized, limited)),
        "1024 rows"
    );
    // In the materialization model, a filter makes all of its rows: who
    // knows how many it drops. With none, a LIMIT stops the scan.
    assert_eq!(
        scanned(steps(&mut db, Model::Materialization, limited)),
        "5000 rows"
    );
    let first = steps(&mut db, Model::Materialization, "SELECT id FROM h LIMIT 3");
    assert_eq!(first[2], ("SCAN h".to_string(), "3 rows".to_string()));
    // A block nested loop join reads its right side once per block of left
    // rows, but in the materialization model, it runs once and is kept.
    db.set_join_memory(10_000);
    let join = "SELECT COUNT(*) FROM h a JOIN h b ON b.id < a.id WHERE a.id < 300 AND b.id < 300";
    let right = |steps: Vec<(String, String)>| {
        steps
            .iter()
            .find(|(step, _)| step.contains("h b"))
            .unwrap()
            .1
            .clone()
    };
    assert_eq!(
        right(steps(&mut db, Model::Materialization, join)),
        "300 rows"
    );
    let iterator = right(steps(&mut db, Model::Iterator, join));
    assert!(
        iterator.ends_with(" runs") && iterator.starts_with("1500 rows in 5"),
        "{iterator}"
    );
}

/// Runs one statement and returns its column names and rows.
fn returned(db: &mut Database, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    match db.execute(sql).unwrap().pop() {
        Some(QueryResult::Rows { columns, rows }) => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

/// Rows sorted by their first value, an INT.
fn by_first(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by_key(|row| match row[0] {
        Value::Int(n) => n,
        _ => panic!("{row:?}"),
    });
    rows
}

#[test]
fn returning_gives_the_rows_each_write_changed() {
    let mut db = setup();
    let (t, f, n) = (Value::Bool(true), Value::Bool(false), Value::Null);
    // INSERT: the rows as stored, in the order of VALUES, omitted columns NULL.
    let (columns, rows) = returned(
        &mut db,
        "INSERT INTO users VALUES (5, 'eve', 40, TRUE), (4, 'dan', NULL, FALSE) RETURNING *",
    );
    assert_eq!(columns, names(&["id", "name", "age", "active"]));
    assert_eq!(
        rows,
        [
            vec![int(5), text("eve"), int(40), t.clone()],
            vec![int(4), text("dan"), n.clone(), f.clone()]
        ]
    );
    let (columns, rows) = returned(
        &mut db,
        "INSERT INTO users (id, name) VALUES (6, 'fay') RETURNING id * 10 AS tens, age IS NULL, UPPER(users.name), name",
    );
    assert_eq!(columns, names(&["tens", "?column?", "upper", "name"]));
    assert_eq!(rows, [vec![int(60), t.clone(), text("FAY"), text("fay")]]);

    // UPDATE: the new values, every right-hand side having seen the old row.
    let (columns, rows) = returned(
        &mut db,
        "UPDATE users SET age = age + 1, name = name || '!' WHERE age > 30 RETURNING id, name, age",
    );
    assert_eq!(columns, names(&["id", "name", "age"]));
    assert_eq!(rows, [vec![int(5), text("eve!"), int(41)]]);
    let (_, rows) = returned(
        &mut db,
        "UPDATE users SET active = NOT active WHERE id = 1 RETURNING active, age",
    );
    assert_eq!(rows, [vec![f.clone(), int(30)]]);

    // DELETE: the rows as they were, every column of them, though the WHERE reads only one.
    let (columns, rows) = returned(&mut db, "DELETE FROM users WHERE id = 2 RETURNING *");
    assert_eq!(columns, names(&["id", "name", "age", "active"]));
    assert_eq!(rows, [vec![int(2), text("bob"), int(25), f.clone()]]);
    assert_eq!(
        returned(&mut db, "DELETE FROM users WHERE active RETURNING name").1,
        [vec![text("eve!")]]
    );

    // No rows written: no rows returned, but still the columns.
    let (columns, rows) = returned(
        &mut db,
        "UPDATE users SET age = 0 WHERE id = 99 RETURNING id, age",
    );
    assert_eq!((columns, rows.len()), (names(&["id", "age"]), 0));
    assert_eq!(
        returned(&mut db, "DELETE FROM users WHERE id = 99 RETURNING *")
            .1
            .len(),
        0
    );

    // The writes happened, and without RETURNING a write still reports a count.
    assert_eq!(
        ints(query(&mut db, "SELECT id FROM users ORDER BY id")),
        [int(1), int(3), int(4), int(6)]
    );
    let done = db
        .execute("DELETE FROM users WHERE id = 6")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(done, QueryResult::Message("1 row(s) deleted".into()));
}

#[test]
fn returning_shows_values_as_their_columns_store_them() {
    let mut db = in_memory();
    db.execute("CREATE TABLE r (id INT PRIMARY KEY, x REAL)")
        .unwrap();
    // An INT stored in a REAL column is a REAL, a REAL with no fraction in an INT column an INT.
    assert_eq!(
        returned(&mut db, "INSERT INTO r VALUES (2.0, 3) RETURNING id, x").1,
        [vec![int(2), Value::Real(3.0)]]
    );
    assert_eq!(
        returned(&mut db, "UPDATE r SET x = 7, id = 4.0 RETURNING *").1,
        [vec![int(4), Value::Real(7.0)]]
    );
    assert_eq!(
        returned(&mut db, "DELETE FROM r RETURNING x / 2").1,
        [vec![Value::Real(3.5)]]
    );
}

#[test]
fn every_storage_returns_the_rows_it_wrote() {
    for storage in ["HEAP", "LSM", "COLUMN"] {
        let mut db = in_memory();
        db.execute(&format!(
            "CREATE TABLE t (id INT PRIMARY KEY, tag TEXT, n INT, body TEXT) USING {storage}"
        ))
        .unwrap();
        if storage != "COLUMN" {
            db.execute("CREATE INDEX t_tag ON t (tag)").unwrap();
        }
        // Enough rows for an LSM tree to flush and a column store to migrate into row groups.
        let values: Vec<String> = (0..1500)
            .map(|i| format!("({i}, 'tag{}', {i}, '{}')", i % 7, "b".repeat(i % 50)))
            .collect();
        let (_, rows) = returned(
            &mut db,
            &format!("INSERT INTO t VALUES {} RETURNING id", values.join(", ")),
        );
        assert_eq!(rows.len(), 1500, "{storage}");
        assert_eq!(rows[1499], [int(1499)], "{storage}");

        // The primary key changes too: an LSM row moves to a new key.
        let sql = "UPDATE t SET id = id + 10000, n = n * 2 WHERE tag = 'tag3' AND id < 30 RETURNING id, n, tag";
        let want: Vec<Vec<Value>> = [3, 10, 17, 24]
            .iter()
            .map(|&i| vec![int(10000 + i), int(2 * i), text("tag3")])
            .collect();
        assert_eq!(by_first(returned(&mut db, sql).1), want, "{storage}");

        // A DELETE through the index returns the columns the index does not have.
        let sql = "DELETE FROM t WHERE tag = 'tag5' AND n < 20 RETURNING id, body, n";
        let want: Vec<Vec<Value>> = [5, 12, 19]
            .iter()
            .map(|&i| vec![int(i), text(&"b".repeat(i as usize % 50)), int(i)])
            .collect();
        assert_eq!(by_first(returned(&mut db, sql).1), want, "{storage}");
        assert_eq!(
            query(&mut db, "SELECT COUNT(*) FROM t"),
            [vec![int(1497)]],
            "{storage}"
        );
        assert_eq!(
            query(&mut db, "SELECT COUNT(*) FROM t WHERE id >= 10000"),
            [vec![int(4)]],
            "{storage}"
        );
    }
}

#[test]
fn a_failing_returning_list_undoes_its_statement() {
    let mut db = setup();
    let totals = |db: &mut Database| query(db, "SELECT COUNT(*), SUM(age) FROM users");
    let before = totals(&mut db);
    for (sql, error) in [
        (
            "INSERT INTO users (id) VALUES (7) RETURNING nope",
            "unknown column: nope",
        ),
        (
            "UPDATE users SET age = 1 RETURNING COUNT(*)",
            "aggregate functions are not allowed in RETURNING",
        ),
        (
            "DELETE FROM users RETURNING (SELECT MAX(id) FROM users)",
            "subqueries are not allowed in RETURNING",
        ),
        (
            "DELETE FROM users WHERE id = 1 RETURNING EXISTS (SELECT 1 FROM users)",
            "subqueries are not allowed in RETURNING",
        ),
        // Evaluated once the rows are written: a division by zero undoes the update.
        (
            "UPDATE users SET age = 0 RETURNING 1 / age",
            "division by zero",
        ),
        (
            "INSERT INTO users VALUES (1, 'dup', 1, TRUE) RETURNING *",
            "duplicate primary key",
        ),
        ("DELETE FROM users RETURNING", "expected an expression"),
        ("DELETE FROM users RETURNING *, id", "expected ';'"),
    ] {
        let e = db.execute(sql).unwrap_err().to_string();
        assert!(e.contains(error), "{sql}: {e}");
        assert_eq!(totals(&mut db), before, "{sql}");
    }
    // In a transaction, only the failing statement is undone.
    db.execute("BEGIN").unwrap();
    assert_eq!(
        returned(&mut db, "INSERT INTO users (id) VALUES (8) RETURNING id").1,
        [vec![int(8)]]
    );
    assert!(
        db.execute("UPDATE users SET age = 0 RETURNING 1 / age")
            .is_err()
    );
    db.execute("COMMIT").unwrap();
    assert_eq!(totals(&mut db), [vec![int(4), int(55)]]);
}

/// The Halloween problem: an UPDATE that reads its rows through an index (or
/// an LSM tree's keys) and changes the column it is ordered by may meet the
/// rows it changed again further on, and change them again. Every update
/// here does that, where the plan shows it reads that way, and must change
/// each row once, as if every row were read before any was written.
#[test]
fn updates_change_each_row_once_though_they_move_it_along_their_scan() {
    const ROWS: i64 = 2000;
    let long = "y".repeat(300);
    for storage in ["HEAP", "LSM", "COLUMN"] {
        let mut db = in_memory();
        db.execute(&format!(
            "CREATE TABLE t (id INT PRIMARY KEY, n INT, g INT, body TEXT) USING {storage}"
        ))
        .unwrap();
        if storage != "COLUMN" {
            db.execute("CREATE INDEX t_n ON t (n); CREATE INDEX t_g ON t USING HASH (g)")
                .unwrap();
        }
        // Enough rows for many heap pages, LSM flushes and column row groups.
        let values: Vec<String> = (0..ROWS)
            .map(|i| format!("({i}, {i}, {}, 'x')", i % 10))
            .collect();
        db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .unwrap();
        // What the table should hold: each row's id, n, g and the length of its body.
        let mut want: Vec<[i64; 4]> = (0..ROWS).map(|i| [i, i, i % 10, 1]).collect();

        let mut check =
            |db: &mut Database, sql: &str, path: &str, change: &dyn Fn(&mut [i64; 4]) -> bool| {
                let found = plan(db, sql);
                assert!(found.starts_with(path), "{storage}: {sql} reads by {found}");
                let changed = want
                    .iter_mut()
                    .map(|row| change(row) as usize)
                    .sum::<usize>();
                let done = db.execute(sql).unwrap().pop().unwrap();
                assert_eq!(
                    done,
                    QueryResult::Message(format!("{changed} row(s) updated")),
                    "{storage}: {sql}"
                );
                want.sort();
                let rows = query(db, "SELECT id, n, g, LENGTH(body) FROM t ORDER BY id");
                let expected: Vec<Vec<Value>> = want
                    .iter()
                    .map(|row| row.iter().map(|&v| int(v)).collect())
                    .collect();
                assert!(
                    rows == expected,
                    "{storage}: {sql} left rows other than expected"
                );
            };

        // Each row moves up by 500, within the range still to be read, and
        // a heap row grows, so it moves to another page and another id too.
        let sql = format!("UPDATE t SET n = n + 500, body = '{long}' WHERE n < 1500");
        let path = match storage {
            "COLUMN" => "SCAN t",
            _ => "SEARCH t USING INDEX t_n (n < 1500)",
        };
        check(&mut db, &sql, path, &|row| {
            let hit = row[1] < 1500;
            if hit {
                row[1] += 500;
                row[3] = 300;
            }
            hit
        });

        // A row stays in the hash bucket being read, under a new entry if it moved.
        let sql = format!("UPDATE t SET n = n + 1, body = '{long}{long}' WHERE g = 3");
        let path = match storage {
            "COLUMN" => "SCAN t",
            _ => "SEARCH t USING HASH INDEX t_g (g = 3)",
        };
        check(&mut db, &sql, path, &|row| {
            let hit = row[2] == 3;
            if hit {
                row[1] += 1;
                row[3] = 600;
            }
            hit
        });

        // Every primary key moves up by 1000, into the range still to be read.
        let sql = "UPDATE t SET id = id + 1000 WHERE id >= 0";
        let path = match storage {
            "HEAP" => "SEARCH t USING INDEX t_pkey (id >= 0)",
            "LSM" => "SEARCH t USING PRIMARY KEY (id >= 0)",
            _ => "SCAN t",
        };
        check(&mut db, sql, path, &|row| {
            row[0] += 1000;
            true
        });
    }
}

#[test]
fn returning_is_no_keyword() {
    let mut db = in_memory();
    // It stays free as the name of a table or a column.
    db.execute("CREATE TABLE returning (returning INT, x INT)")
        .unwrap();
    let (columns, rows) = returned(
        &mut db,
        "INSERT INTO returning VALUES (1, 2) RETURNING returning",
    );
    assert_eq!((columns, rows), (names(&["returning"]), vec![vec![int(1)]]));
    let sql = "UPDATE returning SET returning = returning + x WHERE x = returning + 1 RETURNING returning, x";
    assert_eq!(returned(&mut db, sql).1, [vec![int(3), int(2)]]);
    // EXPLAIN shows how the rows are found, as without RETURNING.
    assert_eq!(
        explain(&mut db, "DELETE FROM returning WHERE x = 2 RETURNING *"),
        explain(&mut db, "DELETE FROM returning WHERE x = 2")
    );
    assert_eq!(
        returned(&mut db, "delete from returning returning returning * 2").1,
        [vec![int(6)]]
    );
}

/// `AND` and `OR` evaluate their right side only where the left one does
/// not decide, in every model and every kind of step: a division by zero on
/// a row whose left side decides is no error, and one on any other row is.
#[test]
fn and_or_skip_their_right_side_alike_in_every_model() {
    const ROWS: i64 = 3000;
    // x is 0 on every seventh row; NULL on every eleventh.
    let x = |i: i64| (i % 11 != 5).then_some(i % 7 - 3);
    for storage in ["HEAP", "COLUMN"] {
        let mut db = in_memory();
        db.execute(&format!(
            "CREATE TABLE t (id INT PRIMARY KEY, x INT) USING {storage}"
        ))
        .unwrap();
        let values: Vec<String> = (0..ROWS)
            .map(|i| format!("({i}, {})", x(i).map_or("NULL".into(), |x| x.to_string())))
            .collect();
        db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .unwrap();

        // What each query should return, worked out here.
        let big = |x: Option<i64>| x.map(|x| x != 0 && 12 / x > 3); // x <> 0 AND 12 / x > 3
        let ids: Vec<Value> = (0..ROWS)
            .filter(|&i| big(x(i)) == Some(true))
            .map(int)
            .collect();
        let small = (0..ROWS)
            .filter(|&i| x(i).is_some_and(|x| x == 0 || 12 / x < 5))
            .count() as i64;
        let queries: Vec<(&str, Vec<Vec<Value>>)> = vec![
            (
                "SELECT id FROM t WHERE x <> 0 AND 12 / x > 3 ORDER BY id",
                ids.iter().map(|id| vec![id.clone()]).collect(),
            ),
            (
                "SELECT COUNT(*) FROM t WHERE x = 0 OR 12 / x < 5",
                vec![vec![int(small)]],
            ),
            (
                "SELECT id, x <> 0 AND 12 / x > 3 FROM t WHERE id < 30 ORDER BY id",
                (0..30)
                    .map(|i| vec![int(i), big(x(i)).map_or(Value::Null, Value::Bool)])
                    .collect(),
            ),
            (
                "SELECT x <> 0 AND 12 / x > 3 AS b, COUNT(*) FROM t GROUP BY x <> 0 AND 12 / x > 3 ORDER BY b",
                [None, Some(false), Some(true)]
                    .into_iter()
                    .map(|b| {
                        let n = (0..ROWS).filter(|&i| big(x(i)) == b).count() as i64;
                        vec![b.map_or(Value::Null, Value::Bool), int(n)]
                    })
                    .collect(),
            ),
            (
                "SELECT SUM(CASE WHEN x <> 0 AND 12 / x > 3 THEN 1 ELSE 0 END) FROM t",
                vec![vec![int(ids.len() as i64)]],
            ),
        ];
        for model in Model::ALL {
            db.set_execution_model(Some(model));
            for (sql, want) in &queries {
                assert_eq!(&query(&mut db, sql), want, "{storage}, {model}: {sql}");
            }
            // Rows the left side leaves open (x = 0 with x >= 0, or NULL) still fail.
            for sql in [
                "SELECT id FROM t WHERE x >= 0 AND 12 / x > 3",
                "SELECT x IS NULL OR 12 / x > 0 FROM t",
            ] {
                let e = db.execute(sql).unwrap_err().to_string();
                assert!(
                    e.contains("division by zero"),
                    "{storage}, {model}: {sql}: {e}"
                );
            }
        }
        db.set_execution_model(None);
        // Writes evaluate alike.
        let done = db
            .execute("UPDATE t SET x = 100 WHERE x <> 0 AND 12 / x > 3")
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            done,
            QueryResult::Message(format!("{} row(s) updated", ids.len())),
            "{storage}"
        );
        // x = 0, or 100 since the update, where 12 / x is 0.
        let gone = (0..ROWS)
            .filter(|&i| x(i) == Some(0) || big(x(i)) == Some(true))
            .count();
        assert_eq!(
            returned(
                &mut db,
                "DELETE FROM t WHERE x = 0 OR 12 / x = 0 RETURNING id"
            )
            .1
            .len(),
            gone,
            "{storage}"
        );
    }
}
