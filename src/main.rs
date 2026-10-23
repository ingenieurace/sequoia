//! Interactive shell, server and client:
//!
//! - `sequoia [database-file] [--replacer POLICY]`: a shell on a database
//!   of its own.
//! - `sequoia [database-file] --serve [ADDR] [--max-connections N]`: serves
//!   the database over TCP, a thread for each connection (see `server.rs`).
//! - `sequoia --connect [ADDR]`: a shell on a server's database.
//!
//! An address is `host:port`, a port alone (on 127.0.0.1), or with `=` a
//! host alone (`--serve=0.0.0.0`, on port [`PORT`]); left out, it is
//! 127.0.0.1 on that port.
//!
//! On a terminal, both shells edit lines as `editor.rs` describes: the up
//! and down arrows bring back the lines typed before.

mod editor;

use editor::{Input, Line};
use sequoia::executor::model::Model;
use sequoia::server::{Client, MAX_CONNECTIONS, PORT, Server};
use sequoia::sql::parse;
use sequoia::storage::Policy;
use sequoia::{Database, SharedDatabase};

const USAGE: &str = "usage: sequoia [database-file] [--replacer arc|lru|lru-K|clock]
       sequoia [database-file] [--replacer ...] --serve [ADDR] [--max-connections N]
       sequoia --connect [ADDR]
ADDR is host:port, a port on 127.0.0.1, or (after =) a host on port 1024;
by default 127.0.0.1:1024";
const POLICIES: &str = "arc, lru, lru-K (K >= 1, e.g. lru-2) or clock";

/// What the command line asks for.
#[derive(Debug, Default, PartialEq)]
struct Args {
    path: Option<String>,
    policy: Option<Policy>,
    /// Where to serve the database, rather than open a shell on it.
    serve: Option<String>,
    max_connections: Option<usize>,
    /// The server to open a shell on, rather than a database.
    connect: Option<String>,
}

/// Reads the arguments after the program's name.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        if !arg.starts_with('-') {
            if parsed.path.is_some() {
                return Err(format!("more than one database file: {arg}"));
            }
            parsed.path = Some(arg);
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        let what = match name {
            "--replacer" => "a policy",
            "--serve" | "--connect" => "an address",
            "--max-connections" => "a number",
            _ => return Err(format!("unknown option: {arg}")),
        };
        let takes_address = matches!(name, "--serve" | "--connect");
        let value = match inline {
            Some(value) => value,
            // An address may be left out: the next argument is one only if
            // it looks like one, and may else be the database file.
            None if takes_address => match args.next_if(|next| is_address(next)) {
                Some(value) => value,
                None => PORT.to_string(),
            },
            None => args.next().ok_or(format!("{name} needs {what}"))?,
        };
        match name {
            "--replacer" => {
                let policy = Policy::from_name(&value)
                    .ok_or(format!("unknown replacement policy: {value} ({POLICIES})"))?;
                parsed.policy = Some(policy);
            }
            "--serve" => parsed.serve = Some(address(&value)),
            "--connect" => parsed.connect = Some(address(&value)),
            _ => {
                let n = value
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or(format!("--max-connections needs a number above 0: {value}"))?;
                parsed.max_connections = Some(n);
            }
        }
    }
    let local = parsed.path.is_some() || parsed.policy.is_some() || parsed.serve.is_some();
    if parsed.connect.is_some() && (local || parsed.max_connections.is_some()) {
        return Err("--connect takes no database file and no other option".into());
    }
    if parsed.max_connections.is_some() && parsed.serve.is_none() {
        return Err("--max-connections is for --serve".into());
    }
    Ok(parsed)
}

/// Whether an argument after `--serve` or `--connect` is its address: a
/// port, or `host:port`.
fn is_address(arg: &str) -> bool {
    arg.parse::<u16>().is_ok() || arg.contains(':')
}

/// A port alone means one on 127.0.0.1, and a host alone, port [`PORT`].
fn address(value: &str) -> String {
    match value.parse::<u16>() {
        Ok(port) => format!("127.0.0.1:{port}"),
        Err(_) if value.contains(':') => value.to_string(),
        Err(_) => format!("{value}:{PORT}"),
    }
}

fn fail(e: impl std::fmt::Display) -> ! {
    eprintln!("error: {e}");
    std::process::exit(1);
}

fn main() {
    let args = parse_args(std::env::args().skip(1)).unwrap_or_else(|e| {
        eprintln!("error: {e}\n{USAGE}");
        std::process::exit(2);
    });
    if let Some(addr) = &args.connect {
        let (client, greeting) = Client::connect(addr).unwrap_or_else(|e| fail(e));
        if greeting.text.starts_with("error:") {
            fail(greeting.text.trim_start_matches("error: "));
        }
        println!("{} (at {addr})", greeting.text);
        shell(&mut Remote {
            client: Some(client),
            in_transaction: greeting.in_transaction,
        });
        return;
    }
    if let Some(addr) = &args.serve {
        return serve(&args, addr);
    }

    let opened = match &args.path {
        Some(path) => Database::open(path),
        None => Ok(Database::in_memory()),
    };
    let mut db = opened.unwrap_or_else(|e| fail(e));
    if let Some(policy) = args.policy {
        db.set_replacement_policy(policy);
    }
    println!(
        "Sequoia. End statements with ';'. Commands: .tables, .model, .workers, .replacer, .exit"
    );
    shell(&mut db);
}

/// Serves the database over TCP until the listener fails.
fn serve(args: &Args, addr: &str) {
    let opened = match &args.path {
        Some(path) => SharedDatabase::open(path),
        None => Ok(SharedDatabase::in_memory()),
    };
    let db = opened.unwrap_or_else(|e| fail(e));
    if let Some(policy) = args.policy {
        db.set_replacement_policy(policy);
    }
    let max = args.max_connections.unwrap_or(MAX_CONNECTIONS);
    let server = Server::bind(db, addr, max).unwrap_or_else(|e| fail(e));
    let at = server.local_addr().unwrap_or_else(|e| fail(e));
    let what = args.path.as_deref().unwrap_or("an in-memory database");
    println!("Sequoia serving {what} on {at}, {max} connections at most");
    if let Err(e) = server.run() {
        fail(e);
    }
}

/// What the shell runs what it reads on: a database of its own, or a server's.
trait Backend {
    /// Runs a command, a line starting with `.`. Returns false to exit.
    fn command(&mut self, line: &str) -> bool;
    /// Runs statements, up to a line ending with `;`, and prints what they
    /// return. Returns false to exit.
    fn statements(&mut self, sql: &str) -> bool;
    fn in_transaction(&self) -> bool;
}

/// Reads statements and commands until `.exit` or the end of the input.
/// Ctrl-C drops the line being typed and the statement it is part of.
fn shell(backend: &mut impl Backend) {
    let mut buffer = String::new();
    let mut input = Input::new();
    loop {
        // The prompt shows a transaction in progress with a `*`, as psql does.
        let prompt = match (buffer.is_empty(), backend.in_transaction()) {
            (true, false) => "sequoia> ",
            (true, true) => "sequoia*> ",
            (false, _) => "    ...> ",
        };
        let line = match input.read_line(prompt) {
            Line::Text(line) => line,
            Line::Interrupt => {
                buffer.clear();
                continue;
            }
            Line::Eof => break,
        };
        let trimmed = line.trim();

        if buffer.is_empty() && trimmed.starts_with('.') {
            if !backend.command(trimmed) {
                break;
            }
            continue;
        }

        buffer.push_str(&line);
        buffer.push('\n');
        if !trimmed.ends_with(';') {
            continue;
        }
        let more = backend.statements(&buffer);
        buffer.clear();
        if !more {
            break;
        }
    }
}

impl Backend for Database {
    fn command(&mut self, line: &str) -> bool {
        command(self, line)
    }

    fn statements(&mut self, sql: &str) -> bool {
        match parse(sql) {
            Ok(statements) => {
                for statement in statements {
                    match self.run(statement) {
                        Ok(result) => println!("{result}"),
                        Err(e) => {
                            println!("error: {e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => println!("error: {e}"),
        }
        true
    }

    fn in_transaction(&self) -> bool {
        Database::in_transaction(self)
    }
}

/// A shell on a server's database: it sends what it reads, and prints the answers.
struct Remote {
    /// None once the connection is closed.
    client: Option<Client>,
    in_transaction: bool,
}

impl Remote {
    fn send(&mut self, input: &str) -> bool {
        let Some(client) = &mut self.client else {
            return false;
        };
        match client.send(input) {
            Ok(answer) => {
                if !answer.text.is_empty() {
                    println!("{}", answer.text);
                }
                self.in_transaction = answer.in_transaction;
                true
            }
            Err(e) => {
                println!("error: lost the connection: {e}");
                self.client = None;
                false
            }
        }
    }
}

impl Backend for Remote {
    fn command(&mut self, line: &str) -> bool {
        if matches!(line, ".exit" | ".quit") {
            if let Some(client) = self.client.take() {
                let _ = client.exit();
            }
            return false;
        }
        self.send(line)
    }

    fn statements(&mut self, sql: &str) -> bool {
        self.send(sql)
    }

    fn in_transaction(&self) -> bool {
        self.in_transaction
    }
}

/// Runs a shell command (a line starting with `.`). Returns false to exit.
fn command(db: &mut Database, line: &str) -> bool {
    let (name, arg) = match line.split_once(char::is_whitespace) {
        Some((name, arg)) => (name, Some(arg.trim())),
        None => (line, None),
    };
    match (name, arg) {
        (".exit" | ".quit", _) => return false,
        (".tables", _) => db.table_names().iter().for_each(|name| println!("{name}")),
        (".model", None) => match db.execution_model() {
            Some(model) => println!("{model}"),
            None => println!("auto (each query in the model that suits it)"),
        },
        (".workers", None) => println!(
            "scans on {}, hash joins on {}",
            db.scan_workers(),
            db.join_workers()
        ),
        (".workers", Some(n)) => match n.parse() {
            Ok(n) if n > 0 => {
                db.set_scan_workers(n);
                db.set_join_workers(n);
            }
            _ => println!("a query needs a number of workers above 0: {n}"),
        },
        (".model", Some("auto")) => db.set_execution_model(None),
        (".model", Some(name)) => match Model::from_name(name) {
            Some(model) => db.set_execution_model(Some(model)),
            None => {
                println!("unknown model: {name} (auto, iterator, materialization or vectorized)")
            }
        },
        (".replacer", None) => {
            let (stats, frames) = db.buffer_stats();
            let rate = stats.hit_rate().map_or(String::new(), |rate| {
                format!(" ({:.1}% hits)", rate * 100.0)
            });
            println!("{}, {frames} frames", db.replacement_policy());
            println!(
                "since it was set: {} hits, {} misses{rate}",
                stats.hits, stats.misses
            );
        }
        (".replacer", Some(name)) => match Policy::from_name(name) {
            Some(policy) => db.set_replacement_policy(policy),
            None => println!("unknown replacement policy: {name} ({POLICIES})"),
        },
        _ => println!("unknown command: {line}"),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Result<Args, String> {
        parse_args(line.split_whitespace().map(String::from))
    }

    #[test]
    fn the_replacer_is_picked_on_the_command_line() {
        assert_eq!(args(""), Ok(Args::default()));
        assert_eq!(
            args("my.db"),
            Ok(Args {
                path: Some("my.db".into()),
                ..Args::default()
            })
        );
        let both = Args {
            path: Some("my.db".into()),
            policy: Some(Policy::Clock),
            ..Args::default()
        };
        assert_eq!(args("my.db --replacer clock"), Ok(both));
        assert_eq!(
            args("--replacer=LRU-2"),
            Ok(Args {
                policy: Some(Policy::LruK(2)),
                ..Args::default()
            })
        );
        assert_eq!(
            args("--replacer lru --replacer arc").unwrap().policy,
            Some(Policy::Arc),
            "the last wins"
        );
        assert!(args("--replacer").unwrap_err().contains("needs a policy"));
        assert!(
            args("--replacer fifo")
                .unwrap_err()
                .contains("unknown replacement policy: fifo")
        );
        assert!(args("--frames 10").unwrap_err().contains("unknown option"));
        assert!(args("a.db b.db").unwrap_err().contains("more than one"));
    }

    #[test]
    fn serving_and_connecting_are_picked_on_the_command_line() {
        assert_eq!(
            args("my.db --serve 5433 --max-connections 8"),
            Ok(Args {
                path: Some("my.db".into()),
                serve: Some("127.0.0.1:5433".into()),
                max_connections: Some(8),
                ..Args::default()
            })
        );
        assert_eq!(
            args("--serve=0.0.0.0:7000").unwrap().serve.as_deref(),
            Some("0.0.0.0:7000")
        );
        assert_eq!(
            args("--connect db.example:5433"),
            Ok(Args {
                connect: Some("db.example:5433".into()),
                ..Args::default()
            })
        );
        // Left out, the address is 127.0.0.1 on port 1024, and an argument
        // that is no address is the database file.
        let local = Some("127.0.0.1:1024".to_string());
        assert_eq!(args("--serve").unwrap().serve, local);
        assert_eq!(args("--connect").unwrap().connect, local);
        assert_eq!(
            args("--serve my.db"),
            Ok(Args {
                path: Some("my.db".into()),
                serve: local.clone(),
                ..Args::default()
            })
        );
        assert_eq!(args("--serve --max-connections 2").unwrap().serve, local);
        assert_eq!(
            args("--serve=0.0.0.0").unwrap().serve.as_deref(),
            Some("0.0.0.0:1024")
        );
        assert!(
            args("my.db --connect 5433")
                .unwrap_err()
                .contains("--connect takes no")
        );
        assert!(
            args("--max-connections 4")
                .unwrap_err()
                .contains("is for --serve")
        );
        assert!(
            args("--serve 5433 --max-connections 0")
                .unwrap_err()
                .contains("above 0")
        );
    }
}
