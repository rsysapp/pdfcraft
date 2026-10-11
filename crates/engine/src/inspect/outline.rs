//! Embed build: the document outline (bookmarks) compared entry by entry, for the save inspector
//! (see [`OutlineChanges`]). Entries are read depth first (see `structure`); they pair in order
//! by their depth and title (a longest common subsequence, after the common start and end), and
//! between two such pairs, entry by entry at the same depth (an entry whose title alone changed).

use super::structure::OutlineEntry;
use super::{Budget, Fail, OutlineChanges, count};

/// The most cells of the pairing table for the entries between the common start and end: two
/// outlines of 2,048 entries that differ throughout (8 MiB). Edits that keep the titles (page
/// deletes and moves) leave no entries between them.
const MAX_CELLS: usize = 1 << 22;

/// Pair `before` with `after` and count what changed. Paired entries that differ in anything but
/// their title and destination, and added entries with an action other than going to a
/// destination, fail: no fact reports them.
pub(crate) fn compare(before: &[OutlineEntry], after: &[OutlineEntry], budget: &mut Budget) -> Result<OutlineChanges, Fail> {
    let key = |entry: &OutlineEntry| (entry.depth, entry.title.clone());
    let prefix = before.iter().zip(after).take_while(|(a, b)| key(a) == key(b)).count();
    let (rest_before, rest_after) = (before.get(prefix..).unwrap_or_default(), after.get(prefix..).unwrap_or_default());
    let suffix = rest_before.iter().rev().zip(rest_after.iter().rev()).take_while(|(a, b)| key(a) == key(b)).count();
    let middle_before = rest_before.get(..rest_before.len().saturating_sub(suffix)).unwrap_or_default();
    let middle_after = rest_after.get(..rest_after.len().saturating_sub(suffix)).unwrap_or_default();
    if middle_before.len().saturating_mul(middle_after.len()) > MAX_CELLS {
        return Err(Fail::TooComplex);
    }
    // Pairs as indices into `before` and `after`, in order.
    let mut pairs: Vec<(usize, usize)> = (0..prefix).map(|index| (index, index)).collect();
    let anchors = in_order(middle_before, middle_after, &key, budget)?;
    // Between consecutive anchors (and the ends), entries at the same depth pair in order.
    let mut from = (0usize, 0usize);
    for &(a, b) in anchors.iter().chain(std::iter::once(&(middle_before.len(), middle_after.len()))) {
        let mut next = from.1;
        for i in from.0..a {
            let depth = middle_before.get(i).map(|entry| entry.depth);
            if let Some(j) = (next..b).find(|j| middle_after.get(*j).map(|entry| entry.depth) == depth) {
                pairs.push((prefix.saturating_add(i), prefix.saturating_add(j)));
                next = j.saturating_add(1);
            }
        }
        if a < middle_before.len() {
            pairs.push((prefix.saturating_add(a), prefix.saturating_add(b)));
        }
        from = (a.saturating_add(1), b.saturating_add(1));
    }
    let (start_before, start_after) = (before.len().saturating_sub(suffix), after.len().saturating_sub(suffix));
    pairs.extend((0..suffix).map(|index| (start_before.saturating_add(index), start_after.saturating_add(index))));
    let mut changes = OutlineChanges::default();
    let mut paired_before = vec![false; before.len()];
    let mut paired_after = vec![false; after.len()];
    for (i, j) in pairs {
        budget.tick()?;
        let (Some(a), Some(b)) = (before.get(i), after.get(j)) else { return Err(Fail::Read("an outline entry went missing".into())) };
        if a.rest != b.rest {
            return Err(Fail::Read("an outline entry changed other than in its title or destination (its style, colour or action)".into()));
        }
        changes.titles_changed = changes.titles_changed.saturating_add(u32::from(a.title != b.title));
        changes.destinations_changed = changes.destinations_changed.saturating_add(u32::from(a.destination != b.destination));
        if let Some(slot) = paired_before.get_mut(i) {
            *slot = true;
        }
        if let Some(slot) = paired_after.get_mut(j) {
            *slot = true;
        }
    }
    changes.removed = count(paired_before.iter().filter(|paired| !**paired).count());
    for (entry, paired) in after.iter().zip(&paired_after) {
        if !paired && entry.acts {
            return Err(Fail::Read("an added outline entry has an action other than going to a destination".into()));
        }
    }
    changes.added = count(paired_after.iter().filter(|paired| !**paired).count());
    Ok(changes)
}

type Key = (usize, Option<Vec<u8>>);

/// The longest common subsequence of `a` and `b` by `key`, as (index in `a`, index in `b`).
fn in_order(a: &[OutlineEntry], b: &[OutlineEntry], key: &impl Fn(&OutlineEntry) -> Key, budget: &mut Budget) -> Result<Vec<(usize, usize)>, Fail> {
    let (keys_a, keys_b): (Vec<Key>, Vec<Key>) = (a.iter().map(key).collect(), b.iter().map(key).collect());
    let width = keys_b.len().saturating_add(1);
    // A common subsequence is no longer than the shorter side: within `MAX_CELLS`, under 2^16.
    let mut table = vec![0u16; keys_a.len().saturating_add(1).saturating_mul(width)];
    let cell = |table: &[u16], i: usize, j: usize| table.get(i.saturating_mul(width).saturating_add(j)).copied().unwrap_or(0);
    for i in (0..keys_a.len()).rev() {
        budget.tick()?;
        for j in (0..keys_b.len()).rev() {
            let value = if keys_a.get(i) == keys_b.get(j) {
                cell(&table, i.saturating_add(1), j.saturating_add(1)).saturating_add(1)
            } else {
                cell(&table, i.saturating_add(1), j).max(cell(&table, i, j.saturating_add(1)))
            };
            if let Some(slot) = table.get_mut(i.saturating_mul(width).saturating_add(j)) {
                *slot = value;
            }
        }
    }
    let (mut i, mut j, mut pairs) = (0usize, 0usize, Vec::new());
    while i < keys_a.len() && j < keys_b.len() {
        if keys_a.get(i) == keys_b.get(j) {
            pairs.push((i, j));
            i = i.saturating_add(1);
            j = j.saturating_add(1);
        } else if cell(&table, i.saturating_add(1), j) >= cell(&table, i, j.saturating_add(1)) {
            i = i.saturating_add(1);
        } else {
            j = j.saturating_add(1);
        }
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::inspect::Limits;
    use crate::inspect::structure::Destination;

    /// An entry at `depth` titled `title`, going to page `page`.
    fn entry(depth: usize, title: &str, page: usize) -> OutlineEntry {
        OutlineEntry { depth, title: Some(title.as_bytes().to_vec()), destination: Destination::Page(page, [0; 32]), rest: [0; 32], acts: false }
    }

    fn run(before: &[OutlineEntry], after: &[OutlineEntry]) -> Result<OutlineChanges, Fail> {
        compare(before, after, &mut Budget::new(Limits::default()))
    }

    #[test]
    fn entries_pair_by_depth_and_title_then_by_place() {
        let base = [entry(0, "Parties", 0), entry(0, "Scope", 1), entry(1, "Goods", 1), entry(0, "Term", 2)];
        assert_eq!(run(&base, &base).unwrap(), OutlineChanges::default());
        // Page 2 deleted: its entry lost its destination, the one after it follows the page.
        let mut deleted = base.clone();
        deleted[1].destination = Destination::None;
        deleted[2].destination = Destination::None;
        deleted[3].destination = Destination::Page(1, [0; 32]);
        assert_eq!(run(&base, &deleted).unwrap(), OutlineChanges { destinations_changed: 3, ..OutlineChanges::default() });
        // A title changed in place.
        let mut renamed = base.clone();
        renamed[2].title = Some(b"Products".to_vec());
        assert_eq!(run(&base, &renamed).unwrap(), OutlineChanges { titles_changed: 1, ..OutlineChanges::default() });
        // An entry removed and another added at another depth.
        let changed = [entry(0, "Parties", 0), entry(0, "Scope", 1), entry(0, "Term", 2), entry(1, "Renewal", 2)];
        assert_eq!(run(&base, &changed).unwrap(), OutlineChanges { removed: 1, added: 1, ..OutlineChanges::default() });
        // Everything else must agree, and an added entry may not act.
        let mut styled = base.clone();
        styled[0].rest = [1; 32];
        assert!(run(&base, &styled).is_err());
        let mut acting = changed.clone();
        acting[3].acts = true;
        assert!(run(&base, &acting).is_err());
    }
}
