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

/// Runs EXPLAIN on a statement and returns the plan.
fn plan(db: &mut Database, sql: &str) -> String {
    match db.execute(&format!("EXPLAIN {sql}")).unwrap().pop() {
        Some(QueryResult::Message(plan)) => plan,
        other => panic!("expected a plan, got {other:?}"),
    }
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
