//! What follows a page when pages are deleted, moved or inserted: destinations that give their page
//! by number (`[2 /Fit]`, read as a page index by PDFium and pdf.js), named destinations, and page
//! labels. Destinations that name their page by reference follow it by themselves; one whose
//! page was deleted loses it (R6b-34, R6b-43); each page keeps the label it showed (R6b-46).
//!
//! Every page edit that renumbers pages runs [`Before::read`] before and [`Before::follow`]
//! after; a viewer then shows the same label for each page and goes to the same page for each
//! destination as before.

use std::collections::{HashMap, HashSet};

use pdfcraft_cos::{Dict, Document, ObjRef, Object, PdfString};

use crate::{OrganizeError, walk};

/// One page's label: the range's label dictionary (`/S`, `/P`, … as written) and its number.
type Label = (Dict, i64);

/// The pages, and their labels, before a page edit.
pub(crate) struct Before {
    pages: Vec<ObjRef>,
    /// Each page's label; `None` when the document has no `/PageLabels`.
    labels: Option<Vec<Option<Label>>>,
}

/// What a destination does once pages moved.
enum Follow {
    Same,
    To(Object),
    Gone,
}

impl Before {
    pub(crate) fn read(doc: &Document) -> Result<Self, OrganizeError> {
        let pages: Vec<ObjRef> = walk(doc)?.into_iter().map(|(page, _)| page).collect();
        let labels = page_labels(doc, pages.len());
        Ok(Self { pages, labels })
    }

    /// After the edit: renumber destinations given by number, drop named destinations to deleted
    /// pages, and give each page the label it had.
    pub(crate) fn follow(self, doc: &mut Document) -> Result<(), OrganizeError> {
        let after: Vec<ObjRef> = walk(doc)?.into_iter().map(|(page, _)| page).collect();
        let position: HashMap<ObjRef, usize> = after.iter().enumerate().map(|(index, page)| (*page, index)).collect();
        let map: Vec<Option<usize>> = self.pages.iter().map(|page| position.get(page).copied()).collect();
        let gone: HashSet<ObjRef> = self.pages.iter().filter(|page| !position.contains_key(page)).copied().collect();
        let mut renumber = Renumber { map, gone, done: HashSet::new() };
        // Links on the pages that were there: an inserted page's links came with it.
        let kept: Vec<ObjRef> = self.pages.iter().filter(|page| position.contains_key(page)).copied().collect();
        renumber.holders(doc, &kept)?;
        renumber.named(doc)?;
        if let Some(labels) = self.labels {
            let old: HashMap<ObjRef, Option<Label>> = self.pages.iter().copied().zip(labels).collect();
            relabel(doc, &after, &old)?;
        }
        Ok(())
    }
}

struct Renumber {
    /// Old page index → new page index (`None`: deleted).
    map: Vec<Option<usize>>,
    gone: HashSet<ObjRef>,
    /// Indirect destinations and actions already followed: one shared by several holders moves once.
    done: HashSet<ObjRef>,
}

impl Renumber {
    /// A destination array as written.
    fn array(&self, items: &[Object]) -> Follow {
        match items.first() {
            Some(Object::Int(number)) => {
                let Some(new) = usize::try_from(*number).ok().and_then(|old| self.map.get(old)) else { return Follow::Same };
                match new {
                    Some(new) if i64::try_from(*new).ok() == Some(*number) => Follow::Same,
                    Some(new) => {
                        let mut items = items.to_vec();
                        if let Some(first) = items.first_mut() {
                            *first = Object::Int(i64::try_from(*new).unwrap_or(i64::MAX));
                        }
                        Follow::To(Object::Array(items))
                    }
                    None => Follow::Gone,
                }
            }
            Some(Object::Ref(page)) if self.gone.contains(page) => Follow::Gone,
            _ => Follow::Same,
        }
    }

    /// A destination value (an array, or a dictionary with `/D` when `with_d`), direct or
    /// indirect. An indirect one is changed in place, so its holder stays as it is.
    fn value(&mut self, doc: &mut Document, value: &Object, with_d: bool) -> Follow {
        if let Object::Ref(r) = value {
            if !self.done.insert(*r) {
                return Follow::Same;
            }
            let target = (*doc.get(*r)).clone();
            return match self.value(doc, &target, with_d) {
                Follow::To(new) => {
                    doc.set(*r, new);
                    Follow::Same
                }
                other => other,
            };
        }
        match value {
            Object::Array(items) => self.array(items),
            Object::Dict(dict) if with_d => match dict.get(b"D").cloned() {
                Some(d) => match self.value(doc, &d, false) {
                    Follow::To(new) => {
                        let mut dict = dict.clone();
                        dict.set(b"D".to_vec(), new);
                        Follow::To(Object::Dict(dict))
                    }
                    other => other,
                },
                None => Follow::Same,
            },
            _ => Follow::Same,
        }
    }

    /// Bookmarks and the links on the pages: `/Dest`, or a go-to action's `/D`. One whose page
    /// went loses the destination (or the action).
    fn holders(&mut self, doc: &mut Document, pages: &[ObjRef]) -> Result<(), OrganizeError> {
        let mut holders = crate::outline::items(doc);
        for page in pages {
            let annots =
                doc.get(*page).as_dict().and_then(|d| d.get(b"Annots").cloned()).map(|a| doc.resolve(&a).as_array().cloned().unwrap_or_default());
            holders.extend(
                annots
                    .unwrap_or_default()
                    .iter()
                    .filter_map(Object::as_ref)
                    .filter(|a| doc.get(*a).as_dict().is_some_and(|d| d.name(b"Subtype") == Some(b"Link"))),
            );
        }
        for holder in holders {
            let Some(dict) = doc.get(holder).as_dict().cloned() else { continue };
            if let Some(dest) = dict.get(b"Dest").cloned() {
                match self.value(doc, &dest, false) {
                    Follow::Same => {}
                    Follow::To(new) => doc.update_dict(holder, |d| d.set(b"Dest".to_vec(), new))?,
                    Follow::Gone => doc.update_dict(holder, |d| {
                        d.remove(b"Dest");
                    })?,
                }
            }
            let Some(action) = dict.get(b"A").cloned() else { continue };
            let resolved = doc.resolve(&action);
            let Some(action_dict) = resolved.as_dict().filter(|a| a.name(b"S") == Some(b"GoTo")).cloned() else { continue };
            let Some(d) = action_dict.get(b"D").cloned() else { continue };
            // An indirect action is changed in place, once.
            if let Object::Ref(r) = action
                && !self.done.insert(r)
            {
                continue;
            }
            match self.value(doc, &d, false) {
                Follow::Same => {}
                Follow::To(new) => {
                    let mut action_dict = action_dict;
                    action_dict.set(b"D".to_vec(), new);
                    match action {
                        Object::Ref(r) => doc.set(r, Object::Dict(action_dict)),
                        _ => doc.update_dict(holder, |h| h.set(b"A".to_vec(), Object::Dict(action_dict)))?,
                    }
                }
                Follow::Gone => doc.update_dict(holder, |h| {
                    h.remove(b"A");
                })?,
            }
        }
        Ok(())
    }

    /// Named destinations, in the catalog's `/Dests` and the `/Names /Dests` tree: one whose
    /// page went is removed, one given by number renumbered.
    fn named(&mut self, doc: &mut Document) -> Result<(), OrganizeError> {
        let Some(root) = doc.root() else { return Ok(()) };
        let catalog = doc.get(root).as_dict().cloned().unwrap_or_default();
        if let Some(entry) = catalog.get(b"Dests").cloned()
            && let Some(mut dests) = doc.resolve(&entry).as_dict().cloned()
        {
            let mut changed = false;
            let names: Vec<Vec<u8>> = dests.iter().map(|(key, _)| key.clone()).collect();
            for name in names {
                let Some(value) = dests.get(&name).cloned() else { continue };
                match self.value(doc, &value, true) {
                    Follow::Same => {}
                    Follow::To(new) => {
                        dests.set(name, new);
                        changed = true;
                    }
                    Follow::Gone => {
                        dests.remove(&name);
                        changed = true;
                    }
                }
            }
            if changed {
                match entry {
                    Object::Ref(r) => doc.set(r, Object::Dict(dests)),
                    _ => doc.update_dict(root, |c| c.set(b"Dests".to_vec(), Object::Dict(dests)))?,
                }
            }
        }
        let names_entry = catalog.get(b"Names").cloned();
        let Some(mut names) = names_entry.as_ref().and_then(|n| doc.resolve(n).as_dict().cloned()) else { return Ok(()) };
        let Some(top) = names.get(b"Dests").cloned() else { return Ok(()) };
        let mut entries = Vec::new();
        tree_entries(doc, &top, &mut entries, &mut HashSet::new(), 0)?;
        let mut changed = false;
        let mut kept = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            match self.value(doc, &value, true) {
                Follow::Same => kept.push((key, value)),
                Follow::To(new) => {
                    kept.push((key, new));
                    changed = true;
                }
                Follow::Gone => changed = true,
            }
        }
        if !changed {
            return Ok(());
        }
        // Written again as one leaf, in order: no node's limits can be wrong.
        kept.sort_by(|a, b| a.0.cmp(&b.0));
        let mut leaf = Dict::new();
        leaf.set(
            b"Names".to_vec(),
            Object::Array(kept.into_iter().flat_map(|(key, value)| [Object::String(PdfString::literal(key)), value]).collect()),
        );
        // Written over the tree's root when it is indirect: a second page edit in the same save
        // must not leave the first one's leaf behind, written and read by nothing.
        match top {
            Object::Ref(r) => {
                doc.set(r, Object::Dict(leaf));
                return Ok(());
            }
            _ => {
                let leaf = doc.add(Object::Dict(leaf));
                names.set(b"Dests".to_vec(), Object::Ref(leaf));
            }
        }
        match names_entry {
            Some(Object::Ref(r)) => doc.set(r, Object::Dict(names)),
            _ => doc.update_dict(root, |c| c.set(b"Names".to_vec(), Object::Dict(names)))?,
        }
        Ok(())
    }
}

/// Every (key, value) of the name tree under `node`, in tree order. A tree that loops, runs too
/// deep or holds a key that is not a string is refused: what it names could not be followed.
fn tree_entries(
    doc: &Document,
    node: &Object,
    out: &mut Vec<(Vec<u8>, Object)>,
    seen: &mut HashSet<ObjRef>,
    depth: usize,
) -> Result<(), OrganizeError> {
    let unreadable = || OrganizeError::Invalid("the named destinations cannot be read".into());
    if depth > 64 || out.len() > 100_000 {
        return Err(unreadable());
    }
    if let Some(r) = node.as_ref()
        && !seen.insert(r)
    {
        return Err(unreadable());
    }
    let Some(dict) = doc.resolve(node).as_dict().cloned() else { return Ok(()) };
    if let Some(items) = dict.get(b"Names").and_then(|n| doc.resolve(n).as_array().cloned()) {
        for pair in items.chunks(2) {
            let (Some(key), Some(value)) = (pair.first(), pair.get(1)) else { return Err(unreadable()) };
            let Some(key) = doc.resolve(key).as_string().map(|key| key.bytes.clone()) else { return Err(unreadable()) };
            out.push((key, value.clone()));
        }
    }
    if let Some(kids) = dict.get(b"Kids").and_then(|k| doc.resolve(k).as_array().cloned()) {
        for kid in kids {
            tree_entries(doc, &kid, out, seen, depth + 1)?;
        }
    }
    Ok(())
}

/// Each page's label (`None` where no range covers it), or `None` without `/PageLabels`.
fn page_labels(doc: &Document, pages: usize) -> Option<Vec<Option<Label>>> {
    let root = doc.root()?;
    let tree = doc.get(root).as_dict().and_then(|c| c.get(b"PageLabels").cloned())?;
    let mut ranges: Vec<(i64, Dict)> = Vec::new();
    let mut stack = vec![(tree, 0u8)];
    let mut seen = HashSet::new();
    while let Some((node, depth)) = stack.pop() {
        if depth > 32 || ranges.len() > 100_000 || node.as_ref().is_some_and(|r| !seen.insert(r)) {
            break;
        }
        let node = doc.resolve(&node);
        let Some(d) = node.as_dict() else { continue };
        if let Some(nums) = d.get(b"Nums").map(|n| doc.resolve(n)).and_then(|n| n.as_array().cloned()) {
            for pair in nums.chunks(2) {
                let (Some(start), Some(spec)) = (pair.first().and_then(Object::as_int), pair.get(1).and_then(|s| doc.resolve(s).as_dict().cloned()))
                else {
                    continue;
                };
                ranges.push((start, spec));
            }
        }
        if let Some(kids) = d.get(b"Kids").map(|k| doc.resolve(k)).and_then(|k| k.as_array().cloned()) {
            stack.extend(kids.into_iter().map(|k| (k, depth + 1)));
        }
    }
    ranges.sort_by_key(|(start, _)| *start);
    Some(
        (0..pages)
            .map(|page| {
                let page = i64::try_from(page).unwrap_or(i64::MAX);
                ranges.iter().rev().find(|(start, _)| *start <= page).map(|(start, spec)| {
                    let first = spec.int(b"St").unwrap_or(1);
                    (spec.clone(), first.saturating_add(page - start))
                })
            })
            .collect(),
    )
}

/// Give each page in `pages` the label it had (`old`); an inserted page shows its number. A page
/// that had none after one that has one cannot be written (only pages before the first range go
/// without), so the edit is refused rather than relabelling it.
fn relabel(doc: &mut Document, pages: &[ObjRef], old: &HashMap<ObjRef, Option<Label>>) -> Result<(), OrganizeError> {
    let mut nums: Vec<Object> = Vec::new();
    let mut previous: Option<Label> = None;
    for (index, page) in pages.iter().enumerate() {
        let number = i64::try_from(index).unwrap_or(i64::MAX).saturating_add(1);
        let label = match old.get(page) {
            Some(label) => label.clone(),
            // Inserted: its page number, unless no page before it has a label either.
            None => previous.as_ref().map(|_| {
                let mut decimal = Dict::new();
                decimal.set(b"S".to_vec(), Object::name("D"));
                (decimal, number)
            }),
        };
        let Some((spec, number)) = label else {
            if previous.is_some() {
                return Err(OrganizeError::Invalid("the page labels cannot follow their pages".into()));
            }
            continue;
        };
        let continues = previous
            .as_ref()
            .is_some_and(|(prev_spec, prev_number)| *prev_spec == spec && (spec.get(b"S").is_none() || prev_number.checked_add(1) == Some(number)));
        if !continues {
            let mut written = spec.clone();
            if number == 1 {
                written.remove(b"St");
            } else {
                written.set(b"St".to_vec(), Object::Int(number));
            }
            nums.push(Object::Int(i64::try_from(index).unwrap_or(i64::MAX)));
            nums.push(Object::Dict(written));
        }
        previous = Some((spec, number));
    }
    let root = doc.root().ok_or(OrganizeError::NoPageTree)?;
    if nums.is_empty() {
        doc.update_dict(root, |c| {
            c.remove(b"PageLabels");
        })?;
        return Ok(());
    }
    let mut tree = Dict::new();
    tree.set(b"Nums".to_vec(), Object::Array(nums));
    // Written over an indirect tree's root, so a later page edit in the same save leaves no
    // earlier tree behind.
    match doc.get(root).as_dict().and_then(|c| c.get(b"PageLabels").cloned()) {
        Some(Object::Ref(r)) => doc.set(r, Object::Dict(tree)),
        _ => {
            let tree = doc.add(Object::Dict(tree));
            doc.update_dict(root, |c| c.set(b"PageLabels".to_vec(), Object::Ref(tree)))?;
        }
    }
    Ok(())
}
