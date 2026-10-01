//! Which columns hold numbers, so the grid can right-align them (DESIGN
//! §4.1; §3.10 lists "number detection for alignment" as P2 work).
//!
//! A column is numeric when it has at least one non-empty cell and every
//! non-empty cell looks like a number ([`looks_numeric`]). Empty cells
//! don't count either way, so a column of numbers with gaps is still
//! numeric. The header row is left out by the caller.
//!
//! This only decides alignment. It never changes a value, and nothing is
//! parsed as a number: sorting and filtering by number (phase 3) have
//! their own rules.

/// The most characters a cell may have and still count as a number. Longer
/// text, or a cell cut short for display, is text.
pub const NUMBER_MAX_CHARS: usize = 64;

/// Whether `text` looks like a number: an optional sign, then digits with
/// an optional decimal part, then an optional exponent. Spaces around it
/// are allowed. The integer part may be grouped in thousands with commas
/// (`1,234,567.50`), as spreadsheets export quoted values.
///
/// ```
/// use leal_core::rows::looks_numeric;
///
/// assert!(looks_numeric("42"));
/// assert!(looks_numeric(" -3.5e-2 "));
/// assert!(looks_numeric("1,234.50"));
/// assert!(!looks_numeric("A-100231"));
/// assert!(!looks_numeric("2025-01-03"));
/// assert!(!looks_numeric("1,23"));
/// ```
#[must_use]
pub fn looks_numeric(text: &str) -> bool {
    let text = text.trim_matches(' ');
    if text.is_empty() || text.len() > NUMBER_MAX_CHARS {
        return false;
    }
    let bytes = text.as_bytes();
    let mut at = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        at = 1;
    }
    let (integer_digits, after_integer) = integer_part(bytes, at);
    at = after_integer;
    let mut fraction_digits = 0;
    if bytes.get(at) == Some(&b'.') {
        fraction_digits = digits(bytes, at + 1);
        at += 1 + fraction_digits;
    }
    if integer_digits == 0 && fraction_digits == 0 {
        return false;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        let mut exponent = at + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let exponent_digits = digits(bytes, exponent);
        if exponent_digits == 0 {
            return false;
        }
        at = exponent + exponent_digits;
    }
    at == bytes.len()
}

/// The number of ASCII digits in `bytes` from `at` on.
fn digits(bytes: &[u8], at: usize) -> usize {
    bytes
        .get(at..)
        .unwrap_or_default()
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count()
}

/// The integer part from `at`: plain digits, or digits grouped in
/// thousands (`1,234`). Returns how many digits it has and where it ends.
/// A comma that doesn't start a group of exactly three digits ends it, so
/// `1,23` is left with a stray `,23` and isn't a number.
fn integer_part(bytes: &[u8], at: usize) -> (usize, usize) {
    let first = digits(bytes, at);
    let mut end = at + first;
    let mut count = first;
    if first == 0 || first > 3 {
        return (count, end);
    }
    while bytes.get(end) == Some(&b',') {
        let group = digits(bytes, end + 1);
        if group != 3 {
            break;
        }
        end += 4;
        count += 3;
    }
    (count, end)
}

/// Collects which columns of a sample of rows are numeric.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NumericColumns {
    columns: Vec<Column>,
}

/// What a column's non-empty cells have shown so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Column {
    /// No non-empty cell yet.
    #[default]
    Empty,
    /// Every non-empty cell was a number.
    Numeric,
    /// At least one wasn't.
    Text,
}

impl NumericColumns {
    /// An empty collection: no columns yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one cell of column `column`. `truncated` says the text is only
    /// the start of the value, which then counts as text.
    pub fn add(&mut self, column: usize, text: &str, truncated: bool) {
        if self.columns.len() <= column {
            self.columns.resize(column + 1, Column::Empty);
        }
        if text.is_empty() && !truncated {
            return;
        }
        let numeric = !truncated && looks_numeric(text);
        let state = &mut self.columns[column];
        *state = match (*state, numeric) {
            (Column::Text, _) | (_, false) => Column::Text,
            (Column::Empty | Column::Numeric, true) => Column::Numeric,
        };
    }

    /// One entry per column seen (the longest row's field count): whether
    /// it is numeric.
    #[must_use]
    pub fn result(&self) -> Vec<bool> {
        self.columns
            .iter()
            .map(|column| *column == Column::Numeric)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        for text in [
            "0",
            "7",
            "-7",
            "+7",
            "40",
            "1196.00",
            ".5",
            "5.",
            "-0.25",
            "1e9",
            "1E9",
            "2.5e-3",
            "6.02E+23",
            " 12 ",
            "1,234",
            "12,345,678.90",
            "-1,000",
            "999,999",
        ] {
            assert!(looks_numeric(text), "{text:?} is a number");
        }
    }

    #[test]
    fn not_numbers() {
        for text in [
            "",
            " ",
            "-",
            "+",
            ".",
            "-.",
            "e5",
            "1e",
            "1e+",
            "1.2.3",
            "A-100231",
            "2025-01-03",
            "12:30",
            "1,23",
            "1,2345",
            "1234,567",
            ",123",
            "1,234,",
            "1 000",
            "--1",
            "0x1F",
            "NaN",
            "inf",
            "١٢٣",
            "12%",
            "$5",
            "\t5",
        ] {
            assert!(!looks_numeric(text), "{text:?} isn't a number");
        }
        assert!(looks_numeric(&"9".repeat(NUMBER_MAX_CHARS)));
        assert!(!looks_numeric(&"9".repeat(NUMBER_MAX_CHARS + 1)));
    }

    #[test]
    fn a_column_is_numeric_if_every_non_empty_cell_is_a_number() {
        let mut columns = NumericColumns::new();
        // Rows of: id, qty (with a gap), price, all empty.
        for (id, qty, price) in [
            ("A-1", "40", "29.90"),
            ("A-2", "", "4.10"),
            ("A-3", "3", "x"),
        ] {
            columns.add(0, id, false);
            columns.add(1, qty, false);
            columns.add(2, price, false);
            columns.add(3, "", false);
        }
        assert_eq!(columns.result(), [false, true, false, false]);
    }

    #[test]
    fn a_truncated_cell_is_text() {
        let mut columns = NumericColumns::new();
        columns.add(0, "12", false);
        columns.add(0, "34", true);
        assert_eq!(columns.result(), [false]);
    }

    #[test]
    fn short_rows_leave_later_columns_alone() {
        let mut columns = NumericColumns::new();
        columns.add(2, "5", false);
        columns.add(0, "1", false);
        assert_eq!(columns.result(), [true, false, true]);
    }
}
