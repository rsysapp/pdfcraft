//! Embed build: the document outline (bookmarks) compared entry by entry, for the save inspector
//! (see [`OutlineChanges`]). Entries are read depth first (see `structure`); they pair in order
//! by their depth and title (a longest common subsequence, after the common start and end), and
//! between two such pairs, entry by entry at the same depth (an entry whose title alone changed).

use std::collections::BTreeMap;

use super::structure::{Destination, NamedKey, OutlineEntry};
use super::{Budget, Fail, OutlineChanges, count};

/// The most cells of the pairing table for the entries between the common start and end: two
/// outlines of 2,048 entries that differ throughout (8 MiB). Edits that keep the titles (page
/// deletes and moves) leave no entries between them.
const MAX_CELLS: usize = 1 << 22;

/// Pair the entries of `before` with those of `after`, as (index in `before`, index in `after`),
/// in order (see the module documentation). Only titles and depths decide.
pub(crate) fn pair(before: &[OutlineEntry], after: &[OutlineEntry], budget: &mut Budget) -> Result<Vec<(usize, usize)>, Fail> {
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
    Ok(pairs)
}

/// Count what changed between `before` and `after`, their entries paired as [`pair`] pairs
/// them. `pages`: each base page present in the edited file, and its index there (a base page
/// missing was deleted). Fails on what no fact reports: paired entries that differ in anything
/// but their title, or whose destination changed other than by following its page (see
/// [`followed`]); added entries with an action other than going to a destination, or going
/// outside the file's pages.
pub(crate) fn compare(
    before: &[OutlineEntry],
    after: &[OutlineEntry],
    pairs: Vec<(usize, usize)>,
    pages: &BTreeMap<usize, usize>,
    budget: &mut Budget,
) -> Result<OutlineChanges, Fail> {
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
        let moved = followed(&a.destination, &b.destination, pages)
            .map_err(|_| Fail::Read("an outline entry's destination changed other than by following its page".into()))?;
        changes.destinations_changed = changes.destinations_changed.saturating_add(u32::from(moved));
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
        if !paired && matches!(entry.destination, Destination::Other(_) | Destination::Unpaged(_)) {
            return Err(Fail::Read("an added outline entry goes somewhere other than one of the file's pages".into()));
        }
    }
    changes.added = count(paired_after.iter().filter(|paired| !**paired).count());
    Ok(changes)
}

/// Whether a destination `after` followed its page from `before` (`true`: its page number
/// changed, or its page was deleted and it goes nowhere now: it has none, or its page is `null`
/// or a reference that names no page of the file), stayed as it was (`false`), or changed otherwise (an error:
/// another page, another view, a destination gained, a named or external destination changed).
/// Outline entries, named destinations and links' destinations all follow this rule.
pub(crate) fn followed(before: &Destination, after: &Destination, pages: &BTreeMap<usize, usize>) -> Result<bool, Fail> {
    match (before, after) {
        (Destination::Page(page, view), Destination::Page(now, same)) if pages.get(page) == Some(now) && view == same => Ok(page != now),
        (Destination::Page(page, _), Destination::None) if !pages.contains_key(page) => Ok(true),
        (Destination::Page(page, view), Destination::Unpaged(same)) if !pages.contains_key(page) && view == same => Ok(true),
        (Destination::None, Destination::None) => Ok(false),
        (Destination::Unpaged(view), Destination::Unpaged(same)) if view == same => Ok(false),
        (Destination::Named(name), Destination::Named(same)) if name == same => Ok(false),
        (Destination::Other(other), Destination::Other(same)) if other == same => Ok(false),
        _ => Err(Fail::Read("a destination changed other than by following its page".into())),
    }
}

/// How many named destinations followed their page (see `DestinationChanges::named`); fails on
/// any other change: a name added, a name gone whose page is still there, a destination changed
/// other than by following its page.
pub(crate) fn named(
    before: &BTreeMap<NamedKey, Destination>,
    after: &BTreeMap<NamedKey, Destination>,
    pages: &BTreeMap<usize, usize>,
    budget: &mut Budget,
) -> Result<u32, Fail> {
    let mut changed = 0u32;
    for (name, destination) in before {
        budget.tick()?;
        let moved = match after.get(name) {
            Some(now) => {
                followed(destination, now, pages).map_err(|_| Fail::Read("a named destination changed other than by following its page".into()))?
            }
            // An editor may drop the name of a page it deleted.
            None if matches!(destination, Destination::Page(page, _) if !pages.contains_key(page)) => true,
            None => return Err(Fail::Read("a named destination was removed, though its page is still there".into())),
        };
        changed = changed.saturating_add(u32::from(moved));
    }
    if after.keys().any(|name| !before.contains_key(name)) {
        return Err(Fail::Read("a named destination was added".into()));
    }
    Ok(changed)
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

    /// Three pages, each where it was.
    fn run(before: &[OutlineEntry], after: &[OutlineEntry]) -> Result<OutlineChanges, Fail> {
        run_with(before, after, &[(0, 0), (1, 1), (2, 2)])
    }

    fn run_with(before: &[OutlineEntry], after: &[OutlineEntry], pages: &[(usize, usize)]) -> Result<OutlineChanges, Fail> {
        let mut budget = Budget::new(Limits::default());
        let pairs = pair(before, after, &mut budget)?;
        compare(before, after, pairs, &pages.iter().copied().collect(), &mut budget)
    }

    #[test]
    fn entries_pair_by_depth_and_title_then_by_place() {
        let base = [entry(0, "Parties", 0), entry(0, "Scope", 1), entry(1, "Goods", 1), entry(0, "Term", 2)];
        assert_eq!(run(&base, &base).unwrap(), OutlineChanges::default());
        // Page 2 deleted: its entries lost their destination, the one after it follows the page.
        let mut deleted = base.clone();
        deleted[1].destination = Destination::None;
        deleted[2].destination = Destination::None;
        deleted[3].destination = Destination::Page(1, [0; 32]);
        let after_delete = [(0, 0), (2, 1)];
        assert_eq!(run_with(&base, &deleted, &after_delete).unwrap(), OutlineChanges { destinations_changed: 3, ..OutlineChanges::default() });
        // A destination that stays on its number while its page went is not where it was.
        let mut stale = deleted.clone();
        stale[3].destination = Destination::Page(2, [0; 32]);
        assert!(run_with(&base, &stale, &after_delete).is_err());
        // Losing a destination whose page is still there, another page, another view, gaining one.
        let mut lost = base.clone();
        lost[0].destination = Destination::None;
        assert!(run(&base, &lost).is_err());
        let mut retargeted = base.clone();
        retargeted[0].destination = Destination::Page(2, [0; 32]);
        assert!(run(&base, &retargeted).is_err());
        let mut zoomed = base.clone();
        zoomed[0].destination = Destination::Page(0, [9; 32]);
        assert!(run(&base, &zoomed).is_err());
        let mut gained = base.clone();
        gained[1].destination = Destination::None;
        assert!(run(&gained, &base).is_err());
        // Pages moved: every destination follows its page.
        let mut moved = base.clone();
        moved[0].destination = Destination::Page(1, [0; 32]);
        moved[1].destination = Destination::Page(2, [0; 32]);
        moved[2].destination = Destination::Page(2, [0; 32]);
        moved[3].destination = Destination::Page(0, [0; 32]);
        assert_eq!(
            run_with(&base, &moved, &[(0, 1), (1, 2), (2, 0)]).unwrap(),
            OutlineChanges { destinations_changed: 4, ..OutlineChanges::default() }
        );
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
        let mut outside = changed.clone();
        outside[3].destination = Destination::Other([3; 32]);
        assert!(run(&base, &outside).is_err());
    }
}
