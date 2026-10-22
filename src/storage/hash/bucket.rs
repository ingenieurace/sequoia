//! A bucket of a hash file: a chain of pages holding entries in no order.
//!
//! ```text
//! 0..2    entry count n
//! 2..4    where the free space starts
//! 4..8    next page of the chain (u32::MAX = none)
//! 8       first page: EMPTY, UNIFORM (every entry has the hash below) or MIXED
//! 9..13   first page: that hash
//! 13..    n x (u16 length, u32 hash, entry bytes)
//! ```
//!
//! The first page's latch guards the whole chain: whoever changes a page of
//! it holds that latch for writing, and whoever reads one, for reading.
//!
//! An entry is added to the first page. When that is full, the first page's
//! entries move to a new page linked after it, so it is empty again; the
//! page id the file points to never changes. A delete closes the gap it
//! leaves; a page it empties is unlinked and freed, the first page by
//! taking over the next one's entries. So a bucket's first page is empty
//! only when the whole bucket is.
//!
//! Whether every entry has one hash tells an extendible file that splitting
//! the bucket would not separate them: they are of one key, or of keys
//! whose hashes are equal.

use crate::error::{Result, err};
use crate::storage::buffer::pool::BufferPool;
use crate::storage::io::disk::{
    PAGE_USABLE, Page, PageId, decode_id, encode_id, read_u32, write_u32,
};

pub const HEADER: usize = 13;
/// The bytes of a page that entries can take.
pub const ROOM: usize = PAGE_USABLE - HEADER;

const EMPTY: u8 = 0;
const UNIFORM: u8 = 1;
const MIXED: u8 = 2;

/// The bytes an entry takes in a page.
pub fn size(entry: &[u8]) -> usize {
    6 + entry.len()
}

/// Makes `page` an empty bucket of one page.
pub fn init(page: &mut Page) {
    page[..HEADER].fill(0);
    set_count(page, 0);
    set_end(page, HEADER);
    set_next(page, None);
    page[8] = EMPTY;
}

pub fn is_empty(page: &Page) -> bool {
    count(page) == 0
}

/// The hash every entry of the bucket whose first page this is has, if one does.
pub fn uniform(page: &Page) -> Option<u32> {
    (page[8] == UNIFORM).then(|| read_u32(page, 9))
}

/// Whether an entry fits in the bucket's first page.
pub fn fits(page: &Page, entry: &[u8]) -> bool {
    end(page) + size(entry) <= PAGE_USABLE
}

/// Adds an entry to the bucket whose first page is `head`; if the page is
/// full, first moves its entries to a new page after it.
pub fn insert(pool: &BufferPool, head: &mut Page, hash: u32, entry: &[u8]) -> Result<()> {
    if !fits(head, entry) {
        let mut page = pool.new_page()?;
        page.copy_from_slice(head);
        let id = page.id();
        drop(page);
        let state = (head[8], read_u32(head, 9));
        init(head);
        set_next(head, Some(id));
        head[8] = state.0;
        write_u32(head, 9, state.1);
    }
    push(head, hash, entry);
    head[8] = match (head[8], read_u32(head, 9)) {
        (EMPTY, _) => {
            write_u32(head, 9, hash);
            UNIFORM
        }
        (UNIFORM, h) if h == hash => UNIFORM,
        _ => MIXED,
    };
    Ok(())
}

/// Appends an entry to a page with room for it.
fn push(page: &mut Page, hash: u32, entry: &[u8]) {
    let at = end(page);
    page[at..at + 2].copy_from_slice(&(entry.len() as u16).to_le_bytes());
    write_u32(page, at + 2, hash);
    page[at + 6..at + size(entry)].copy_from_slice(entry);
    set_end(page, at + size(entry));
    set_count(page, count(page) + 1);
}

/// Adds the entries of the bucket whose first page is `head` that start
/// with `key`, whose hash is `hash`, to `out`.
pub fn find(
    pool: &BufferPool,
    head: &Page,
    hash: u32,
    key: &[u8],
    out: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let mut collect = |page: &Page| -> Result<Option<PageId>> {
        for (h, entry, _) in entries(page)? {
            if h == hash && entry.starts_with(key) {
                out.push(entry.to_vec());
            }
        }
        Ok(next(page))
    };
    let mut next = collect(head)?;
    while let Some(id) = next {
        next = collect(&*pool.fetch_read(id)?)?;
    }
    Ok(())
}

/// Removes an entry from the bucket whose first page is `head`. Returns
/// false if it was not there.
pub fn delete(pool: &BufferPool, head: &mut Page, hash: u32, entry: &[u8]) -> Result<bool> {
    if remove(head, hash, entry)? {
        if is_empty(head)
            && let Some(id) = next(head)
        {
            // The next page's entries move up, so the first page is empty only with the bucket.
            let state = (head[8], read_u32(head, 9));
            head.copy_from_slice(&*pool.fetch_read(id)?);
            head[8] = state.0;
            write_u32(head, 9, state.1);
            pool.delete_page(id)?;
        }
        if is_empty(head) {
            head[8] = EMPTY;
        }
        return Ok(true);
    }
    let (mut before, mut id) = (None, next(head));
    while let Some(this) = id {
        let mut page = pool.fetch_write(this)?;
        if remove(&mut page, hash, entry)? {
            if is_empty(&page) {
                let after = next(&page);
                drop(page);
                match before {
                    None => set_next(head, after),
                    Some(before) => set_next(&mut *pool.fetch_write(before)?, after),
                }
                pool.delete_page(this)?;
            }
            return Ok(true);
        }
        (before, id) = (Some(this), next(&page));
    }
    Ok(false)
}

/// Removes an entry from one page, closing the gap. Returns false if it was not there.
fn remove(page: &mut Page, hash: u32, entry: &[u8]) -> Result<bool> {
    let Some((at, len)) = entries(page)?
        .find(|(h, e, _)| *h == hash && *e == entry)
        .map(|(_, e, at)| (at, e.len()))
    else {
        return Ok(false);
    };
    let (end, size) = (end(page), 6 + len);
    page.copy_within(at + size..end, at);
    set_end(page, end - size);
    set_count(page, count(page) - 1);
    Ok(true)
}

/// Every entry of the bucket whose first page is `head`, with its hash.
pub fn all(pool: &BufferPool, head: &Page) -> Result<Vec<(u32, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut collect = |page: &Page| -> Result<Option<PageId>> {
        out.extend(entries(page)?.map(|(h, e, _)| (h, e.to_vec())));
        Ok(next(page))
    };
    let mut next = collect(head)?;
    while let Some(id) = next {
        next = collect(&*pool.fetch_read(id)?)?;
    }
    Ok(out)
}

/// Makes the bucket whose first page is `head` hold exactly `entries`:
/// fills its pages anew, reusing those of its chain, adding pages if it
/// needs more and freeing those it no longer does.
pub fn rewrite(pool: &BufferPool, head: &mut Page, entries: &[(u32, Vec<u8>)]) -> Result<()> {
    let mut old = Vec::new();
    let mut id = next(head);
    while let Some(this) = id {
        old.push(this);
        id = next(&*pool.fetch_read(this)?);
    }
    old.reverse(); // reused from the front, popped from the back

    // Fill pages from the last of the chain to the first, each linking to
    // the one filled before it, so the first page is filled last, and the
    // overflow pages are full.
    init(head);
    let mut next_page = None;
    let mut chunk = Vec::new();
    let mut chunks = Vec::new();
    let mut bytes = 0;
    for (hash, entry) in entries.iter().rev() {
        if bytes + size(entry) > ROOM {
            chunks.push(std::mem::take(&mut chunk));
            bytes = 0;
        }
        bytes += size(entry);
        chunk.push((*hash, entry.as_slice()));
    }
    for chunk in &chunks {
        let mut page = match old.pop() {
            Some(id) => pool.fetch_write(id)?,
            None => pool.new_page()?,
        };
        init(&mut page);
        set_next(&mut page, next_page);
        for (hash, entry) in chunk {
            push(&mut page, *hash, entry);
        }
        next_page = Some(page.id());
    }
    set_next(head, next_page);
    for (hash, entry) in &chunk {
        push(head, *hash, entry);
    }
    for id in old {
        pool.delete_page(id)?;
    }
    let mut hashes = entries.iter().map(|(h, _)| *h);
    match hashes.next() {
        None => head[8] = EMPTY,
        Some(first) => {
            head[8] = if hashes.all(|h| h == first) {
                UNIFORM
            } else {
                MIXED
            };
            write_u32(head, 9, first);
        }
    }
    Ok(())
}

/// Frees every page of the bucket whose first page is `head`, the first one
/// too. No one must be able to reach the bucket any more, but a reader that
/// found it before may still be leaving its first page (and so its chain),
/// which this waits out.
pub fn destroy(pool: &BufferPool, head: PageId) -> Result<()> {
    let mut id = Some(head);
    while let Some(this) = id {
        id = next(&*pool.fetch_read(this)?);
        pool.delete_unreachable_page(this)?;
    }
    Ok(())
}

/// The entries of one page: hash, bytes, and where each starts.
fn entries(page: &Page) -> Result<Entries<'_>> {
    let end = end(page);
    if !(HEADER..=PAGE_USABLE).contains(&end) {
        return err("corrupted hash bucket: bad free space offset");
    }
    Ok(Entries {
        page: &page[..end],
        at: HEADER,
        left: count(page),
    })
}

/// Reads entries one after another, without copying them.
struct Entries<'a> {
    /// The page, up to where its free space starts.
    page: &'a [u8],
    at: usize,
    left: usize,
}

impl<'a> Iterator for Entries<'a> {
    type Item = (u32, &'a [u8], usize);

    /// The next entry; none past the free space, as in a corrupted page.
    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        let (page, at) = (self.page, self.at);
        let len = u16::from_le_bytes(page.get(at..at + 2)?.try_into().unwrap()) as usize;
        let hash = u32::from_le_bytes(page.get(at + 2..at + 6)?.try_into().unwrap());
        let entry = page.get(at + 6..at + 6 + len)?;
        self.at += 6 + len;
        self.left -= 1;
        Some((hash, entry, at))
    }
}

fn count(page: &Page) -> usize {
    u16::from_le_bytes([page[0], page[1]]) as usize
}

fn set_count(page: &mut Page, n: usize) {
    page[0..2].copy_from_slice(&(n as u16).to_le_bytes());
}

fn end(page: &Page) -> usize {
    u16::from_le_bytes([page[2], page[3]]) as usize
}

fn set_end(page: &mut Page, end: usize) {
    page[2..4].copy_from_slice(&(end as u16).to_le_bytes());
}

fn next(page: &Page) -> Option<PageId> {
    decode_id(read_u32(page, 4))
}

fn set_next(page: &mut Page, next: Option<PageId>) {
    write_u32(page, 4, encode_id(next));
}
