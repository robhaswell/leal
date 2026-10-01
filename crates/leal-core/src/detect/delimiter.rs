//! Choosing the delimiter: the one that gives the most consistent field
//! count across the sampled rows (DESIGN §3.2).

use std::cmp::Ordering;

use super::rows::{after_first_line_ending, whole_rows};
use super::units::Units;
use crate::dialect::Delimiter;

/// One non-blank row of a sample, as a delimiter splits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Split {
    /// The number of fields.
    pub fields: usize,
    /// Whether its quotes are badly formed under this delimiter.
    pub irregular: bool,
}

/// How consistently a delimiter splits a sample's non-blank rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Score {
    /// Non-blank rows.
    pub rows: usize,
    /// The most common field count (ties go to the one seen first).
    pub mode: usize,
    /// Rows with that many fields.
    pub mode_rows: usize,
    /// Rows whose quotes are well formed.
    pub regular_rows: usize,
}

impl Score {
    /// The score of rows in file order. `None` if there are none.
    pub(crate) fn of(rows: &[Split]) -> Option<Self> {
        // (count, rows), in order of first appearance. Samples are at most
        // 64 KB, so there are few distinct counts.
        let mut histogram: Vec<(usize, usize)> = Vec::new();
        for row in rows {
            match histogram.iter_mut().find(|(count, _)| *count == row.fields) {
                Some((_, n)) => *n += 1,
                None => histogram.push((row.fields, 1)),
            }
        }
        // `max_by_key` keeps the last of equal elements, so search in
        // reverse to keep the first seen.
        let &(mode, mode_rows) = histogram.iter().rev().max_by_key(|(_, n)| *n)?;
        Some(Score {
            rows: rows.len(),
            mode,
            mode_rows,
            regular_rows: rows.iter().filter(|r| !r.irregular).count(),
        })
    }

    /// Compares two shares, `a` of `of_a` and `b` of `of_b`, without
    /// dividing.
    fn share_cmp(a: usize, of_a: usize, b: usize, of_b: usize) -> Ordering {
        a.saturating_mul(of_b).cmp(&b.saturating_mul(of_a))
    }

    /// Compares how consistent two scores are: the share of rows with the
    /// most common count.
    fn consistency_cmp(&self, other: &Score) -> Ordering {
        Self::share_cmp(self.mode_rows, self.rows, other.mode_rows, other.rows)
    }

    /// Compares the share of rows whose quotes are well formed.
    fn regularity_cmp(&self, other: &Score) -> Ordering {
        Self::share_cmp(self.regular_rows, self.rows, other.regular_rows, other.rows)
    }

    /// At least as consistent as `other`.
    pub(crate) fn at_least_as_consistent_as(&self, other: &Score) -> bool {
        self.consistency_cmp(other) != Ordering::Less
    }

    /// True if the delimiter actually splits rows: most rows have more than
    /// one field.
    pub(crate) fn splits(&self) -> bool {
        self.mode >= 2
    }

    /// The order [`best`] ranks scores in.
    fn rank_cmp(&self, other: &Score) -> Ordering {
        self.consistency_cmp(other)
            .then_with(|| self.regularity_cmp(other))
            .then_with(|| self.mode.cmp(&other.mode))
    }
}

/// The whole, non-blank rows of a sample under `delimiter`.
pub(crate) fn splits(units: Units<'_>, delimiter: Delimiter, cut: bool) -> Vec<Split> {
    whole_rows(units, delimiter.byte(), cut)
        .into_iter()
        .filter(|r| !r.is_blank())
        .map(|r| Split {
            fields: r.fields,
            irregular: r.irregular,
        })
        .collect()
}

/// The rows of a sample taken from the middle of the file: those after its
/// first line ending, without the last row if the sample was cut.
pub(crate) fn splits_from(units: Units<'_>, delimiter: Delimiter, cut: bool) -> Vec<Split> {
    match after_first_line_ending(units) {
        Some(start) => splits(units.starting_at(start), delimiter, cut),
        None => Vec::new(),
    }
}

/// Every delimiter's score, in [`Delimiter::ALL`] order.
pub(crate) type Scores = [(Delimiter, Option<Score>); 4];

/// Scores each delimiter on how it splits the rows.
pub(crate) fn scores(rows: impl Fn(Delimiter) -> Vec<Split>) -> Scores {
    Delimiter::ALL.map(|d| (d, Score::of(&rows(d))))
}

/// The best delimiter among those that split rows: the most consistent,
/// then the one with more well-formed quotes, then the one with more
/// fields, then the earlier in [`Delimiter::ALL`]. `None` if no delimiter
/// splits the rows (a one-column or empty file).
pub(crate) fn best(scores: &Scores) -> Option<(Delimiter, Score)> {
    let mut best: Option<(Delimiter, Score)> = None;
    for &(d, score) in scores {
        let Some(score) = score.filter(Score::splits) else {
            continue;
        };
        if best.is_none_or(|(_, b)| score.rank_cmp(&b) == Ordering::Greater) {
            best = Some((d, score));
        }
    }
    best
}

/// The score of `delimiter` in `scores`.
pub(crate) fn score_of(scores: &Scores, delimiter: Delimiter) -> Option<Score> {
    scores
        .iter()
        .find(|(d, _)| *d == delimiter)
        .and_then(|(_, s)| *s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Encoding;

    fn guess(text: &[u8]) -> Option<Delimiter> {
        let units = Units::new(text, Encoding::Utf8);
        best(&scores(|d| splits(units, d, false))).map(|(d, _)| d)
    }

    fn counts(units: Units<'_>, d: Delimiter, cut: bool, middle: bool) -> Vec<usize> {
        let rows = if middle {
            splits_from(units, d, cut)
        } else {
            splits(units, d, cut)
        };
        rows.into_iter().map(|r| r.fields).collect()
    }

    #[test]
    fn the_mode_ties_go_to_the_first_seen() {
        let rows: Vec<Split> = [3, 2, 2, 3]
            .into_iter()
            .map(|fields| Split {
                fields,
                irregular: fields == 2,
            })
            .collect();
        let s = Score::of(&rows).unwrap();
        assert_eq!((s.rows, s.mode, s.mode_rows, s.regular_rows), (4, 3, 2, 2));
        assert_eq!(Score::of(&[]), None);
    }

    /// A pipe file whose quoted values hold commas splits as evenly under
    /// ",", but its quotes only make sense under "|".
    #[test]
    fn well_formed_quotes_break_a_tie() {
        assert_eq!(guess(b"a|\"x,y\"\nb|\"z,w\"\n"), Some(Delimiter::Pipe));
        assert_eq!(guess(b"a\t\"x;y\"\nb\t\"z;w\"\n"), Some(Delimiter::Tab));
    }

    #[test]
    fn the_most_consistent_delimiter_wins() {
        assert_eq!(guess(b"a,b\n1,2\n"), Some(Delimiter::Comma));
        // Decimal commas: ";" splits every row into three, "," doesn't.
        assert_eq!(
            guess(b"product;price\nApple;1,20\nPear;0,95\n"),
            Some(Delimiter::Semicolon)
        );
        // A comma in one tab-separated value doesn't make it a comma file.
        assert_eq!(
            guess(b"name\tnote\nAda\tfirst, programmer\nAlan\tx\n"),
            Some(Delimiter::Tab)
        );
        // Delimiters inside quotes don't count.
        assert_eq!(guess(b"a,\"b;c|d\"\n1,\"2;3|4\"\n"), Some(Delimiter::Comma));
    }

    #[test]
    fn ties_go_to_more_fields_then_to_the_preferred_order() {
        assert_eq!(guess(b"a;b;c,d\n"), Some(Delimiter::Semicolon));
        assert_eq!(guess(b"a;b,c\n1;2,3\n"), Some(Delimiter::Comma));
        assert_eq!(guess(b"a|b\tc\n"), Some(Delimiter::Tab));
    }

    #[test]
    fn no_delimiter_splits_a_one_column_file() {
        assert_eq!(guess(b""), None);
        assert_eq!(guess(b"score\n12\n7\n"), None);
        // Blank lines don't count.
        assert_eq!(guess(b"\n\n\n"), None);
        assert_eq!(guess(b"note\nhello, world\nfoo\n"), None);
    }

    #[test]
    fn a_middle_sample_skips_its_partial_first_row() {
        let units = Units::new(b"x,y,z\na;b\nc;d\ne;", Encoding::Utf8);
        assert_eq!(counts(units, Delimiter::Semicolon, true, true), [2, 2]);
        assert_eq!(counts(units, Delimiter::Semicolon, false, true), [2, 2, 2]);
        assert_eq!(
            counts(units, Delimiter::Semicolon, false, false),
            [1, 2, 2, 2]
        );
        let none = Units::new(b"no line ending", Encoding::Utf8);
        assert_eq!(
            counts(none, Delimiter::Comma, true, true),
            Vec::<usize>::new()
        );
    }
}
