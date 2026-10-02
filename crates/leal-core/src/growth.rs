//! Growing a list that readers share, without making them wait for the copy.
//!
//! The row index's offsets and the diagnostics' row marks are behind
//! `RwLock`s that the main thread reads on every draw. When one of them
//! outgrows its room, or gives back what it grew into once the file is
//! indexed, the vector is copied, and copying tens of megabytes while the
//! write lock is held made every reader, the main thread's among them, wait
//! for it (phase 1 review, app-10). [`room`] makes the copy beforehand,
//! while the caller holds only the *read* lock, so readers carry on; the
//! caller then swaps it in under the write lock ([`swap_in`]), which takes
//! no time, and drops the old vector after releasing the lock.
//!
//! This works because each list has one writer (its indexer): nothing can
//! change the list between the copy and the swap. [`swap_in`] still checks
//! the length, and falls back to growing in place if it changed.

/// A copy of `list` with room for `extra` more items, if it needs one: when
/// `exact`, sized to hold exactly `list.len() + extra` (the last items of
/// the list), else at least double its `capacity` once that is too small,
/// as `Vec` grows. `None` if `list` already has the room it needs.
pub(crate) fn room<T: Copy>(
    list: &[T],
    capacity: usize,
    extra: usize,
    exact: bool,
) -> Option<Vec<T>> {
    let needed = list.len().saturating_add(extra);
    let target = if exact {
        if capacity == needed {
            return None;
        }
        needed
    } else {
        if needed <= capacity {
            return None;
        }
        needed.max(capacity.saturating_mul(2))
    };
    let mut copy = Vec::with_capacity(target);
    copy.extend_from_slice(list);
    Some(copy)
}

/// Puts `copy` (from [`room`]) in place of `list` if `list` hasn't changed
/// length since, and returns the old vector for the caller to drop once it
/// has released its lock. `None` (and `list` untouched) if there was no copy
/// or `list` changed.
pub(crate) fn swap_in<T>(list: &mut Vec<T>, copy: Option<Vec<T>>) -> Option<Vec<T>> {
    let mut copy = copy?;
    if copy.len() != list.len() {
        return None;
    }
    std::mem::swap(list, &mut copy);
    Some(copy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_copy_while_there_is_room() {
        let mut list = Vec::with_capacity(8);
        list.extend([1u32, 2, 3]);
        assert_eq!(room(&list, list.capacity(), 5, false), None);
    }

    #[test]
    fn a_full_list_gets_at_least_double_the_room() {
        let mut list = Vec::with_capacity(4);
        list.extend([1u32, 2, 3, 4]);
        let copy = room(&list, list.capacity(), 1, false).unwrap();
        assert_eq!(copy, [1, 2, 3, 4]);
        assert!(copy.capacity() >= 8);
        let big = room(&list, list.capacity(), 100, false).unwrap();
        assert!(big.capacity() >= 104);
    }

    #[test]
    fn exact_room_holds_exactly_the_last_items() {
        let mut list = Vec::with_capacity(64);
        list.extend([1u32, 2, 3]);
        let mut copy = room(&list, list.capacity(), 2, true).unwrap();
        assert_eq!(copy.capacity(), 5);
        copy.extend([4, 5]);
        assert_eq!(copy.capacity(), 5, "no growing when the last items go in");
        let mut exact = Vec::with_capacity(3);
        exact.extend([1u32, 2, 3]);
        assert_eq!(room(&exact, exact.capacity(), 0, true), None);
    }

    #[test]
    fn the_copy_goes_in_only_if_the_list_is_unchanged() {
        let mut list = vec![1u32, 2];
        let copy = room(&list, list.capacity(), 10, false);
        let old = swap_in(&mut list, copy).unwrap();
        assert_eq!(old, [1, 2]);
        assert_eq!(list, [1, 2]);
        assert!(list.capacity() >= 12);

        let copy = room(&list, list.capacity(), 100, false);
        list.push(3);
        assert_eq!(swap_in(&mut list, copy), None, "changed meanwhile");
        assert_eq!(list, [1, 2, 3]);
        assert_eq!(swap_in(&mut list, None), None);
    }
}
