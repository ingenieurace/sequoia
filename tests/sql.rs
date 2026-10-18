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
    }
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
    let size = std::fs::metadata(&path.0).unwrap().len();
    db.execute("DROP TABLE t").unwrap();
    fill(&mut db);
    assert_eq!(std::fs::metadata(&path.0).unwrap().len(), size, "freed pages are reused");

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
        std::fs::metadata(&path.0).unwrap().len()
    };

    let mut db = Database::open(&path.0).unwrap();
    for i in 0..100 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, '{}')", "y".repeat(100))).unwrap();
    }
    assert_eq!(std::fs::metadata(&path.0).unwrap().len(), size, "the file did not grow");
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
    let size = std::fs::metadata(&path.0).unwrap().len();
    // Shrinking or same-size updates stay in place.
    db.execute("UPDATE t SET body = 'y'").unwrap();
    assert_eq!(std::fs::metadata(&path.0).unwrap().len(), size);

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
    let size = std::fs::metadata(&path.0).unwrap().len();
    db.execute("DROP TABLE t").unwrap();
    fill(&mut db);
    assert_eq!(std::fs::metadata(&path.0).unwrap().len(), size, "freed pages are reused");
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
    let size = std::fs::metadata(&path.0).unwrap().len();
    db.execute("DROP TABLE t").unwrap();
    fill(&mut db);
    assert_eq!(std::fs::metadata(&path.0).unwrap().len(), size, "freed pages are reused");
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
