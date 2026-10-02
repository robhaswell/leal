//! The header-row heuristic (DESIGN §3.2): is the first row text that looks
//! different from the rows under it?
//!
//! Each cell is sorted into a kind: empty, number, date or time, boolean,
//! or text. Text also has a *shape*: its letter case (lower, upper, title
//! or mixed) and whether it has digits, spaces or other symbols. Each column
//! then votes, comparing the first row's cell with the cells below it:
//!
//! - **+1 (header)** if most of the column is a number, date or boolean and
//!   the first cell is text; or if the column is mostly text and no cell
//!   below has the first cell's shape (`name` over `Ada`, `Bob`).
//! - **−1 (not a header)** if the column is mostly text and the first cell
//!   is a number, date or boolean.
//! - **0** otherwise: the first cell is empty (pandas' unnamed index), the
//!   column has nothing below it, the first cell has the same kind as a
//!   typed column (`2026-01-03` over dates), or its shape appears below.
//!
//! The first row is a header if the votes add up to more than zero. A file
//! with fewer than two non-blank rows, or a blank first row, has none.
//! Shapes only ever vote for a header, never against one, because text
//! under text is weak evidence either way.

/// How many rows under the first one are looked at.
pub(crate) const ROWS_LOOKED_AT: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Empty,
    Number,
    Date,
    Boolean,
    Text(Shape),
}

impl Kind {
    fn is_typed(self) -> bool {
        matches!(self, Kind::Number | Kind::Date | Kind::Boolean)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    None,
    Lower,
    Upper,
    Title,
    Mixed,
}

#[allow(clippy::struct_excessive_bools)] // independent features of the text
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    case: Case,
    digits: bool,
    spaces: bool,
    symbols: bool,
}

/// Decides whether `rows[0]` is a header row over `rows[1..]`. Each row is
/// its fields' display values; blank rows are left out by the caller,
/// except that a blank first row is passed as `None`.
pub(crate) fn is_header(first: Option<&[String]>, below: &[Vec<String>]) -> bool {
    let Some(first) = first else {
        return false;
    };
    if below.is_empty() {
        return false;
    }
    let mut votes: i64 = 0;
    for (column, cell) in first.iter().enumerate() {
        let head = kind(cell);
        let cells: Vec<Kind> = below
            .iter()
            .filter_map(|row| row.get(column))
            .map(|c| kind(c))
            .filter(|k| *k != Kind::Empty)
            .collect();
        votes += vote(head, &cells);
    }
    votes > 0
}

fn vote(head: Kind, cells: &[Kind]) -> i64 {
    if head == Kind::Empty || cells.is_empty() {
        return 0;
    }
    let typed = [Kind::Number, Kind::Date, Kind::Boolean]
        .into_iter()
        .find(|t| cells.iter().filter(|k| *k == t).count() * 2 > cells.len());
    match (typed, head) {
        (Some(_), Kind::Text(_)) => 1,
        (Some(_), _) => 0,
        (None, h) if h.is_typed() => -1,
        (None, Kind::Text(shape)) => {
            let seen = cells.contains(&Kind::Text(shape));
            i64::from(!seen)
        }
        (None, _) => 0,
    }
}

fn kind(cell: &str) -> Kind {
    let s = cell.trim();
    if s.is_empty() {
        Kind::Empty
    } else if is_number(s) {
        Kind::Number
    } else if is_date(s) {
        Kind::Date
    } else if is_boolean(s) {
        Kind::Boolean
    } else {
        Kind::Text(shape(s))
    }
}

/// Currency signs that may come before or after a number.
const CURRENCY: &[char] = &['$', '€', '£', '¥', '₹', '¢'];

/// A number as people write them in CSV files: an optional sign or
/// parentheses, an optional currency sign or trailing `%`, digits grouped
/// with `,` `.` `'` or spaces, a decimal point or comma, and an optional
/// exponent. `1,20`, `-12`, `€3.50`, `(4.00)`, `12%` and `1e-3` are numbers.
fn is_number(s: &str) -> bool {
    let s = s
        .strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .unwrap_or(s);
    let s = s.strip_prefix(['+', '-', '\u{2212}']).unwrap_or(s);
    let s = s.trim_start_matches(CURRENCY).trim_end_matches(CURRENCY);
    let s = s.strip_suffix(['%', '\u{2030}']).unwrap_or(s);
    let s = s.strip_prefix(['+', '-', '\u{2212}']).unwrap_or(s);
    let (mantissa, exponent) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (s, None),
    };
    let exponent_ok = exponent.is_none_or(|e| {
        let digits = e.strip_prefix(['+', '-']).unwrap_or(e);
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    });
    exponent_ok && is_grouped_digits(mantissa)
}

/// Digits with single separators between them (`1,234.5`, `0,80`, `.5`).
fn is_grouped_digits(s: &str) -> bool {
    let separator = |c: char| matches!(c, ',' | '.' | '\'' | ' ' | '\u{A0}' | '\u{202F}');
    let mut digits = 0;
    let mut previous_separator = false;
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_digit() {
            digits += 1;
            previous_separator = false;
        } else if separator(c) && !previous_separator && (i > 0 || matches!(c, '.' | ',')) {
            previous_separator = true;
        } else {
            return false;
        }
    }
    digits > 0 && !previous_separator
}

/// A date or time made of digit groups: `2026-01-03`, `03/01/2026`,
/// `10:00`, `2026-01-03 10:00:00+00`, `2026-01-03T10:00Z`.
fn is_date(s: &str) -> bool {
    let first_is_digit = s.starts_with(|c: char| c.is_ascii_digit());
    let allowed = s
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '-' | '/' | '.' | ':' | ' ' | 'T' | 'Z' | '+'));
    let separators = s.chars().filter(|c| matches!(c, '-' | '/' | ':')).count();
    first_is_digit && allowed && separators >= 1
}

fn is_boolean(s: &str) -> bool {
    ["true", "false", "yes", "no"]
        .iter()
        .any(|b| s.eq_ignore_ascii_case(b))
}

fn shape(s: &str) -> Shape {
    // Only whether there are lower-case letters matters, but how many
    // upper-case ones (`A` is title case, `AB` upper case).
    let mut lower = false;
    let mut upper = 0;
    let mut word_starts_upper = true;
    let mut inner_upper = false;
    let mut previous_letter = false;
    let mut digits = false;
    let mut spaces = false;
    let mut symbols = false;
    for c in s.chars() {
        if c.is_alphabetic() {
            if c.is_uppercase() {
                upper += 1;
                if previous_letter {
                    inner_upper = true;
                }
            } else {
                lower = true;
                if !previous_letter {
                    word_starts_upper = false;
                }
            }
            previous_letter = true;
            continue;
        }
        previous_letter = false;
        if c.is_numeric() {
            digits = true;
        } else if c.is_whitespace() {
            spaces = true;
        } else {
            symbols = true;
        }
    }
    let case = match (lower, upper) {
        (false, 0) => Case::None,
        (true, 0) => Case::Lower,
        (false, u) if u >= 2 => Case::Upper,
        _ if word_starts_upper && !inner_upper => Case::Title,
        _ => Case::Mixed,
    };
    Shape {
        case,
        digits,
        spaces,
        symbols,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Vec<Vec<String>> {
        text.lines()
            .map(|l| l.split(',').map(str::to_owned).collect())
            .collect()
    }

    fn header(text: &str) -> bool {
        let rows = rows(text);
        is_header(rows.first().map(Vec::as_slice), &rows[1..])
    }

    #[test]
    fn numbers_dates_and_booleans() {
        for n in [
            "1", "-12", "1234.5", "1,20", "0,80", "1.234,56", "€3.50", "£4.20", "(4.00)", "12%",
            "1e-3", ".5", "1 000",
        ] {
            assert_eq!(kind(n), Kind::Number, "{n}");
        }
        for d in [
            "2026-01-03",
            "03/01/2026",
            "10:00",
            "2026-01-03 10:00:00+00",
            "2026-01-03T10:00Z",
        ] {
            assert_eq!(kind(d), Kind::Date, "{d}");
        }
        for b in ["TRUE", "False", "yes", "NO"] {
            assert_eq!(kind(b), Kind::Boolean, "{b}");
        }
        for t in ["A-1", "1,", "1..2", "e5", "12 High St", "-", "x1"] {
            assert!(matches!(kind(t), Kind::Text(_)), "{t}");
        }
        assert_eq!(kind("  "), Kind::Empty);
    }

    #[test]
    fn shapes() {
        let case = |s| shape(s).case;
        assert_eq!(case("name"), Case::Lower);
        assert_eq!(case("SKU"), Case::Upper);
        assert_eq!(case("Ada"), Case::Title);
        assert_eq!(case("Ada Lovelace"), Case::Title);
        assert_eq!(case("A"), Case::Title);
        assert_eq!(case("McDonald"), Case::Mixed);
        assert_eq!(case("iPhone"), Case::Mixed);
        assert_eq!(case(","), Case::None);
        let s = shape("12 High St, London");
        assert!(s.digits && s.spaces && s.symbols);
        assert_eq!(
            shape("created_at"),
            Shape {
                case: Case::Lower,
                digits: false,
                spaces: false,
                symbols: true
            }
        );
    }

    #[test]
    fn a_text_header_over_typed_columns() {
        assert!(header("id,name\n1,Ada\n2,Bob"));
        assert!(header("date,amount\n2026-01-03,12.50\n2026-01-04,8.00"));
        assert!(header("Done\nTRUE\nFALSE"));
        assert!(header("score\n12\n7\n30"));
    }

    #[test]
    fn rows_that_look_alike_have_no_header() {
        assert!(!header("2026-01-03,12.50,Coffee\n2026-01-04,8.00,Lunch"));
        assert!(!header("1,2,3\n4,5,6"));
        assert!(!header("Ada,London\nBob,Paris"));
        assert!(!header("a,1\nb,2"));
    }

    #[test]
    fn a_text_header_over_text_of_another_shape() {
        assert!(header("name,address\nAda,12 High St\nBob,a;b"));
        // `v` over `a` is the same shape, but `id` over numbers decides.
        assert!(header("id,v\n1,a"));
    }

    #[test]
    fn a_number_in_a_text_column_is_evidence_against() {
        assert!(!header("2024,x\nNorth,y\nSouth,z"));
    }

    #[test]
    fn empty_first_cells_abstain() {
        assert!(header(",name\n0,Ada\n1,Alan"));
        assert!(!header(",\n1,2"));
    }

    #[test]
    fn too_few_rows_have_no_header() {
        assert!(!header("id,name"));
        assert!(!is_header(None, &rows("1,2")));
    }

    // The edges of the vote (p1-review tests-5).

    /// A number, date or boolean over a text column votes against a
    /// header: here it cancels `name`'s vote for one.
    #[test]
    fn a_typed_first_cell_over_text_votes_against() {
        for first in ["2024", "2026-01-03", "TRUE"] {
            assert!(
                !header(&format!("{first},name\nNorth,Ada\nSouth,Bob")),
                "{first}"
            );
        }
        // Without it, `name` decides.
        assert!(header("x,name\nNorth,Ada\nSouth,Bob"));
    }

    /// A first cell of a typed column's own kind abstains, rather than
    /// voting against: `1` over numbers leaves `id` to decide.
    #[test]
    fn a_first_cell_of_its_columns_type_abstains() {
        assert!(header("id,1\nAda,2\nBob,3"));
    }

    /// A column is typed only if more than half its cells are: exactly
    /// half isn't, and then `name` over a lower-case word of its shape
    /// doesn't vote. Two thirds is.
    #[test]
    fn a_typed_column_needs_more_than_half() {
        assert!(!header("name\n12\nabc"));
        assert!(header("name\n12\n13\nabc"));
    }

    /// A column with nothing below its first cell abstains.
    #[test]
    fn a_column_with_nothing_below_abstains() {
        assert!(!header("x,name\nabc"));
    }

    /// A word is lower case with any number of lower-case letters and no
    /// upper-case ones.
    #[test]
    fn lower_case_needs_only_one_lower_case_letter() {
        assert_eq!(shape("a").case, Case::Lower);
        assert_eq!(shape("a1").case, Case::Lower);
        assert_eq!(shape("Ab").case, Case::Title);
    }
}
