//! The piece list (task 2.4a, `docs/tasks/2.4.md` §1): which row of the
//! file, or which inserted row, each logical row is.
//!
//! **Row ids.** Every row has a [`RowId`] for life: an original row's is
//! its physical row (below 2³²); an inserted row's is 2³² + n, from a
//! counter of the edits' own, never reused. An inserted row also records
//! its **gap**: the physical row it was inserted before (the file's row
//! count at the end), fixed for life. Original rows never reorder (sorting
//! is a view, DESIGN §3.8), so rows only appear and disappear, and two
//! rows' logical order never changes.
//!
//! **Pieces.** A [`RowMap`] lists the logical rows as pieces, in order:
//! a stretch of original rows, or a run of inserted rows with consecutive
//! ids and one gap. Their **keys** (an original piece's first physical
//! row, an inserted one's gap) never decrease, inserted pieces first at
//! equal keys; neighbours that continue each other merge. So a map with no
//! row inserted or deleted is one piece, and is kept as none at all: the
//! **identity**, which costs the readers nothing.
//!
//! The pieces are kept in leaves of at most [`LEAF`], shared (`Arc`), with
//! each leaf's first logical row and first key in two top arrays. A look-up
//! searches a top array, then one leaf: O(log P) for P pieces. An edit
//! rebuilds the leaves it touches and the top arrays: O(LEAF + P / LEAF).
//! A snapshot (a reader's copy of the edits) clones the leaves' `Arc`s.

use std::ops::Range;
use std::sync::Arc;

/// The most pieces in a leaf.
pub(crate) const LEAF: usize = 512;

/// Where inserted rows' ids start.
const INSERTED_BASE: u64 = 1 << 32;

/// A row's identity, for life (see the module's docs). Opaque outside the
/// core: the app names rows by their logical place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowId(u64);

impl RowId {
    /// Original row `row` of the file.
    pub(crate) fn original(row: u32) -> RowId {
        RowId(u64::from(row))
    }

    /// The `n`th inserted row.
    pub(crate) fn inserted(n: u32) -> RowId {
        RowId(INSERTED_BASE + u64::from(n))
    }

    /// The id of physical row `row`, or the first id past every original
    /// row's if it is past them (for a range's end).
    pub(crate) fn original_bound(row: usize) -> RowId {
        RowId(u64::try_from(row).unwrap_or(u64::MAX).min(INSERTED_BASE))
    }

    /// The physical row, for an original row.
    pub(crate) fn physical(self) -> Option<u32> {
        u32::try_from(self.0).ok()
    }

    /// The inserted row's number, for an inserted row.
    pub(crate) fn inserted_index(self) -> Option<u32> {
        self.0
            .checked_sub(INSERTED_BASE)
            .and_then(|n| u32::try_from(n).ok())
    }

    /// The id `k` after this one, in a run of consecutive ids.
    pub(crate) fn plus(self, k: u32) -> RowId {
        RowId(self.0 + u64::from(k))
    }

    /// How many ids there are from this one up to `end`.
    pub(crate) fn until(self, end: RowId) -> u32 {
        u32::try_from(end.0.saturating_sub(self.0)).unwrap_or(u32::MAX)
    }
}

/// What a logical row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    /// Physical row `n` of the file.
    Original(u32),
    /// Inserted row `n`.
    Inserted(u32),
}

impl Slot {
    pub(crate) fn id(self) -> RowId {
        match self {
            Slot::Original(row) => RowId::original(row),
            Slot::Inserted(n) => RowId::inserted(n),
        }
    }
}

/// Consecutive logical rows of one kind, for a walk over rows
/// ([`RowMap::segments`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Segment {
    /// These physical rows of the file, in order.
    Original(Range<u32>),
    /// These inserted rows (by number), in order.
    Inserted(Range<u32>),
}

impl Segment {
    pub(crate) fn len(&self) -> usize {
        let range = match self {
            Segment::Original(range) | Segment::Inserted(range) => range,
        };
        to_usize(range.end - range.start)
    }
}

/// Consecutive logical rows: original rows `start..start + len`, or
/// inserted rows `first..first + len`, all inserted before physical row
/// `gap`. 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Piece {
    Original { start: u32, len: u32 },
    Inserted { gap: u32, first: u32, len: u32 },
}

impl Piece {
    pub(crate) fn len(self) -> u32 {
        match self {
            Piece::Original { len, .. } | Piece::Inserted { len, .. } => len,
        }
    }

    /// Its place among the pieces: its first physical row or its gap,
    /// inserted pieces first.
    fn key(self) -> (u32, bool) {
        match self {
            Piece::Original { start, .. } => (start, true),
            Piece::Inserted { gap, .. } => (gap, false),
        }
    }

    /// Rows `range` of it (offsets within it).
    fn slice(self, range: Range<u32>) -> Piece {
        let len = range.end - range.start;
        match self {
            Piece::Original { start, .. } => Piece::Original {
                start: start + range.start,
                len,
            },
            Piece::Inserted { gap, first, .. } => Piece::Inserted {
                gap,
                first: first + range.start,
                len,
            },
        }
    }

    /// The row at `offset` within it.
    fn slot(self, offset: u32) -> Slot {
        match self {
            Piece::Original { start, .. } => Slot::Original(start + offset),
            Piece::Inserted { first, .. } => Slot::Inserted(first + offset),
        }
    }

    pub(crate) fn segment(self) -> Segment {
        match self {
            Piece::Original { start, len } => Segment::Original(start..start + len),
            Piece::Inserted { first, len, .. } => Segment::Inserted(first..first + len),
        }
    }

    /// The ids of its rows, in order.
    pub(crate) fn ids(self) -> Range<RowId> {
        let first = self.slot(0).id();
        first..first.plus(self.len())
    }

    /// `self` then `next` as one piece, if `next` continues it.
    fn merge(self, next: Piece) -> Option<Piece> {
        match (self, next) {
            (Piece::Original { start, len }, Piece::Original { start: s, len: l })
                if start + len == s =>
            {
                Some(Piece::Original {
                    start,
                    len: len + l,
                })
            }
            (
                Piece::Inserted { gap, first, len },
                Piece::Inserted {
                    gap: g,
                    first: f,
                    len: l,
                },
            ) if gap == g && first + len == f => Some(Piece::Inserted {
                gap,
                first,
                len: len + l,
            }),
            _ => None,
        }
    }

    /// Whether `next` may follow `self`: original rows in file order, each
    /// inserted run after every original row before its gap and before
    /// every original row from it on.
    fn precedes(self, next: Piece) -> bool {
        match (self, next) {
            (Piece::Original { start, len }, Piece::Original { start: s, .. }) => start + len <= s,
            (Piece::Original { start, len }, Piece::Inserted { gap, .. }) => start + len <= gap,
            (Piece::Inserted { gap, .. }, Piece::Original { start, .. }) => gap <= start,
            (Piece::Inserted { gap, .. }, Piece::Inserted { gap: g, .. }) => gap <= g,
        }
    }
}

/// At most [`LEAF`] pieces, with their running row counts.
#[derive(Debug)]
struct Leaf {
    pieces: Vec<Piece>,
    /// `ends[i]`: the rows in `pieces[..=i]`.
    ends: Vec<usize>,
}

impl Leaf {
    fn new(pieces: Vec<Piece>) -> Leaf {
        let mut total = 0;
        let ends = pieces
            .iter()
            .map(|piece| {
                total += to_usize(piece.len());
                total
            })
            .collect();
        Leaf { pieces, ends }
    }

    fn rows(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }

    /// The rows before piece `i`.
    fn before(&self, i: usize) -> usize {
        i.checked_sub(1).map_or(0, |prev| self.ends[prev])
    }
}

/// The logical rows, as pieces (see the module's docs). `Clone` is a
/// snapshot: it shares the leaves.
#[derive(Clone, Debug, Default)]
pub(crate) struct RowMap {
    /// The file's row count once a row has been inserted or deleted;
    /// `None` while the map is the identity, whatever the row count.
    physical: Option<u32>,
    leaves: Vec<Arc<Leaf>>,
    /// Each leaf's first logical row.
    starts: Vec<usize>,
    /// Each leaf's first piece's key.
    keys: Vec<(u32, bool)>,
    /// The logical row count (once not the identity).
    len: usize,
}

impl RowMap {
    /// True while no row has been inserted or deleted (or every insert and
    /// delete has been undone): logical rows are physical rows.
    pub(crate) fn is_identity(&self) -> bool {
        self.physical.is_none()
    }

    /// The logical row count, unless the map is the identity (then it is
    /// the file's).
    pub(crate) fn len(&self) -> Option<usize> {
        self.physical.map(|_| self.len)
    }

    /// The file's row count the map was made for, unless it is the
    /// identity.
    pub(crate) fn physical_rows(&self) -> Option<u32> {
        self.physical
    }

    /// How many logical rows can be read when the first `available`
    /// physical rows can: those before the first original row at or past
    /// `available` (inserted rows are in memory).
    pub(crate) fn rows_within(&self, available: usize) -> usize {
        let Some(physical) = self.physical else {
            return available;
        };
        if available >= to_usize(physical) {
            return self.len;
        }
        let logical = match self.logical_of(u32::try_from(available).unwrap_or(u32::MAX)) {
            Ok(logical) => return logical,
            Err(logical) => logical,
        };
        // Row `available` is deleted: the inserted rows up to the next
        // original row can be read too.
        let Some((mut leaf, mut i, _)) = self.locate(logical) else {
            return logical;
        };
        let mut within = logical;
        while let Some(pieces) = self.leaves.get(leaf) {
            match pieces.pieces.get(i) {
                Some(Piece::Inserted { len, .. }) => within += to_usize(*len),
                Some(Piece::Original { .. }) => break,
                None => {
                    leaf += 1;
                    i = 0;
                    continue;
                }
            }
            i += 1;
        }
        within
    }

    /// What logical row `logical` is: on the identity, physical row
    /// `logical` (the caller knows whether the file has it); otherwise
    /// `None` past the end.
    pub(crate) fn slot(&self, logical: usize) -> Option<Slot> {
        if self.physical.is_none() {
            return u32::try_from(logical).ok().map(Slot::Original);
        }
        let (leaf, i, offset) = self.locate(logical)?;
        Some(self.leaves[leaf].pieces[i].slot(offset))
    }

    /// The leaf, the piece in it and the offset in the piece of logical
    /// row `logical`, if there is one (not on the identity).
    fn locate(&self, logical: usize) -> Option<(usize, usize, u32)> {
        if logical >= self.len {
            return None;
        }
        let leaf = self.starts.partition_point(|&start| start <= logical) - 1;
        let within = logical - self.starts[leaf];
        let ends = &self.leaves[leaf].ends;
        let i = ends.partition_point(|&end| end <= within);
        let offset = within - self.leaves[leaf].before(i);
        Some((leaf, i, u32::try_from(offset).unwrap_or(u32::MAX)))
    }

    /// The logical row of physical row `physical`, or, if it is deleted,
    /// `Err` with the logical row it would be: the rows before it.
    pub(crate) fn logical_of(&self, physical: u32) -> Result<usize, usize> {
        if self.physical.is_none() {
            return Ok(to_usize(physical));
        }
        // The last piece whose key is at or before `physical`'s own.
        let key = (physical, true);
        let leaf = self.keys.partition_point(|&k| k <= key);
        let Some(leaf) = leaf.checked_sub(1) else {
            return Err(0);
        };
        let pieces = &self.leaves[leaf];
        let i = pieces.pieces.partition_point(|piece| piece.key() <= key);
        // i >= 1: the leaf's first key is at or before `key`.
        let candidate = pieces.pieces[i - 1];
        let before = self.starts[leaf] + pieces.before(i - 1);
        if let Piece::Original { start, len } = candidate
            && physical < start + len
        {
            return Ok(before + to_usize(physical - start));
        }
        Err(before + to_usize(candidate.len()))
    }

    /// A physical row that splits the original rows at logical row
    /// `logical`: the live original rows before `logical` are those before
    /// it, and those at or after `logical` are those from it on. For an
    /// original row, its physical row; for an inserted one, its gap; past
    /// the end, the file's row count.
    pub(crate) fn physical_at_or_after(&self, logical: usize) -> usize {
        let Some(physical) = self.physical else {
            return logical;
        };
        match self.locate(logical) {
            None => to_usize(physical),
            Some((leaf, i, offset)) => match self.leaves[leaf].pieces[i] {
                Piece::Original { start, .. } => to_usize(start + offset),
                Piece::Inserted { gap, .. } => to_usize(gap),
            },
        }
    }

    /// The logical row of inserted row `n`, whose gap is `gap`, if it is
    /// in the map: O(log P + r) for r inserted runs at that gap.
    pub(crate) fn logical_of_inserted(&self, n: u32, gap: u32) -> Option<usize> {
        self.physical?;
        let key = (gap, false);
        // The first leaf that may hold a piece with this key.
        let mut leaf = self.keys.partition_point(|&k| k < key).saturating_sub(1);
        let mut i = self
            .leaves
            .get(leaf)?
            .pieces
            .partition_point(|p| p.key() < key);
        loop {
            let pieces = self.leaves.get(leaf)?;
            let Some(&piece) = pieces.pieces.get(i) else {
                leaf += 1;
                i = 0;
                continue;
            };
            if piece.key() != key {
                return None;
            }
            if let Piece::Inserted { first, len, .. } = piece
                && (first..first + len).contains(&n)
            {
                return Some(self.starts[leaf] + pieces.before(i) + to_usize(n - first));
            }
            i += 1;
        }
    }

    /// Logical rows `rows` (clamped to the map), as segments in order. On
    /// the identity, one original segment: the caller clamps `rows` to the
    /// rows the file has.
    pub(crate) fn segments(&self, rows: Range<usize>) -> Vec<Segment> {
        if rows.start >= rows.end {
            return Vec::new();
        }
        if self.physical.is_none() {
            let start = u32::try_from(rows.start).unwrap_or(u32::MAX);
            let end = u32::try_from(rows.end).unwrap_or(u32::MAX);
            return vec![Segment::Original(start..end)];
        }
        let mut segments = Vec::new();
        let Some((mut leaf, mut i, offset)) = self.locate(rows.start) else {
            return segments;
        };
        let mut offset = offset;
        let mut left = rows.end.min(self.len) - rows.start;
        while left > 0 {
            let Some(pieces) = self.leaves.get(leaf) else {
                break;
            };
            let Some(&piece) = pieces.pieces.get(i) else {
                leaf += 1;
                i = 0;
                continue;
            };
            let take = (piece.len() - offset).min(u32::try_from(left).unwrap_or(u32::MAX));
            segments.push(piece.slice(offset..offset + take).segment());
            left -= to_usize(take);
            offset = 0;
            i += 1;
        }
        segments
    }

    /// The live stretches of original rows among physical rows `rows`, in
    /// order.
    pub(crate) fn originals_in(&self, rows: Range<u32>) -> Vec<Range<u32>> {
        if rows.start >= rows.end {
            return Vec::new();
        }
        if self.physical.is_none() {
            return vec![rows];
        }
        let key = (rows.start, true);
        let mut leaf = self.keys.partition_point(|&k| k <= key).saturating_sub(1);
        let mut i = self
            .leaves
            .get(leaf)
            .map_or(0, |l| l.pieces.partition_point(|p| p.key() <= key))
            .saturating_sub(1);
        let mut live = Vec::new();
        while let Some(pieces) = self.leaves.get(leaf) {
            let Some(&piece) = pieces.pieces.get(i) else {
                leaf += 1;
                i = 0;
                continue;
            };
            if piece.key().0 >= rows.end {
                break;
            }
            if let Piece::Original { start, len } = piece {
                let from = start.max(rows.start);
                let to = (start + len).min(rows.end);
                if from < to {
                    live.push(from..to);
                }
            }
            i += 1;
        }
        live
    }

    /// The first original row still in the document at or after physical
    /// row `physical`: the file's row count if there is none.
    pub(crate) fn next_live(&self, physical: u32) -> u32 {
        let Some(rows) = self.physical else {
            return physical;
        };
        let logical = match self.logical_of(physical) {
            Ok(_) => return physical,
            Err(logical) => logical,
        };
        let Some((mut leaf, mut i, _)) = self.locate(logical) else {
            return rows;
        };
        while let Some(pieces) = self.leaves.get(leaf) {
            match pieces.pieces.get(i) {
                Some(Piece::Original { start, .. }) => return *start,
                Some(Piece::Inserted { .. }) => i += 1,
                None => {
                    leaf += 1;
                    i = 0;
                }
            }
        }
        rows
    }

    /// One past the last original row still in the document before
    /// physical row `physical`: 0 if there is none.
    pub(crate) fn live_end_before(&self, physical: u32) -> u32 {
        if self.physical.is_none() {
            return physical;
        }
        let (Ok(logical) | Err(logical)) = self.logical_of(physical);
        // The rows before it, last first.
        let Some(last) = logical.checked_sub(1) else {
            return 0;
        };
        let Some((mut leaf, mut i, offset)) = self.locate(last) else {
            return 0;
        };
        let mut offset = Some(offset);
        loop {
            match self.leaves[leaf].pieces[i] {
                Piece::Original { start, len } => {
                    return start + offset.map_or(len, |offset| offset + 1);
                }
                Piece::Inserted { .. } => {}
            }
            offset = None;
            if i > 0 {
                i -= 1;
            } else if leaf > 0 {
                leaf -= 1;
                i = self.leaves[leaf].pieces.len() - 1;
            } else {
                return 0;
            }
        }
    }

    /// A count of physical rows (an estimate of the file's, say) as logical
    /// rows: less the deleted rows, plus the inserted ones.
    pub(crate) fn shift(&self, physical: usize) -> usize {
        match self.physical {
            None => physical,
            Some(rows) => (physical + self.len).saturating_sub(to_usize(rows)),
        }
    }

    /// Every piece, in order (for tests and checks).
    #[cfg(test)]
    fn pieces(&self) -> Vec<Piece> {
        self.leaves
            .iter()
            .flat_map(|leaf| leaf.pieces.iter().copied())
            .collect()
    }

    /// Starts tracking rows of a file of `physical` rows, if the map is the
    /// identity.
    pub(crate) fn begin(&mut self, physical: u32) {
        if self.physical.is_some() {
            return;
        }
        self.physical = Some(physical);
        let pieces = if physical == 0 {
            Vec::new()
        } else {
            vec![Piece::Original {
                start: 0,
                len: physical,
            }]
        };
        self.leaves = vec![Arc::new(Leaf::new(pieces))];
        self.rebuild_top();
    }

    /// Puts `pieces` (in order) at logical row `at`. `false`, with the map
    /// unchanged, if they don't fit there: their rows would be out of file
    /// order, or before or after their gaps. The map must not be the
    /// identity ([`begin`](Self::begin)).
    pub(crate) fn insert(&mut self, at: usize, pieces: &[Piece]) -> bool {
        if at > self.len || pieces.is_empty() {
            return pieces.is_empty();
        }
        let (lo, hi) = self.window(at, at);
        let mut work = self.collect(lo..hi);
        let base = self.starts.get(lo).copied().unwrap_or(0);
        let index = split_at(&mut work, at - base);
        let after = index + pieces.len();
        work.splice(index..index, pieces.iter().copied());
        let first = index.saturating_sub(1);
        let last = (after + 1).min(work.len());
        if !work[first..last]
            .windows(2)
            .all(|pair| pair[0].precedes(pair[1]))
        {
            return false;
        }
        self.replace(lo..hi, work);
        true
    }

    /// Removes logical rows `rows` (within the map) and returns their
    /// pieces, in order. The map must not be the identity.
    pub(crate) fn remove(&mut self, rows: Range<usize>) -> Vec<Piece> {
        let rows = rows.start..rows.end.min(self.len);
        if rows.start >= rows.end {
            return Vec::new();
        }
        let (lo, hi) = self.window(rows.start, rows.end - 1);
        let mut work = self.collect(lo..hi);
        let base = self.starts.get(lo).copied().unwrap_or(0);
        let start = split_at(&mut work, rows.start - base);
        let end = split_at(&mut work, rows.end - base);
        let removed: Vec<Piece> = work.drain(start..end).collect();
        self.replace(lo..hi, work);
        removed
    }

    /// The leaves an edit between logical rows `first` and `last` works
    /// on: theirs, and one either side, for merging.
    fn window(&self, first: usize, last: usize) -> (usize, usize) {
        if self.leaves.is_empty() {
            return (0, 0);
        }
        let leaf_of = |row: usize| {
            self.starts
                .partition_point(|&start| start <= row)
                .saturating_sub(1)
        };
        let lo = leaf_of(first).saturating_sub(1);
        let hi = (leaf_of(last) + 2).min(self.leaves.len());
        (lo, hi)
    }

    fn collect(&self, leaves: Range<usize>) -> Vec<Piece> {
        self.leaves[leaves]
            .iter()
            .flat_map(|leaf| leaf.pieces.iter().copied())
            .collect()
    }

    /// Replaces leaves `leaves` with `work`'s pieces, merged and cut into
    /// leaves again, then goes back to the identity if the map is one.
    fn replace(&mut self, leaves: Range<usize>, work: Vec<Piece>) {
        // The window holds a leaf either side of the edit, so every piece
        // that could continue another after it is in `work`.
        let mut merged: Vec<Piece> = Vec::with_capacity(work.len());
        for piece in work {
            if let Some(last) = merged.last_mut()
                && let Some(both) = last.merge(piece)
            {
                *last = both;
            } else if piece.len() > 0 {
                merged.push(piece);
            }
        }
        let count = merged.len().div_ceil(LEAF).max(1);
        let size = merged.len().div_ceil(count).max(1);
        let mut new = Vec::with_capacity(count);
        let mut rest = merged.into_iter().peekable();
        while rest.peek().is_some() {
            new.push(Arc::new(Leaf::new(rest.by_ref().take(size).collect())));
        }
        self.leaves.splice(leaves, new);
        self.rebuild_top();
        let physical = self.physical.unwrap_or(0);
        let identity = match self.leaves.as_slice() {
            [] => physical == 0,
            [leaf] => match leaf.pieces.as_slice() {
                [] => physical == 0,
                [Piece::Original { start: 0, len }] => *len == physical,
                _ => false,
            },
            _ => false,
        };
        if identity {
            *self = RowMap::default();
        }
    }

    fn rebuild_top(&mut self) {
        self.leaves.retain(|leaf| !leaf.pieces.is_empty());
        self.starts.clear();
        self.keys.clear();
        let mut total = 0;
        for leaf in &self.leaves {
            self.starts.push(total);
            self.keys.push(leaf.pieces[0].key());
            total += leaf.rows();
        }
        self.len = total;
    }
}

/// Splits `pieces` so that one starts at row `row` (counted from the first
/// piece's start), and returns its index (`pieces.len()` at the end).
fn split_at(pieces: &mut Vec<Piece>, row: usize) -> usize {
    let mut seen = 0;
    for i in 0..pieces.len() {
        if seen == row {
            return i;
        }
        let len = to_usize(pieces[i].len());
        if row < seen + len {
            let offset = u32::try_from(row - seen).unwrap_or(u32::MAX);
            let piece = pieces[i];
            pieces[i] = piece.slice(0..offset);
            pieces.insert(i + 1, piece.slice(offset..piece.len()));
            return i + 1;
        }
        seen += len;
    }
    pieces.len()
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests;
