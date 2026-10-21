//! Interactive shell. Usage: `sequoia [database-file]`

use std::io::{self, BufRead, Write};

use sequoia::Database;
use sequoia::executor::model::Model;
use sequoia::sql::parse;

fn main() {
    let opened = match std::env::args().nth(1) {
        Some(path) => Database::open(path),
        None => Ok(Database::in_memory()),
    };
    let mut db = opened.unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    println!("Sequoia. End statements with ';'. Commands: .tables, .model, .exit");
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
            match trimmed {
                ".exit" | ".quit" => break,
                ".tables" => db.table_names().iter().for_each(|name| println!("{name}")),
                ".model" => match db.execution_model() {
                    Some(model) => println!("{model}"),
                    None => println!("auto (each query in the model that suits it)"),
                },
                _ => match trimmed.strip_prefix(".model ").map(str::trim) {
                    Some("auto") => db.set_execution_model(None),
                    Some(name) => match Model::from_name(name) {
                        Some(model) => db.set_execution_model(Some(model)),
                        None => println!("unknown model: {name} (auto, iterator, materialization or vectorized)"),
                    },
                    None => println!("unknown command: {trimmed}"),
                },
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
