use std::fmt;

use crate::error::{Result, err};

const KEYWORDS: &[&str] = &[
    "ALL",
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BOOL",
    "BOOLEAN",
    "BY",
    "CASE",
    "CHECKPOINT",
    "COMMIT",
    "CREATE",
    "CROSS",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FLOAT",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INT",
    "INTEGER",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "OUTER",
    "NATURAL",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "PRIMARY",
    "REAL",
    "RELEASE",
    "RIGHT",
    "ROLLBACK",
    "SAVEPOINT",
    "SELECT",
    "SET",
    "TABLE",
    "TEXT",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WITH",
];

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Upper-cased SQL keyword.
    Keyword(String),
    Ident(String),
    /// `"name"`: a name as written, case and all, which may be a keyword
    /// or hold any character (`""` stands for `"`). It is never read as a
    /// keyword, nor as a word that means something only where it stands.
    QuotedIdent(String),
    Number(i64),
    Real(f64),
    Str(String),
    /// `X'0A1B'`: bytes, two hexadecimal digits for each.
    Blob(Vec<u8>),
    Symbol(&'static str),
}

/// Splits SQL text into tokens.
pub fn tokenize(input: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();

        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && next == Some('-') {
            // Line comment.
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            // Block comment, which may hold others, as in PostgreSQL.
            let mut depth = 0;
            loop {
                match (chars.get(i), chars.get(i + 1)) {
                    (Some('/'), Some('*')) => {
                        depth += 1;
                        i += 2;
                    }
                    (Some('*'), Some('/')) => {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    }
                    (Some(_), _) => i += 1,
                    (None, _) => return err("unterminated /* comment"),
                }
            }
        } else if c == '"' {
            // A quoted name; "" escapes a double quote.
            let mut name = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return err("unterminated quoted name"),
                    Some('"') if chars.get(i + 1) == Some(&'"') => {
                        name.push('"');
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        name.push(ch);
                        i += 1;
                    }
                }
            }
            if name.is_empty() {
                return err("a quoted name cannot be empty");
            }
            tokens.push(Token::QuotedIdent(name));
        } else if c.is_ascii_digit() {
            // Digits, then for a REAL a fraction (`1.5`), an exponent (`1e20`, `2.5E-3`), or both.
            let digits = |i: &mut usize| {
                while *i < chars.len() && chars[*i].is_ascii_digit() {
                    *i += 1;
                }
            };
            let start = i;
            digits(&mut i);
            let mut real = false;
            if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(char::is_ascii_digit) {
                real = true;
                i += 1;
                digits(&mut i);
            }
            if matches!(chars.get(i), Some('e' | 'E')) {
                let after_sign = i + 1 + matches!(chars.get(i + 1), Some('+' | '-')) as usize;
                if chars.get(after_sign).is_some_and(char::is_ascii_digit) {
                    real = true;
                    i = after_sign;
                    digits(&mut i);
                }
            }
            let text: String = chars[start..i].iter().collect();
            match real {
                true => match text.parse::<f64>() {
                    Ok(r) if r.is_finite() => tokens.push(Token::Real(r)),
                    _ => return err(format!("number out of range: {text}")),
                },
                false => match text.parse() {
                    Ok(n) => tokens.push(Token::Number(n)),
                    Err(_) => return err(format!("integer out of range: {text}")),
                },
            }
        } else if matches!(c, 'x' | 'X') && next == Some('\'') {
            // A blob literal: hexadecimal digits in quotes.
            let start = i + 2;
            let Some(len) = chars[start..].iter().position(|&ch| ch == '\'') else {
                return err("unterminated blob literal");
            };
            let digits: String = chars[start..start + len].iter().collect();
            let bytes: Option<Vec<u8>> = (digits.len() % 2 == 0)
                .then(|| {
                    (0..digits.len())
                        .step_by(2)
                        .map(|at| u8::from_str_radix(digits.get(at..at + 2)?, 16).ok())
                        .collect()
                })
                .flatten();
            match bytes {
                Some(bytes) if digits.is_ascii() => tokens.push(Token::Blob(bytes)),
                _ => return err(format!("invalid blob literal: X'{digits}'")),
            }
            i = start + len + 1;
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let upper = word.to_uppercase();
            tokens.push(if KEYWORDS.contains(&upper.as_str()) {
                Token::Keyword(upper)
            } else {
                Token::Ident(word)
            });
        } else if c == '\'' {
            // String literal; '' escapes a single quote.
            let mut s = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return err("unterminated string literal"),
                    Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            tokens.push(Token::Str(s));
        } else {
            let (symbol, len) = match (c, next) {
                ('!', Some('=')) | ('<', Some('>')) => ("!=", 2),
                ('<', Some('=')) => ("<=", 2),
                ('>', Some('=')) => (">=", 2),
                ('<', _) => ("<", 1),
                ('>', _) => (">", 1),
                ('=', _) => ("=", 1),
                ('(', _) => ("(", 1),
                (')', _) => (")", 1),
                (',', _) => (",", 1),
                ('.', _) => (".", 1),
                (';', _) => (";", 1),
                ('*', _) => ("*", 1),
                ('+', _) => ("+", 1),
                ('-', _) => ("-", 1),
                ('/', _) => ("/", 1),
                ('%', _) => ("%", 1),
                ('|', Some('|')) => ("||", 2),
                _ => return err(format!("unexpected character: {c}")),
            };
            tokens.push(Token::Symbol(symbol));
            i += len;
        }
    }
    Ok(tokens)
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Token::Keyword(s) | Token::Ident(s) => write!(f, "{s}"),
            Token::QuotedIdent(s) => f.write_str(&quote_ident(s)),
            Token::Number(n) => write!(f, "{n}"),
            Token::Real(r) => write!(f, "{r:?}"),
            Token::Str(s) => write!(f, "'{s}'"),
            Token::Blob(b) => write!(f, "X'{}'", crate::types::hex(b)),
            Token::Symbol(s) => write!(f, "{s}"),
        }
    }
}

/// A name as SQL writes it so that it reads back as itself: as it is if it
/// is a plain word that is not a keyword, else in double quotes.
pub fn quote_ident(name: &str) -> String {
    let mut chars = name.chars();
    let plain = chars
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
        && !KEYWORDS.contains(&name.to_uppercase().as_str());
    match plain {
        true => name.to_string(),
        false => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}
