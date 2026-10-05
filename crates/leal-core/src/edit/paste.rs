//! Clipboard text as cells, for Paste (task 2.6, DESIGN §4.2: "TSV on the
//! clipboard").
//!
//! Every text pasted is read as tab-separated values, which is what Leal's
//! own Copy puts on the clipboard (`push_tsv_cell` in `document/values.rs`)
//! and what Numbers, Excel and Google Sheets put there as text. Text with
//! no tabs is a column of cells, a line each: comma-separated text is not
//! guessed at, so a line of CSV pastes as one cell, commas and all.
//!
//! The rules:
//! - cells are separated by a tab, and rows by a line break (CRLF, LF or a
//!   lone CR);
//! - a cell that starts with `"` and has a closing `"` followed by a tab, a
//!   line break or the end of the text is quoted: its value is the text
//!   between, with each `""` read as one `"`, and may hold tabs and line
//!   breaks (each, CRLF, LF or CR, becomes the file's own line ending when
//!   it is pasted: [`Pasted::line_breaks_as`]);
//! - any other cell is its text as it is, quotes included (so `"Hi" she
//!   said` pastes as written);
//! - one line break at the very end is dropped: spreadsheets end a copy
//!   with one. So empty text, or a lone line break, is one empty value.
//!   Leal's Copy ends with one only when its last line is empty (a
//!   one-column copy ending in an empty cell), so its own copies read back
//!   whole: `"a\n\n"` is two rows, `"a"` and `""`, not the one value `"a"`.

use std::borrow::Cow;

use crate::dialect::LineEnding;

/// Clipboard text split into cells ([`parse_tsv`]): rows of cells, which
/// may differ in length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pasted<'a> {
    rows: Vec<Vec<Cow<'a, str>>>,
    width: usize,
    cells: usize,
}

impl<'a> Pasted<'a> {
    /// The rows, each its cells in order.
    #[must_use]
    pub fn rows(&self) -> &[Vec<Cow<'a, str>>] {
        &self.rows
    }

    /// How many rows.
    #[must_use]
    pub fn height(&self) -> usize {
        self.rows.len()
    }

    /// The longest row's cell count.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// How many cells, in all rows.
    #[must_use]
    pub fn cells(&self) -> usize {
        self.cells
    }

    /// Each line break inside a value (CRLF, LF or a lone CR) as `ending`,
    /// the file's own (task 2.6, as a line break typed into a value is):
    /// an Excel cell's LF goes into a CRLF file as CRLF. Only a value with
    /// a line break other than `ending` is copied.
    pub fn line_breaks_as(&mut self, ending: LineEnding) {
        let ending = match ending {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
            LineEnding::Cr => "\r",
        };
        for cell in self.rows.iter_mut().flatten() {
            if memchr::memchr2(b'\r', b'\n', cell.as_bytes()).is_some() {
                let converted = with_line_breaks(cell, ending);
                if converted != cell.as_ref() {
                    *cell = Cow::Owned(converted);
                }
            }
        }
    }

    /// The value, if the text is one cell: Paste puts it into every
    /// selected cell.
    #[must_use]
    pub fn single(&self) -> Option<&str> {
        match self.rows.as_slice() {
            [row] => match row.as_slice() {
                [value] => Some(value),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Splits clipboard `text` into cells as tab-separated values (see the
/// module's rules). It stops once it has found more than `limit` cells,
/// and returns `Err` with the count so far (`limit + 1`), so a huge paste
/// is refused without its values being copied.
///
/// # Errors
///
/// The count so far, if `text` has more than `limit` cells.
pub fn parse_tsv(text: &str, limit: usize) -> Result<Pasted<'_>, usize> {
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .or_else(|| text.strip_suffix('\r'))
        .unwrap_or(text);
    let bytes = text.as_bytes();
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut width = 0;
    let mut cells = 0;
    let mut at = 0;
    loop {
        let (value, end) = cell(text, at);
        row.push(value);
        cells += 1;
        if cells > limit {
            return Err(cells);
        }
        let next = match bytes.get(end) {
            Some(b'\t') => {
                at = end + 1;
                continue;
            }
            None => None,
            Some(b'\r') if bytes.get(end + 1) == Some(&b'\n') => Some(end + 2),
            Some(_) => Some(end + 1),
        };
        width = width.max(row.len());
        rows.push(std::mem::take(&mut row));
        match next {
            Some(next) => at = next,
            None => break,
        }
    }
    Ok(Pasted { rows, width, cells })
}

/// `value` with each CRLF, LF and lone CR as `ending`.
fn with_line_breaks(value: &str, ending: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find(['\r', '\n']) {
        out.push_str(&rest[..at]);
        out.push_str(ending);
        let width = if rest[at..].starts_with("\r\n") { 2 } else { 1 };
        rest = &rest[at + width..];
    }
    out.push_str(rest);
    out
}

/// The cell that starts at byte `at` of `text`, and where it ends: at the
/// tab or line break after it, or the end of the text.
fn cell(text: &str, at: usize) -> (Cow<'_, str>, usize) {
    let bytes = text.as_bytes();
    if bytes.get(at) == Some(&b'"')
        && let Some(close) = closing_quote(bytes, at + 1)
    {
        let inner = &text[at + 1..close];
        let value = if inner.contains("\"\"") {
            Cow::Owned(inner.replace("\"\"", "\""))
        } else {
            Cow::Borrowed(inner)
        };
        return (value, close + 1);
    }
    let end = memchr::memchr3(b'\t', b'\n', b'\r', &bytes[at..]).map_or(bytes.len(), |k| at + k);
    (Cow::Borrowed(&text[at..end]), end)
}

/// The closing quote of a quoted cell whose value starts at `from`: a `"`
/// not doubled, followed by a tab, a line break or the end. `None` if
/// there is none, and the cell is read as it is.
fn closing_quote(bytes: &[u8], from: usize) -> Option<usize> {
    let mut at = from;
    while let Some(k) = memchr::memchr(b'"', &bytes[at..]) {
        let quote = at + k;
        match bytes.get(quote + 1) {
            Some(b'"') => at = quote + 2,
            None | Some(b'\t' | b'\n' | b'\r') => return Some(quote),
            Some(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Vec<Vec<String>> {
        parse_tsv(text, usize::MAX)
            .unwrap()
            .rows()
            .iter()
            .map(|row| row.iter().map(|cell| cell.to_string()).collect())
            .collect()
    }

    fn of(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|&cell| cell.to_owned()).collect())
            .collect()
    }

    #[test]
    fn tabs_and_line_breaks_split_cells_and_rows() {
        assert_eq!(rows("a\tb\nc\td"), of(&[&["a", "b"], &["c", "d"]]));
        assert_eq!(rows("a\tb\r\nc\td\r\n"), of(&[&["a", "b"], &["c", "d"]]));
        assert_eq!(rows("a\rb\r"), of(&[&["a"], &["b"]]));
        assert_eq!(rows("a\t\tc"), of(&[&["a", "", "c"]]));
        assert_eq!(rows("a\n\nb"), of(&[&["a"], &[""], &["b"]]));
        // Rows of different lengths stay as they are.
        assert_eq!(rows("a\tb\tc\nd"), of(&[&["a", "b", "c"], &["d"]]));
        let pasted = parse_tsv("a\tb\tc\nd", 10).unwrap();
        assert_eq!((pasted.height(), pasted.width(), pasted.cells()), (2, 3, 4));
    }

    #[test]
    fn one_final_line_break_is_dropped() {
        assert_eq!(rows("x\n"), of(&[&["x"]]));
        assert_eq!(rows("x\r\n"), of(&[&["x"]]));
        // A spreadsheet's copy of a cell and an empty one below it.
        assert_eq!(rows("x\r\n\r\n"), of(&[&["x"], &[""]]));
        assert_eq!(rows("a\t\n"), of(&[&["a", ""]]));
    }

    #[test]
    fn empty_text_is_one_empty_value() {
        assert_eq!(parse_tsv("", 10).unwrap().single(), Some(""));
        assert_eq!(parse_tsv("\r\n", 10).unwrap().single(), Some(""));
        assert_eq!(parse_tsv("x", 10).unwrap().single(), Some("x"));
        assert_eq!(parse_tsv("x\ty", 10).unwrap().single(), None);
        assert_eq!(parse_tsv("x\ny", 10).unwrap().single(), None);
    }

    #[test]
    fn quoted_cells_hold_tabs_line_breaks_and_quotes() {
        assert_eq!(rows("\"two\nlines\"\tb"), of(&[&["two\nlines", "b"]]));
        assert_eq!(rows("\"cr\r\nlf\"\n"), of(&[&["cr\r\nlf"]]));
        assert_eq!(rows("\"a\tb\""), of(&[&["a\tb"]]));
        assert_eq!(rows("\"say \"\"hi\"\"\""), of(&[&["say \"hi\""]]));
        assert_eq!(rows("\"\"\"\"\t\"\""), of(&[&["\"", ""]]));
        assert_eq!(
            parse_tsv("\"two\nlines\"\n", 10).unwrap().single(),
            Some("two\nlines")
        );
    }

    #[test]
    fn quotes_that_dont_close_a_cell_are_text() {
        assert_eq!(rows("\"Hi\" she said"), of(&[&["\"Hi\" she said"]]));
        assert_eq!(
            rows("\"never closed\nx"),
            of(&[&["\"never closed"], &["x"]])
        );
        assert_eq!(rows("6\" nails\t\"x"), of(&[&["6\" nails", "\"x"]]));
        assert_eq!(rows("a,\"b, c\",d"), of(&[&["a,\"b, c\",d"]]));
    }

    #[test]
    fn copy_text_reads_back_as_its_cells() {
        let values = [
            "plain",
            "",
            "a\tb",
            "two\nlines",
            "cr\r\nlf",
            "say \"hi\"",
            "\"",
            "é 東京",
        ];
        let mut text = String::new();
        for (k, value) in values.iter().enumerate() {
            if k > 0 {
                text.push(if k % 2 == 0 { '\n' } else { '\t' });
            }
            crate::document::push_tsv_cell(&mut text, value);
        }
        let read: Vec<String> = rows(&text).into_iter().flatten().collect();
        assert_eq!(read, values);
    }

    #[test]
    fn line_breaks_in_values_become_the_files() {
        let text = "\"lf\nx\"\t\"crlf\r\nx\"\t\"cr\rx\"\t\"all\r\n\n\r\n\rx\"\tplain";
        for (ending, line) in [
            (LineEnding::Crlf, "\r\n"),
            (LineEnding::Lf, "\n"),
            (LineEnding::Cr, "\r"),
        ] {
            let mut pasted = parse_tsv(text, 10).unwrap();
            pasted.line_breaks_as(ending);
            let cells: Vec<&str> = pasted.rows()[0].iter().map(AsRef::as_ref).collect();
            let all = format!("all{line}{line}{line}{line}x");
            assert_eq!(
                cells,
                [
                    format!("lf{line}x").as_str(),
                    &format!("crlf{line}x"),
                    &format!("cr{line}x"),
                    &all,
                    "plain"
                ]
            );
            // A value already in the file's ending stays borrowed.
            let kept = pasted.rows()[0]
                .iter()
                .filter(|cell| matches!(cell, Cow::Borrowed(_)))
                .count();
            assert_eq!(
                kept, 2,
                "{ending:?}: the plain cell and the one already right"
            );
        }
    }

    #[test]
    fn it_stops_past_the_limit() {
        assert_eq!(parse_tsv("a\tb\nc", 3).unwrap().cells(), 3);
        assert_eq!(parse_tsv("a\tb\nc\td", 3), Err(4));
        assert_eq!(parse_tsv(&"x\n".repeat(1_000_000), 10), Err(11));
    }
}
