//! Property tests for the fidelity helper itself: it accepts exactly the
//! output it should, and pinpoints the first wrong byte when it doesn't.

use leal_testkit::fidelity::{Change, Origin, apply_changes, check_only_changed};
use leal_testkit::strategies::bytes::csv_bytes;
use proptest::collection::vec;
use proptest::prelude::*;

/// An original plus a valid, sorted, non-overlapping list of changes.
fn original_and_changes() -> impl Strategy<Value = (Vec<u8>, Vec<Change>)> {
    csv_bytes().prop_flat_map(|original| {
        let len = original.len();
        // Each change: two cut points in 0..=len and a replacement.
        let change = (0..=len, 0..=len, vec(any::<u8>(), 0..4));
        (Just(original), vec(change, 0..4)).prop_map(|(original, raw)| {
            let mut cuts: Vec<(usize, usize, Vec<u8>)> = raw
                .into_iter()
                .map(|(a, b, r)| (a.min(b), a.max(b), r))
                .collect();
            cuts.sort_by_key(|&(start, end, _)| (start, end));
            // Drop any change that overlaps the one before it.
            let mut changes: Vec<Change> = Vec::new();
            for (start, end, r) in cuts {
                if changes.last().is_none_or(|c| start >= c.range.end) {
                    changes.push(Change::replace(start..end, r));
                }
            }
            (original, changes)
        })
    })
}

proptest! {
    #[test]
    fn accepts_exactly_the_expected_output((original, changes) in original_and_changes()) {
        let output = apply_changes(&original, &changes);
        check_only_changed(&original, &output, &changes)?;
    }

    #[test]
    fn pinpoints_a_corrupted_byte(
        (original, changes) in original_and_changes(),
        at in any::<prop::sample::Index>(),
        flip in 1u8..=255,
    ) {
        let expected = apply_changes(&original, &changes);
        prop_assume!(!expected.is_empty());
        let i = at.index(expected.len());
        let mut output = expected.clone();
        output[i] ^= flip;
        let e = check_only_changed(&original, &output, &changes).unwrap_err();
        prop_assert_eq!(e.offset, i);
        prop_assert_eq!(e.expected_region.clone(), i..i + 1);
        prop_assert_eq!(e.output_region.clone(), i..i + 1);
        // The origin points back at the byte that was corrupted.
        let byte = match e.origin {
            Origin::Original { offset, .. } => original[offset],
            Origin::Change { index, offset } => changes[index].replacement[offset],
            Origin::End => unreachable!("same length"),
        };
        prop_assert_eq!(byte, expected[i]);
    }

    #[test]
    fn detects_an_inserted_or_dropped_byte(
        (original, changes) in original_and_changes(),
        at in any::<prop::sample::Index>(),
        insert in any::<bool>(),
    ) {
        let expected = apply_changes(&original, &changes);
        let mut output = expected.clone();
        if insert {
            let i = at.index(expected.len() + 1);
            output.insert(i, b'X');
        } else {
            prop_assume!(!expected.is_empty());
            output.remove(at.index(expected.len()));
        }
        let e = check_only_changed(&original, &output, &changes).unwrap_err();
        prop_assert_eq!(e.expected_len, expected.len());
        prop_assert_eq!(e.output_len, output.len());
        prop_assert!(e.to_string().len() < 2_000);
    }
}
