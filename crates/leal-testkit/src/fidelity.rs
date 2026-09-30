//! Fidelity assertions (DESIGN §5).
//!
//! - [`assert_identical`] checks rule F1: the output is byte-identical to the
//!   original.
//! - [`assert_only_changed`] checks F2, F3 and F6: the output is the original
//!   with exactly the given splices, and nothing else.
//!
//! Both have a `check_*` twin that returns a [`FidelityError`] instead of
//! panicking, for use inside `proptest!` bodies (`check_only_changed(..)?`).
//!
//! On failure the message is short, however large the files are: the first
//! differing offset, a little escaped context from both sides, where the
//! expected byte came from (an original offset and its line, or a change),
//! and the extent of the differing region.

use std::fmt;
use std::ops::Range;

/// Bytes of context shown on each side of the first difference.
const CONTEXT: usize = 24;

/// One expected splice: replace `original[range]` with `replacement`.
///
/// An insertion is an empty range; a deletion is an empty replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The bytes of the original that are replaced.
    pub range: Range<usize>,
    /// What replaces them.
    pub replacement: Vec<u8>,
}

impl Change {
    /// Replaces `original[range]` with `replacement`.
    pub fn replace(range: Range<usize>, replacement: impl Into<Vec<u8>>) -> Self {
        Change {
            range,
            replacement: replacement.into(),
        }
    }

    /// Inserts `bytes` before `original[at]` (or at the end if `at` is the
    /// original's length).
    pub fn insert(at: usize, bytes: impl Into<Vec<u8>>) -> Self {
        Change::replace(at..at, bytes)
    }

    /// Deletes `original[range]`.
    #[must_use]
    pub fn delete(range: Range<usize>) -> Self {
        Change::replace(range, Vec::new())
    }
}

/// Where a byte of the expected output comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Copied unchanged from `original[offset]`. `line` is the 0-based line
    /// of that byte in the original, counting CR, LF and CRLF and ignoring
    /// quotes (so it can differ from the physical row).
    Original {
        /// Offset in the original.
        offset: usize,
        /// Line in the original.
        line: usize,
    },
    /// Byte `offset` of the replacement of change number `index`.
    Change {
        /// Index into the `changes` slice.
        index: usize,
        /// Offset within that change's replacement.
        offset: usize,
    },
    /// Past the end of the expected output: the output is too long.
    End,
}

/// Why a fidelity check failed. `Display` gives the full report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FidelityError {
    /// The first offset at which the output and the expected bytes differ
    /// (the same offset in both, since everything before it is equal).
    pub offset: usize,
    /// Where the expected byte at `offset` comes from.
    pub origin: Origin,
    /// The differing part of the expected bytes: from `offset` up to where
    /// the two agree again until the end.
    pub expected_region: Range<usize>,
    /// The matching differing part of the output.
    pub output_region: Range<usize>,
    /// Length of the original.
    pub original_len: usize,
    /// Length of the expected output (original plus changes).
    pub expected_len: usize,
    /// Length of the actual output.
    pub output_len: usize,
    report: String,
}

impl fmt::Display for FidelityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.report)
    }
}

/// Being an `Error` means proptest's blanket `impl<E: Error> From<E> for
/// TestCaseError` applies, so `check_*(..)?` inside `proptest!` fails the
/// case with the full report.
impl std::error::Error for FidelityError {}

/// Returns `original` with `changes` applied.
///
/// # Panics
///
/// Panics if the changes are invalid: a range out of bounds or reversed, or
/// the changes not sorted by position or overlapping. Two changes may touch
/// (`0..2` then `2..3`), and several insertions at one offset apply in the
/// order given. An invalid change list is a bug in the test, not the code
/// under test, so it panics rather than failing the check.
#[must_use]
pub fn apply_changes(original: &[u8], changes: &[Change]) -> Vec<u8> {
    validate(original, changes);
    let added: usize = changes.iter().map(|c| c.replacement.len()).sum();
    let mut out = Vec::with_capacity(original.len() + added);
    let mut pos = 0;
    for change in changes {
        out.extend_from_slice(&original[pos..change.range.start]);
        out.extend_from_slice(&change.replacement);
        pos = change.range.end;
    }
    out.extend_from_slice(&original[pos..]);
    out
}

/// Checks that `output` is byte-identical to `original` (rule F1).
///
/// # Errors
///
/// Returns a [`FidelityError`] describing the first difference.
pub fn check_identical(original: &[u8], output: &[u8]) -> Result<(), FidelityError> {
    check_only_changed(original, output, &[])
}

/// Checks that `output` is `original` with exactly `changes` applied, and
/// nothing else changed.
///
/// # Errors
///
/// Returns a [`FidelityError`] describing the first difference.
///
/// # Panics
///
/// Panics if `changes` is invalid; see [`apply_changes`].
pub fn check_only_changed(
    original: &[u8],
    output: &[u8],
    changes: &[Change],
) -> Result<(), FidelityError> {
    let expected = apply_changes(original, changes);
    if expected == output {
        return Ok(());
    }
    Err(describe(original, &expected, output, changes))
}

/// Asserts that `output` is byte-identical to `original` (rule F1).
///
/// # Panics
///
/// Panics with a short report if they differ.
#[track_caller]
pub fn assert_identical(original: &[u8], output: &[u8]) {
    if let Err(e) = check_identical(original, output) {
        panic!("{e}");
    }
}

/// Asserts that `output` is `original` with exactly `changes` applied.
///
/// ```
/// use leal_testkit::fidelity::{Change, assert_only_changed};
///
/// let original = b"id,name\n1,Ada\n";
/// let output = b"id,name\n1,Grace\n";
/// assert_only_changed(original, output, &[Change::replace(10..13, "Grace")]);
/// ```
///
/// # Panics
///
/// Panics with a short report if anything else differs, or if `changes` is
/// invalid (see [`apply_changes`]).
#[track_caller]
pub fn assert_only_changed(original: &[u8], output: &[u8], changes: &[Change]) {
    if let Err(e) = check_only_changed(original, output, changes) {
        panic!("{e}");
    }
}

#[track_caller]
fn validate(original: &[u8], changes: &[Change]) {
    let mut prev_end = 0;
    for (i, c) in changes.iter().enumerate() {
        assert!(
            c.range.start <= c.range.end && c.range.end <= original.len(),
            "invalid changes: change #{i} has range {:?}, but the original is {} bytes",
            c.range,
            original.len()
        );
        assert!(
            c.range.start >= prev_end,
            "invalid changes: change #{i} ({:?}) starts before the previous change ends ({prev_end}); \
             changes must be sorted and must not overlap",
            c.range
        );
        prev_end = c.range.end;
    }
}

fn describe(original: &[u8], expected: &[u8], output: &[u8], changes: &[Change]) -> FidelityError {
    let offset = expected
        .iter()
        .zip(output)
        .position(|(a, b)| a != b)
        .unwrap_or(expected.len().min(output.len()));

    // Length of the common suffix, not overlapping the common prefix.
    let room = expected.len().min(output.len()) - offset;
    let suffix = expected
        .iter()
        .rev()
        .zip(output.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    let expected_region = offset..expected.len() - suffix;
    let output_region = offset..output.len() - suffix;

    let origin = origin_of(original, changes, offset, expected.len());

    let mut r = String::new();
    let plural = if changes.len() == 1 { "" } else { "s" };
    r.push_str(&format!(
        "fidelity check failed: output differs from the expected bytes at offset {offset}\n"
    ));
    r.push_str(&format!(
        "  lengths: original {}, expected {}, output {} ({} change{plural} expected)\n",
        original.len(),
        expected.len(),
        output.len(),
        changes.len(),
    ));
    match &origin {
        Origin::Original { offset: o, line } => {
            r.push_str(&format!(
                "  the expected byte there is original byte {o}, on line {line} \
                 (0-based; CR, LF and CRLF counted, quotes ignored)\n"
            ));
            match nearest_change(changes, *o) {
                Some((i, distance)) => {
                    let c = &changes[i];
                    r.push_str(&format!(
                        "  so this is an unexpected change to original bytes; the nearest expected \
                         change is #{i} (original {:?} -> {} bytes), {distance} bytes away\n",
                        c.range,
                        c.replacement.len()
                    ));
                }
                None => {
                    r.push_str("  so this is an unexpected change to original bytes\n");
                }
            }
        }
        Origin::Change { index, offset: o } => {
            let c = &changes[*index];
            r.push_str(&format!(
                "  the expected byte there is byte {o} of change #{index}'s replacement \
                 (original {:?} -> {} bytes)\n",
                c.range,
                c.replacement.len()
            ));
        }
        Origin::End => {
            r.push_str("  the expected bytes end there; the output has extra bytes\n");
        }
    }
    r.push_str(&format!(
        "  differing region: expected {expected_region:?}, output {output_region:?}; \
         after that both match to the end\n"
    ));
    r.push_str(&format!("  expected: {}\n", context(expected, offset)));
    r.push_str(&format!("  output:   {}\n", context(output, offset)));
    r.push_str(&format!(
        "  (`|` marks offset {offset}; at most {CONTEXT} bytes shown on each side)"
    ));

    FidelityError {
        offset,
        origin,
        expected_region,
        output_region,
        original_len: original.len(),
        expected_len: expected.len(),
        output_len: output.len(),
        report: r,
    }
}

/// Maps an offset in the expected output back to its source.
fn origin_of(original: &[u8], changes: &[Change], offset: usize, expected_len: usize) -> Origin {
    if offset >= expected_len {
        return Origin::End;
    }
    // Walk the expected output as alternating kept runs and replacements.
    let mut exp_pos = 0; // start of the current segment in the expected output
    let mut orig_pos = 0; // start of the current kept run in the original
    for (index, c) in changes.iter().enumerate() {
        let kept = c.range.start - orig_pos;
        if offset < exp_pos + kept {
            return original_origin(original, orig_pos + (offset - exp_pos));
        }
        exp_pos += kept;
        if offset < exp_pos + c.replacement.len() {
            return Origin::Change {
                index,
                offset: offset - exp_pos,
            };
        }
        exp_pos += c.replacement.len();
        orig_pos = c.range.end;
    }
    original_origin(original, orig_pos + (offset - exp_pos))
}

fn original_origin(original: &[u8], offset: usize) -> Origin {
    Origin::Original {
        offset,
        line: line_of(original, offset),
    }
}

/// The 0-based line containing `bytes[offset]`, counting LF, CRLF and lone
/// CR as line endings. A line ending belongs to the line it ends.
fn line_of(bytes: &[u8], offset: usize) -> usize {
    let before = &bytes[..offset.min(bytes.len())];
    before
        .iter()
        .enumerate()
        .filter(|&(i, &b)| b == b'\n' || (b == b'\r' && bytes.get(i + 1) != Some(&b'\n')))
        .count()
}

/// The change closest to original offset `at`, and its distance in bytes.
fn nearest_change(changes: &[Change], at: usize) -> Option<(usize, usize)> {
    changes
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let distance = if at < c.range.start {
                c.range.start - at
            } else {
                at.saturating_sub(c.range.end)
            };
            (i, distance)
        })
        .min_by_key(|&(_, d)| d)
}

/// `"…before" | "after…"`, escaped, around `offset`.
fn context(bytes: &[u8], offset: usize) -> String {
    let start = offset.saturating_sub(CONTEXT);
    let end = (offset + CONTEXT).min(bytes.len());
    let offset = offset.min(bytes.len());
    format!(
        "{}\"{}\" | \"{}\"{}",
        if start > 0 { "…" } else { "" },
        bytes[start..offset].escape_ascii(),
        bytes[offset..end].escape_ascii(),
        if end < bytes.len() { "…" } else { "" },
    )
}

#[cfg(test)]
mod tests {
    use proptest::test_runner::TestCaseError;

    use super::*;

    fn err(original: &[u8], output: &[u8], changes: &[Change]) -> FidelityError {
        check_only_changed(original, output, changes).expect_err("check should fail")
    }

    // ---- passing cases -------------------------------------------------

    #[test]
    fn identical_passes() {
        assert_identical(b"", b"");
        assert_identical(b"a,b\r\n", b"a,b\r\n");
        assert_only_changed(b"a,b\n", b"a,b\n", &[]);
    }

    #[test]
    fn replace_insert_delete_pass() {
        let original = b"id,name\n1,Ada\n";
        assert_only_changed(
            original,
            b"id,name\n1,Grace\n",
            &[Change::replace(10..13, "Grace")],
        );
        assert_only_changed(
            original,
            b"id,name\n1,Ada\n2,Bob\n",
            &[Change::insert(14, "2,Bob\n")],
        );
        assert_only_changed(original, b"1,Ada\n", &[Change::delete(0..8)]);
        assert_only_changed(original, b"#id,name\n1,Ada\n", &[Change::insert(0, "#")]);
    }

    #[test]
    fn several_and_touching_changes_pass() {
        let original = b"a,b,c\n";
        let changes = [
            Change::replace(0..1, "A"),
            Change::replace(1..2, ";"), // touches the previous change
            Change::insert(2, "x"),
            Change::insert(2, "y"), // two insertions at one offset apply in order
            Change::delete(4..5),
        ];
        assert_eq!(apply_changes(original, &changes), b"A;xyb,\n");
        assert_only_changed(original, b"A;xyb,\n", &changes);
    }

    // ---- invalid change lists are test bugs, so they panic -------------

    #[test]
    #[should_panic(expected = "starts before the previous change ends")]
    fn overlapping_changes_panic() {
        let _ = apply_changes(b"abcdef", &[Change::delete(0..3), Change::delete(2..4)]);
    }

    #[test]
    #[should_panic(expected = "starts before the previous change ends")]
    fn unsorted_changes_panic() {
        let _ = apply_changes(b"abcdef", &[Change::delete(3..4), Change::delete(0..1)]);
    }

    #[test]
    #[should_panic(expected = "but the original is 3 bytes")]
    fn out_of_range_change_panics() {
        let _ = apply_changes(b"abc", &[Change::delete(2..4)]);
    }

    #[test]
    #[should_panic(expected = "has range 2..1")]
    #[allow(clippy::reversed_empty_ranges)]
    fn reversed_range_panics() {
        let _ = apply_changes(b"abc", &[Change::delete(2..1)]);
    }

    // ---- failure details -------------------------------------------------

    #[test]
    fn unexpected_change_to_original_bytes() {
        // Row 2's "3" became "X", but only row 1 was supposed to change.
        let original = b"a,b\n1,2\n3,4\n";
        let output = b"a,b\n1,Z\nX,4\n";
        let e = err(original, output, &[Change::replace(6..7, "Z")]);
        assert_eq!(e.offset, 8);
        assert_eq!(e.origin, Origin::Original { offset: 8, line: 2 });
        assert_eq!(e.expected_region, 8..9);
        assert_eq!(e.output_region, 8..9);
        let msg = e.to_string();
        assert!(msg.contains("original byte 8, on line 2"), "{msg}");
        assert!(
            msg.contains("nearest expected change is #0 (original 6..7 -> 1 bytes), 1 bytes away"),
            "{msg}"
        );
    }

    #[test]
    fn missing_change() {
        let original = b"a,b\n1,2\n";
        let e = err(original, original, &[Change::replace(6..7, "X2")]);
        assert_eq!(e.offset, 6);
        assert_eq!(
            e.origin,
            Origin::Change {
                index: 0,
                offset: 0
            }
        );
        assert!(
            e.to_string()
                .contains("byte 0 of change #0's replacement (original 6..7 -> 2 bytes)")
        );
        // A replacement that starts like the original differs one byte later.
        let e = err(original, original, &[Change::replace(6..7, "22")]);
        assert_eq!(
            e.origin,
            Origin::Change {
                index: 0,
                offset: 1
            }
        );
    }

    #[test]
    fn change_applied_differently() {
        // The replacement is right except its second byte.
        let e = err(b"x,y\n", b"x,aZc\n", &[Change::replace(2..3, "abc")]);
        assert_eq!(
            e.origin,
            Origin::Change {
                index: 0,
                offset: 1
            }
        );
        assert_eq!(e.expected_region, 3..4);
    }

    #[test]
    fn output_too_long() {
        let e = err(b"a,b\n", b"a,b\n\n", &[]);
        assert_eq!(e.offset, 4);
        assert_eq!(e.origin, Origin::End);
        assert_eq!(e.expected_region, 4..4);
        assert_eq!(e.output_region, 4..5);
        assert!(e.to_string().contains("the output has extra bytes"));
    }

    #[test]
    fn output_too_short() {
        // A dropped trailing newline: the classic normalization bug.
        let e = err(b"a,b\r\n", b"a,b", &[]);
        assert_eq!(e.offset, 3);
        assert_eq!(e.origin, Origin::Original { offset: 3, line: 0 });
        assert_eq!(e.expected_region, 3..5);
        assert_eq!(e.output_region, 3..3);
    }

    #[test]
    fn line_counts_cr_lf_and_crlf_once_each() {
        let bytes = b"a\r\nb\rc\nd";
        assert_eq!(line_of(bytes, 0), 0);
        assert_eq!(line_of(bytes, 1), 0); // CR of CRLF
        assert_eq!(line_of(bytes, 2), 0); // LF of CRLF
        assert_eq!(line_of(bytes, 3), 1);
        assert_eq!(line_of(bytes, 5), 2);
        assert_eq!(line_of(bytes, 7), 3);
    }

    #[test]
    fn full_report_format() {
        let original = b"id,name\r\n1,\"Ada\"\r\n2,Bob\r\n";
        // Expected: only "Bob" -> "Rob". Actual: that, plus CRLF -> LF on row 1.
        let output = b"id,name\r\n1,\"Ada\"\n2,Rob\r\n";
        let e = err(original, output, &[Change::replace(20..21, "R")]);
        let expected = "\
fidelity check failed: output differs from the expected bytes at offset 16
  lengths: original 25, expected 25, output 24 (1 change expected)
  the expected byte there is original byte 16, on line 1 (0-based; CR, LF and CRLF counted, quotes ignored)
  so this is an unexpected change to original bytes; the nearest expected change is #0 (original 20..21 -> 1 bytes), 4 bytes away
  differing region: expected 16..17, output 16..16; after that both match to the end
  expected: \"id,name\\r\\n1,\\\"Ada\\\"\" | \"\\r\\n2,Rob\\r\\n\"
  output:   \"id,name\\r\\n1,\\\"Ada\\\"\" | \"\\n2,Rob\\r\\n\"
  (`|` marks offset 16; at most 24 bytes shown on each side)";
        assert_eq!(e.to_string(), expected);
    }

    #[test]
    fn report_stays_short_for_huge_inputs() {
        let original = vec![b'x'; 10_000_000];
        let mut output = original.clone();
        output[5_000_000] = b'\0';
        let e = err(&original, &output, &[]);
        assert_eq!(e.offset, 5_000_000);
        let msg = e.to_string();
        assert!(msg.len() < 1_000, "report is {} bytes", msg.len());
        assert!(
            msg.contains("…\"xxxxxxxxxxxxxxxxxxxxxxxx\" | \"\\x00xxxxxxxxxxxxxxxxxxxxxxx\"…"),
            "{msg}"
        );
    }

    #[test]
    fn non_ascii_bytes_are_escaped() {
        let e = err("é,\u{FEFF}".as_bytes(), b"\xE9,", &[]);
        let msg = e.to_string();
        assert!(
            msg.contains("expected: \"\" | \"\\xc3\\xa9,\\xef\\xbb\\xbf\""),
            "{msg}"
        );
    }

    #[test]
    #[should_panic(expected = "output differs from the expected bytes at offset 1")]
    fn assert_identical_panics_with_the_report() {
        assert_identical(b"abc", b"aXc");
    }

    #[test]
    #[should_panic(expected = "nearest expected change is #0")]
    fn assert_only_changed_panics_with_the_report() {
        assert_only_changed(b"abc", b"XBc", &[Change::replace(1..2, "B")]);
    }

    #[test]
    fn converts_into_a_proptest_failure() {
        let e = err(b"a", b"b", &[]);
        let report = e.to_string();
        match TestCaseError::from(e) {
            TestCaseError::Fail(reason) => assert_eq!(reason.message(), report),
            TestCaseError::Reject(_) => panic!("expected a failure, not a rejection"),
        }
    }
}
