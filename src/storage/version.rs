//! Row versions: the header every stored row starts with, for multiversion
//! concurrency control (see `mvcc/mod.rs`).
//!
//! ```text
//! 0..8    xmin  the transaction that wrote this version
//! 8..16   xmax  the transaction that deleted it, or replaced it by a newer
//!               one; 0 (NO_XID) while none has
//! 16..    the row (see `tuple.rs`)
//! ```
//!
//! An UPDATE does not change a row in place: it sets the old version's xmax
//! and writes a new version, so a transaction that began before still reads
//! the old one. Which versions a reader sees is up to its snapshot, through
//! [`Visibility`]; the storage layer only keeps the headers.

use crate::error::{Result, err};

/// A transaction id. Ids are handed out in order and never reused.
pub type Xid = u64;

/// The xmax of a version no transaction has deleted.
pub const NO_XID: Xid = 0;

/// The xmin of a version every transaction sees: one written before the
/// database had versions, or moved into a column store's row group.
pub const FROZEN: Xid = 1;

/// The first id handed to a transaction.
pub const FIRST_XID: Xid = 2;

/// Bytes of the header before each row.
pub const HEADER: usize = 16;

/// A version's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub xmin: Xid,
    pub xmax: Xid,
}

impl Header {
    /// The header of a new version, written by `xmin`.
    pub fn new(xmin: Xid) -> Self {
        Header { xmin, xmax: NO_XID }
    }

    /// Reads the header a stored row starts with, and the row after it.
    pub fn read(bytes: &[u8]) -> Result<(Header, &[u8])> {
        if bytes.len() < HEADER {
            return err("corrupted row: no version header");
        }
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        Ok((
            Header {
                xmin: word(0),
                xmax: word(8),
            },
            &bytes[HEADER..],
        ))
    }

    /// A row stored with this header.
    pub fn with(self, row: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER + row.len());
        bytes.extend_from_slice(&self.xmin.to_le_bytes());
        bytes.extend_from_slice(&self.xmax.to_le_bytes());
        bytes.extend_from_slice(row);
        bytes
    }

    /// Sets the xmax of a stored row, in place.
    pub fn set_xmax(bytes: &mut [u8], xmax: Xid) {
        bytes[8..HEADER].copy_from_slice(&xmax.to_le_bytes());
    }
}

/// Which versions a statement reads: a snapshot's (see `mvcc/snapshot.rs`),
/// or another rule for a check that must see more, such as whether a key
/// is taken. An error stops the scan.
pub trait Visibility: Sync {
    fn visible(&self, header: Header) -> Result<bool>;
}

/// What a vacuum does with a version (see [`Horizon`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    /// Every transaction, running or to come, sees it: it may be frozen.
    Settled,
    /// No transaction, running or to come, sees it: it may be removed.
    Dead,
    /// Some transaction may see it, and another not: it stays as it is.
    Kept,
}

/// Tells each version's [`Fate`], by the transactions running when asked.
pub trait Horizon {
    fn fate(&self, header: Header) -> Fate;
}

/// Every version, whatever its header says: for building an index, which
/// holds an entry for each, and for an upgrade, which reads tables that
/// have no headers.
#[derive(Clone, Copy)]
pub struct All;

impl Visibility for All {
    fn visible(&self, _: Header) -> Result<bool> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_is_read_back_and_its_xmax_set_in_place() {
        let mut bytes = Header::new(7).with(b"row");
        assert_eq!(bytes.len(), HEADER + 3);
        let (header, row) = Header::read(&bytes).unwrap();
        assert_eq!(
            header,
            Header {
                xmin: 7,
                xmax: NO_XID
            }
        );
        assert_eq!(row, b"row");
        Header::set_xmax(&mut bytes, 9);
        assert_eq!(Header::read(&bytes).unwrap().0, Header { xmin: 7, xmax: 9 });
        assert!(Header::read(b"short").is_err());
    }
}
