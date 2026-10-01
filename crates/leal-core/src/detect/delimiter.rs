//! Choosing the delimiter: the one that gives the most consistent field
//! count across the rows (DESIGN §3.2).

use std::cmp::Ordering;
use std::collections::HashMap;

use super::rows::{Ended, whole_rows};
use super::units::Units;
use crate::dialect::Delimiter;

/// Counts how a delimiter splits non-blank rows, one row at a time, so a
/// whole file can be tallied without keeping its rows.
#[derive(Clone, Debug, Default)]
pub(crate) struct Tally {
    rows: usize,
    regular_rows: usize,
    /// Field count → (rows with it, the row it was first seen in).
    counts: HashMap<usize, (usize, usize)>,
}

impl Tally {
    /// Counts one row; blank rows don't count.
    pub(crate) fn add(&mut self, row: &Ended) {
        if row.is_blank() {
            return;
        }
        let seen = self.rows;
        self.counts.entry(row.fields).or_insert((0, seen)).0 += 1;
        self.rows += 1;
        if !row.irregular {
            self.regular_rows += 1;
        }
    }

    /// The score, or `None` if there were no non-blank rows.
    pub(crate) fn score(&self) -> Option<Score> {
        // The most rows; a tie goes to the count seen first.
        let (&mode, &(mode_rows, _)) = self
            .counts
            .iter()
            .max_by(|(_, (a, a_first)), (_, (b, b_first))| a.cmp(b).then(b_first.cmp(a_first)))?;
        Some(Score {
            rows: self.rows,
            mode,
            mode_rows,
            regular_rows: self.regular_rows,
        })
    }
}

/// How consistently a delimiter splits non-blank rows.
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

/// The tally of a window's whole rows under `delimiter` (see
/// [`whole_rows`]).
pub(crate) fn tally(units: Units<'_>, delimiter: Delimiter, cut: bool) -> Tally {
    let mut tally = Tally::default();
    for row in whole_rows(units, delimiter.byte(), cut) {
        tally.add(&Ended {
            len: row.span.len(),
            fields: row.fields,
            ending: row.ending,
            irregular: row.irregular,
        });
    }
    tally
}

/// Every delimiter's score, in [`Delimiter::ALL`] order. A delimiter that
/// wasn't tallied has no score.
pub(crate) type Scores = [(Delimiter, Option<Score>); 4];

/// Scores each delimiter from its tally.
pub(crate) fn scores(tally: impl Fn(Delimiter) -> Option<Score>) -> Scores {
    Delimiter::ALL.map(|d| (d, tally(d)))
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
        best(&scores(|d| tally(units, d, false).score())).map(|(d, _)| d)
    }

    fn row(fields: usize, irregular: bool) -> Ended {
        Ended {
            len: 1,
            fields,
            ending: None,
            irregular,
        }
    }

    #[test]
    fn the_mode_ties_go_to_the_first_seen() {
        let mut t = Tally::default();
        for fields in [3, 2, 2, 3] {
            t.add(&row(fields, fields == 2));
        }
        // A blank row doesn't count.
        t.add(&Ended {
            len: 0,
            fields: 1,
            ending: Some(crate::dialect::LineEnding::Lf),
            irregular: false,
        });
        let s = t.score().unwrap();
        assert_eq!((s.rows, s.mode, s.mode_rows, s.regular_rows), (4, 3, 2, 2));
        assert_eq!(Tally::default().score(), None);
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
}
