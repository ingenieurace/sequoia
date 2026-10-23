//! Line editing for the shell: the up and down arrows go through the lines
//! typed before, and the line being typed can be edited where the cursor is.
//!
//! | key                    | does                                         |
//! |------------------------|----------------------------------------------|
//! | Up, Ctrl-P             | the line typed before the one shown          |
//! | Down, Ctrl-N           | the line typed after it, or back to the one being typed |
//! | Left, Right, Ctrl-B/F  | moves the cursor                             |
//! | Home, End, Ctrl-A/E    | to the start or end of the line              |
//! | Backspace, Delete      | deletes before or under the cursor           |
//! | Ctrl-U, Ctrl-K         | deletes up to the cursor, or from it on      |
//! | Ctrl-C                 | drops the line, and the statement it is part of |
//! | Ctrl-D                 | on an empty line, leaves the shell           |
//!
//! Each line entered that is not blank is remembered, unless it repeats the
//! one before; the history lasts as long as the shell. A line taken from
//! the history and changed is entered as a new line: the history keeps the
//! old one.
//!
//! On a terminal, on Unix, the shell reads keys one at a time: while a line
//! is read, `stty` turns off the terminal's own line editing and echo, and
//! turns them on again before the line is run, so what statements print
//! comes out as usual, and the terminal is left as it was even if one
//! panics. Elsewhere (input from a pipe or file, or on Windows) lines are
//! read whole, as typed, with no editing.

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::process::{Command, Stdio};

/// What reading a line found.
#[derive(Debug, PartialEq)]
pub enum Line {
    Text(String),
    /// Ctrl-C: the line was dropped.
    Interrupt,
    /// The end of the input, or Ctrl-D on an empty line.
    Eof,
}

/// Where the shell reads its lines from.
pub enum Input {
    /// Whole lines, with no editing.
    Plain(io::Lines<io::StdinLock<'static>>),
    /// Keys, from a terminal, and the lines entered so far, oldest first.
    Keys { history: Vec<String> },
}

impl Input {
    /// Keys if the input and output are a terminal that `stty` can set,
    /// whole lines otherwise.
    pub fn new() -> Input {
        let terminal = io::stdin().is_terminal() && io::stdout().is_terminal();
        if terminal && cfg!(unix) && stty(&["-g"]).is_some() {
            Input::Keys {
                history: Vec::new(),
            }
        } else {
            Input::Plain(io::stdin().lock().lines())
        }
    }

    /// Shows `prompt` and reads a line after it.
    pub fn read_line(&mut self, prompt: &str) -> Line {
        print!("{prompt}");
        io::stdout().flush().ok();
        match self {
            Input::Plain(lines) => match lines.next() {
                Some(Ok(line)) => Line::Text(line),
                _ => Line::Eof,
            },
            Input::Keys { history } => {
                let Some(_raw) = Raw::enter() else {
                    // The terminal would not change mode: a whole line, then.
                    let mut line = String::new();
                    return match io::stdin().lock().read_line(&mut line) {
                        Ok(n) if n > 0 => Line::Text(line.trim_end_matches(['\r', '\n']).into()),
                        _ => Line::Eof,
                    };
                };
                let mut bytes = io::stdin().lock().bytes().map_while(Result::ok);
                let line = edit(prompt, history, &mut bytes, &mut io::stdout());
                if let Line::Text(text) = &line
                    && !text.trim().is_empty()
                    && history.last() != Some(text)
                {
                    history.push(text.clone());
                }
                line
            }
        }
    }
}

/// Reads keys from `bytes` and edits a line after `prompt` with them, which
/// `out` shows, until it is entered or dropped. `history` is the lines
/// entered before, oldest first.
fn edit(
    prompt: &str,
    history: &[String],
    bytes: &mut impl Iterator<Item = u8>,
    out: &mut impl Write,
) -> Line {
    let mut buffer = Buffer::new(history.len());
    loop {
        let Some(key) = read_key(bytes) else {
            let _ = writeln!(out);
            return Line::Eof;
        };
        let done = buffer.apply(key, history);
        let shown = match done {
            Some(Done::Entered) => writeln!(out),
            Some(Done::Interrupted) => writeln!(out, "^C"),
            Some(Done::Eof) => writeln!(out),
            None => write!(out, "{}", buffer.render(prompt)),
        };
        let _ = shown.and_then(|()| out.flush());
        match done {
            Some(Done::Entered) => return Line::Text(buffer.text()),
            Some(Done::Interrupted) => return Line::Interrupt,
            Some(Done::Eof) => return Line::Eof,
            None => {}
        }
    }
}

/// A key, as the terminal sends it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    /// Ctrl-U: deletes everything before the cursor.
    KillStart,
    /// Ctrl-K: deletes everything from the cursor on.
    KillEnd,
    /// Ctrl-C.
    Interrupt,
    /// Ctrl-D.
    EndOfFile,
    /// A key that does nothing here.
    Ignored,
}

/// Reads one key: a byte, the bytes of a character in UTF-8, or an escape
/// sequence (`ESC [ A` for Up, `ESC [ 3 ~` for Delete, ...). None at the
/// end of the input.
pub fn read_key(bytes: &mut impl Iterator<Item = u8>) -> Option<Key> {
    let byte = bytes.next()?;
    Some(match byte {
        b'\r' | b'\n' => Key::Enter,
        0x7F | 0x08 => Key::Backspace,
        0x01 => Key::Home,
        0x02 => Key::Left,
        0x03 => Key::Interrupt,
        0x04 => Key::EndOfFile,
        0x05 => Key::End,
        0x06 => Key::Right,
        0x0B => Key::KillEnd,
        0x0E => Key::Down,
        0x10 => Key::Up,
        0x15 => Key::KillStart,
        0x1B => escape(bytes),
        byte if byte < 0x20 => Key::Ignored,
        byte => utf8(byte, bytes),
    })
}

/// The key of an escape sequence, after its ESC: `[` or `O`, maybe numbers
/// separated by `;`, and a final byte.
fn escape(bytes: &mut impl Iterator<Item = u8>) -> Key {
    if !matches!(bytes.next(), Some(b'[' | b'O')) {
        return Key::Ignored;
    }
    let mut number = 0u32;
    loop {
        let Some(byte) = bytes.next() else {
            return Key::Ignored;
        };
        match byte {
            b'0'..=b'9' => number = number.saturating_mul(10) + (byte - b'0') as u32,
            b';' => number = 0,
            b'A' => return Key::Up,
            b'B' => return Key::Down,
            b'C' => return Key::Right,
            b'D' => return Key::Left,
            b'H' => return Key::Home,
            b'F' => return Key::End,
            b'~' => {
                return match number {
                    1 | 7 => Key::Home,
                    4 | 8 => Key::End,
                    3 => Key::Delete,
                    _ => Key::Ignored,
                };
            }
            // Any other final byte ends a sequence this does not know.
            0x40..=0x7E => return Key::Ignored,
            _ => {}
        }
    }
}

/// The character whose UTF-8 encoding starts with `first`.
fn utf8(first: u8, bytes: &mut impl Iterator<Item = u8>) -> Key {
    let len = match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => return Key::Ignored,
    };
    let mut encoded = vec![first];
    encoded.extend(bytes.take(len - 1));
    match std::str::from_utf8(&encoded)
        .ok()
        .and_then(|s| s.chars().next())
    {
        Some(c) => Key::Char(c),
        None => Key::Ignored,
    }
}

/// How a line being edited ended.
#[derive(Debug, PartialEq)]
enum Done {
    Entered,
    Interrupted,
    Eof,
}

/// The line being edited, and where it is in the history.
struct Buffer {
    chars: Vec<char>,
    /// Characters before the cursor.
    cursor: usize,
    /// The history line shown; the history's length for the line being typed.
    at: usize,
    /// The line being typed, kept while history lines are shown.
    draft: Vec<char>,
}

impl Buffer {
    fn new(history: usize) -> Buffer {
        Buffer {
            chars: Vec::new(),
            cursor: 0,
            at: history,
            draft: Vec::new(),
        }
    }

    fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Applies a key; returns how the line ended, if it did.
    fn apply(&mut self, key: Key, history: &[String]) -> Option<Done> {
        match key {
            Key::Char(c) => {
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
            }
            Key::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            Key::Delete if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.chars.len(),
            Key::KillStart => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            Key::KillEnd => self.chars.truncate(self.cursor),
            Key::Up if self.at > 0 => {
                if self.at == history.len() {
                    self.draft = std::mem::take(&mut self.chars);
                }
                self.at -= 1;
                self.show(history[self.at].chars().collect());
            }
            Key::Down if self.at < history.len() => {
                self.at += 1;
                let line = match history.get(self.at) {
                    Some(line) => line.chars().collect(),
                    None => std::mem::take(&mut self.draft),
                };
                self.show(line);
            }
            Key::Enter => return Some(Done::Entered),
            Key::Interrupt => return Some(Done::Interrupted),
            Key::EndOfFile if self.chars.is_empty() => return Some(Done::Eof),
            Key::EndOfFile => return self.apply(Key::Delete, history),
            _ => {}
        }
        None
    }

    /// Shows another line, with the cursor at its end.
    fn show(&mut self, chars: Vec<char>) {
        self.chars = chars;
        self.cursor = self.chars.len();
    }

    /// What draws the line over the one shown: back to the start of the
    /// row, the prompt and the line, the rest of the row cleared, and the
    /// cursor moved back to where it is in the line.
    fn render(&self, prompt: &str) -> String {
        let line: String = self.chars.iter().collect();
        let back: usize = self.chars[self.cursor..].iter().map(|&c| width(c)).sum();
        let mut out = format!("\r{prompt}{line}\x1b[K");
        if back > 0 {
            out += &format!("\x1b[{back}D");
        }
        out
    }
}

/// How many columns a character takes on a terminal: two for the wide
/// characters of East Asian scripts and most emoji, one for the others.
fn width(c: char) -> usize {
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

/// The terminal with its own line editing, echo and signal keys off, until
/// dropped, which sets it back as it was.
struct Raw {
    saved: String,
}

impl Raw {
    fn enter() -> Option<Raw> {
        let saved = stty(&["-g"])?;
        // Output is left as it is, so a newline still starts a new row.
        stty(&[
            "-icanon", "-echo", "-isig", "-iexten", "-ixon", "min", "1", "time", "0",
        ])?;
        Some(Raw { saved })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = stty(&[&self.saved]);
    }
}

/// Runs `stty` on the terminal the shell reads from, and returns what it
/// printed, or None if it failed.
fn stty(args: &[&str]) -> Option<String> {
    let output = Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Key> {
        let mut bytes = bytes.iter().copied();
        std::iter::from_fn(|| read_key(&mut bytes)).collect()
    }

    /// The line `edit` makes of these bytes, with this history, and what it showed.
    fn edited(bytes: &[u8], history: &[&str]) -> (Line, String) {
        let history: Vec<String> = history.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        let line = edit("> ", &history, &mut bytes.iter().copied(), &mut out);
        (line, String::from_utf8(out).unwrap())
    }

    fn text(bytes: &[u8], history: &[&str]) -> Line {
        edited(bytes, history).0
    }

    #[test]
    fn keys_are_read_from_bytes_and_escape_sequences() {
        assert_eq!(
            keys(b"a\x1b[A\x1b[B\x1b[C\x1b[D\x1bOH\x1b[F\x1b[3~\x1b[1~\x1b[4~\r\n"),
            [
                Key::Char('a'),
                Key::Up,
                Key::Down,
                Key::Right,
                Key::Left,
                Key::Home,
                Key::End,
                Key::Delete,
                Key::Home,
                Key::End,
                Key::Enter,
                Key::Enter,
            ]
        );
        // Modifiers (Ctrl-Up is ESC [ 1 ; 5 A), unknown sequences, control bytes.
        assert_eq!(
            keys(b"\x1b[1;5A\x1b[15~\x1b[Z\x07\x7f\x03\x04"),
            [
                Key::Up,
                Key::Ignored,
                Key::Ignored,
                Key::Ignored,
                Key::Backspace,
                Key::Interrupt,
                Key::EndOfFile,
            ]
        );
        assert_eq!(
            keys("é世🌲".as_bytes()),
            [Key::Char('é'), Key::Char('世'), Key::Char('🌲')]
        );
        assert_eq!(keys(&[0xFF, b'x']), [Key::Ignored, Key::Char('x')]);
        assert_eq!(keys(b"\x1b["), [Key::Ignored], "cut short");
    }

    #[test]
    fn up_and_down_go_through_the_history() {
        let history = ["SELECT 1 FROM t;", "SELECT 2 FROM t;"];
        assert_eq!(text(b"\x1b[A\r", &history), Line::Text(history[1].into()));
        assert_eq!(
            text(b"\x1b[A\x1b[A\r", &history),
            Line::Text(history[0].into())
        );
        // Past the oldest, it stays there.
        assert_eq!(
            text(b"\x1b[A\x1b[A\x1b[A\x1b[A\r", &history),
            Line::Text(history[0].into())
        );
        // Down from the history brings back what was being typed.
        assert_eq!(
            text(b"draft\x1b[A\x1b[A\x1b[B\x1b[B\r", &history),
            Line::Text("draft".into())
        );
        assert_eq!(text(b"x\x1b[B\r", &history), Line::Text("x".into()));
        // A line from the history can be changed before it is entered.
        assert_eq!(
            text(b"\x1b[A\x7f\x7fu;\r", &history),
            Line::Text("SELECT 2 FROM u;".into())
        );
        assert_eq!(text(b"\x1b[A\r", &[]), Line::Text(String::new()));
    }

    #[test]
    fn the_cursor_moves_and_edits_where_it_is() {
        assert_eq!(text(b"ac\x1b[Db\r", &[]), Line::Text("abc".into()));
        assert_eq!(text(b"abc\x01\x1b[3~\r", &[]), Line::Text("bc".into()));
        assert_eq!(text(b"abc\x01x\x05y\r", &[]), Line::Text("xabcy".into()));
        assert_eq!(text(b"abcd\x02\x02\x15\r", &[]), Line::Text("cd".into()));
        assert_eq!(text(b"abcd\x02\x02\x0b\r", &[]), Line::Text("ab".into()));
        assert_eq!(text(b"\x7f\x1b[D\x1b[3~a\r", &[]), Line::Text("a".into()));
        assert_eq!(text(b"ab\x01\x04\r", &[]), Line::Text("b".into()));
    }

    #[test]
    fn a_line_ends_entered_dropped_or_with_the_input() {
        assert_eq!(text(b"abc\x03", &[]), Line::Interrupt);
        assert_eq!(text(b"\x04", &[]), Line::Eof);
        assert_eq!(text(b"abc", &[]), Line::Eof, "the input ends");
        let (_, shown) = edited(b"ab\x1b[D\r", &[]);
        // Each key redraws the line, the cursor one back from its end after Left.
        assert_eq!(shown, "\r> a\x1b[K\r> ab\x1b[K\r> ab\x1b[K\x1b[1D\n");
    }

    #[test]
    fn wide_characters_move_the_cursor_two_columns() {
        let mut buffer = Buffer::new(0);
        for c in "a世b".chars() {
            buffer.apply(Key::Char(c), &[]);
        }
        buffer.apply(Key::Home, &[]);
        assert!(buffer.render("> ").ends_with("\x1b[4D"));
        buffer.apply(Key::Right, &[]);
        assert!(buffer.render("> ").ends_with("\x1b[3D"));
    }
}
