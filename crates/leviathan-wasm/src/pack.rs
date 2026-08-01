//! Packing rows into one buffer, so the boundary costs one copy instead of *n*.
//!
//! A [`Row`] holds two `String`s. Handing fifty of them to JavaScript the
//! obvious way — one object per row — allocates fifty objects, a hundred
//! strings, and a hundred and fifty property slots, per scroll tick, in the
//! thread that is supposed to be painting. That is the cost ADR-002 exists to
//! avoid, and it has been an assertion in a doc comment ever since M0.
//!
//! This is the mechanism behind it. Every row becomes a fixed-width record in a
//! flat `Vec<u8>`, its two strings are appended to a blob at the end, and the
//! whole thing crosses as one `Uint8Array`. JS reads the fixed part with a
//! `DataView` — no allocation at all — and decodes a string only when a row is
//! actually painted.
//!
//! ## Layout
//!
//! All little-endian, which is what `DataView` and every browser agree on.
//!
//! ```text
//! header   16 bytes    version u32 | rows u32 | string bytes u32 | reserved u32
//! rows     40 × n      starting at byte 16, 8-byte aligned by construction
//! strings  variable    key then preview, per row, in row order
//! ```
//!
//! One row:
//!
//! ```text
//!  0..8   offset        u64   the row's byte offset — its identity (C36)
//!  8..16  value_start   u64
//! 16..24  value_end     u64   u64::MAX when the value ran past its budget
//! 24..32  children      u64
//! 32      kind          u8    see `KIND_*`
//! 33      flags         u8    see `flags`
//! 34..36  key bytes     u16
//! 36..40  preview bytes u32
//! ```
//!
//! Strings are found by walking, not by storing an offset per row: the decoder
//! consumes `key bytes` then `preview bytes` as it steps through rows, because
//! it steps through them in the order they were written. Two lengths beat two
//! offsets by eight bytes a row, and the constraint they impose — decode in
//! order — is one the consumer was going to obey anyway.
//!
//! ## Why the version field
//!
//! A stale `dist/` is the single most likely confusing bug in this project (the
//! protocol module says so, and it is right). A layout change with a matching
//! decoder change is invisible until the two are built at different times, and
//! then it is byte soup. Eight bytes of header make it an error message.

use leviathan_core::{Count, Row, ValueKind};

/// Bumped whenever the byte layout below changes in any way.
///
/// The TypeScript decoder asserts on it. Changing the layout without changing
/// this is the one mistake this module cannot survive.
pub const LAYOUT_VERSION: u32 = 1;

/// Bytes before the first row record.
pub const HEADER_BYTES: usize = 16;

/// Bytes per row record.
pub const ROW_BYTES: usize = 40;

/// `value_end` when the value's extent was never determined.
const UNKNOWN_END: u64 = u64::MAX;

/// Stable discriminants for [`ValueKind`], mirrored in TypeScript.
///
/// Deliberately not `as u8` on the enum: that would make the boundary depend on
/// declaration order, so reordering the variants for readability would silently
/// change what the UI paints.
mod kind {
    pub const OBJECT: u8 = 0;
    pub const ARRAY: u8 = 1;
    pub const STRING: u8 = 2;
    pub const NUMBER: u8 = 3;
    pub const TRUE: u8 = 4;
    pub const FALSE: u8 = 5;
    pub const NULL: u8 = 6;
    pub const INVALID: u8 = 7;
}

/// Bit meanings of the `flags` byte.
mod flags {
    /// The child count is exact rather than a lower bound (C33).
    pub const COUNT_EXACT: u8 = 1 << 0;
    /// The preview was cut short.
    pub const TRUNCATED: u8 = 1 << 1;
    /// The row has a key, so the key length is meaningful.
    pub const HAS_KEY: u8 = 1 << 2;
    /// The row can be expanded.
    pub const EXPANDABLE: u8 = 1 << 3;
}

const fn discriminant(kind: ValueKind) -> u8 {
    match kind {
        ValueKind::Object => kind::OBJECT,
        ValueKind::Array => kind::ARRAY,
        ValueKind::String => kind::STRING,
        ValueKind::Number => kind::NUMBER,
        ValueKind::True => kind::TRUE,
        ValueKind::False => kind::FALSE,
        ValueKind::Null => kind::NULL,
        ValueKind::Invalid => kind::INVALID,
    }
}

/// Pack `rows` into one buffer in the layout documented above.
///
/// Truncation is possible in principle and impossible in practice: a key longer
/// than 65 535 bytes is clamped, and previews are already bounded by
/// `RowOptions::preview_chars` long before they could reach 4 GB. Clamping
/// rather than failing is the house rule — a row with an absurd key is still a
/// row worth showing (C34).
#[must_use]
pub fn rows(rows: &[Row]) -> Vec<u8> {
    let count = rows.len();
    let strings: usize = rows
        .iter()
        .map(|row| key_bytes(row).len() + row.preview.len())
        .sum();

    let mut buffer = Vec::with_capacity(HEADER_BYTES + count * ROW_BYTES + strings);
    buffer.extend_from_slice(&LAYOUT_VERSION.to_le_bytes());
    buffer.extend_from_slice(&(count as u32).to_le_bytes());
    buffer.extend_from_slice(&(strings as u32).to_le_bytes());
    buffer.extend_from_slice(&0u32.to_le_bytes());

    for row in rows {
        let key = key_bytes(row);
        let key_len = u16::try_from(key.len()).unwrap_or(u16::MAX);
        let preview_len = u32::try_from(row.preview.len()).unwrap_or(u32::MAX);

        let mut bits = 0u8;
        if row.children.is_exact() {
            bits |= flags::COUNT_EXACT;
        }
        if row.truncated {
            bits |= flags::TRUNCATED;
        }
        if row.key.is_some() {
            bits |= flags::HAS_KEY;
        }
        if row.expandable() {
            bits |= flags::EXPANDABLE;
        }

        buffer.extend_from_slice(&row.offset.to_le_bytes());
        buffer.extend_from_slice(&row.value_start.to_le_bytes());
        buffer.extend_from_slice(&row.value_end.unwrap_or(UNKNOWN_END).to_le_bytes());
        buffer.extend_from_slice(&children(row.children).to_le_bytes());
        buffer.push(discriminant(row.kind));
        buffer.push(bits);
        buffer.extend_from_slice(&key_len.to_le_bytes());
        buffer.extend_from_slice(&preview_len.to_le_bytes());
    }

    for row in rows {
        let key = key_bytes(row);
        buffer.extend_from_slice(&key[..key.len().min(usize::from(u16::MAX))]);
        buffer.extend_from_slice(row.preview.as_bytes());
    }

    buffer
}

fn key_bytes(row: &Row) -> &[u8] {
    row.key.as_deref().unwrap_or("").as_bytes()
}

const fn children(count: Count) -> u64 {
    count.value()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: Option<&str>, preview: &str) -> Row {
        Row {
            offset: 10,
            value_start: 14,
            value_end: Some(20),
            kind: ValueKind::String,
            key: key.map(str::to_string),
            preview: preview.to_string(),
            truncated: false,
            children: Count::None,
        }
    }

    fn header(buffer: &[u8]) -> (u32, u32, u32) {
        let u32_at = |at: usize| u32::from_le_bytes(buffer[at..at + 4].try_into().unwrap());
        (u32_at(0), u32_at(4), u32_at(8))
    }

    #[test]
    fn an_empty_slice_is_a_header_and_nothing_else() {
        let packed = rows(&[]);
        assert_eq!(packed.len(), HEADER_BYTES);
        assert_eq!(header(&packed), (LAYOUT_VERSION, 0, 0));
    }

    #[test]
    fn the_buffer_is_exactly_as_long_as_its_header_claims() {
        // The decoder trusts these three numbers to find the string blob; if
        // they can disagree with the buffer, every row after the first is soup.
        let packed = rows(&[row(Some("key"), "value"), row(None, "second")]);
        let (version, count, strings) = header(&packed);

        assert_eq!(version, LAYOUT_VERSION);
        assert_eq!(count, 2);
        assert_eq!(
            strings,
            ("key".len() + "value".len() + "second".len()) as u32
        );
        assert_eq!(
            packed.len(),
            HEADER_BYTES + 2 * ROW_BYTES + strings as usize
        );
    }

    #[test]
    fn strings_are_concatenated_in_row_order() {
        let packed = rows(&[row(Some("a"), "one"), row(Some("bb"), "two")]);
        let blob = &packed[HEADER_BYTES + 2 * ROW_BYTES..];
        assert_eq!(blob, b"aonebbtwo");
    }

    #[test]
    fn a_row_without_a_key_contributes_no_key_bytes() {
        let packed = rows(&[row(None, "x")]);
        let key_len = u16::from_le_bytes(
            packed[HEADER_BYTES + 34..HEADER_BYTES + 36]
                .try_into()
                .unwrap(),
        );
        assert_eq!(key_len, 0);
        assert_eq!(packed[HEADER_BYTES + 33] & flags::HAS_KEY, 0);
    }

    #[test]
    fn an_empty_key_is_not_the_same_as_no_key() {
        // `{"":1}` is legal JSON, and the tree must not render it as an array
        // element. The length is zero either way, so the flag is what carries it.
        let packed = rows(&[row(Some(""), "1")]);
        assert_ne!(packed[HEADER_BYTES + 33] & flags::HAS_KEY, 0);
    }

    #[test]
    fn an_undetermined_end_is_distinguishable_from_a_real_one() {
        let mut unfinished = row(None, "");
        unfinished.value_end = None;
        let packed = rows(&[unfinished]);
        let end = u64::from_le_bytes(
            packed[HEADER_BYTES + 16..HEADER_BYTES + 24]
                .try_into()
                .unwrap(),
        );
        assert_eq!(end, UNKNOWN_END);
    }

    #[test]
    fn an_inexact_count_says_so() {
        let mut container = row(None, "");
        container.kind = ValueKind::Array;
        container.children = Count::AtLeast(1000);
        let packed = rows(&[container]);

        let bits = packed[HEADER_BYTES + 33];
        assert_eq!(bits & flags::COUNT_EXACT, 0, "AtLeast is not exact");
        assert_ne!(bits & flags::EXPANDABLE, 0, "but it is still expandable");

        let count = u64::from_le_bytes(
            packed[HEADER_BYTES + 24..HEADER_BYTES + 32]
                .try_into()
                .unwrap(),
        );
        assert_eq!(count, 1000);
    }

    #[test]
    fn every_kind_has_its_own_discriminant() {
        // A collision here would paint one kind of value as another, silently.
        let all = [
            ValueKind::Object,
            ValueKind::Array,
            ValueKind::String,
            ValueKind::Number,
            ValueKind::True,
            ValueKind::False,
            ValueKind::Null,
            ValueKind::Invalid,
        ];
        let mut seen: Vec<u8> = all.iter().map(|k| discriminant(*k)).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), all.len());
        assert_eq!(*seen.last().unwrap(), 7, "and they stay dense");
    }

    #[test]
    fn multibyte_keys_are_measured_in_bytes_not_characters() {
        // The decoder slices the blob by byte offset, so a length in characters
        // would desynchronize every row after the first non-ASCII one.
        let packed = rows(&[row(Some("café"), "→")]);
        let key_len = u16::from_le_bytes(
            packed[HEADER_BYTES + 34..HEADER_BYTES + 36]
                .try_into()
                .unwrap(),
        );
        let preview_len = u32::from_le_bytes(
            packed[HEADER_BYTES + 36..HEADER_BYTES + 40]
                .try_into()
                .unwrap(),
        );
        assert_eq!(key_len, 5, "é is two bytes");
        assert_eq!(preview_len, 3, "→ is three");
        assert_eq!(&packed[HEADER_BYTES + ROW_BYTES..], "café→".as_bytes());
    }
}
