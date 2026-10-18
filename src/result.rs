use std::fmt;

use crate::catalog::table::Row;

/// The outcome of executing one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// A status message, e.g. "2 row(s) inserted".
    Message(String),
    /// Rows returned by SELECT.
    Rows { columns: Vec<String>, rows: Vec<Row> },
}

impl fmt::Display for QueryResult {
    /// Prints messages as-is and rows as an ASCII table.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (columns, rows) = match self {
            QueryResult::Message(message) => return f.write_str(message),
            QueryResult::Rows { columns, rows } => (columns, rows),
        };

        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|row| row.iter().map(|v| v.to_string()).collect())
            .collect();
        let widths: Vec<usize> = columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                cells.iter().map(|row| row[i].chars().count()).fold(name.chars().count(), usize::max)
            })
            .collect();

        let separator: Vec<String> = widths.iter().map(|w| "-".repeat(w + 2)).collect();
        let separator = format!("+{}+", separator.join("+"));
        let write_line = |f: &mut fmt::Formatter, items: &[String]| {
            let padded: Vec<String> =
                items.iter().zip(&widths).map(|(item, w)| format!(" {item:<w$} ")).collect();
            writeln!(f, "|{}|", padded.join("|"))
        };

        writeln!(f, "{separator}")?;
        write_line(f, columns)?;
        writeln!(f, "{separator}")?;
        for row in &cells {
            write_line(f, row)?;
        }
        writeln!(f, "{separator}")?;
        write!(f, "({} row(s))", rows.len())
    }
}
