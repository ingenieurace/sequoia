//! Many threads using one database, each through a session of its own.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use sequoia::{QueryResult, Session, SharedDatabase, Value};

fn rows(session: &mut Session, sql: &str) -> sequoia::Result<Vec<Vec<Value>>> {
    match session.execute(sql)?.pop() {
        Some(QueryResult::Rows { rows, .. }) => Ok(rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn int(session: &mut Session, sql: &str) -> i64 {
    match rows(session, sql).unwrap()[0][0] {
        Value::Int(n) => n,
        ref other => panic!("expected an INT, got {other:?}"),
    }
}

fn locked(result: sequoia::Result<Vec<QueryResult>>) -> bool {
    matches!(result, Err(e) if e.to_string().contains("database is locked"))
}

#[test]
fn readers_never_wait_for_each_other() {
    let db = SharedDatabase::in_memory();
    let mut setup = db.session();
    setup
        .execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
        .unwrap();
    let values: Vec<String> = (0..2000).map(|i| format!("({i}, {})", i % 10)).collect();
    setup
        .execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
        .unwrap();
    // With no time to wait at all, a reader would fail if another held it off.
    let start = Arc::new(Barrier::new(8));
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let (db, start) = (db.clone(), start.clone());
            thread::spawn(move || {
                let mut session = db.session();
                session.set_busy_timeout(Duration::ZERO);
                start.wait();
                for _ in 0..50 {
                    let sum = int(&mut session, "SELECT SUM(n) FROM t WHERE id >= 0");
                    assert_eq!(sum, 9000);
                }
            })
        })
        .collect();
    for reader in readers {
        reader.join().unwrap();
    }
}

#[test]
fn a_transaction_keeps_the_others_out_until_it_ends() {
    let db = SharedDatabase::in_memory();
    let (mut a, mut b) = (db.session(), db.session());
    b.set_busy_timeout(Duration::from_millis(50));
    a.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();

    a.execute("BEGIN; INSERT INTO t VALUES (1)").unwrap();
    assert!(a.in_transaction());
    // Neither its rows nor the table to write to, while it is open.
    assert!(locked(b.execute("SELECT * FROM t")));
    assert!(locked(b.execute("INSERT INTO t VALUES (2)")));
    assert!(b.table_names().is_err());
    // It reads what it wrote itself.
    assert_eq!(int(&mut a, "SELECT COUNT(*) FROM t"), 1);
    a.execute("ROLLBACK").unwrap();
    assert_eq!(int(&mut b, "SELECT COUNT(*) FROM t"), 0);

    a.execute("BEGIN; INSERT INTO t VALUES (3); COMMIT")
        .unwrap();
    assert_eq!(int(&mut b, "SELECT COUNT(*) FROM t"), 1);

    // A session that waits for its turn gets it when the transaction ends.
    a.execute("BEGIN; INSERT INTO t VALUES (4)").unwrap();
    let waiting = {
        let db = db.clone();
        thread::spawn(move || {
            let mut c = db.session();
            c.set_busy_timeout(Duration::from_secs(30));
            int(&mut c, "SELECT COUNT(*) FROM t")
        })
    };
    thread::sleep(Duration::from_millis(100));
    a.execute("COMMIT").unwrap();
    assert_eq!(waiting.join().unwrap(), 2);
}

#[test]
fn a_session_dropped_in_a_transaction_rolls_it_back() {
    let db = SharedDatabase::in_memory();
    let mut a = db.session();
    a.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    a.execute("BEGIN; INSERT INTO t VALUES (1), (2)").unwrap();
    drop(a);
    let mut b = db.session();
    b.set_busy_timeout(Duration::ZERO);
    assert_eq!(int(&mut b, "SELECT COUNT(*) FROM t"), 0);
    b.execute("INSERT INTO t VALUES (1)").unwrap();
}

#[test]
fn writers_take_turns_and_lose_nothing() {
    let db = SharedDatabase::in_memory();
    db.session()
        .execute("CREATE TABLE t (id INT PRIMARY KEY, writer INT)")
        .unwrap();
    let writers: Vec<_> = (0..8i64)
        .map(|w| {
            let db = db.clone();
            thread::spawn(move || {
                let mut session = db.session();
                session.set_busy_timeout(Duration::from_secs(60));
                for i in 0..50 {
                    let id = w * 1000 + i;
                    session
                        .execute(&format!("INSERT INTO t VALUES ({id}, {w})"))
                        .unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let mut session = db.session();
    assert_eq!(int(&mut session, "SELECT COUNT(*) FROM t"), 400);
    for w in 0..8 {
        let n = int(
            &mut session,
            &format!("SELECT COUNT(*) FROM t WHERE writer = {w}"),
        );
        assert_eq!(n, 50, "writer {w}");
    }
}

/// Transfers between accounts, in transactions some of which roll back,
/// while readers check that the money adds up: none ever sees a transfer
/// half done.
fn transfers_add_up(db: SharedDatabase) {
    const ACCOUNTS: i64 = 10;
    let mut setup = db.session();
    setup
        .execute("CREATE TABLE account (id INT PRIMARY KEY, balance INT)")
        .unwrap();
    setup
        .execute("CREATE TABLE transfer (id INT PRIMARY KEY, amount INT)")
        .unwrap();
    let accounts: Vec<String> = (0..ACCOUNTS).map(|i| format!("({i}, 100)")).collect();
    setup
        .execute(&format!(
            "INSERT INTO account VALUES {}",
            accounts.join(", ")
        ))
        .unwrap();

    let writers: Vec<_> = (0..4i64)
        .map(|w| {
            let db = db.clone();
            thread::spawn(move || {
                let mut session = db.session();
                session.set_busy_timeout(Duration::from_secs(60));
                let mut committed = 0;
                for i in 0..40i64 {
                    let (from, to) = ((w + i) % ACCOUNTS, (w * 3 + i * 7 + 1) % ACCOUNTS);
                    let amount = 1 + i % 5;
                    session
                        .execute(&format!(
                            "BEGIN;
                             UPDATE account SET balance = balance - {amount} WHERE id = {from};
                             UPDATE account SET balance = balance + {amount} WHERE id = {to};
                             INSERT INTO transfer VALUES ({}, {amount})",
                            w * 1000 + i
                        ))
                        .unwrap();
                    if i % 4 == 3 {
                        session.execute("ROLLBACK").unwrap();
                    } else {
                        session.execute("COMMIT").unwrap();
                        committed += 1;
                    }
                }
                committed
            })
        })
        .collect();
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let db = db.clone();
            thread::spawn(move || {
                let mut session = db.session();
                session.set_busy_timeout(Duration::from_secs(60));
                for _ in 0..40 {
                    let total = int(&mut session, "SELECT SUM(balance) FROM account");
                    assert_eq!(total, ACCOUNTS * 100, "a transfer seen half done");
                }
            })
        })
        .collect();
    let committed: i64 = writers.into_iter().map(|w| w.join().unwrap()).sum();
    for reader in readers {
        reader.join().unwrap();
    }
    let mut session = db.session();
    assert_eq!(
        int(&mut session, "SELECT SUM(balance) FROM account"),
        ACCOUNTS * 100
    );
    assert_eq!(
        int(&mut session, "SELECT COUNT(*) FROM transfer"),
        committed
    );
    assert_eq!(committed, 4 * 30);
}

#[test]
fn transfers_add_up_in_memory() {
    transfers_add_up(SharedDatabase::in_memory());
}

#[test]
fn transfers_add_up_in_a_file_and_stay_there() {
    let path = std::env::temp_dir().join(format!("sequoia-shared-{}.db", std::process::id()));
    let remove = || {
        for suffix in ["", "-wal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    };
    remove();
    transfers_add_up(SharedDatabase::open(&path).unwrap());
    // Every session and the database are gone: open it again.
    let mut session = SharedDatabase::open(&path).unwrap().session();
    assert_eq!(int(&mut session, "SELECT SUM(balance) FROM account"), 1000);
    assert_eq!(int(&mut session, "SELECT COUNT(*) FROM transfer"), 120);
    drop(session);
    remove();
}
