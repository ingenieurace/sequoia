use std::fmt;

use crate::catalog::table::Row;

/// How many columns text takes on a terminal (see [`char_width`]).
pub fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// How many columns a character takes on a terminal: two for the wide
/// characters of East Asian scripts and most emoji, one for the others.
pub fn char_width(c: char) -> usize {
    let wide = matches!(c as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

/// The outcome of executing one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// A status message, e.g. "2 row(s) inserted".
    Message(String),
    /// Rows returned by SELECT.
    Rows {
        columns: Vec<String>,
        rows: Vec<Row>,
    },
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
                cells
                    .iter()
                    .map(|row| row[i].chars().count())
                    .fold(name.chars().count(), usize::max)
            })
            .collect();

        let separator: Vec<String> = widths.iter().map(|w| "-".repeat(w + 2)).collect();
        let separator = format!("+{}+", separator.join("+"));
        let write_line = |f: &mut fmt::Formatter, items: &[String]| {
            let padded: Vec<String> = items
                .iter()
                .zip(&widths)
                .map(|(item, w)| format!(" {item:<w$} "))
                .collect();
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
