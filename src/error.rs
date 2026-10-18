use std::fmt;

/// A single error type shared by every layer of the database.
#[derive(Debug, Clone, PartialEq)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

/// Shorthand for returning an error with a message.
pub fn err<T>(message: impl Into<String>) -> Result<T> {
    Err(Error(message.into()))
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(format!("io error: {e}"))
    }
}
