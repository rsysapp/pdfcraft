//! Embed build: which glyphs of a page are still there after an edit. A glyph is its name and its
//! place: an edited glyph is the base glyph with the same name (its text, or for a glyph without
//! a Unicode value its font and glyph ID) and the same optional-content membership (the groups it
//! is inside and whether it shows by default, R6b-29) whose em box lies within
//! [`POSITION_TOLERANCE`] on every side. Glyphs are paired one to one (a maximum matching, so a
//! glyph drawn twice at one place pairs twice), with no alignment by reading order: what is left
//! over in the base was removed, what is left over in the edited page was added. The glyphs
//! paired must then be drawn in the same order on both pages ([`Pairing::in_order`]): text kept
//! at its places but drawn in another order reads otherwise. A space drawn as a glyph counts like
//! any other glyph.

use std::collections::HashMap;

use super::structure::Glyphs;
use super::{Budget, Fail};

/// How far (PDF points) an edited glyph's em box may lie from the base glyph's on each side. A
/// redactor rewrites the text it keeps (a `TJ` adjustment where glyphs were removed), which moves
/// glyphs by rounding errors, hundredths of a point.
pub(crate) const POSITION_TOLERANCE: f32 = 0.5;

/// The base glyphs a glyph may pair with, at most: more base glyphs with the same text within the
/// tolerance of one place than this is not text a page draws.
const MAX_CANDIDATES: usize = 64;

/// The candidate pairs the augmenting-path search may try for one page, in all: a page needs far
/// fewer (a few per glyph drawn more than once at one place).
const MAX_SEARCH_STEPS: u64 = 20_000_000;

/// What pairing the glyphs of two pages found: indices of unpaired glyphs.
pub(crate) struct Pairing {
    /// Base glyphs with no edited glyph: removed.
    pub removed: Vec<usize>,
    /// Edited glyphs with no base glyph: added.
    pub added: Vec<usize>,
    /// The glyphs paired are drawn in the same order on both pages: taken in the edited page's
    /// order, the base glyphs they pair with come in the base page's order. Drawing order is the
    /// order readers extract text in (copying, search, reading aloud), so text drawn in another
    /// order at the same places reads otherwise.
    pub in_order: bool,
}

const NONE: u32 = u32::MAX;

fn cell(value: f32) -> i64 {
    // Finite by construction (the extraction refuses other boxes), at most ±10,000 + page offsets.
    value.floor() as i64
}

fn centre(rect: [f32; 4]) -> (f32, f32) {
    ((rect[0] + rect[2]) / 2.0, (rect[1] + rect[3]) / 2.0)
}

fn same_place(a: [f32; 4], b: [f32; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= POSITION_TOLERANCE)
}

fn distance(a: [f32; 4], b: [f32; 4]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum()
}

/// Pair the glyphs of `edited` with those of `base`.
pub(crate) fn pair(base: &Glyphs, edited: &Glyphs, budget: &mut Budget) -> Result<Pairing, Fail> {
    pair_by(base, edited, true, budget)
}

/// Pair them by name and place alone, whatever optional content each is in: fewer glyphs left
/// over than [`pair`] leaves means some changed only their membership.
pub(crate) fn pair_ignoring_layers(base: &Glyphs, edited: &Glyphs, budget: &mut Budget) -> Result<Pairing, Fail> {
    pair_by(base, edited, false, budget)
}

fn pair_by(base: &Glyphs, edited: &Glyphs, layers: bool, budget: &mut Budget) -> Result<Pairing, Fail> {
    // Base glyphs by the 1-point cell of their centre: a glyph within the tolerance of another
    // has its centre within half a point, so in the same or a neighbouring cell.
    let mut cells: HashMap<(i64, i64), Vec<u32>> = HashMap::new();
    for index in 0..base.len() {
        budget.tick()?;
        let Some((_, rect)) = base.glyph(index) else { return Err(Fail::Read("a glyph went missing".into())) };
        let (x, y) = centre(rect);
        cells.entry((cell(x), cell(y))).or_default().push(u32::try_from(index).map_err(|_| Fail::TooComplex)?);
    }
    // Each edited glyph's candidates, nearest first.
    let mut candidates: Vec<Vec<u32>> = Vec::with_capacity(edited.len());
    for index in 0..edited.len() {
        budget.tick()?;
        let Some((text, rect)) = edited.glyph(index) else { return Err(Fail::Read("a glyph went missing".into())) };
        let (x, y) = centre(rect);
        let mut found: Vec<(f32, u32)> = Vec::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                let Some(list) = cells.get(&(cell(x).saturating_add(dx), cell(y).saturating_add(dy))) else { continue };
                for &other in list {
                    budget.tick()?;
                    let Some((other_text, other_rect)) = base.glyph(other as usize) else { continue };
                    if other_text == text && same_place(other_rect, rect) && (!layers || base.layer(other as usize) == edited.layer(index)) {
                        found.push((distance(other_rect, rect), other));
                    }
                }
            }
        }
        if found.len() > MAX_CANDIDATES {
            return Err(Fail::TooComplex);
        }
        found.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        candidates.push(found.into_iter().map(|(_, other)| other).collect());
    }
    let mut base_to_edited = vec![NONE; base.len()];
    let mut edited_to_base = vec![NONE; edited.len()];
    // The nearest free candidate first: almost every glyph pairs here.
    for (index, list) in candidates.iter().enumerate() {
        if let Some(&free) = list.iter().find(|&&other| base_to_edited.get(other as usize) == Some(&NONE)) {
            set(&mut base_to_edited, free, index);
            set(&mut edited_to_base, index as u32, free as usize);
        }
    }
    // Then augmenting paths for the rest (glyphs drawn several times at one place).
    let mut seen = vec![0u32; base.len()];
    let mut steps = 0u64;
    for index in 0..edited.len() {
        if edited_to_base.get(index) != Some(&NONE) {
            continue;
        }
        let stamp = u32::try_from(index).map_err(|_| Fail::TooComplex)?.saturating_add(1);
        augment(index, &candidates, (&mut base_to_edited, &mut edited_to_base), &mut seen, stamp, &mut steps, budget)?;
    }
    let mut last = None;
    let mut in_order = true;
    for &other in edited_to_base.iter().filter(|other| **other != NONE) {
        in_order &= last.is_none_or(|last| other > last);
        last = Some(other);
    }
    Ok(Pairing {
        in_order,
        removed: base_to_edited.iter().enumerate().filter(|(_, other)| **other == NONE).map(|(index, _)| index).collect(),
        added: edited_to_base.iter().enumerate().filter(|(_, other)| **other == NONE).map(|(index, _)| index).collect(),
    })
}

fn set(slots: &mut [u32], at: u32, value: usize) {
    if let Some(slot) = slots.get_mut(at as usize) {
        *slot = u32::try_from(value).unwrap_or(NONE);
    }
}

/// One step of an augmenting-path search: an edited glyph, the next candidate to try, and the
/// base glyph taken through it.
struct Frame {
    edited: usize,
    next: usize,
    taken: u32,
}

/// Find an augmenting path from edited glyph `start` (Kuhn's algorithm, iteratively: a path can
/// be as long as a page's glyphs).
fn augment(
    start: usize,
    candidates: &[Vec<u32>],
    (base_to_edited, edited_to_base): (&mut [u32], &mut [u32]),
    seen: &mut [u32],
    stamp: u32,
    steps: &mut u64,
    budget: &mut Budget,
) -> Result<bool, Fail> {
    let mut stack = vec![Frame { edited: start, next: 0, taken: NONE }];
    while let Some(top) = stack.last_mut() {
        let Some(&base) = candidates.get(top.edited).and_then(|list| list.get(top.next)) else {
            stack.pop();
            continue;
        };
        top.next = top.next.saturating_add(1);
        *steps = steps.saturating_add(1);
        if *steps > MAX_SEARCH_STEPS {
            return Err(Fail::TooComplex);
        }
        budget.tick()?;
        let Some(mark) = seen.get_mut(base as usize) else { continue };
        if *mark == stamp {
            continue;
        }
        *mark = stamp;
        top.taken = base;
        match base_to_edited.get(base as usize).copied() {
            Some(NONE) => {
                // Free: every edited glyph on the path takes the base glyph it went through.
                for frame in &stack {
                    set(base_to_edited, frame.taken, frame.edited);
                    set(edited_to_base, frame.edited as u32, frame.taken as usize);
                }
                return Ok(true);
            }
            Some(owner) => stack.push(Frame { edited: owner as usize, next: 0, taken: NONE }),
            None => {}
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::inspect::Limits;

    /// Glyphs at `x` named by text, or, written `#n` (font A) or `%n` (font B), without text: by
    /// font and glyph ID `n`.
    fn glyphs(list: &[(&str, f32)]) -> Glyphs {
        let mut out = Glyphs::default();
        for (text, x) in list {
            let font = match text.chars().next() {
                Some('#') => Some([1u8; 32]),
                Some('%') => Some([2u8; 32]),
                _ => None,
            };
            match font {
                Some(font) => {
                    out.unnamed.insert(out.page.glyphs.len() as u32, (font, text[1..].parse().unwrap()));
                }
                None => out.page.chars.extend(text.chars()),
            }
            out.page.glyphs.push((out.page.chars.len() as u32, [*x, 100.0, x + 6.0, 112.0]));
        }
        out
    }

    fn run(base: &[(&str, f32)], edited: &[(&str, f32)]) -> (Vec<usize>, Vec<usize>) {
        let mut budget = Budget::new(Limits::default());
        let pairing = pair(&glyphs(base), &glyphs(edited), &mut budget).unwrap();
        (pairing.removed, pairing.added)
    }

    #[test]
    fn glyphs_pair_by_text_and_place() {
        // Unchanged, and moved by a rounding error.
        assert_eq!(run(&[("a", 10.0), ("b", 16.0)], &[("a", 10.0), ("b", 16.3)]), (vec![], vec![]));
        // The first of two equal glyphs removed: it is the one reported, whatever else remains.
        assert_eq!(run(&[("0", 10.0), ("0", 16.0)], &[("0", 16.0)]), (vec![0], vec![]));
        // Moved by more than the tolerance: removed there, added here.
        assert_eq!(run(&[("a", 10.0)], &[("a", 10.6)]), (vec![0], vec![0]));
        // Another character at the same place.
        assert_eq!(run(&[("a", 10.0)], &[("e", 10.0)]), (vec![0], vec![0]));
        // A space is a glyph.
        assert_eq!(run(&[("1", 10.0), (" ", 16.0), ("0", 22.0)], &[("1", 10.0), ("0", 22.0)]), (vec![1], vec![]));
    }

    #[test]
    fn glyphs_without_text_pair_by_font_and_glyph() {
        // The same glyph of the same font at the same place.
        assert_eq!(run(&[("#7", 10.0), ("#8", 16.0)], &[("#7", 10.0), ("#8", 16.2)]), (vec![], vec![]));
        // Another glyph of the font, or the same glyph ID of another font, is another glyph.
        assert_eq!(run(&[("#7", 10.0)], &[("#9", 10.0)]), (vec![0], vec![0]));
        assert_eq!(run(&[("#7", 10.0)], &[("%7", 10.0)]), (vec![0], vec![0]));
        // A glyph without text is never one with text, even an empty one.
        assert_eq!(run(&[("#7", 10.0)], &[("", 10.0)]), (vec![0], vec![0]));
        // Removed between named glyphs.
        assert_eq!(run(&[("a", 10.0), ("#7", 16.0), ("b", 22.0)], &[("a", 10.0), ("b", 22.0)]), (vec![1], vec![]));
    }

    #[test]
    fn glyphs_drawn_twice_at_one_place_pair_one_to_one() {
        // Bold drawn twice, 0.3 pt apart: both pair.
        assert_eq!(run(&[("B", 10.0), ("B", 10.3)], &[("B", 10.0), ("B", 10.3)]), (vec![], vec![]));
        // A third copy added.
        assert_eq!(run(&[("B", 10.0), ("B", 10.3)], &[("B", 10.0), ("B", 10.3), ("B", 10.1)]), (vec![], vec![2]));
        // A chain the nearest-first pass gets wrong (edited 10.25 takes base 10.0, leaving the
        // edited 9.6 nothing): the augmenting path pairs all of them.
        assert_eq!(run(&[("x", 10.0), ("x", 10.5)], &[("x", 10.25), ("x", 9.6)]), (vec![], vec![]));
    }

    #[test]
    fn too_many_glyphs_at_one_place_is_too_complex() {
        let base: Vec<(&str, f32)> = (0..100).map(|_| ("x", 10.0)).collect();
        let mut budget = Budget::new(Limits::default());
        assert!(matches!(pair(&glyphs(&base), &glyphs(&base), &mut budget), Err(Fail::TooComplex)));
    }
}
