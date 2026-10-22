//! The TCP server: a thread and a session for each connection.

use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};

use sequoia::SharedDatabase;
use sequoia::server::{Answer, Client, Server};

/// A server of a new in-memory database, running in a thread of its own.
fn start(max_connections: usize) -> SocketAddr {
    let server = Server::bind(SharedDatabase::in_memory(), "127.0.0.1:0", max_connections).unwrap();
    let addr = server.local_addr().unwrap();
    thread::spawn(move || server.run());
    addr
}

fn connect(addr: SocketAddr) -> Client {
    let (client, greeting) = Client::connect(addr).unwrap();
    assert!(greeting.text.starts_with("Sequoia server"), "{greeting:?}");
    client
}

fn send(client: &mut Client, input: &str) -> Answer {
    client.send(input).unwrap()
}

#[test]
fn connections_share_the_database() {
    let addr = start(8);
    let (mut a, mut b) = (connect(addr), connect(addr));
    send(&mut a, "CREATE TABLE t (id INT PRIMARY KEY, name TEXT);");
    // Statements over several lines, the last ending with ';'. A line of the
    // answer that starts with '.' comes back whole.
    let answer = send(&mut a, "INSERT INTO t VALUES\n(1, '.hidden'),\n(2, 'ann');");
    assert_eq!(answer.text, "2 row(s) inserted");
    let answer = send(&mut b, "SELECT name FROM t ORDER BY id;");
    assert!(
        answer.text.contains(".hidden") && answer.text.contains("ann"),
        "{answer:?}"
    );
    assert_eq!(send(&mut b, ".tables").text, "t");
    let answer = send(&mut b, "SELECT nothing FROM t;");
    assert!(answer.text.starts_with("error:"), "{answer:?}");
    a.exit().unwrap();
    b.exit().unwrap();
}

#[test]
fn a_transaction_holds_off_the_other_connections_until_it_commits() {
    let addr = start(8);
    let mut a = connect(addr);
    send(&mut a, "CREATE TABLE t (id INT PRIMARY KEY);");
    let answer = send(&mut a, "BEGIN; INSERT INTO t VALUES (1);");
    assert!(answer.in_transaction, "{answer:?}");

    let reader = thread::spawn(move || {
        let mut b = connect(addr);
        let start = Instant::now();
        let answer = send(&mut b, "SELECT COUNT(*) FROM t;");
        (answer, start.elapsed())
    });
    thread::sleep(Duration::from_millis(300));
    let answer = send(&mut a, "COMMIT;");
    assert!(!answer.in_transaction);
    let (answer, waited) = reader.join().unwrap();
    // It waited for the commit, and then saw the row.
    assert!(waited >= Duration::from_millis(200), "{waited:?}");
    assert!(
        answer.text.contains('1') && !answer.text.contains("error"),
        "{answer:?}"
    );
}

#[test]
fn a_closed_connection_rolls_back_its_transaction() {
    let addr = start(8);
    let mut a = connect(addr);
    send(&mut a, "CREATE TABLE t (id INT PRIMARY KEY);");
    send(&mut a, "BEGIN; INSERT INTO t VALUES (1);");
    drop(a); // no .exit: the socket just closes
    let mut b = connect(addr);
    let answer = send(&mut b, "SELECT COUNT(*) FROM t;");
    assert!(
        answer.text.contains('0') && !answer.text.contains("error"),
        "{answer:?}"
    );
}

#[test]
fn connections_past_the_most_are_refused() {
    let addr = start(1);
    let a = connect(addr);
    let (_, refused) = Client::connect(addr).unwrap();
    assert!(refused.text.contains("too many connections"), "{refused:?}");
    a.exit().unwrap();
    // Once the first thread has ended, there is room again.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, greeting) = Client::connect(addr).unwrap();
        if greeting.text.starts_with("Sequoia server") {
            break;
        }
        assert!(Instant::now() < deadline, "no room after the first left");
        thread::sleep(Duration::from_millis(20));
    }
}
