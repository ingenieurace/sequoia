use std::fmt;

use crate::error::{Result, err};

const KEYWORDS: &[&str] = &[
    "ALL", "AND", "AS", "ASC", "BEGIN", "BETWEEN", "BOOL", "BOOLEAN", "BY", "CASE", "CHECKPOINT", "COMMIT",
    "CREATE", "CROSS", "DELETE", "DESC", "DISTINCT", "DROP", "ELSE", "END", "EXISTS", "EXPLAIN", "FALSE",
    "FLOAT", "FROM", "GROUP", "HAVING", "IN", "INDEX", "INNER", "INSERT", "INT", "INTEGER", "INTO", "IS", "JOIN", "KEY",
    "LEFT", "LIKE", "LIMIT", "OUTER", "NOT", "NULL", "OFFSET", "ON", "OR", "ORDER", "PRIMARY", "REAL",
    "RELEASE", "ROLLBACK", "SAVEPOINT", "SELECT", "SET", "TABLE", "TEXT", "THEN", "TO", "TRANSACTION", "TRUE", "UNION",
    "UNIQUE", "UPDATE", "USING", "VALUES", "WHEN", "WHERE",
];

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Upper-cased SQL keyword.
    Keyword(String),
    Ident(String),
    Number(i64),
    Real(f64),
    Str(String),
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
            Token::Number(n) => write!(f, "{n}"),
            Token::Real(r) => write!(f, "{r:?}"),
            Token::Str(s) => write!(f, "'{s}'"),
            Token::Symbol(s) => write!(f, "{s}"),
        }
    }
}
