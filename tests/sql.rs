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
    // Rows keep going to the end of the heap after reopening.
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
