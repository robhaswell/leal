//! What **Find** matches (task 1.8, DESIGN §4.2): a query, and the matcher
//! that decides whether a cell's display value holds it.
//!
//! A cell matches when its **display value** ([`RowParser::display_value`])
//! contains the query's text. That is the value the grid and the inspector
//! show: unquoted and unescaped (`""` is `"`), decoded, with invalid bytes
//! as U+FFFD. So a search never sees the file's quoting, and a quote in the
//! query matches a quote in a value.
//!
//! **Case-insensitive by default**, with Unicode simple case folding (as
//! the `regex` crate defines it): `marlow` matches `Marlow` and `MARLOW`,
//! and `straße` matches `STRASSE` only if the user turns case sensitivity
//! on and types it so. The query is literal text: no wildcards or regular
//! expressions.
//!
//! A search of a whole file (`document::Search`) looks at the file's raw
//! bytes first where it can, which skips rows that can't match without
//! splitting them into fields ([`Matcher::raw_candidates`]); every row it
//! keeps is then checked value by value with [`Matcher::is_match`].
//!
//! [`RowParser::display_value`]: crate::rows::RowParser::display_value

use std::fmt;
use std::ops::Range;

use regex::{Regex, RegexBuilder};

use crate::dialect::Encoding;

/// The most a compiled query may take, in bytes. A query of a few thousand
/// characters fits; one that doesn't is refused ([`FindError::TooLong`])
/// rather than taking a lot of memory.
const COMPILED_LIMIT: usize = 4 << 20;

/// What the user is looking for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    /// The text to find, literally.
    pub text: String,
    /// Whether case matters. Off by default (DESIGN §4.2).
    pub case_sensitive: bool,
}

impl Query {
    /// A case-insensitive query for `text`.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Query {
        Query {
            text: text.into(),
            case_sensitive: false,
        }
    }
}

/// Why a query can't be searched for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindError {
    /// The query is empty: there is nothing to find.
    Empty,
    /// The query is too long to search for.
    TooLong,
}

impl fmt::Display for FindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FindError::Empty => f.write_str("the query is empty"),
            FindError::TooLong => f.write_str("the query is too long to search for"),
        }
    }
}

impl std::error::Error for FindError {}

/// Decides whether display values hold a query.
#[derive(Clone, Debug)]
pub struct Matcher {
    /// The query, on display values.
    text: Regex,
    /// The query on the file's raw bytes, when a match in a display value
    /// is always a match in the bytes too (see [`Matcher::new`]).
    raw: Option<regex::bytes::Regex>,
}

impl Matcher {
    /// A matcher for `query` in a file in `encoding`.
    ///
    /// It can look at raw bytes ([`raw_candidates`](Self::raw_candidates))
    /// only for UTF-8, and only if the query has no U+FFFD. Then every
    /// match in a display value is a match in the row's bytes too, of the
    /// query with each quote as `"` or `""`: a value is its raw bytes less
    /// the quotes around it and one quote of each `""` in a quoted field (a
    /// quote anywhere else is itself), so a stretch of a value is the same
    /// bytes in the file but for its quotes; and a stretch with no U+FFFD
    /// is valid UTF-8 there, which the byte search matches exactly as the
    /// text search does (Unicode mode, with the same case folding).
    ///
    /// # Errors
    ///
    /// [`FindError::Empty`] for an empty query, [`FindError::TooLong`] for
    /// one too long to search for.
    pub fn new(query: &Query, encoding: Encoding) -> Result<Matcher, FindError> {
        if query.text.is_empty() {
            return Err(FindError::Empty);
        }
        let pattern = regex::escape(&query.text);
        let text = RegexBuilder::new(&pattern)
            .case_insensitive(!query.case_sensitive)
            .size_limit(COMPILED_LIMIT)
            .build()
            .map_err(|_| FindError::TooLong)?;
        let raw = if encoding == Encoding::Utf8 && !query.text.contains('\u{FFFD}') {
            // `regex::escape` leaves quotes as they are.
            let raw_pattern = pattern.replace('"', "\"\"?");
            Some(
                regex::bytes::RegexBuilder::new(&raw_pattern)
                    .case_insensitive(!query.case_sensitive)
                    .size_limit(COMPILED_LIMIT)
                    .build()
                    .map_err(|_| FindError::TooLong)?,
            )
        } else {
            None
        };
        Ok(Matcher { text, raw })
    }

    /// Whether `value` (a display value) holds the query.
    #[must_use]
    pub fn is_match(&self, value: &str) -> bool {
        self.text.is_match(value)
    }

    /// Whether [`raw_candidates`](Self::raw_candidates) can be used.
    #[must_use]
    pub fn searches_raw_bytes(&self) -> bool {
        self.raw.is_some()
    }

    /// The first place at or after `start` in `bytes` (a stretch of the
    /// file) where the query could match, or `None` if no row there can
    /// hold a match. Always `Some(start)` when the matcher can't look at
    /// raw bytes, so a caller checks every row.
    #[must_use]
    pub fn raw_candidates(&self, bytes: &[u8], start: usize) -> Option<usize> {
        match &self.raw {
            Some(raw) => raw.find_at(bytes, start).map(|found| found.start()),
            None => (start < bytes.len()).then_some(start),
        }
    }

    /// Where the query is in `value`, as ranges of UTF-16 code units (what
    /// AppKit's text ranges count), for the first `max_chars` characters
    /// of it only: the part the grid shows. A match that runs past them is
    /// cut there.
    #[must_use]
    pub fn utf16_ranges(&self, value: &str, max_chars: usize) -> Vec<Range<usize>> {
        let shown = value
            .char_indices()
            .nth(max_chars)
            .map_or(value.len(), |(at, _)| at);
        let mut ranges = Vec::new();
        // UTF-16 length of `value[..counted]`, kept as the matches go on.
        let mut counted = 0;
        let mut units = 0;
        for found in self.text.find_iter(value) {
            if found.start() >= shown {
                break;
            }
            units += utf16_len(&value[counted..found.start()]);
            let end = found.end().min(shown);
            let start = units;
            units += utf16_len(&value[found.start()..end]);
            counted = end;
            if units > start {
                ranges.push(start..units);
            }
        }
        ranges
    }
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

// A highlight's ranges are a list of `Range`s, often of one.
#[allow(clippy::single_range_in_vec_init)]
#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(text: &str) -> Matcher {
        Matcher::new(&Query::new(text), Encoding::Utf8).unwrap()
    }

    #[test]
    fn matching_ignores_case_by_default() {
        let m = matcher("marlow");
        assert!(m.is_match("Marlow Foods"));
        assert!(m.is_match("orders@MARLOW.example"));
        assert!(!m.is_match("Marl ow"));
        // Unicode case folding, not only ASCII.
        assert!(matcher("ÉCOLE").is_match("une école"));
        // The Kelvin sign folds to k.
        assert!(matcher("k").is_match("5 \u{212A}"));
    }

    #[test]
    fn case_can_matter() {
        let query = Query {
            text: "Marlow".to_owned(),
            case_sensitive: true,
        };
        let m = Matcher::new(&query, Encoding::Utf8).unwrap();
        assert!(m.is_match("Marlow"));
        assert!(!m.is_match("marlow"));
    }

    #[test]
    fn the_query_is_literal_text() {
        let m = matcher("a.b*");
        assert!(m.is_match("x a.b* y"));
        assert!(!m.is_match("axbbb"));
        assert!(matcher("(1)").is_match("item (1)"));
    }

    #[test]
    fn an_empty_query_is_refused() {
        assert_eq!(
            Matcher::new(&Query::new(""), Encoding::Utf8).unwrap_err(),
            FindError::Empty
        );
    }

    #[test]
    fn a_huge_query_is_refused_not_compiled() {
        let text = "ǅ".repeat(200_000);
        assert_eq!(
            Matcher::new(&Query::new(text), Encoding::Utf8).unwrap_err(),
            FindError::TooLong
        );
    }

    #[test]
    fn raw_bytes_are_searched_only_where_that_is_exact() {
        assert!(matcher("marlow").searches_raw_bytes());
        // U+FFFD stands for invalid bytes, which the raw bytes don't have.
        assert!(!matcher("\u{FFFD}").searches_raw_bytes());
        let latin = Matcher::new(&Query::new("café"), Encoding::Windows1252).unwrap();
        assert!(!latin.searches_raw_bytes());
        // Then every row is a candidate.
        assert_eq!(latin.raw_candidates(b"abc", 1), Some(1));
        assert_eq!(latin.raw_candidates(b"abc", 3), None);
    }

    #[test]
    fn raw_candidates_find_every_case() {
        let m = matcher("marlow");
        let bytes = b"1,MarLow\n2,x\n3,\"marlow\"\n";
        assert_eq!(m.raw_candidates(bytes, 0), Some(2));
        assert_eq!(m.raw_candidates(bytes, 3), Some(16));
        assert_eq!(m.raw_candidates(bytes, 17), None);
        // Invalid bytes never match, as they show as U+FFFD.
        assert_eq!(m.raw_candidates(b"\xffmarlow", 0), Some(1));
        // A quote in the query is `"` or, in a quoted field, `""`.
        let quote = matcher("say \"hi\"");
        assert!(quote.searches_raw_bytes());
        assert_eq!(quote.raw_candidates(b"1,\"say \"\"hi\"\"\"", 0), Some(3));
        assert_eq!(quote.raw_candidates(b"1,say \"hi\"", 0), Some(2));
        assert_eq!(quote.raw_candidates(b"1,say hi", 0), None);
    }

    #[test]
    fn ranges_are_in_utf16_units_and_cut_at_the_shown_part() {
        let m = matcher("ab");
        assert_eq!(m.utf16_ranges("xxABxab", 100), [2..4, 5..7]);
        // Emoji are two UTF-16 units; é is one.
        assert_eq!(m.utf16_ranges("😀é ab", 100), [4..6]);
        // Only the first 5 characters show: the second match is past them,
        // and a match across the edge is cut.
        assert_eq!(m.utf16_ranges("abxxab", 5), [0..2, 4..5]);
        assert_eq!(m.utf16_ranges("abxxa ab", 5), [0..2]);
        assert!(m.utf16_ranges("xxxxxab", 5).is_empty());
    }
}
