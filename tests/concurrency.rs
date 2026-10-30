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

/// Whether a statement failed because another transaction changed a row
/// first, or the two waited for each other: the transaction may be tried again.
fn conflict(result: &sequoia::Result<Vec<QueryResult>>) -> bool {
    matches!(result, Err(e) if e.0.contains("could not serialize") || e.0.contains("deadlock"))
}

/// Two sessions on a table `t (id, n)` of rows (1, 0) and (2, 0); the
/// second gives up after 50 ms.
fn two_sessions() -> (SharedDatabase, Session, Session) {
    let db = SharedDatabase::in_memory();
    let (mut a, mut b) = (db.session(), db.session());
    b.set_busy_timeout(Duration::from_millis(50));
    a.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
        .unwrap();
    a.execute("INSERT INTO t VALUES (1, 0), (2, 0)").unwrap();
    (db, a, b)
}

#[test]
fn readers_see_a_snapshot_and_never_wait_for_writers() {
    let (_db, mut a, mut b) = two_sessions();
    b.set_busy_timeout(Duration::ZERO);
    a.execute("BEGIN; INSERT INTO t VALUES (3, 0); UPDATE t SET n = 1 WHERE id = 1")
        .unwrap();
    assert!(a.in_transaction());
    // Another session reads what was committed, at once, and writes other rows.
    assert_eq!(int(&mut b, "SELECT COUNT(*) FROM t"), 2);
    assert_eq!(int(&mut b, "SELECT n FROM t WHERE id = 1"), 0);
    b.execute("INSERT INTO t VALUES (4, 0)").unwrap();
    assert_eq!(b.table_names().unwrap(), ["t"]);
    // The transaction reads its own writes, and not what committed after it began.
    assert_eq!(int(&mut a, "SELECT COUNT(*) FROM t"), 3);
    assert_eq!(int(&mut a, "SELECT n FROM t WHERE id = 1"), 1);
    a.execute("COMMIT").unwrap();
    assert_eq!(int(&mut b, "SELECT COUNT(*) FROM t"), 4);
    assert_eq!(int(&mut b, "SELECT n FROM t WHERE id = 1"), 1);

    // Repeatable reads: a transaction's snapshot stays as its first statement took it.
    b.execute("BEGIN").unwrap();
    assert_eq!(int(&mut b, "SELECT SUM(n) FROM t"), 1);
    a.execute("UPDATE t SET n = n + 100").unwrap();
    a.execute("DELETE FROM t WHERE id = 2").unwrap();
    assert_eq!(int(&mut b, "SELECT SUM(n) FROM t"), 1);
    assert_eq!(int(&mut b, "SELECT COUNT(*) FROM t WHERE id = 2"), 1);
    b.execute("COMMIT").unwrap();
    assert_eq!(int(&mut b, "SELECT SUM(n) FROM t"), 301);
}

#[test]
fn the_first_to_change_a_row_wins() {
    let (db, mut a, mut b) = two_sessions();
    a.execute("BEGIN; UPDATE t SET n = 1 WHERE id = 1").unwrap();
    b.execute("BEGIN").unwrap();
    assert_eq!(int(&mut b, "SELECT n FROM t WHERE id = 1"), 0);
    // It waits for a, which does not end in time: the statement fails, alone.
    assert!(conflict(&b.execute("UPDATE t SET n = 2 WHERE id = 1")));
    assert!(b.in_transaction());
    b.execute("UPDATE t SET n = 2 WHERE id = 2").unwrap();
    a.execute("COMMIT").unwrap();
    // a committed after b began: b can never change that row.
    let lost = b.execute("UPDATE t SET n = 2 WHERE id = 1");
    assert!(
        matches!(&lost, Err(e) if e.0.contains("committed after this one began")),
        "{lost:?}"
    );
    b.execute("ROLLBACK").unwrap();
    assert_eq!(
        rows(&mut b, "SELECT n FROM t ORDER BY id").unwrap(),
        [[Value::Int(1)], [Value::Int(0)]]
    );

    // A statement outside a transaction waits, and then changes what is
    // there: here what is left once the other rolls back.
    a.execute("BEGIN; UPDATE t SET n = 50 WHERE id = 1")
        .unwrap();
    let waiting = {
        let db = db.clone();
        thread::spawn(move || {
            let mut c = db.session();
            c.set_busy_timeout(Duration::from_secs(30));
            c.execute("UPDATE t SET n = n + 10 WHERE id = 1").unwrap();
        })
    };
    thread::sleep(Duration::from_millis(100));
    a.execute("ROLLBACK").unwrap();
    waiting.join().unwrap();
    assert_eq!(int(&mut a, "SELECT n FROM t WHERE id = 1"), 11);
    // And once the other commits, the committed value: no update is lost.
    a.execute("BEGIN; UPDATE t SET n = n * 2 WHERE id = 1")
        .unwrap();
    let waiting = {
        let db = db.clone();
        thread::spawn(move || {
            let mut c = db.session();
            c.set_busy_timeout(Duration::from_secs(30));
            c.execute("UPDATE t SET n = n + 1 WHERE id = 1").unwrap();
        })
    };
    thread::sleep(Duration::from_millis(100));
    a.execute("COMMIT").unwrap();
    waiting.join().unwrap();
    assert_eq!(int(&mut a, "SELECT n FROM t WHERE id = 1"), 23);
}

#[test]
fn a_key_another_transaction_wrote_waits_for_it() {
    let (db, mut a, mut b) = two_sessions();
    a.execute("BEGIN; INSERT INTO t VALUES (5, 0)").unwrap();
    assert!(conflict(&b.execute("INSERT INTO t VALUES (5, 1)")));
    a.execute("ROLLBACK").unwrap();
    b.execute("INSERT INTO t VALUES (5, 1)").unwrap();

    a.execute("BEGIN; DELETE FROM t WHERE id = 5").unwrap();
    let waiting = {
        let db = db.clone();
        thread::spawn(move || {
            let mut c = db.session();
            c.set_busy_timeout(Duration::from_secs(30));
            c.execute("INSERT INTO t VALUES (5, 2)")
        })
    };
    thread::sleep(Duration::from_millis(100));
    a.execute("COMMIT").unwrap();
    // The key was let go: the insert takes it.
    waiting.join().unwrap().unwrap();
    assert_eq!(int(&mut a, "SELECT n FROM t WHERE id = 5"), 2);

    a.execute("BEGIN; INSERT INTO t VALUES (6, 0)").unwrap();
    let waiting = {
        let db = db.clone();
        thread::spawn(move || {
            let mut c = db.session();
            c.set_busy_timeout(Duration::from_secs(30));
            c.execute("INSERT INTO t VALUES (6, 1)")
        })
    };
    thread::sleep(Duration::from_millis(100));
    a.execute("COMMIT").unwrap();
    let taken = waiting.join().unwrap();
    assert!(
        matches!(&taken, Err(e) if e.0.contains("duplicate primary key")),
        "{taken:?}"
    );
}

#[test]
fn transactions_that_wait_for_each_other_are_a_deadlock() {
    let (db, mut a, _) = two_sessions();
    a.execute("BEGIN; UPDATE t SET n = 1 WHERE id = 1").unwrap();
    let mut b = db.session();
    b.set_busy_timeout(Duration::from_secs(30));
    b.execute("BEGIN; UPDATE t SET n = 2 WHERE id = 2").unwrap();
    let b = thread::spawn(move || {
        // Waits for a's row.
        let result = b.execute("UPDATE t SET n = 2 WHERE id = 1");
        (b, result)
    });
    thread::sleep(Duration::from_millis(200));
    a.set_busy_timeout(Duration::from_secs(30));
    let start = std::time::Instant::now();
    let deadlock = a.execute("UPDATE t SET n = 1 WHERE id = 2");
    assert!(
        matches!(&deadlock, Err(e) if e.0.contains("deadlock detected")),
        "{deadlock:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(10), "found at once");
    a.execute("ROLLBACK").unwrap();
    let (mut b, result) = b.join().unwrap();
    result.unwrap();
    b.execute("COMMIT").unwrap();
    assert_eq!(
        rows(&mut a, "SELECT n FROM t ORDER BY id").unwrap(),
        [[Value::Int(2)], [Value::Int(2)]]
    );
}

#[test]
fn a_schema_change_waits_for_open_transactions_and_keeps_the_others_out() {
    let (_db, mut a, mut b) = two_sessions();
    a.execute("BEGIN; INSERT INTO t VALUES (3, 0)").unwrap();
    assert!(locked(b.execute("CREATE INDEX t_n ON t (n)")));
    assert!(locked(b.execute("ALTER TABLE t ADD extra INT")));
    a.execute("COMMIT").unwrap();

    b.execute("BEGIN; CREATE INDEX t_n ON t (n); INSERT INTO t VALUES (4, 4)")
        .unwrap();
    a.set_busy_timeout(Duration::from_millis(50));
    assert!(locked(a.execute("SELECT COUNT(*) FROM t")));
    assert!(locked(a.execute("BEGIN")));
    assert!(!a.in_transaction());
    assert_eq!(int(&mut b, "SELECT COUNT(*) FROM t WHERE n = 4"), 1);
    b.execute("ROLLBACK").unwrap();
    // The index and the row are gone; the others go on.
    assert_eq!(int(&mut a, "SELECT COUNT(*) FROM t"), 3);
    assert!(a.execute("DROP INDEX t_n").is_err());
}

#[test]
fn vacuum_keeps_the_versions_a_snapshot_still_reads() {
    let (_db, mut a, mut b) = two_sessions();
    let removed = |session: &mut Session| match session.execute("VACUUM t").unwrap().pop() {
        Some(QueryResult::Message(text)) => text.split(' ').next().unwrap().parse::<i64>().unwrap(),
        other => panic!("expected a message, got {other:?}"),
    };
    a.execute("BEGIN").unwrap();
    assert_eq!(int(&mut a, "SELECT SUM(n) FROM t"), 0);
    b.execute("UPDATE t SET n = 7").unwrap();
    b.execute("DELETE FROM t WHERE id = 2").unwrap();
    assert_eq!(removed(&mut b), 0, "a still reads them");
    assert_eq!(int(&mut a, "SELECT COUNT(*) FROM t WHERE n = 0"), 2);
    a.execute("COMMIT").unwrap();
    // The two versions updated, and the one deleted.
    assert_eq!(removed(&mut b), 3);
    assert_eq!(
        rows(&mut a, "SELECT id, n FROM t").unwrap(),
        [[Value::Int(1), Value::Int(7)]]
    );
    assert!(b.execute("BEGIN; VACUUM").is_err(), "not in a transaction");
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
                    // Another transfer may change an account first: then
                    // this one rolls back and tries again, as a client would.
                    loop {
                        let result = session.execute(&format!(
                            "BEGIN;
                             UPDATE account SET balance = balance - {amount} WHERE id = {from};
                             UPDATE account SET balance = balance + {amount} WHERE id = {to};
                             INSERT INTO transfer VALUES ({}, {amount})",
                            w * 1000 + i
                        ));
                        if !conflict(&result) {
                            result.unwrap();
                            break;
                        }
                        session.execute("ROLLBACK").unwrap();
                    }
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

/// A snapshot reads the same rows, by every way a query finds them, while
/// another session updates, deletes, inserts and moves primary keys, in
/// every kind of table; vacuum leaves what it reads, and then the rows are
/// as committed.
#[test]
fn every_storage_keeps_versions_for_snapshots() {
    for using in ["HEAP", "LSM", "COLUMN"] {
        let db = SharedDatabase::in_memory();
        let (mut reader, mut writer) = (db.session(), db.session());
        writer
            .execute(&format!(
                "CREATE TABLE t (id INT PRIMARY KEY, tag TEXT, n INT) USING {using}"
            ))
            .unwrap();
        if using != "COLUMN" {
            writer.execute("CREATE INDEX t_tag ON t (tag)").unwrap();
        }
        let values: Vec<String> = (0..3000)
            .map(|i| format!("({i}, 'tag{}', {i})", i % 10))
            .collect();
        writer
            .execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .unwrap();
        let queries = [
            "SELECT COUNT(*), SUM(n) FROM t",
            "SELECT COUNT(*), SUM(n) FROM t WHERE tag = 'tag3'",
            "SELECT COUNT(*), SUM(n) FROM t WHERE id = 1500",
            "SELECT COUNT(*), SUM(n) FROM t WHERE id >= 100 AND id < 200",
            "SELECT COUNT(*), SUM(n) FROM t WHERE n >= 2990",
        ];
        let read = |session: &mut Session| -> Vec<Vec<Vec<Value>>> {
            queries
                .iter()
                .map(|sql| rows(session, sql).unwrap())
                .collect()
        };
        reader.execute("BEGIN").unwrap();
        let before = read(&mut reader);

        writer
            .execute("UPDATE t SET n = n + 1, tag = 'moved' WHERE id % 3 = 0")
            .unwrap();
        writer.execute("DELETE FROM t WHERE id % 7 = 0").unwrap();
        writer
            .execute("UPDATE t SET id = id + 10000 WHERE id >= 100 AND id < 150")
            .unwrap();
        writer
            .execute("BEGIN; INSERT INTO t VALUES (99999, 'tag3', 5); UPDATE t SET n = 0 WHERE id = 1500; COMMIT")
            .unwrap();
        writer.execute("VACUUM t").unwrap();
        assert_eq!(read(&mut reader), before, "{using}: the snapshot moved");
        let after = read(&mut writer);
        assert_ne!(after, before, "{using}");
        reader.execute("COMMIT").unwrap();
        assert_eq!(read(&mut reader), after, "{using}");
        writer.execute("VACUUM").unwrap();
        assert_eq!(read(&mut reader), after, "{using}: vacuum changed the rows");
        assert_eq!(
            int(&mut reader, "SELECT COUNT(*) FROM t"),
            3000 - (0..3000).filter(|i| i % 7 == 0).count() as i64 + 1,
            "{using}"
        );
    }
}
