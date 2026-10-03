//! The piece list against a plain `Vec` of every logical row's slot.

use std::collections::HashMap;

use super::*;

/// A small, seeded random number generator (xorshift), so a failure
/// repeats.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % u64::try_from(n.max(1)).unwrap()).unwrap()
    }
}

/// The model: each logical row's slot, and each inserted row's gap.
struct Model {
    physical: u32,
    slots: Vec<Slot>,
    gaps: HashMap<u32, u32>,
}

impl Model {
    fn new(physical: u32) -> Model {
        Model {
            physical,
            slots: (0..physical).map(Slot::Original).collect(),
            gaps: HashMap::new(),
        }
    }

    /// Where a row inserted at `at` goes: before the row there.
    fn gap_at(&self, at: usize) -> u32 {
        match self.slots.get(at) {
            Some(Slot::Original(row)) => *row,
            Some(Slot::Inserted(n)) => self.gaps[n],
            None => self.physical,
        }
    }

    fn expand(pieces: &[Piece]) -> Vec<Slot> {
        pieces
            .iter()
            .flat_map(|piece| (0..piece.len()).map(move |k| piece.slot(k)))
            .collect()
    }
}

/// An edit the test can undo.
enum Done {
    Inserted { at: usize, pieces: Vec<Piece> },
    Removed { at: usize, pieces: Vec<Piece> },
}

fn physical(model: &Model) -> usize {
    usize::try_from(model.physical).unwrap()
}

/// Every answer the map gives, against the model.
fn check_all(map: &RowMap, model: &Model) {
    let identity = model.slots.len() == physical(model)
        && model
            .slots
            .iter()
            .enumerate()
            .all(|(l, s)| *s == Slot::Original(u32::try_from(l).unwrap()));
    assert_eq!(map.is_identity(), identity, "back to the identity");
    if identity {
        return;
    }
    assert_eq!(map.len(), Some(model.slots.len()));
    let pieces = map.pieces();
    assert_eq!(Model::expand(&pieces), model.slots);
    for pair in pieces.windows(2) {
        assert!(pair[0].precedes(pair[1]), "{pair:?} in order");
        assert!(pair[0].merge(pair[1]).is_none(), "{pair:?} merged");
    }
    let mut start = 0;
    for (i, leaf) in map.leaves.iter().enumerate() {
        assert!(!leaf.pieces.is_empty() && leaf.pieces.len() <= LEAF);
        assert_eq!((map.starts[i], map.keys[i]), (start, leaf.pieces[0].key()));
        start += leaf.rows();
    }
    for (l, slot) in model.slots.iter().enumerate() {
        assert_eq!(map.slot(l), Some(*slot));
    }
    let mut logical = vec![None; physical(model)];
    // A deleted row's place: the originals before it, and the inserted
    // rows at gaps at or before it.
    let mut live: Vec<u32> = Vec::new();
    let mut gaps: Vec<u32> = Vec::new();
    for (l, slot) in model.slots.iter().enumerate() {
        match slot {
            Slot::Original(p) => {
                logical[usize::try_from(*p).unwrap()] = Some(l);
                live.push(*p);
            }
            Slot::Inserted(n) => gaps.push(model.gaps[n]),
        }
    }
    gaps.sort_unstable();
    for (p, l) in logical.iter().enumerate() {
        let p32 = u32::try_from(p).unwrap();
        match l {
            Some(l) => assert_eq!(map.logical_of(p32), Ok(*l)),
            None => {
                let before =
                    live.partition_point(|&q| q < p32) + gaps.partition_point(|&g| g <= p32);
                assert_eq!(map.logical_of(p32), Err(before), "deleted row {p}");
            }
        }
    }
}

/// A few answers, at random places; with `linear`, also those whose
/// expected value takes a pass over the model.
fn check_some(map: &RowMap, model: &Model, rng: &mut Rng, linear: bool) {
    let len = model.slots.len();
    assert_eq!(map.len().unwrap_or(len), len);
    let l = rng.below(len + 1);
    assert_eq!(map.slot(l).filter(|_| l < len), model.slots.get(l).copied());
    let split = u32::try_from(map.physical_at_or_after(l)).unwrap();
    if !map.is_identity() {
        assert_eq!(split, model.gap_at(l));
    }
    if let Some(Slot::Inserted(n)) = model.slots.get(l) {
        assert_eq!(map.logical_of_inserted(*n, model.gaps[n]), Some(l));
    }
    let end = (l + rng.below(40)).min(len);
    let expanded: Vec<Slot> = map
        .segments(l..end)
        .iter()
        .flat_map(|segment| match segment {
            Segment::Original(r) => r.clone().map(Slot::Original).collect::<Vec<_>>(),
            Segment::Inserted(r) => r.clone().map(Slot::Inserted).collect(),
        })
        .collect();
    assert_eq!(expanded, model.slots[l..end]);
    if !linear {
        return;
    }
    // `physical_at_or_after` splits the live originals at `l`.
    assert!(
        model.slots[..l]
            .iter()
            .all(|s| !matches!(s, Slot::Original(p) if *p >= split))
    );
    assert!(
        model.slots[l..]
            .iter()
            .all(|s| !matches!(s, Slot::Original(p) if *p < split))
    );
    let available = rng.below(physical(model) + 1);
    let within = model
        .slots
        .iter()
        .position(|s| matches!(s, Slot::Original(p) if usize::try_from(*p).unwrap() >= available))
        .unwrap_or(len);
    assert_eq!(map.rows_within(available), within);
    let from = u32::try_from(rng.below(physical(model) + 1)).unwrap();
    let to = (from + u32::try_from(rng.below(50)).unwrap()).min(model.physical);
    let mut live: Vec<u32> = model
        .slots
        .iter()
        .filter_map(|s| match s {
            Slot::Original(p) if (from..to).contains(p) => Some(*p),
            _ => None,
        })
        .collect();
    live.sort_unstable();
    let got: Vec<u32> = map.originals_in(from..to).into_iter().flatten().collect();
    assert_eq!(got, live);
    // The live originals either side of a physical row.
    let mut all: Vec<u32> = model
        .slots
        .iter()
        .filter_map(|s| match s {
            Slot::Original(p) => Some(*p),
            Slot::Inserted(_) => None,
        })
        .collect();
    all.sort_unstable();
    let p = u32::try_from(rng.below(physical(model) + 1)).unwrap();
    let next = all
        .iter()
        .copied()
        .find(|&q| q >= p)
        .unwrap_or(model.physical);
    assert_eq!(map.next_live(p), next, "next live from {p}");
    let end = all.iter().rev().find(|&&q| q < p).map_or(0, |&q| q + 1);
    assert_eq!(map.live_end_before(p), end, "live end before {p}");
    let shifted = physical(model) + 10;
    assert_eq!(map.shift(shifted), len + 10);
}

fn undo(map: &mut RowMap, model: &mut Model, edit: Done) {
    match edit {
        Done::Inserted { at, pieces } => {
            let count = Model::expand(&pieces).len();
            assert_eq!(map.remove(at..at + count), pieces);
            model.slots.drain(at..at + count);
        }
        Done::Removed { at, pieces } => {
            assert!(map.insert(at, &pieces), "an undo fits");
            let back = Model::expand(&pieces);
            model.slots.splice(at..at, back);
        }
    }
}

/// 100,000 random inserts, deletes and undos, checked against the model.
#[test]
fn the_map_agrees_with_a_vec_over_100k_random_edits() {
    let physical = 20_000;
    let mut map = RowMap::default();
    let mut model = Model::new(physical);
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut done: Vec<Done> = Vec::new();
    let mut next = 0;
    let mut most_pieces = 0;
    for step in 0..100_000 {
        let len = model.slots.len();
        match rng.below(20) {
            0..=7 => {
                let at = rng.below(len + 1);
                let count = u32::try_from(1 + rng.below(3)).unwrap();
                let gap = model.gap_at(at);
                map.begin(physical);
                let pieces = vec![Piece::Inserted {
                    gap,
                    first: next,
                    len: count,
                }];
                assert!(map.insert(at, &pieces));
                for k in 0..count {
                    model.gaps.insert(next + k, gap);
                }
                let new = Model::expand(&pieces);
                model.slots.splice(at..at, new);
                next += count;
                done.push(Done::Inserted { at, pieces });
            }
            8..=16 if len > 0 => {
                let at = rng.below(len);
                let count = if rng.below(100) == 0 {
                    300
                } else {
                    1 + rng.below(2)
                };
                let end = (at + count).min(len);
                map.begin(physical);
                let pieces = map.remove(at..end);
                let removed: Vec<Slot> = model.slots.drain(at..end).collect();
                assert_eq!(Model::expand(&pieces), removed);
                done.push(Done::Removed { at, pieces });
            }
            _ => {
                if let Some(edit) = done.pop() {
                    undo(&mut map, &mut model, edit);
                }
            }
        }
        check_some(&map, &model, &mut rng, step % 50 == 0);
        most_pieces = most_pieces.max(map.leaves.iter().map(|l| l.pieces.len()).sum::<usize>());
        if step % 4_999 == 0 {
            check_all(&map, &model);
        }
    }
    check_all(&map, &model);
    assert!(
        most_pieces > 4 * LEAF,
        "several leaves: {most_pieces} pieces at most"
    );
    // Undoing everything gives the identity back.
    while let Some(edit) = done.pop() {
        undo(&mut map, &mut model, edit);
    }
    assert!(map.is_identity());
}

#[test]
fn a_run_out_of_order_does_not_fit() {
    let mut map = RowMap::default();
    map.begin(10);
    let before = map.pieces();
    // Row 2 is there already.
    assert!(!map.insert(5, &[Piece::Original { start: 2, len: 1 }]));
    // An inserted row before row 3 can't go after row 6.
    let row = [Piece::Inserted {
        gap: 3,
        first: 0,
        len: 1,
    }];
    assert!(!map.insert(7, &row));
    assert_eq!(map.pieces(), before);
    assert!(map.insert(3, &row));
    assert_eq!(map.len(), Some(11));
}

#[test]
fn deleting_every_row_leaves_an_empty_map() {
    let mut map = RowMap::default();
    map.begin(4);
    let removed = map.remove(0..4);
    assert_eq!(removed, [Piece::Original { start: 0, len: 4 }]);
    assert_eq!(map.len(), Some(0));
    assert_eq!(map.slot(0), None);
    assert_eq!(map.logical_of(2), Err(0));
    assert_eq!(map.rows_within(4), 0);
    assert_eq!(map.physical_at_or_after(0), 4);
    assert!(map.segments(0..4).is_empty());
    assert!(map.insert(0, &removed));
    assert!(map.is_identity());
}

#[test]
fn an_empty_file_takes_inserted_rows() {
    let mut map = RowMap::default();
    map.begin(0);
    let rows = [Piece::Inserted {
        gap: 0,
        first: 0,
        len: 2,
    }];
    assert!(map.insert(0, &rows));
    assert_eq!(map.len(), Some(2));
    assert_eq!(map.slot(1), Some(Slot::Inserted(1)));
    assert_eq!(map.remove(0..2), rows);
    assert!(map.is_identity());
}

#[test]
fn row_ids_keep_originals_and_inserted_apart() {
    let original = RowId::original(u32::MAX);
    let inserted = RowId::inserted(0);
    assert!(original < inserted);
    assert_eq!(original.physical(), Some(u32::MAX));
    assert_eq!(original.inserted_index(), None);
    assert_eq!(inserted.physical(), None);
    assert_eq!(inserted.inserted_index(), Some(0));
    assert_eq!(inserted.plus(3), RowId::inserted(3));
    assert_eq!(RowId::original_bound(usize::MAX), inserted);
    assert_eq!(Slot::Inserted(4).id(), RowId::inserted(4));
}
