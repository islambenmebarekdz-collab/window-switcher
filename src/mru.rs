//! Pure most-recently-used (MRU) ordering helpers.
//!
//! These are deliberately free of any Win32 types so the ordering logic that
//! drives both the app switcher order and the per-app window selection can be
//! unit-tested in isolation. `app.rs` wires them to real window handles.

/// Move `item` to the front of `mru`, removing any existing occurrence so it
/// never appears twice. The front is the most-recently-used position.
pub fn promote<T: PartialEq>(mru: &mut Vec<T>, item: T) {
    mru.retain(|x| x != &item);
    mru.insert(0, item);
}

/// Drop every entry of `mru` that is not in `present`. Used to forget apps or
/// windows that have since been closed.
pub fn retain_present<T: PartialEq>(mru: &mut Vec<T>, present: &[T]) {
    mru.retain(|x| present.contains(x));
}

/// Compute a display order for `present` items: entries already known in `mru`
/// come first (in MRU order), followed by any `present` items not yet in `mru`
/// (in their given order). Closed items in `mru` are dropped, so the result is
/// also the pruned, up-to-date MRU list.
pub fn order_by_mru<T: Clone + PartialEq>(mru: &[T], present: &[T]) -> Vec<T> {
    let mut ordered: Vec<T> = mru
        .iter()
        .filter(|item| present.contains(item))
        .cloned()
        .collect();
    for item in present {
        if !ordered.contains(item) {
            ordered.push(item.clone());
        }
    }
    ordered
}

/// Return the index into `candidates` of the item that appears earliest (most
/// recent) in `mru`, or `None` when no candidate is present in `mru`.
pub fn most_recent_index<T: PartialEq>(mru: &[T], candidates: &[T]) -> Option<usize> {
    for item in mru {
        if let Some(i) = candidates.iter().position(|c| c == item) {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promote_moves_to_front_without_duplicating() {
        let mut mru = vec!["a", "b", "c"];
        promote(&mut mru, "c");
        assert_eq!(mru, vec!["c", "a", "b"]);
        // Promoting an item already at the front is a no-op in effect.
        promote(&mut mru, "c");
        assert_eq!(mru, vec!["c", "a", "b"]);
        // Promoting a brand-new item adds it at the front.
        promote(&mut mru, "z");
        assert_eq!(mru, vec!["z", "c", "a", "b"]);
    }

    #[test]
    fn retain_present_drops_closed_entries() {
        let mut mru = vec![1, 2, 3, 4];
        retain_present(&mut mru, &[3, 1]);
        // Order among survivors is preserved.
        assert_eq!(mru, vec![1, 3]);
    }

    #[test]
    fn order_by_mru_keeps_known_first_then_appends_new() {
        // b, a are known (b most recent); c, d are freshly seen.
        let mru = vec!["b", "a"];
        let present = vec!["a", "b", "c", "d"];
        assert_eq!(order_by_mru(&mru, &present), vec!["b", "a", "c", "d"]);
    }

    #[test]
    fn order_by_mru_prunes_closed_apps() {
        // "x" is in the MRU but no longer present, so it is dropped.
        let mru = vec!["x", "a"];
        let present = vec!["a", "b"];
        assert_eq!(order_by_mru(&mru, &present), vec!["a", "b"]);
    }

    #[test]
    fn most_recent_index_picks_the_freshest_candidate() {
        // Window MRU: 30 is most recent, then 10, then 20.
        let mru = vec![30, 10, 20];
        // App owns windows 10 and 20; 20 comes first in the app's own list.
        let candidates = vec![20, 10];
        // 10 is more recent than 20 in the MRU, so its index (1) wins.
        assert_eq!(most_recent_index(&mru, &candidates), Some(1));
    }

    #[test]
    fn most_recent_index_returns_none_when_no_history() {
        let mru: Vec<isize> = vec![99];
        let candidates = vec![1, 2, 3];
        assert_eq!(most_recent_index(&mru, &candidates), None);
    }
}
