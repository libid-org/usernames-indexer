//! Keyset paging, shared by every list the store serves: a page asks for the
//! rows past a cursor in the list's key order, and one row past the limit
//! tells whether another page exists.

use std::str::{
    FromStr,
    Split,
};

/// Text that is not a cursor this list handed out.
#[derive(Debug, thiserror::Error)]
#[error("not a cursor this list handed out")]
pub struct InvalidCursor;

/// One page of a list, as a request asks for it: on every chain or one,
/// past the cursor a previous page handed out, at most `limit` rows.
#[derive(Debug, Clone, Copy)]
pub struct Page<C> {
    /// One chain, or every chain the store holds.
    pub chain: Option<i64>,
    /// The cursor the previous page handed out, if this is not the first.
    pub before: Option<C>,
    /// The most rows the page holds.
    pub limit: i64,
}

/// A page as the store read it: its rows, and the cursor of the next page
/// while there is one.
pub struct Rows<R, C> {
    /// In the list's order.
    pub rows: Vec<R>,
    /// The cursor to pass back as the next page's `before`.
    pub next: Option<C>,
}

/// Where a list's page ends: the key of the last row it served, which the
/// next page reads past.
pub trait Cursor: Sized {
    /// The row the cursor is read from.
    type Row;

    /// The cursor after `row`; `None` only for a stored value its writer
    /// never writes.
    fn after(row: &Self::Row) -> Option<Self>;
}

impl<C: Cursor> Page<C> {
    /// How many rows a lookup reads: one past the limit.
    pub(crate) fn fetch(&self) -> i64 {
        self.limit.saturating_add(1)
    }

    /// The page out of the rows its lookup read: at most `limit`, and the
    /// cursor after the last when the extra row proved more exist.
    pub(crate) fn split(&self, mut rows: Vec<C::Row>) -> Rows<C::Row, C> {
        let limit = usize::try_from(self.limit).unwrap_or_default();
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = more.then(|| rows.last()).flatten().and_then(C::after);
        Rows { rows, next }
    }
}

/// A cursor's text: its parts, `-`-separated, each parsed by its type.
pub(crate) struct CursorText<'a>(Split<'a, char>);

impl<'a> CursorText<'a> {
    pub(crate) fn of(raw: &'a str) -> Self {
        Self(raw.split('-'))
    }

    pub(crate) fn next<T: FromStr>(&mut self) -> Result<T, InvalidCursor> {
        self.0
            .next()
            .ok_or(InvalidCursor)?
            .parse()
            .map_err(|_| InvalidCursor)
    }

    /// The cursor, when nothing follows its last part.
    pub(crate) fn end<T>(mut self, cursor: T) -> Result<T, InvalidCursor> {
        match self.0.next() {
            None => Ok(cursor),
            Some(_) => Err(InvalidCursor),
        }
    }
}
