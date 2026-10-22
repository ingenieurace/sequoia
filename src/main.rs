//! Interactive shell. Usage: `sequoia [database-file] [--replacer POLICY]`

use std::io::{self, BufRead, Write};

use sequoia::Database;
use sequoia::executor::model::Model;
use sequoia::sql::parse;
use sequoia::storage::Policy;

const USAGE: &str = "usage: sequoia [database-file] [--replacer arc|lru|lru-K|clock]";
const POLICIES: &str = "arc, lru, lru-K (K >= 1, e.g. lru-2) or clock";

/// What the command line asks for.
#[derive(Debug, Default, PartialEq)]
struct Args {
    path: Option<String>,
    policy: Option<Policy>,
}

/// Reads the arguments after the program's name.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let policy = match arg.strip_prefix("--replacer") {
            Some("") => args.next().ok_or("--replacer needs a policy")?,
            Some(rest) if rest.starts_with('=') => rest[1..].to_string(),
            _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}")),
            _ if parsed.path.is_some() => {
                return Err(format!("more than one database file: {arg}"));
            }
            _ => {
                parsed.path = Some(arg);
                continue;
            }
        };
        let policy = Policy::from_name(&policy)
            .ok_or(format!("unknown replacement policy: {policy} ({POLICIES})"))?;
        parsed.policy = Some(policy);
    }
    Ok(parsed)
}

fn main() {
    let args = parse_args(std::env::args().skip(1)).unwrap_or_else(|e| {
        eprintln!("error: {e}\n{USAGE}");
        std::process::exit(2);
    });
    let opened = match args.path {
        Some(path) => Database::open(path),
        None => Ok(Database::in_memory()),
    };
    let mut db = opened.unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });
    if let Some(policy) = args.policy {
        db.set_replacement_policy(policy);
    }

    println!("Sequoia. End statements with ';'. Commands: .tables, .model, .replacer, .exit");
    let mut buffer = String::new();
    let mut lines = io::stdin().lock().lines();

    loop {
        // The prompt shows a transaction in progress with a `*`, as psql does.
        let prompt = match (buffer.is_empty(), db.in_transaction()) {
            (true, false) => "sequoia> ",
            (true, true) => "sequoia*> ",
            (false, _) => "    ...> ",
        };
        print!("{prompt}");
        io::stdout().flush().ok();
        let Some(Ok(line)) = lines.next() else { break };
        let trimmed = line.trim();

        if buffer.is_empty() && trimmed.starts_with('.') {
            if !command(&mut db, trimmed) {
                break;
            }
            continue;
        }

        buffer.push_str(&line);
        buffer.push('\n');
        if !trimmed.ends_with(';') {
            continue;
        }

        match parse(&buffer) {
            Ok(statements) => {
                for statement in statements {
                    match db.run(statement) {
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
        buffer.clear();
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
                policy: None
            })
        );
        let both = Args {
            path: Some("my.db".into()),
            policy: Some(Policy::Clock),
        };
        assert_eq!(args("my.db --replacer clock"), Ok(both));
        assert_eq!(
            args("--replacer=LRU-2"),
            Ok(Args {
                path: None,
                policy: Some(Policy::LruK(2))
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
}
