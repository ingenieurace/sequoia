//! A TCP server for a [`SharedDatabase`]: a thread for each connection, with
//! a session of its own (see `shared.rs`), as MySQL runs them. Past
//! `max_connections`, a connection is refused with an error.
//!
//! The protocol is lines of text, as the shell reads them, so that `nc`
//! will do for a client:
//!
//! ```text
//! client                              server
//!                                     Sequoia server ...        greeting
//!                                     .end
//! SELECT name                         (nothing yet: no ';')
//! FROM t;                             name                      the result,
//!                                     ----                      as the shell
//!                                     ann                       prints it
//!                                     .end
//! BEGIN;                              transaction started
//!                                     .end*                     * = in a transaction
//! .exit                               (closes the connection)
//! ```
//!
//! The client sends statements, a line ending with `;` running all of them
//! up to it, or a command, a line starting with `.` (`.tables`, `.model`,
//! `.workers`, `.exit`). Each answer ends with a line `.end`, or `.end*` when the
//! session is in a transaction; any line of it that starts with `.` has
//! another `.` put before it, which the client takes off.

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::executor::model::Model;
use crate::shared::{Session, SharedDatabase};
use crate::sql::parse;

/// The port the shell serves on and connects to by default.
pub const PORT: u16 = 1024;

/// Connections a server takes at once by default.
pub const MAX_CONNECTIONS: usize = 64;

const GREETING: &str =
    "Sequoia server. End statements with ';'. Commands: .tables, .model, .workers, .exit";

pub struct Server {
    db: SharedDatabase,
    listener: TcpListener,
    max_connections: usize,
    open: Arc<AtomicUsize>,
}

impl Server {
    /// Listens on `addr` for connections to `db`, taking at most
    /// `max_connections` at once.
    pub fn bind(
        db: SharedDatabase,
        addr: impl ToSocketAddrs,
        max_connections: usize,
    ) -> io::Result<Server> {
        Ok(Server {
            db,
            listener: TcpListener::bind(addr)?,
            max_connections,
            open: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Takes connections, each in a thread of its own, until the listener fails.
    pub fn run(&self) -> io::Result<()> {
        for (n, stream) in self.listener.incoming().enumerate() {
            let stream = stream?;
            if self.open.fetch_add(1, Ordering::SeqCst) >= self.max_connections {
                self.open.fetch_sub(1, Ordering::SeqCst);
                let message = format!(
                    "error: too many connections ({} at most)",
                    self.max_connections
                );
                let _ = answer(&mut BufWriter::new(&stream), &message, false);
                continue;
            }
            let open = Open(self.open.clone());
            let session = self.db.session();
            let spawned = std::thread::Builder::new()
                .name(format!("sequoia-connection-{n}"))
                .spawn(move || {
                    let _open = open;
                    let _ = serve(session, stream);
                });
            if let Err(e) = spawned {
                eprintln!("error: no thread for a connection: {e}");
            }
        }
        Ok(())
    }
}

/// A connection counted open until it is dropped.
struct Open(Arc<AtomicUsize>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Answers one connection until it sends `.exit` or closes. Dropping the
/// session rolls back a transaction left open.
fn serve(mut session: Session, stream: TcpStream) -> io::Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    answer(&mut writer, GREETING, false)?;
    let mut buffer = String::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if buffer.is_empty() && trimmed.starts_with('.') {
            let Some(text) = command(&mut session, trimmed) else {
                break;
            };
            answer(&mut writer, &text, session.in_transaction())?;
            continue;
        }
        buffer.push_str(&line);
        buffer.push('\n');
        if trimmed.ends_with(';') {
            let text = statements(&mut session, &buffer);
            buffer.clear();
            answer(&mut writer, &text, session.in_transaction())?;
        }
    }
    Ok(())
}

/// Runs statements as the shell does, stopping at the first error, and
/// returns what it would print.
fn statements(session: &mut Session, sql: &str) -> String {
    let statements = match parse(sql) {
        Ok(statements) => statements,
        Err(e) => return format!("error: {e}"),
    };
    let mut out = Vec::new();
    for statement in statements {
        match session.run(statement) {
            Ok(result) => out.push(result.to_string()),
            Err(e) => {
                out.push(format!("error: {e}"));
                break;
            }
        }
    }
    out.join("\n")
}

/// Runs a command line, or returns None for `.exit`.
fn command(session: &mut Session, line: &str) -> Option<String> {
    let (name, arg) = match line.split_once(char::is_whitespace) {
        Some((name, arg)) => (name, Some(arg.trim())),
        None => (line, None),
    };
    Some(match (name, arg) {
        (".exit" | ".quit", _) => return None,
        (".tables", _) => match session.table_names() {
            Ok(names) => names.join("\n"),
            Err(e) => format!("error: {e}"),
        },
        (".model", None) => match session.execution_model() {
            Some(model) => model.to_string(),
            None => "auto (each query in the model that suits it)".into(),
        },
        (".workers", None) => session.join_workers().to_string(),
        (".workers", Some(n)) => match n.parse() {
            Ok(n) if n > 0 => {
                session.set_join_workers(n);
                String::new()
            }
            _ => format!("a hash join needs a number of workers above 0: {n}"),
        },
        (".model", Some("auto")) => {
            session.set_execution_model(None);
            String::new()
        }
        (".model", Some(name)) => match Model::from_name(name) {
            Some(model) => {
                session.set_execution_model(Some(model));
                String::new()
            }
            None => {
                format!("unknown model: {name} (auto, iterator, materialization or vectorized)")
            }
        },
        _ => format!("unknown command: {line}"),
    })
}

/// Writes an answer: its lines, each starting with `.` given another, then
/// the end line.
fn answer(writer: &mut impl Write, text: &str, in_transaction: bool) -> io::Result<()> {
    for line in text.lines() {
        if line.starts_with('.') {
            writer.write_all(b".")?;
        }
        writeln!(writer, "{line}")?;
    }
    writeln!(writer, "{}", if in_transaction { ".end*" } else { ".end" })?;
    writer.flush()
}

/// A connection to a server, as the shell's `--connect` uses it.
pub struct Client {
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
}

/// What the server answered: the text, and whether the session is now in a
/// transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub text: String,
    pub in_transaction: bool,
}

impl Client {
    /// Connects, and returns the server's greeting, which is an error if it
    /// refused the connection.
    pub fn connect(addr: impl ToSocketAddrs) -> io::Result<(Client, Answer)> {
        let stream = TcpStream::connect(addr)?;
        let mut client = Client {
            reader: BufReader::new(stream.try_clone()?),
            writer: BufWriter::new(stream),
        };
        let greeting = client.read()?;
        Ok((client, greeting))
    }

    /// Sends a command line, or lines of statements of which only the last
    /// ends with `;`, and returns the answer.
    pub fn send(&mut self, input: &str) -> io::Result<Answer> {
        for line in input.lines() {
            writeln!(self.writer, "{line}")?;
        }
        self.writer.flush()?;
        self.read()
    }

    /// Sends `.exit`, which closes the connection.
    pub fn exit(mut self) -> io::Result<()> {
        writeln!(self.writer, ".exit")?;
        self.writer.flush()
    }

    fn read(&mut self) -> io::Result<Answer> {
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the server closed the connection",
                ));
            }
            let line = line.trim_end_matches(['\r', '\n']);
            match line {
                ".end" | ".end*" => {
                    return Ok(Answer {
                        text: lines.join("\n"),
                        in_transaction: line == ".end*",
                    });
                }
                _ => lines.push(line.strip_prefix('.').unwrap_or(line).to_string()),
            }
        }
    }
}
