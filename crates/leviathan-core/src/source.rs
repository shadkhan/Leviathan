//! The byte-range contract between the core and whatever is holding the bytes.
//!
//! The core is sans-IO: it does not open files, does not know about `Blob`s, and
//! does not await. When it needs to materialize something it did not store — a
//! key name, a string value, an exact span — it asks the caller for a byte range
//! and re-lexes it. See `DEEP_REASONING.md` C1 and C2.

use core::fmt;

/// Why a byte range could not be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// The requested range extends past the end of the source.
    OutOfRange {
        /// First byte requested.
        start: u64,
        /// Number of bytes requested.
        len: u32,
        /// Total bytes the source can offer.
        available: u64,
    },
    /// The caller's underlying source failed (I/O error, revoked file handle,
    /// the user moved the file out from under us mid-session).
    Unavailable(String),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::OutOfRange {
                start,
                len,
                available,
            } => write!(
                f,
                "byte range {start}..{} extends past end of source ({available} bytes)",
                start.saturating_add(u64::from(*len))
            ),
            SourceError::Unavailable(why) => write!(f, "source unavailable: {why}"),
        }
    }
}

impl core::error::Error for SourceError {}

/// A source that can hand back arbitrary byte ranges of the document.
///
/// Implementations are expected to be *fast for small ranges* — Leviathan asks
/// for kilobytes, never megabytes, and asks often. A `Blob.slice().arrayBuffer()`
/// in a Web Worker and a `pread` on a native file both cost about a millisecond,
/// which is what makes "store nothing, re-read on demand" a winning trade.
///
/// The returned slice borrows from `self`, so only one range is live at a time.
/// This is deliberate: it lets implementations reuse a single scratch buffer
/// instead of allocating per call.
///
/// # Example
///
/// ```
/// use leviathan_core::ByteRange;
///
/// let mut src: &[u8] = br#"{"name":"leviathan"}"#;
/// assert_eq!(src.read(8, 11).unwrap(), br#""leviathan""#);
/// assert!(src.read(100, 4).is_err());
/// ```
pub trait ByteRange {
    /// Return exactly `len` bytes starting at `start`.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::OutOfRange`] if the range is not fully available,
    /// or [`SourceError::Unavailable`] if the underlying source failed.
    fn read(&mut self, start: u64, len: u32) -> Result<&[u8], SourceError>;

    /// Total size of the source in bytes, if known.
    ///
    /// A stream being consumed for the first time may not know its own length;
    /// indexing does not require it, only the scrollbar does.
    fn len_hint(&self) -> Option<u64> {
        None
    }
}

/// In-memory sources: the whole document is already a slice.
///
/// This is the path used for pasted input and small files, and by every test in
/// the crate. It is also the proof that the trait is not secretly file-shaped.
impl ByteRange for &[u8] {
    fn read(&mut self, start: u64, len: u32) -> Result<&[u8], SourceError> {
        let available = self.len() as u64;
        let end = start.saturating_add(u64::from(len));
        if end > available {
            return Err(SourceError::OutOfRange {
                start,
                len,
                available,
            });
        }
        // Both bounds are <= available, which came from a usize, so this is exact.
        Ok(&self[start as usize..end as usize])
    }

    fn len_hint(&self) -> Option<u64> {
        Some(self.len() as u64)
    }
}

/// Read up to `len` bytes, shortening the request if the source is smaller.
///
/// Every reader in the engine is speculative at the edges: a row window asks for
/// a budget past the last row, and an expansion asks for a window past the end
/// of the container. Neither knows whether the file extends that far, so running
/// off the end is the normal condition at the end of a file rather than an
/// error, and both callers want the short read instead of the diagnostic.
pub(crate) fn read_clamped<S: ByteRange>(
    source: &mut S,
    start: u64,
    len: u64,
) -> Result<&[u8], SourceError> {
    let mut len = match source.len_hint() {
        Some(total) => len.min(total.saturating_sub(start)),
        None => len,
    };

    // A source that will not state its length can still be asked. The
    // out-of-range error carries the size, so one probe always suffices — and
    // this branch is skipped entirely by every source that knows its own length.
    if source.len_hint().is_none() {
        if let Err(SourceError::OutOfRange { available, .. }) = source.read(start, as_u32(len)) {
            len = available.saturating_sub(start);
        }
    }

    source.read(start, as_u32(len))
}

fn as_u32(len: u64) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_clamped_stops_at_the_end_of_the_source() {
        let mut src: &[u8] = b"0123456789";
        assert_eq!(read_clamped(&mut src, 6, 1000).unwrap(), b"6789");
        assert_eq!(read_clamped(&mut src, 10, 1000).unwrap(), b"");
        assert_eq!(read_clamped(&mut src, 0, 4).unwrap(), b"0123");
    }

    #[test]
    fn read_clamped_discovers_the_end_of_a_source_that_will_not_say() {
        /// A source that knows its length but refuses to volunteer it — which is
        /// what a stream being consumed for the first time looks like.
        struct Coy<'a>(&'a [u8]);

        impl ByteRange for Coy<'_> {
            fn read(&mut self, start: u64, len: u32) -> Result<&[u8], SourceError> {
                self.0.read(start, len)
            }
            fn len_hint(&self) -> Option<u64> {
                None
            }
        }

        let mut src = Coy(b"0123456789");
        assert_eq!(read_clamped(&mut src, 6, 1000).unwrap(), b"6789");
    }

    #[test]
    fn reads_a_range() {
        let mut src: &[u8] = b"0123456789";
        assert_eq!(src.read(2, 3).unwrap(), b"234");
        assert_eq!(src.read(0, 10).unwrap(), b"0123456789");
        assert_eq!(src.read(10, 0).unwrap(), b"");
    }

    #[test]
    fn rejects_out_of_range() {
        let mut src: &[u8] = b"0123456789";
        assert_eq!(
            src.read(8, 4),
            Err(SourceError::OutOfRange {
                start: 8,
                len: 4,
                available: 10
            })
        );
    }

    #[test]
    fn does_not_overflow_on_absurd_ranges() {
        let mut src: &[u8] = b"abc";
        assert!(src.read(u64::MAX, u32::MAX).is_err());
    }

    #[test]
    fn len_hint_is_reported() {
        let src: &[u8] = b"abc";
        assert_eq!(src.len_hint(), Some(3));
    }
}
