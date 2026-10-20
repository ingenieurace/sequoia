use sequoia::{Database, QueryResult, Value};

fn setup() -> Database {
    let mut db = Database::in_memory();
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
    let rows = query(&mut db, "SELECT name, age + 1 FROM users WHERE age >= 25 ORDER BY age DESC LIMIT 1");
    assert_eq!(rows, vec![vec![text("alice"), int(31)]]);
}

#[test]
fn null_handling() {
    let mut db = setup();
    assert_eq!(query(&mut db, "SELECT id FROM users WHERE age IS NULL"), vec![vec![int(3)]]);
    // Comparisons with NULL are unknown, so carol is excluded either way.
    assert_eq!(query(&mut db, "SELECT id FROM users WHERE age > 0 OR age <= 0").len(), 2);
    // NULL sorts first.
    assert_eq!(query(&mut db, "SELECT id FROM users ORDER BY age")[0], vec![int(3)]);
    assert_eq!(query(&mut db, "SELECT id FROM users WHERE id = 1 OR id = 2").len(), 2);
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
        assert_eq!(query(&mut db, &sql), [vec![and.clone(), or.clone()]], "{a}, {b}");
    }
}

#[test]
fn update_and_delete() {
    let mut db = setup();
    db.execute("UPDATE users SET age = age * 2, active = NOT active WHERE name = 'bob'").unwrap();
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
    assert!(db.execute("INSERT INTO users VALUES (1, 'dup', 1, TRUE)").is_err());
    assert!(db.execute("INSERT INTO users (name) VALUES ('no id')").is_err());
    assert!(db.execute("INSERT INTO users VALUES (9, 42, 1, TRUE)").is_err());
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
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)").unwrap();
        db.execute("INSERT INTO notes VALUES (1, 'it''s here'), (2, NULL)").unwrap();
    }
    let mut db = Database::open(&path).unwrap();
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
        let path = std::env::temp_dir().join(format!("sequoia-sql-{name}-{}.db", std::process::id()));
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
        let mut db = Database::open(&path.0).unwrap();
        db.execute("CREATE TABLE logs (id INT PRIMARY KEY, msg TEXT)").unwrap();
        for i in 0..500 {
            db.execute(&format!("INSERT INTO logs VALUES ({i}, '{}')", "x".repeat(100))).unwrap();
        }
        db.execute("DELETE FROM logs WHERE id >= 400").unwrap();
        db.execute("UPDATE logs SET msg = 'short' WHERE id < 10").unwrap();
    }
    let mut db = Database::open(&path.0).unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM logs").len(), 400);
    assert_eq!(query(&mut db, "SELECT msg FROM logs WHERE id = 5"), vec![vec![text("short")]]);
    // Inserts and key checks still work after reopening.
    db.execute("INSERT INTO logs VALUES (1000, 'new')").unwrap();
    assert!(db.execute("INSERT INTO logs VALUES (1, 'dup')").is_err());
}

#[test]
fn dropped_tables_free_their_pages() {
    let path = TempDb::new("drop");
    let mut db = Database::open(&path.0).unwrap();
    let fill = |db: &mut Database| {
        db.execute("CREATE TABLE t (body TEXT)").unwrap();
        for _ in 0..100 {
            db.execute(&format!("INSERT INTO t VALUES ('{}')", "y".repeat(200))).unwrap();
        }
    };
    fill(&mut db);
    let size = file_size(&mut db, &path.0);
    db.execute("DROP TABLE t").unwrap();
    fill(&mut db);
    assert_eq!(file_size(&mut db, &path.0), size, "freed pages are reused");

    drop(db);
    let mut db = Database::open(&path.0).unwrap();
    assert_eq!(db.table_names(), vec!["t"]);
    assert_eq!(query(&mut db, "SELECT * FROM t").len(), 100);
}

#[test]
fn rows_larger_than_a_page_are_rejected() {
    let mut db = setup();
    let huge = "z".repeat(5000);
    assert!(db.execute(&format!("INSERT INTO users (id, name) VALUES (9, '{huge}')")).is_err());
    assert_eq!(query(&mut db, "SELECT id FROM users").len(), 3);
}

#[test]
fn deleted_space_is_reused_after_reopening() {
    let path = TempDb::new("reuse");
    let size = {
        let mut db = Database::open(&path.0).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, body TEXT)").unwrap();
        for i in 0..200 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, '{}')", "x".repeat(100))).unwrap();
        }
        db.execute("DELETE FROM t WHERE id < 100").unwrap();
        file_size(&mut db, &path.0)
    };

    let mut db = Database::open(&path.0).unwrap();
    for i in 0..100 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, '{}')", "y".repeat(100))).unwrap();
    }
    assert_eq!(file_size(&mut db, &path.0), size, "the file did not grow");
    assert_eq!(query(&mut db, "SELECT id FROM t").len(), 200);
}

#[test]
fn updates_that_grow_rows_keep_every_row() {
    let path = TempDb::new("grow");
    let mut db = Database::open(&path.0).unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, body TEXT)").unwrap();
    for i in 0..100 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'x')")).unwrap();
    }
    let size = file_size(&mut db, &path.0);
    // Shrinking or same-size updates stay in place.
    db.execute("UPDATE t SET body = 'y'").unwrap();
    assert_eq!(file_size(&mut db, &path.0), size);

    // Rows too big for their page move elsewhere.
    db.execute(&format!("UPDATE t SET body = '{}' WHERE id < 50", "z".repeat(500))).unwrap();
    drop(db);
    let mut db = Database::open(&path.0).unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM t").len(), 100);
    assert_eq!(query(&mut db, "SELECT id FROM t WHERE body = 'y'").len(), 50);
    assert_eq!(query(&mut db, &format!("SELECT id FROM t WHERE body = '{}'", "z".repeat(500))).len(), 50);
}

fn setup_lsm() -> Database {
    let mut db = Database::in_memory();
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
    assert_eq!(query(&mut db, "SELECT name FROM users WHERE age IS NULL"), vec![vec![text("carol")]]);

    db.execute("UPDATE users SET age = age * 2, active = NOT active WHERE name = 'bob'").unwrap();
    assert_eq!(
        query(&mut db, "SELECT age, active FROM users WHERE id = 2"),
        vec![vec![int(50), Value::Bool(true)]]
    );

    // Changing the primary key moves the row to its new key.
    db.execute("UPDATE users SET id = id + 10 WHERE id >= 2").unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM users"), vec![vec![int(1)], vec![int(12)], vec![int(13)]]);

    db.execute("DELETE FROM users WHERE id != 12").unwrap();
    assert_eq!(query(&mut db, "SELECT name FROM users"), vec![vec![text("bob")]]);
    db.execute("INSERT INTO users (id, name) VALUES (1, 'alice again')").unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM users").len(), 2);
}

#[test]
fn lsm_tables_enforce_constraints() {
    let mut db = setup_lsm();
    assert!(db.execute("INSERT INTO users VALUES (1, 'dup', 1, TRUE)").is_err());
    assert!(db.execute("INSERT INTO users VALUES (9, 'x', 1, TRUE), (9, 'y', 1, TRUE)").is_err());
    assert!(db.execute("INSERT INTO users (name) VALUES ('no id')").is_err());
    assert!(db.execute("UPDATE users SET id = 1").is_err());
    assert!(db.execute(&format!("INSERT INTO users (id, name) VALUES (9, '{}')", "z".repeat(5000))).is_err());
    assert_eq!(query(&mut db, "SELECT id FROM users").len(), 3, "failed statements change nothing");

    // Swapping two keys in one statement is fine.
    db.execute("UPDATE users SET id = 3 - id WHERE id <= 2").unwrap();
    assert_eq!(query(&mut db, "SELECT name FROM users WHERE id = 1"), vec![vec![text("bob")]]);
    assert_eq!(query(&mut db, "SELECT name FROM users WHERE id = 2"), vec![vec![text("alice")]]);
}

#[test]
fn storage_clause_is_parsed_and_checked() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE a (x INT) using lsm").unwrap();
    db.execute("CREATE TABLE b (x INT) USING heap").unwrap();
    db.execute("CREATE TABLE d (x INT) USING Column").unwrap();
    assert!(db.execute("CREATE TABLE c (x INT) USING btree").is_err());
    // HEAP, LSM and COLUMN are not reserved words.
    db.execute("CREATE TABLE lsm (heap INT, column INT)").unwrap();
}

#[test]
fn lsm_tables_survive_flushes_compactions_and_reopening() {
    let path = TempDb::new("lsm");
    let row = |i: i64, tag: &str| vec![int(i), text(&format!("{tag}{}", "x".repeat(100)))];
    {
        let mut db = Database::open(&path.0).unwrap();
        db.execute("CREATE TABLE logs (id INT PRIMARY KEY, msg TEXT) USING LSM").unwrap();
        // About 1 MB of writes: many MemTable flushes and several compactions.
        for batch in 0..30 {
            let values: Vec<String> =
                (0..250).map(|i| format!("({}, 'v{batch}{}')", batch * 250 + i, "x".repeat(100))).collect();
            db.execute(&format!("INSERT INTO logs VALUES {}", values.join(", "))).unwrap();
        }
        db.execute(&format!("UPDATE logs SET msg = 'new{}' WHERE id < 1000", "x".repeat(100))).unwrap();
        db.execute("DELETE FROM logs WHERE id >= 7000").unwrap();
    }

    let mut db = Database::open(&path.0).unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM logs").len(), 7000);
    assert_eq!(query(&mut db, "SELECT * FROM logs WHERE id = 5"), vec![row(5, "new")]);
    assert_eq!(query(&mut db, "SELECT * FROM logs WHERE id = 6999"), vec![row(6999, "v27")]);
    assert!(query(&mut db, "SELECT id FROM logs WHERE id = 7000").is_empty());
    assert!(db.execute("INSERT INTO logs VALUES (6999, 'dup')").is_err());
    db.execute("INSERT INTO logs VALUES (7000, 'back')").unwrap();
}

#[test]
fn lsm_tables_without_a_primary_key_use_rowids() {
    let path = TempDb::new("rowid");
    {
        let mut db = Database::open(&path.0).unwrap();
        db.execute("CREATE TABLE events (name TEXT) USING LSM").unwrap();
        db.execute("INSERT INTO events VALUES ('a'), ('a'), ('b')").unwrap();
        db.execute("UPDATE events SET name = 'c' WHERE name = 'b'").unwrap();
    }
    let mut db = Database::open(&path.0).unwrap();
    // New rows after reopening must not overwrite old ones.
    db.execute("INSERT INTO events VALUES ('d')").unwrap();
    assert_eq!(
        query(&mut db, "SELECT name FROM events"),
        vec![vec![text("a")], vec![text("a")], vec![text("c")], vec![text("d")]]
    );
}

#[test]
fn dropped_lsm_tables_free_their_pages() {
    let path = TempDb::new("lsm-drop");
    let mut db = Database::open(&path.0).unwrap();
    let fill = |db: &mut Database| {
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, body TEXT) USING LSM").unwrap();
        for i in 0..1000 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, '{}')", "y".repeat(200))).unwrap();
        }
    };
    fill(&mut db);
    let size = file_size(&mut db, &path.0);
    db.execute("DROP TABLE t").unwrap();
    fill(&mut db);
    assert_eq!(file_size(&mut db, &path.0), size, "freed pages are reused");
}

fn setup_column() -> Database {
    let mut db = Database::in_memory();
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
    assert_eq!(query(&mut db, "SELECT name FROM users WHERE age IS NULL"), vec![vec![text("carol")]]);

    db.execute("UPDATE users SET age = age * 2, active = NOT active WHERE name = 'bob'").unwrap();
    assert_eq!(
        query(&mut db, "SELECT age, active FROM users WHERE id = 2"),
        vec![vec![int(50), Value::Bool(true)]]
    );
    db.execute("UPDATE users SET id = id + 10 WHERE id >= 2").unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM users ORDER BY id"), vec![vec![int(1)], vec![int(12)], vec![int(13)]]);

    db.execute("DELETE FROM users WHERE id != 12").unwrap();
    assert_eq!(query(&mut db, "SELECT * FROM users"), vec![vec![int(12), text("bob"), int(50), Value::Bool(true)]]);
}

#[test]
fn column_tables_enforce_constraints() {
    let mut db = setup_column();
    assert!(db.execute("INSERT INTO users VALUES (1, 'dup', 1, TRUE)").is_err());
    assert!(db.execute("INSERT INTO users VALUES (9, 'x', 1, TRUE), (9, 'y', 1, TRUE)").is_err());
    assert!(db.execute("INSERT INTO users (name) VALUES ('no id')").is_err());
    assert!(db.execute("UPDATE users SET id = 1").is_err());
    assert!(db.execute("INSERT INTO users VALUES ('one', 'x', 1, TRUE)").is_err());
    assert!(db.execute(&format!("INSERT INTO users (id, name) VALUES (9, '{}')", "z".repeat(5000))).is_err());
    assert_eq!(query(&mut db, "SELECT id FROM users").len(), 3, "failed statements change nothing");

    db.execute("UPDATE users SET id = 3 - id WHERE id <= 2").unwrap();
    assert_eq!(query(&mut db, "SELECT name FROM users WHERE id = 1"), vec![vec![text("bob")]]);
}

#[test]
fn column_tables_survive_migrations_and_reopening() {
    let path = TempDb::new("column");
    let cities = ["lima", "oslo", "paris", "tokyo"];
    {
        let mut db = Database::open(&path.0).unwrap();
        db.execute("CREATE TABLE visits (id INT PRIMARY KEY, city TEXT, nights INT, paid BOOL) USING COLUMN").unwrap();
        // Several row groups of 1024 rows, plus rows left in the delta store.
        for batch in 0..5 {
            let values: Vec<String> = (0..1000)
                .map(|i| {
                    let id = batch * 1000 + i;
                    let nights = if id % 7 == 0 { "NULL".to_string() } else { (id % 10).to_string() };
                    format!("({id}, '{}', {nights}, {})", cities[id as usize % 4], id % 2 == 0)
                })
                .collect();
            db.execute(&format!("INSERT INTO visits VALUES {}", values.join(", "))).unwrap();
        }
        // Touches rows in row groups and in the delta store.
        db.execute("UPDATE visits SET city = 'rome' WHERE id / 100 * 100 = id").unwrap();
        db.execute("DELETE FROM visits WHERE city = 'oslo'").unwrap();
    }

    let mut db = Database::open(&path.0).unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM visits").len(), 3750);
    assert_eq!(query(&mut db, "SELECT city FROM visits WHERE city = 'rome'").len(), 50);
    assert!(query(&mut db, "SELECT id FROM visits WHERE city = 'oslo'").is_empty());
    assert_eq!(query(&mut db, "SELECT id FROM visits WHERE nights IS NULL").len(), 537);
    assert_eq!(
        query(&mut db, "SELECT * FROM visits WHERE id = 4994"),
        vec![vec![int(4994), text("paris"), int(4), Value::Bool(true)]]
    );
    // Selecting only some columns still filters and sorts on others.
    assert_eq!(
        query(&mut db, "SELECT id FROM visits WHERE paid AND nights = 9 ORDER BY city DESC, id LIMIT 2"),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        query(&mut db, "SELECT city FROM visits WHERE nights = 8 ORDER BY id DESC LIMIT 1"),
        vec![vec![text("lima")]]
    );
    assert!(db.execute("INSERT INTO visits VALUES (6, 'dup', 1, TRUE)").is_err());
    db.execute("INSERT INTO visits VALUES (5, 'back', 1, TRUE)").unwrap(); // its old row was deleted
}

#[test]
fn dropped_column_tables_free_their_pages() {
    let path = TempDb::new("column-drop");
    let mut db = Database::open(&path.0).unwrap();
    let fill = |db: &mut Database| {
        db.execute("CREATE TABLE t (id INT, body TEXT) USING COLUMN").unwrap();
        for batch in 0..3 {
            let values: Vec<String> =
                (0..1000).map(|i| format!("({}, '{}')", batch * 1000 + i, "y".repeat(50))).collect();
            db.execute(&format!("INSERT INTO t VALUES {}", values.join(", "))).unwrap();
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
    let mut db = Database::in_memory();
    let cities = ["lima", "oslo", "paris", "tokyo"];
    for storage in ["HEAP", "COLUMN"] {
        let schema = "(id INT PRIMARY KEY, city TEXT, nights INT, paid BOOL)";
        db.execute(&format!("CREATE TABLE {storage}_t {schema} USING {storage}")).unwrap();
        // Row groups of sorted ids, runs of cities and few distinct nights, plus rows left in the delta store.
        for batch in 0..3 {
            let values: Vec<String> = (0..1000)
                .map(|i| {
                    let id = batch * 1000 + i;
                    let nights = if id % 11 == 0 { "NULL".to_string() } else { (id % 6 - 2).to_string() };
                    let paid = if id % 13 == 0 { "NULL".to_string() } else { (id % 3 == 0).to_string() };
                    format!("({id}, '{}', {nights}, {paid})", cities[id as usize / 300 % 4])
                })
                .collect();
            db.execute(&format!("INSERT INTO {storage}_t VALUES {}", values.join(", "))).unwrap();
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
        same(&mut db, &format!("SELECT id, nights FROM {{t}} WHERE {filter}"));
    }
    assert_eq!(same(&mut db, "SELECT id FROM {t} WHERE city = 'oslo' AND nights = 3"), 136);
    // Type errors are still reported.
    assert!(db.execute("SELECT id FROM COLUMN_t WHERE city = 1").is_err());

    let statements =
        ["UPDATE {t} SET nights = 9 WHERE city = 'paris' AND id < 1500", "DELETE FROM {t} WHERE nights = 0"];
    for statement in statements {
        for table in ["HEAP_t", "COLUMN_t"] {
            db.execute(&statement.replace("{t}", table)).unwrap();
        }
        same(&mut db, "SELECT * FROM {t}");
    }
    same(&mut db, "SELECT * FROM {t} WHERE nights = 9 AND city > 'lima'");
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
    assert_eq!(plan(&mut db, "SELECT * FROM users WHERE id = 2"), "SEARCH users USING INDEX users_pkey (id = 2)");
    assert_eq!(plan(&mut db, "SELECT * FROM users WHERE name = 'bob'"), "SCAN users");
    assert_eq!(plan(&mut db, "SELECT * FROM users WHERE id != 2 OR id = 3"), "SCAN users", "no conjunct to use");

    db.execute("CREATE INDEX users_name ON users (name)").unwrap();
    db.execute("CREATE INDEX users_age ON users (age)").unwrap();
    assert_eq!(
        plan(&mut db, "UPDATE users SET age = 1 WHERE 'bob' = name"),
        "SEARCH users USING INDEX users_name (name = 'bob')"
    );
    assert_eq!(
        plan(&mut db, "DELETE FROM users WHERE age > 20 AND age <= 30 AND age >= 21 AND age < 99"),
        "SEARCH users USING INDEX users_age (age >= 21 AND age <= 30)",
        "the tightest bounds"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM users WHERE age > 20 AND name = 'bob'"),
        "SEARCH users USING INDEX users_name (name = 'bob')",
        "an equality beats a range"
    );
    assert_eq!(plan(&mut db, "SELECT * FROM users WHERE age = 'x'"), "SCAN users", "a literal of the wrong type");

    assert_eq!(query(&mut db, "SELECT id FROM users WHERE name = 'bob'"), vec![vec![int(2)]]);
    // Rows come back in index order, and NULL ages are not in the index.
    assert_eq!(
        query(&mut db, "SELECT name FROM users WHERE age < 100"),
        vec![vec![text("bob")], vec![text("alice")]]
    );
    assert!(query(&mut db, "SELECT id FROM users WHERE age > 30").is_empty());
    assert!(db.execute("EXPLAIN INSERT INTO users VALUES (4, 'x', 1, TRUE)").is_err());
}

#[test]
fn indexed_tables_answer_like_unindexed_ones() {
    let mut db = Database::in_memory();
    for storage in ["HEAP", "LSM"] {
        for indexed in [false, true] {
            let t = format!("{storage}_{indexed}");
            let schema = "(id INT PRIMARY KEY, city TEXT, nights INT)";
            db.execute(&format!("CREATE TABLE {t} {schema} USING {storage}")).unwrap();
            if indexed {
                db.execute(&format!("CREATE INDEX {t}_city ON {t} (city)")).unwrap();
                db.execute(&format!("CREATE INDEX {t}_nights ON {t} (nights)")).unwrap();
            }
            let values: Vec<String> = (0..2000i64)
                .map(|id| {
                    let nights = if id % 11 == 0 { "NULL".to_string() } else { (id % 7 - 3).to_string() };
                    format!("({id}, '{}', {nights})", ["lima", "oslo", "paris", "tokyo", "a\0b"][id as usize % 5])
                })
                .collect();
            db.execute(&format!("INSERT INTO {t} VALUES {}", values.join(", "))).unwrap();
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
        &format!("UPDATE {{t}} SET city = '{}' WHERE id / 3 * 3 = id", "z".repeat(300)),
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
    assert!(plan(&mut db, "SELECT * FROM HEAP_true WHERE city = 'rome'").contains("USING INDEX HEAP_true_city"));
    assert!(plan(&mut db, "SELECT * FROM LSM_true WHERE nights < 0").contains("USING INDEX LSM_true_nights"));
    assert_eq!(plan(&mut db, "SELECT * FROM LSM_true WHERE id = 7"), "SEARCH LSM_true USING PRIMARY KEY (id = 7)");
    assert_eq!(plan(&mut db, "SELECT * FROM LSM_false WHERE city = 'oslo'"), "SCAN LSM_false");
    assert_eq!(
        plan(&mut db, "SELECT * FROM LSM_false WHERE id >= 1990 AND city = 'paris'"),
        "SEARCH LSM_false USING PRIMARY KEY (id >= 1990)"
    );
}

#[test]
fn lsm_primary_key_ranges_read_only_their_keys() {
    let mut db = Database::in_memory();
    for (t, key) in [("ints", "INT"), ("texts", "TEXT")] {
        db.execute(&format!("CREATE TABLE {t} (k {key} PRIMARY KEY, v INT) USING LSM")).unwrap();
        // Enough rows to fill SSTables, then deletes and new rows still in the MemTable.
        let values: Vec<String> = (0..3000).map(|i| format!("({}, {i})", key_of(t, i))).collect();
        db.execute(&format!("INSERT INTO {t} VALUES {}", values.join(", "))).unwrap();
        db.execute(&format!("DELETE FROM {t} WHERE v >= 1000 AND v < 1010")).unwrap();
        db.execute(&format!("UPDATE {t} SET v = v + 10000 WHERE v >= 2000 AND v < 2005")).unwrap();
    }
    let keys = |db: &mut Database, sql: &str| -> Vec<i64> {
        query(db, sql).into_iter().map(|row| match row[1] { Value::Int(v) => v % 10000, _ => unreachable!() }).collect()
    };
    for t in ["ints", "texts"] {
        let k = |i| key_of(t, i);
        let range = |db: &mut Database, condition: String| {
            let sql = format!("SELECT * FROM {t} WHERE {condition}");
            assert!(plan(db, &sql).contains("USING PRIMARY KEY"), "{sql}");
            keys(db, &sql)
        };
        assert_eq!(range(&mut db, format!("k >= {} AND k < {}", k(5), k(9))), [5, 6, 7, 8]);
        assert_eq!(range(&mut db, format!("k > {} AND k <= {}", k(5), k(9))), [6, 7, 8, 9]);
        assert_eq!(range(&mut db, format!("k > {} AND k < {}", k(5), k(6))), Vec::<i64>::new());
        assert_eq!(range(&mut db, format!("k >= {} AND k < {}", k(9), k(5))), Vec::<i64>::new());
        assert_eq!(range(&mut db, format!("k >= {} AND k <= {}", k(998), k(1011))), [998, 999, 1010, 1011]);
        assert_eq!(range(&mut db, format!("k >= {} AND k < {}", k(1998), k(2007))), (1998..2007).collect::<Vec<_>>());
        assert_eq!(range(&mut db, format!("k > {}", k(2996))), [2997, 2998, 2999]);
        assert_eq!(range(&mut db, format!("k < {}", k(3))), [0, 1, 2]);
        // Other conditions are still checked on the rows read.
        assert_eq!(range(&mut db, format!("k >= {} AND k < {} AND v != 7", k(5), k(9))), [5, 6, 8]);
    }
}

/// The key of row `i` of table `t`: a number, or text that sorts the same way.
fn key_of(t: &str, i: i64) -> String {
    if t == "ints" { i.to_string() } else { format!("'key {i:05}'") }
}

#[test]
fn unique_indexes_reject_duplicates() {
    let mut db = setup();
    db.execute("CREATE UNIQUE INDEX users_name ON users (name)").unwrap();
    assert!(db.execute("INSERT INTO users VALUES (4, 'bob', 1, TRUE)").is_err());
    assert!(db.execute("INSERT INTO users VALUES (4, 'dave', 1, TRUE), (5, 'dave', 1, TRUE)").is_err());
    assert!(db.execute("UPDATE users SET name = 'alice' WHERE id = 2").is_err());
    // NULLs do not clash, and rows may swap values in one statement.
    db.execute("INSERT INTO users (id) VALUES (4), (5)").unwrap();
    db.execute("UPDATE users SET id = id + 1").unwrap();
    db.execute("UPDATE users SET id = 3 - id WHERE id < 3").unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM users WHERE name = 'alice'"), vec![vec![int(1)]]);
    assert_eq!(query(&mut db, "SELECT name FROM users WHERE id = 3"), vec![vec![text("bob")]]);
    assert_eq!(query(&mut db, "SELECT id FROM users WHERE id >= 1 ORDER BY id").len(), 5);

    // A unique index cannot be built over duplicates, and none is left behind.
    db.execute("INSERT INTO users VALUES (9, 'zed', 30, TRUE)").unwrap();
    assert!(db.execute("CREATE UNIQUE INDEX users_age ON users (age)").is_err());
    assert_eq!(plan(&mut db, "SELECT * FROM users WHERE age = 30"), "SCAN users");
    db.execute("CREATE INDEX users_age ON users (age)").unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM users WHERE age = 30").len(), 2);
}

#[test]
fn index_statements_are_checked() {
    let mut db = setup();
    assert!(db.execute("CREATE INDEX i ON missing (id)").is_err());
    assert!(db.execute("CREATE INDEX i ON users (missing)").is_err());
    assert!(db.execute("CREATE INDEX users_pkey ON users (name)").is_err(), "name taken");
    assert!(db.execute("DROP INDEX users_pkey").is_err(), "the primary key needs it");
    assert!(db.execute("DROP INDEX missing").is_err());
    assert!(db.execute("CREATE INDEX i ON users").is_err());

    // Values too long for an index entry are rejected, leaving the table unchanged.
    db.execute("CREATE INDEX users_name ON users (name)").unwrap();
    let long = "n".repeat(2000);
    assert!(db.execute(&format!("INSERT INTO users (id, name) VALUES (9, '{long}')")).is_err());
    assert!(db.execute(&format!("UPDATE users SET name = '{long}' WHERE id = 1")).is_err());
    assert_eq!(query(&mut db, "SELECT name FROM users WHERE id = 1"), vec![vec![text("alice")]]);
    assert_eq!(query(&mut db, "SELECT id FROM users").len(), 3);

    db.execute("DROP INDEX users_name").unwrap();
    db.execute(&format!("INSERT INTO users (id, name) VALUES (9, '{long}')")).unwrap();
    assert!(db.execute("CREATE INDEX users_name ON users (name)").is_err());

    db.execute("CREATE TABLE c (id INT PRIMARY KEY, x INT) USING COLUMN").unwrap();
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
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, tag TEXT, n INT)").unwrap();
        db.execute("CREATE INDEX t_tag ON t (tag)").unwrap();
        for batch in 0..10 {
            let values: Vec<String> =
                (0..200).map(|i| batch * 200 + i).map(|id| format!("({id}, 'tag{}', {id})", id % 50)).collect();
            db.execute(&format!("INSERT INTO t VALUES {}", values.join(", "))).unwrap();
        }
    };
    {
        let mut db = Database::open(&path.0).unwrap();
        fill(&mut db);
        db.execute("CREATE INDEX t_n ON t (n)").unwrap();
        db.execute("DELETE FROM t WHERE n >= 1000").unwrap();
    }
    let mut db = Database::open(&path.0).unwrap();
    assert_eq!(plan(&mut db, "SELECT * FROM t WHERE tag = 'tag7'"), "SEARCH t USING INDEX t_tag (tag = 'tag7')");
    assert_eq!(query(&mut db, "SELECT id FROM t WHERE tag = 'tag7'").len(), 20);
    assert_eq!(query(&mut db, "SELECT id FROM t WHERE n >= 990").len(), 10);
    assert!(db.execute("INSERT INTO t VALUES (5, 'dup', 0)").is_err(), "the primary key index was reopened");
    db.execute("INSERT INTO t VALUES (5000, 'tag7', 5000)").unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM t WHERE tag = 'tag7'").len(), 21);

    db.execute("DROP INDEX t_n").unwrap();
    assert_eq!(plan(&mut db, "SELECT * FROM t WHERE n = 3"), "SCAN t");
    db.execute("DROP TABLE t").unwrap();
    let size = file_size(&mut db, &path.0);
    fill(&mut db);
    assert_eq!(file_size(&mut db, &path.0), size, "the indexes' pages were freed and reused");
    drop(db);

    let mut db = Database::open(&path.0).unwrap();
    assert_eq!(query(&mut db, "SELECT id FROM t WHERE tag = 'tag1'").len(), 40);
}

#[test]
fn order_by_gives_the_same_rows_when_sorts_spill_to_disk() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, k INT, name TEXT)").unwrap();
    let values: Vec<String> = (0..3000i64)
        .map(|id| {
            let k = if id % 13 == 0 { "NULL".to_string() } else { (id * 7 % 10).to_string() };
            format!("({id}, {k}, '{}')", ["ann", "bo", "cy", ""][id as usize % 4].repeat(id as usize % 5))
        })
        .collect();
    db.execute(&format!("INSERT INTO t VALUES {}", values.join(", "))).unwrap();

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
    assert_eq!(query(&mut db, "SELECT k, id FROM t ORDER BY k DESC LIMIT 3"), expected[..3]);
}

#[test]
fn limit_without_order_by_stops_reading_rows() {
    let mut db = setup();
    assert_eq!(query(&mut db, "SELECT id FROM users LIMIT 2"), vec![vec![int(1)], vec![int(2)]]);
    assert!(query(&mut db, "SELECT id FROM users LIMIT 0").is_empty());
    // The third row would divide by zero, but two rows are found before it.
    let sql = "SELECT id FROM users WHERE 10 / (id - 3) < 0";
    assert!(db.execute(sql).is_err());
    assert_eq!(query(&mut db, &format!("{sql} LIMIT 2")), vec![vec![int(1)], vec![int(2)]]);
    // With an order, every row is read, so the error is still reported.
    assert!(db.execute(&format!("{sql} ORDER BY id LIMIT 2")).is_err());
}

fn setup_people(storage: &str) -> Database {
    let mut db = Database::in_memory();
    db.execute(&format!("CREATE TABLE p (id INT PRIMARY KEY, city TEXT, age INT, vip BOOL) USING {storage}")).unwrap();
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
        query(&mut db, "SELECT COUNT(*), COUNT(age), COUNT(city), SUM(age), MIN(age), MAX(age), AVG(age) FROM p"),
        vec![vec![int(7), int(5), int(5), int(161), int(21), int(45), int(32)]]
    );
    // Over no rows there is still one row: COUNT is 0, the others NULL.
    assert_eq!(
        query(&mut db, "SELECT COUNT(*), SUM(age), MIN(city), AVG(age) FROM p WHERE id > 100"),
        vec![vec![int(0), Value::Null, Value::Null, Value::Null]]
    );
    assert_eq!(query(&mut db, "SELECT MIN(city), MAX(city), MAX(vip) FROM p"), vec![vec![
        text("lima"),
        text("oslo"),
        Value::Bool(true)
    ]]);
    // Expressions over aggregates, and the same aggregate twice.
    assert_eq!(
        query(&mut db, "SELECT SUM(age) - MIN(age) * 2, COUNT(*) + COUNT(*) FROM p WHERE vip"),
        vec![vec![int(115 - 60), int(6)]]
    );
    // AVG truncates toward zero, as / does: there is no fractional type.
    db.execute("CREATE TABLE n (x INT); INSERT INTO n VALUES (-3), (-4), (NULL)").unwrap();
    assert_eq!(query(&mut db, "SELECT AVG(x), SUM(x) / COUNT(x) FROM n"), vec![vec![int(-3), int(-3)]]);
}

#[test]
fn group_by_with_having_order_by_and_limit() {
    let mut db = setup_people("HEAP");
    // NULL keys form one group; groups come out in the order they were first seen.
    assert_eq!(
        query(&mut db, "SELECT city, COUNT(*), MAX(age) FROM p GROUP BY city"),
        vec![
            vec![text("oslo"), int(3), int(45)],
            vec![text("lima"), int(2), int(25)],
            vec![Value::Null, int(2), int(40)],
        ]
    );
    assert_eq!(
        query(&mut db, "SELECT city FROM p GROUP BY city HAVING MIN(age) < 25 OR COUNT(age) = 0 ORDER BY city"),
        vec![vec![text("oslo")]]
    );
    assert_eq!(
        query(&mut db, "SELECT city, SUM(age) FROM p WHERE age > 0 GROUP BY city ORDER BY SUM(age) DESC LIMIT 2"),
        vec![vec![text("oslo"), int(96)], vec![Value::Null, int(40)]]
    );
    // Grouping by an expression, and using it in the select list and ORDER BY.
    assert_eq!(
        query(&mut db, "SELECT age / 10 * 10, COUNT(*) FROM p WHERE age > 0 GROUP BY age / 10 ORDER BY age / 10"),
        vec![vec![int(20), int(2)], vec![int(30), int(1)], vec![int(40), int(2)]]
    );
    assert_eq!(
        query(&mut db, "SELECT city, vip, COUNT(*) FROM p GROUP BY city, vip HAVING COUNT(*) > 1"),
        vec![vec![text("oslo"), Value::Bool(true), int(2)]]
    );
    // A repeated GROUP BY expression changes nothing.
    assert_eq!(
        query(&mut db, "SELECT city, COUNT(*) FROM p GROUP BY city, city ORDER BY city"),
        vec![vec![Value::Null, int(2)], vec![text("lima"), int(2)], vec![text("oslo"), int(3)]]
    );
    // HAVING without GROUP BY makes one group, which it may filter out.
    assert_eq!(query(&mut db, "SELECT COUNT(*) FROM p HAVING COUNT(*) > 100"), Vec::<Vec<Value>>::new());
    assert!(query(&mut db, "SELECT city FROM p WHERE id > 100 GROUP BY city").is_empty(), "no rows, no groups");
    match db.execute("SELECT city, COUNT(*), SUM(age) * 2 FROM p GROUP BY city").unwrap().pop() {
        Some(QueryResult::Rows { columns, .. }) => assert_eq!(columns, ["city", "count", "?column?"]),
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
            db.execute("CREATE INDEX p_city ON p (city); CREATE INDEX p_age ON p (age)").unwrap();
        }
        results.push(queries.iter().map(|sql| query(&mut db, sql)).collect::<Vec<_>>());
    }
    assert_eq!(results[0], results[1], "heap and LSM");
    assert_eq!(results[0], results[2], "heap and column");
}

#[test]
fn misplaced_aggregates_and_ungrouped_columns_are_rejected() {
    let mut db = setup_people("HEAP");
    let errors = [
        ("SELECT * FROM p GROUP BY city", "SELECT *"),
        ("SELECT age FROM p GROUP BY city", "column age must appear in GROUP BY"),
        ("SELECT COUNT(*), age FROM p", "column age must appear in GROUP BY"),
        ("SELECT city FROM p GROUP BY city ORDER BY age", "column age must appear in GROUP BY"),
        ("SELECT COUNT(SUM(age)) FROM p", "cannot be nested"),
        ("SELECT city FROM p WHERE COUNT(*) > 1", "not allowed in WHERE"),
        ("SELECT COUNT(*) FROM p GROUP BY COUNT(*)", "not allowed in GROUP BY"),
        ("UPDATE p SET age = MAX(age)", "not allowed in UPDATE"),
        ("DELETE FROM p WHERE age = MIN(age)", "not allowed in WHERE"),
        ("INSERT INTO p (id) VALUES (COUNT(*))", "not allowed in VALUES"),
        ("SELECT SUM(city) FROM p", "SUM expects INT"),
        ("SELECT AVG(vip) FROM p", "AVG expects INT"),
        ("SELECT MEDIAN(age) FROM p", "unknown function: MEDIAN"),
        ("SELECT COUNT(*) FROM p GROUP BY nope", "unknown column: nope"),
        ("SELECT nope FROM p GROUP BY city", "unknown column: nope"),
    ];
    for (sql, expected) in errors {
        let e = db.execute(sql).expect_err(sql);
        assert!(e.0.contains(expected), "{sql}: {e}");
    }
    db.execute(&format!("INSERT INTO p VALUES (8, 'x', {}, TRUE), (9, 'x', 1, TRUE)", i64::MAX)).unwrap();
    assert!(db.execute("SELECT SUM(age) FROM p WHERE city = 'x'").unwrap_err().0.contains("overflow"));
    assert_eq!(query(&mut db, "SELECT AVG(age) FROM p WHERE city = 'x'"), vec![vec![int(i64::MAX / 2 + 1)]]);
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
    assert_eq!(explain(&mut db, "SELECT COUNT(*) FROM p"), "PROJECT COUNT(*)\n  AGGREGATE COUNT(*)\n    SCAN p");
    assert_eq!(explain(&mut db, "UPDATE p SET age = 1 WHERE age IS NULL"), "FILTER age IS NULL\n  SCAN p");
    assert_eq!(explain(&mut db, "DELETE FROM p"), "SCAN p");
}

#[test]
fn select_distinct_drops_repeated_rows() {
    let mut db = setup_people("HEAP");
    let column = |rows: Vec<Vec<Value>>| rows.into_iter().map(|mut r| r.remove(0)).collect::<Vec<_>>();
    // NULLs are equal to each other here; rows come out in the order first seen.
    assert_eq!(column(query(&mut db, "SELECT DISTINCT city FROM p")), [text("oslo"), text("lima"), Value::Null]);
    assert_eq!(column(query(&mut db, "SELECT DISTINCT city FROM p ORDER BY city")), [
        Value::Null,
        text("lima"),
        text("oslo")
    ]);
    // LIMIT counts distinct rows, with or without ORDER BY.
    assert_eq!(column(query(&mut db, "SELECT DISTINCT city FROM p ORDER BY city DESC LIMIT 2")), [
        text("oslo"),
        text("lima")
    ]);
    assert_eq!(column(query(&mut db, "SELECT DISTINCT city FROM p LIMIT 2")), [text("oslo"), text("lima")]);
    // Rows are distinct as a whole.
    let pairs = query(&mut db, "SELECT DISTINCT city, vip FROM p ORDER BY city, vip");
    let (t, f) = (Value::Bool(true), Value::Bool(false));
    assert_eq!(pairs, [
        vec![Value::Null, f.clone()],
        vec![Value::Null, t.clone()],
        vec![text("lima"), Value::Null],
        vec![text("lima"), f.clone()],
        vec![text("oslo"), f],
        vec![text("oslo"), t],
    ]);
    assert_eq!(query(&mut db, "SELECT DISTINCT * FROM p").len(), 7);
    assert_eq!(column(query(&mut db, "SELECT DISTINCT age / 10 FROM p WHERE age > 0 ORDER BY age / 10")), [
        int(2),
        int(3),
        int(4)
    ]);
    // After grouping: the groups' counts are 3, 2 and 2.
    assert_eq!(column(query(&mut db, "SELECT DISTINCT COUNT(*) FROM p GROUP BY city")), [int(3), int(2)]);

    let mut shop = setup_shop();
    let sql = "SELECT DISTINCT u.city FROM users u JOIN orders o ON o.user_id = u.id ORDER BY city";
    assert_eq!(column(query(&mut shop, sql)), [text("lima"), text("oslo")]);

    let e = db.execute("SELECT DISTINCT city FROM p ORDER BY age").unwrap_err();
    assert!(e.0.contains("ORDER BY age must appear in the select list"), "{e}");
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
    assert_eq!(query(&mut db, "SELECT city, COUNT(DISTINCT vip) FROM p GROUP BY city ORDER BY city"), [
        vec![Value::Null, int(2)],
        vec![text("lima"), int(1)],
        vec![text("oslo"), int(2)],
    ]);
    // In HAVING and ORDER BY too, and over no rows.
    assert_eq!(
        query(&mut db, "SELECT city FROM p GROUP BY city HAVING COUNT(DISTINCT vip) > 1 ORDER BY COUNT(DISTINCT age), city"),
        [vec![Value::Null], vec![text("oslo")]]
    );
    assert_eq!(query(&mut db, "SELECT COUNT(DISTINCT age), SUM(DISTINCT age) FROM p WHERE id > 100"), [vec![
        int(0),
        Value::Null
    ]]);
    match db.execute("SELECT COUNT(DISTINCT city) FROM p").unwrap().pop() {
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
        let mut rows = db.query("SELECT name, id * 10 FROM users WHERE id >= 2").unwrap();
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
        assert!(db.query(&format!("{sql} ORDER BY id")).unwrap().next().unwrap().is_err());
    }
    assert!(db.query("DELETE FROM users").is_err(), "only a SELECT");
    assert!(db.query("SELECT * FROM users; SELECT * FROM users").is_err(), "only one");
    assert!(db.query("SELECT nope FROM missing").is_err());
    // The same rows as execute gives.
    let sql = "SELECT * FROM users ORDER BY name DESC";
    let streamed: Vec<Vec<Value>> = db.query(sql).unwrap().map(|row| row.unwrap()).collect();
    assert_eq!(streamed, query(&mut db, sql));
}

fn setup_shop() -> Database {
    let mut db = Database::in_memory();
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
        rows.into_iter().map(|r| (r[0].clone(), r[1].clone())).collect::<Vec<_>>()
    };
    let join = "FROM users u LEFT JOIN orders o ON o.user_id = u.id";
    assert_eq!(
        pairs("SELECT u.name, o.id FROM users u JOIN orders o ON o.user_id = u.id ORDER BY o.id"),
        [(text("ann"), int(10)), (text("ann"), int(11)), (text("bo"), int(12))]
    );
    // Every user, with NULLs where there is no order.
    assert_eq!(
        pairs(&format!("SELECT u.name, o.id {join} ORDER BY u.id, o.id")),
        [(text("ann"), int(10)), (text("ann"), int(11)), (text("bo"), int(12)), (text("cy"), Value::Null)]
    );
    // Every order; NULL and dangling user ids match no user.
    assert_eq!(
        pairs("SELECT o.id, u.name FROM orders o LEFT OUTER JOIN users u ON u.id = o.user_id ORDER BY o.id"),
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
        pairs(&format!("SELECT u.name, o.id {join} AND o.total > 6 ORDER BY u.id")),
        [(text("ann"), int(11)), (text("bo"), Value::Null), (text("cy"), Value::Null)]
    );
    assert_eq!(
        pairs(&format!("SELECT u.name, o.id {join} WHERE o.id IS NULL")),
        [(text("cy"), Value::Null)],
        "users without orders"
    );
    // Commas and CROSS JOIN pair every row, and WHERE can join them.
    assert_eq!(pairs("SELECT COUNT(*), 0 FROM users, orders"), [(int(15), int(0))]);
    let sql = "SELECT COUNT(*), 0 FROM users CROSS JOIN orders WHERE orders.user_id = users.id";
    assert_eq!(pairs(sql), [(int(3), int(0))]);
    // Without aliases, tables qualify their own columns; unambiguous names need no table.
    assert_eq!(
        pairs("SELECT name, total FROM users INNER JOIN orders ON user_id = users.id WHERE total < 6 ORDER BY total"),
        [(text("bo"), int(3)), (text("ann"), int(5))]
    );
    // SELECT * gives every column of every table, in FROM order.
    match db.execute("SELECT * FROM users u JOIN orders o ON o.user_id = u.id WHERE o.id = 12").unwrap().pop() {
        Some(QueryResult::Rows { columns, rows }) => {
            assert_eq!(columns, ["id", "name", "city", "id", "user_id", "total"]);
            assert_eq!(rows, vec![vec![int(2), text("bo"), text("lima"), int(12), int(2), int(3)]]);
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
        query(&mut db, "SELECT a.name, b.name FROM users AS a JOIN users AS b ON a.city = b.city AND a.id < b.id"),
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
            let mut db = Database::in_memory();
            db.execute(&format!("CREATE TABLE a (id INT PRIMARY KEY, k INT, tag TEXT) USING {storage}")).unwrap();
            db.execute(&format!("CREATE TABLE b (id INT PRIMARY KEY, a_k INT, v INT) USING {storage}")).unwrap();
            if indexed && storage != "COLUMN" {
                db.execute("CREATE INDEX b_a_k ON b (a_k)").unwrap();
            }
            // Few left rows and many right ones, so that an index makes for an index join.
            let a: Vec<String> = (0..20i64)
                .map(|i| {
                    let k = if i % 9 == 0 { "NULL".into() } else { (i % 13).to_string() };
                    format!("({i}, {k}, '{}')", ["x", "y"][i as usize % 2])
                })
                .collect();
            let b: Vec<String> = (0..300i64)
                .map(|i| {
                    let a_k = if i % 7 == 0 { "NULL".into() } else { (i % 17).to_string() };
                    format!("({i}, {a_k}, {})", i % 11)
                })
                .collect();
            db.execute(&format!("INSERT INTO a VALUES {}", a.join(", "))).unwrap();
            db.execute(&format!("INSERT INTO b VALUES {}", b.join(", "))).unwrap();

            for (condition, method) in conditions {
                for kind in ["JOIN", "LEFT JOIN"] {
                    for filter in ["", " WHERE a.id < 12", " WHERE b.id IS NULL OR b.v != 3"] {
                        let sql = |on: &str| format!("SELECT a.id, b.id, b.v FROM a {kind} b ON {on}{filter}");
                        let mut planned = query(&mut db, &sql(condition));
                        let mut looped = query(&mut db, &sql(&format!("({condition}) OR FALSE")));
                        planned.sort_by(|x, y| x[0].sort_cmp(&y[0]).then(x[1].sort_cmp(&y[1])));
                        looped.sort_by(|x, y| x[0].sort_cmp(&y[0]).then(x[1].sort_cmp(&y[1])));
                        let label = format!("{storage}, indexed {indexed}: {}", sql(condition));
                        assert_eq!(planned, looped, "{label}");
                        assert!(explain(&mut db, &sql(condition)).contains(method), "{label}");
                    }
                }
            }
            let plan = explain(&mut db, "SELECT * FROM a JOIN b ON b.a_k = a.k");
            let expected = if indexed && storage != "COLUMN" { "INDEX JOIN" } else { "HASH JOIN" };
            assert!(plan.contains(expected), "{storage}, indexed {indexed}: {plan}");
            let plan = explain(&mut db, "SELECT * FROM a JOIN b ON b.id = a.k");
            assert!(plan.contains(if storage == "COLUMN" { "HASH JOIN" } else { "INDEX JOIN" }), "{plan}");
        }
    }
}

#[test]
fn index_join_is_picked_for_few_left_rows_by_row_counts() {
    let mut db = Database::in_memory();
    db.execute(
        "CREATE TABLE l (id INT PRIMARY KEY, k INT);
         CREATE TABLE r (id INT PRIMARY KEY, k INT, v INT);
         CREATE INDEX r_k ON r (k);
         CREATE TABLE p (id INT PRIMARY KEY, v INT) USING LSM;",
    )
    .unwrap();
    let rows = |count: i64, row: &dyn Fn(i64) -> String| (0..count).map(row).collect::<Vec<_>>().join(", ");
    db.execute(&format!("INSERT INTO l VALUES {}", rows(10, &|i| format!("({i}, {i})")))).unwrap();
    db.execute(&format!("INSERT INTO r VALUES {}", rows(1000, &|i| format!("({i}, {}, {})", i % 100, i % 7)))).unwrap();
    db.execute(&format!("INSERT INTO p VALUES {}", rows(100, &|i| format!("({i}, {i})")))).unwrap();

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
        ["INDEX JOIN", "HASH JOIN"].into_iter().find(|m| plan.contains(m)).unwrap_or_else(|| panic!("{plan}"))
    };
    // Thousands of lookups do not: the counts follow inserts and deletes.
    let more = |from: i64| format!("INSERT INTO l VALUES {}", rows(2000, &|i| format!("({}, {i})", from + i)));
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
    assert_eq!(method(&mut db, &format!("{sql} WHERE l.id > 1500")), "HASH JOIN");
    assert_eq!(method(&mut db, &format!("{sql} WHERE l.id > 1500 AND l.k < 9 AND l.k > 1")), "INDEX JOIN");
    // An LSM primary key lookup is cheaper: worth it for as many left rows as right ones.
    db.execute("DELETE FROM l WHERE id >= 100").unwrap();
    assert_eq!(method(&mut db, "SELECT * FROM l JOIN p ON p.id = l.k"), "INDEX JOIN");
    db.execute(&format!("INSERT INTO l VALUES {}", rows(200, &|i| format!("({}, {i})", 100 + i)))).unwrap();
    assert_eq!(method(&mut db, "SELECT * FROM l JOIN p ON p.id = l.k"), "HASH JOIN");
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
    let names: Vec<Value> = query(&mut db, sql).into_iter().map(|row| row[0].clone()).collect();
    assert_eq!(names, [text("cy"), text("bo"), text("ann"), text("ann")]);

    // Not the join's keys, or a right key of a LEFT JOIN: sorted after the join.
    for sql in [
        "SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id ORDER BY u.name",
        "SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id ORDER BY o.total, u.id",
        "SELECT u.name FROM users u LEFT JOIN orders o ON o.user_id = u.id ORDER BY o.user_id",
        "SELECT u.city FROM users u JOIN orders o ON o.user_id = u.id GROUP BY u.city ORDER BY u.city",
    ] {
        let plan = explain(&mut db, sql);
        assert!(plan.contains("SORT BY") && !plan.contains("MERGE"), "{sql}\n{plan}");
    }
}

#[test]
fn merge_join_agrees_with_a_hash_join_and_a_sort() {
    let mut db = Database::in_memory();
    db.execute(
        "CREATE TABLE a (id INT PRIMARY KEY, k INT, g INT);
         CREATE TABLE b (id INT PRIMARY KEY, k INT, t TEXT, h INT);
         CREATE TABLE c (id INT PRIMARY KEY, v INT);",
    )
    .unwrap();
    let a: Vec<String> = (0..60i64)
        .map(|i| format!("({i}, {}, {})", if i % 7 == 0 { "NULL".into() } else { (i % 9).to_string() }, i % 4))
        .collect();
    let b: Vec<String> = (0..50i64)
        .map(|i| {
            let k = if i % 6 == 0 { "NULL".into() } else { (i % 11).to_string() };
            format!("({i}, {k}, '{}', {})", i % 5, i % 4)
        })
        .collect();
    let c: Vec<String> = (0..40i64).map(|i| format!("({i}, {})", i % 3)).collect();
    db.execute(&format!("INSERT INTO a VALUES {}", a.join(", "))).unwrap();
    db.execute(&format!("INSERT INTO b VALUES {}", b.join(", "))).unwrap();
    db.execute(&format!("INSERT INTO c VALUES {}", c.join(", "))).unwrap();

    // (FROM, ORDER BY, other columns, whether a merge join does the sort)
    let cases = [
        ("a JOIN b ON b.k = a.k", "a.k", "a.id, b.id", true),
        ("a LEFT JOIN b ON b.k = a.k", "a.k DESC", "a.id, b.id", true),
        ("a JOIN b ON b.k = a.k", "b.k DESC", "a.id, b.id", true),
        ("a LEFT JOIN b ON b.k = a.k", "b.k", "a.id, b.id", false),
        ("a JOIN b ON b.k = a.k AND b.id > a.g", "a.k, a.g DESC", "a.id, b.id", true),
        ("a LEFT JOIN b ON b.k = a.k AND b.h = a.g", "a.k DESC", "a.id, b.id", true),
        ("a JOIN b ON b.k = a.k AND b.h = a.g", "a.k, a.id", "b.id", false),
        ("a JOIN b ON b.k = a.k WHERE a.id < 30 AND b.id > 5", "a.k", "a.id, b.id", true),
        ("a JOIN b ON b.k = a.k JOIN c ON c.id = b.id", "a.k DESC", "a.id, b.id, c.v", true),
        ("a JOIN b ON b.k = a.k LEFT JOIN c ON c.v = a.g", "a.k", "a.id, b.id, c.id", true),
        ("a JOIN b ON b.k = a.k JOIN c ON c.v < a.g", "a.k", "a.id, b.id, c.id", false),
        ("a JOIN c ON c.id = a.k JOIN b ON b.k = c.v", "c.v DESC, a.id", "b.id", true),
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
    for memory in [usize::MAX, 300] {
        db.set_sort_memory(memory);
        for (from, order, others, merged) in cases {
            let keys: Vec<&str> = order.split(", ").map(|k| k.trim_end_matches(" DESC")).collect();
            let sql = |order: &str| format!("SELECT {}, {others} FROM {from} ORDER BY {order}", keys.join(", "));
            let plan = explain(&mut db, &sql(order));
            assert_eq!(plan.contains("MERGE JOIN") && !plan.contains("SORT BY"), merged, "{}\n{plan}", sql(order));
            let mut rows = query(&mut db, &sql(order));
            let mut expected = query(&mut db, &sql(&hidden(order)));
            assert!(explain(&mut db, &sql(&hidden(order))).contains("SORT BY"));
            // The same keys in the same order; rows with equal keys may come in any order.
            let prefix = |rows: &[Vec<Value>]| rows.iter().map(|r| r[..keys.len()].to_vec()).collect::<Vec<_>>();
            assert_eq!(prefix(&rows), prefix(&expected), "{}, sort memory {memory}", sql(order));
            let by_all = |x: &Vec<Value>, y: &Vec<Value>| {
                x.iter().zip(y).map(|(a, b)| a.sort_cmp(b)).find(|o| o.is_ne()).unwrap_or(std::cmp::Ordering::Equal)
            };
            rows.sort_by(by_all);
            expected.sort_by(by_all);
            assert_eq!(rows, expected, "{}, sort memory {memory}", sql(order));
        }
    }
}

#[test]
fn block_nested_loop_join_agrees_for_every_block_size() {
    let mut db = Database::in_memory();
    db.execute(
        "CREATE TABLE a (id INT PRIMARY KEY, k INT);
         CREATE TABLE b (id INT PRIMARY KEY, v INT);
         CREATE TABLE empty (id INT PRIMARY KEY, v INT);",
    )
    .unwrap();
    let a: Vec<String> =
        (0..50i64).map(|i| format!("({i}, {})", if i % 7 == 0 { "NULL".into() } else { (i % 10).to_string() })).collect();
    let b: Vec<String> = (0..30i64).map(|i| format!("({i}, {})", i % 8)).collect();
    db.execute(&format!("INSERT INTO a VALUES {}", a.join(", "))).unwrap();
    db.execute(&format!("INSERT INTO b VALUES {}", b.join(", "))).unwrap();

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
        assert!(explain(&mut db, sql).contains("BLOCK NESTED LOOP JOIN"), "{sql}");
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
fn join_names_and_conditions_are_checked() {
    let mut db = setup_shop();
    let errors = [
        ("SELECT id FROM users u JOIN orders o ON o.user_id = u.id", "column reference id is ambiguous"),
        ("SELECT x.id FROM users u JOIN orders o ON o.user_id = u.id", "unknown column: x.id"),
        ("SELECT users.id FROM users u", "unknown column: users.id"),
        ("SELECT * FROM users u JOIN orders u ON u.id = 1", "appears more than once"),
        ("SELECT * FROM users JOIN users ON users.id = 1", "appears more than once"),
        ("SELECT * FROM users u JOIN orders o ON c.name = u.city JOIN cities c ON c.name = u.city", "unknown"),
        ("SELECT * FROM users u JOIN orders o ON COUNT(*) > 1", "not allowed in JOIN ... ON"),
        ("SELECT * FROM users u JOIN orders o", "expected ON"),
        ("SELECT * FROM missing m JOIN users u ON u.id = m.id", "unknown table: missing"),
        ("SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id GROUP BY u.city", "u.name must appear"),
    ];
    for (sql, expected) in errors {
        let e = db.execute(sql).expect_err(sql);
        assert!(e.0.contains(expected), "{sql}: {e}");
    }
    db.execute("CREATE TABLE cities (name TEXT PRIMARY KEY)").unwrap();
    let e = db.execute("SELECT * FROM users u JOIN orders o ON c.name = u.city JOIN cities c ON c.name = u.city");
    assert!(e.unwrap_err().0.contains("joined after it"));
    // A single table may still be referred to by its name.
    assert_eq!(query(&mut db, "SELECT users.name FROM users WHERE users.id = 2"), vec![vec![text("bo")]]);
}

fn ints(rows: Vec<Vec<Value>>) -> Vec<Value> {
    rows.into_iter().map(|mut r| r.remove(0)).collect()
}

#[test]
fn subqueries_as_values_with_exists_and_in() {
    let mut db = setup_shop();
    // Uncorrelated: the average total is 25 / 5 = 5.
    assert_eq!(ints(query(&mut db, "SELECT id FROM orders WHERE total > (SELECT AVG(total) FROM orders) ORDER BY id")), [
        int(11),
        int(13)
    ]);
    // Correlated, in the select list; inner columns win over outer ones of the same name.
    assert_eq!(
        query(&mut db, "SELECT name, (SELECT SUM(total) FROM orders WHERE user_id = users.id) FROM users ORDER BY id"),
        [vec![text("ann"), int(12)], vec![text("bo"), int(3)], vec![text("cy"), Value::Null]]
    );
    assert_eq!(
        ints(query(&mut db, "SELECT (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) FROM users u ORDER BY u.id")),
        [int(2), int(1), int(0)]
    );
    let names = |db: &mut Database, sql: &str| ints(query(db, sql));
    assert_eq!(
        names(&mut db, "SELECT name FROM users u WHERE EXISTS (SELECT * FROM orders o WHERE o.user_id = u.id) ORDER BY name"),
        [text("ann"), text("bo")]
    );
    assert_eq!(names(&mut db, "SELECT name FROM users u WHERE NOT EXISTS (SELECT 1 FROM orders WHERE user_id = u.id)"), [
        text("cy")
    ]);
    assert_eq!(names(&mut db, "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders) ORDER BY id"), [
        text("ann"),
        text("bo")
    ]);
    // NOT IN is never true when the subquery returns a NULL, nor for a NULL value.
    assert!(query(&mut db, "SELECT name FROM users WHERE id NOT IN (SELECT user_id FROM orders)").is_empty());
    assert_eq!(
        names(&mut db, "SELECT name FROM users WHERE id NOT IN (SELECT user_id FROM orders WHERE user_id IS NOT NULL)"),
        [text("cy")]
    );
    assert_eq!(ints(query(&mut db, "SELECT id FROM orders WHERE user_id NOT IN (SELECT id FROM users)")), [int(14)]);
    // Over no rows: IN is false, even for NULL, and a value is NULL.
    assert_eq!(
        query(&mut db, "SELECT COUNT(*) FROM users WHERE NULL NOT IN (SELECT id FROM users WHERE id > 100)"),
        [vec![int(3)]]
    );
    assert_eq!(query(&mut db, "SELECT (SELECT id FROM users WHERE id > 100) FROM users WHERE id = 1"), [vec![
        Value::Null
    ]]);
    // Values of another type match nothing.
    assert_eq!(query(&mut db, "SELECT COUNT(*) FROM users WHERE name IN (SELECT id FROM users)"), [vec![int(0)]]);

    // IN with a list.
    assert_eq!(ints(query(&mut db, "SELECT id FROM orders WHERE user_id IN (1, 99) ORDER BY id")), [
        int(10),
        int(11),
        int(14)
    ]);
    assert_eq!(ints(query(&mut db, "SELECT id FROM orders WHERE total NOT IN (3, 5 + 4) ORDER BY id")), [
        int(10),
        int(11),
        int(14)
    ]);
    assert!(query(&mut db, "SELECT id FROM orders WHERE total NOT IN (3, NULL)").is_empty());
    assert!(db.execute("SELECT id FROM orders WHERE total IN ('x')").is_err(), "compared like =");
}

#[test]
fn nested_correlated_and_grouped_subqueries() {
    let mut db = setup_shop();
    // The same table inside and out.
    assert_eq!(ints(query(&mut db, "SELECT id FROM orders WHERE total = (SELECT MAX(total) FROM orders)")), [int(13)]);
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
    assert_eq!(query(&mut db, sql), [vec![text("ann"), int(11)], vec![text("bo"), int(12)]]);
    assert!(explain(&mut db, sql).contains(
        "FILTER o.total = (SELECT MAX(total) FROM orders x WHERE x.user_id = u.id)\n      HASH JOIN"
    ));
    let sql = "SELECT u.name, o.id FROM users u LEFT JOIN orders o ON o.user_id = u.id
               WHERE NOT EXISTS (SELECT * FROM orders x WHERE x.user_id = u.id AND x.total > 6) ORDER BY u.id";
    assert_eq!(query(&mut db, sql), [vec![text("bo"), int(12)], vec![text("cy"), Value::Null]]);

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
    assert_eq!(query(&mut db, "SELECT name FROM users WHERE id = (SELECT MIN(user_id) FROM orders)"), [vec![text(
        "ann"
    )]]);
    assert_eq!(
        explain(&mut db, "SELECT name FROM users u WHERE EXISTS (SELECT * FROM orders o WHERE o.user_id = u.id)"),
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
    assert_eq!(ints(query(&mut db, "SELECT city FROM users ORDER BY id")), [text("lima"), text("lima"), text("oslo")]);
    // Correlated, by a column only the outer table has.
    db.execute("CREATE TABLE stats (uid INT, n INT); INSERT INTO stats VALUES (1, 0), (2, 0), (3, 0)").unwrap();
    db.execute("UPDATE stats SET n = (SELECT COUNT(*) FROM orders WHERE user_id = uid)").unwrap();
    assert_eq!(ints(query(&mut db, "SELECT n FROM stats ORDER BY uid")), [int(2), int(1), int(0)]);

    db.execute("DELETE FROM orders WHERE user_id NOT IN (SELECT id FROM users)").unwrap();
    assert_eq!(ints(query(&mut db, "SELECT id FROM orders ORDER BY id")), [int(10), int(11), int(12), int(13)]);
    // The subquery sees the table as it was before the statement: the average is 6.
    db.execute("DELETE FROM orders WHERE total < (SELECT AVG(total) FROM orders)").unwrap();
    assert_eq!(ints(query(&mut db, "SELECT id FROM orders ORDER BY id")), [int(11), int(13)]);
    db.execute("INSERT INTO orders VALUES ((SELECT MAX(id) FROM orders) + 1, 3, (SELECT COUNT(*) FROM users))").unwrap();
    assert_eq!(query(&mut db, "SELECT * FROM orders WHERE user_id = 3"), [vec![int(14), int(3), int(3)]]);
    assert_eq!(
        explain(&mut db, "DELETE FROM orders WHERE id IN (SELECT id FROM orders WHERE total > 5)"),
        "FILTER id IN (SELECT id FROM orders WHERE total > 5)\n  SCAN orders"
    );
    // A failing subquery rolls the statement back.
    assert!(db.execute("UPDATE orders SET total = (SELECT id FROM users)").is_err());
    assert_eq!(ints(query(&mut db, "SELECT total FROM orders ORDER BY id")), [int(7), int(9), int(3)]);
}

#[test]
fn subquery_mistakes_are_reported() {
    let mut db = setup_shop();
    let errors = [
        ("SELECT (SELECT id FROM users) FROM users", "more than one row"),
        ("SELECT * FROM users WHERE id IN (SELECT id, name FROM users)", "only one column"),
        ("SELECT (SELECT * FROM users) FROM users", "only one column"),
        // Planned even where it would never run.
        ("SELECT * FROM users WHERE id > 100 AND EXISTS (SELECT nope FROM orders)", "unknown column: nope"),
        ("SELECT * FROM users WHERE EXISTS (SELECT * FROM missing)", "missing"),
        ("SELECT * FROM users u JOIN orders o ON o.user_id IN (SELECT id FROM users)", "not allowed in JOIN ... ON"),
        ("SELECT COUNT(*) FROM users GROUP BY (SELECT 1 FROM orders LIMIT 1)", "not allowed in GROUP BY"),
        ("SELECT SUM((SELECT 1 FROM orders LIMIT 1)) FROM users", "not allowed in aggregate functions"),
        (
            "SELECT city, (SELECT COUNT(*) FROM orders WHERE user_id = users.id) FROM users GROUP BY city",
            "column users.id must appear in GROUP BY",
        ),
        ("SELECT * FROM users WHERE id IN (SELECT id FROM orders", "expected"),
        ("SELECT * FROM users WHERE EXISTS SELECT 1 FROM users", "expected"),
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
        let mut db = Database::in_memory();
        db.execute(&format!(
            "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, city TEXT) USING {storage};
             CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, total INT) USING {storage};
             INSERT INTO users VALUES (1, 'ann', 'oslo'), (2, 'bo', 'lima'), (3, 'cy', 'oslo');
             INSERT INTO orders VALUES (10, 1, 5), (11, 1, 7), (12, 2, 3), (13, NULL, 9), (14, 99, 1);"
        ))
        .unwrap();
        if storage != "COLUMN" {
            db.execute("CREATE INDEX orders_user ON orders (user_id)").unwrap();
        }
        results.push(queries.iter().map(|sql| query(&mut db, sql)).collect::<Vec<_>>());
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
    assert_eq!(query(&mut db, "SELECT age FROM users WHERE id = 1"), [vec![int(99)]]);
    db.execute("ROLLBACK").unwrap();
    assert!(!db.in_transaction());
    assert_eq!(ids(&mut db), [int(1), int(2), int(3)]);
    assert_eq!(query(&mut db, "SELECT age FROM users WHERE id = 1"), [vec![int(30)]]);

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
    let rows = (0..1500).map(|i| format!("({i}, '{}')", "x".repeat(i % 40))).collect::<Vec<_>>().join(", ");
    db.execute(&format!("INSERT INTO l VALUES {rows}; INSERT INTO c VALUES {rows}")).unwrap();
    assert_eq!(db.table_names(), ["c", "l"]);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(db.table_names(), ["users"]);
    assert_eq!(ids(&mut db), [int(1), int(3), int(5)]);
    assert!(explain(&mut db, "SELECT * FROM users WHERE name = 'x'").contains("SCAN users"), "no index");

    db.execute("BEGIN; CREATE TABLE l (id INT PRIMARY KEY, v TEXT) USING LSM").unwrap();
    db.execute(&format!("INSERT INTO l VALUES {rows}; CREATE INDEX users_name ON users (name); COMMIT")).unwrap();
    assert_eq!(query(&mut db, "SELECT COUNT(*) FROM l"), [vec![int(1500)]]);
    assert!(explain(&mut db, "SELECT * FROM users WHERE name = 'x'").contains("USING INDEX users_name"));
}

#[test]
fn a_failing_statement_in_a_transaction_is_undone_alone() {
    let mut db = setup();
    db.execute("BEGIN; INSERT INTO users (id) VALUES (4)").unwrap();
    // The third row is a duplicate, after two rows went in.
    assert!(db.execute("INSERT INTO users (id) VALUES (5), (6), (1), (7)").is_err());
    assert!(db.execute("UPDATE users SET id = 4").is_err());
    assert!(db.in_transaction(), "the transaction goes on");
    db.execute("INSERT INTO users (id) VALUES (8); COMMIT").unwrap();
    assert_eq!(ints(query(&mut db, "SELECT id FROM users ORDER BY id")), [int(1), int(2), int(3), int(4), int(8)]);

    // execute stops at the first error, and the transaction stays open.
    assert!(db.execute("BEGIN; INSERT INTO users (id) VALUES (9); SELECT nope FROM users; COMMIT").is_err());
    assert!(db.in_transaction());
    db.execute("ROLLBACK").unwrap();
    assert_eq!(query(&mut db, "SELECT COUNT(*) FROM users WHERE id = 9"), [vec![int(0)]]);
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
    assert!(db.execute("ROLLBACK TO b").unwrap_err().0.contains("no such savepoint: b"));
    db.execute("INSERT INTO users (id) VALUES (7); ROLLBACK TRANSACTION TO a").unwrap();
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
        ("ROLLBACK TO a", "ROLLBACK TO can only be used in a transaction"),
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
        let mut db = Database::open(&path.0).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY); INSERT INTO t VALUES (1)").unwrap();
        db.execute("BEGIN; INSERT INTO t VALUES (2); COMMIT").unwrap();
        db.execute("BEGIN; INSERT INTO t VALUES (3); DELETE FROM t WHERE id = 1").unwrap();
    }
    let mut db = Database::open(&path.0).unwrap();
    assert_eq!(ints(query(&mut db, "SELECT id FROM t ORDER BY id")), [int(1), int(2)]);
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
    assert_eq!(query(&mut db, sql), [vec![text("ann"), int(12)], vec![text("bo"), int(3)]]);
    assert!(explain(&mut db, sql).contains("HASH JOIN ON u.id = s.user_id"), "{}", explain(&mut db, sql));
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
    assert_eq!(query(&mut db, "SELECT COUNT(*) FROM (SELECT DISTINCT city FROM (SELECT * FROM users) a) b"), [vec![
        int(2)
    ]]);
    assert_eq!(ints(query(&mut db, "SELECT * FROM (SELECT id FROM orders ORDER BY total DESC LIMIT 2) t ORDER BY id")), [
        int(11),
        int(13)
    ]);
    assert_eq!(
        query(
            &mut db,
            "SELECT city, MAX(n) FROM (SELECT u.city, (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) AS n
             FROM users u) t GROUP BY city ORDER BY city"
        ),
        [vec![text("lima"), int(1)], vec![text("oslo"), int(2)]]
    );
    // Its columns are named as its select list names them.
    assert_eq!(columns(&mut db, "SELECT * FROM (SELECT id + 1 AS next, name, COUNT(*) FROM users GROUP BY id, name) d"), [
        "next", "name", "count"
    ]);
    assert_eq!(columns(&mut db, "SELECT * FROM (SELECT u.id, o.id FROM users u, orders o) d"), ["id", "id"]);
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
    assert_eq!(ints(query(&mut db, "SELECT city FROM users ORDER BY id")), [text("rich"), text("lima"), text("oslo")]);

    let errors = [
        ("SELECT * FROM (SELECT * FROM users)", "must have an alias"),
        ("SELECT * FROM users u, (SELECT * FROM orders WHERE user_id = u.id) o", "unknown column: u.id"),
        ("SELECT d.id FROM (SELECT u.id, o.id FROM users u, orders o) d", "ambiguous"),
        ("SELECT d.nope FROM (SELECT id FROM users) d", "unknown column: d.nope"),
        ("SELECT * FROM (SELECT id FROM users) u, users u", "appears more than once"),
    ];
    for (sql, expected) in errors {
        let e = db.execute(sql).expect_err(sql);
        assert!(e.0.contains(expected), "{sql}: {e}");
    }
}

#[test]
fn select_list_names_and_order_by_them() {
    let mut db = setup_shop();
    assert_eq!(columns(&mut db, "SELECT name AS who, u.id * 2 AS twice, total FROM users u, orders LIMIT 1"), [
        "who", "twice", "total"
    ]);
    assert_eq!(ints(query(&mut db, "SELECT name AS n FROM users ORDER BY n DESC")), [text("cy"), text("bo"), text("ann")]);
    // A name of the select list wins over a column's, as in PostgreSQL and SQLite.
    assert_eq!(ints(query(&mut db, "SELECT - id AS id FROM users ORDER BY id")), [int(-3), int(-2), int(-1)]);
    assert_eq!(ints(query(&mut db, "SELECT DISTINCT city AS c FROM users ORDER BY c")), [text("lima"), text("oslo")]);
    assert_eq!(
        query(&mut db, "SELECT user_id AS u, COUNT(*) AS n FROM orders GROUP BY user_id ORDER BY n DESC, u LIMIT 2"),
        [vec![int(1), int(2)], vec![Value::Null, int(1)]]
    );
}
