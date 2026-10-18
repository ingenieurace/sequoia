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
