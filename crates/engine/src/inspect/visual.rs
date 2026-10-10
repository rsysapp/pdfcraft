//! Embed build: is a rewritten page the base page with redactions applied, and nothing else?
//!
//! Both pages' content is rendered upright, without annotations, at the same scale. A pixel has
//! changed when a channel differs by more than [`NOISE`] (a rewritten text run lands a rounding
//! error away from where the original glyph-by-glyph advances put it, which moves an anti-aliased
//! edge by a level or two). Every connected region of changed pixels must be explained by
//! uniformly filled rectangles of the edited render:
//! - the region's fill colour is the colour most of its changed pixels now have;
//! - a fill is a connected part of what survives an opening of that colour's pixels by a 5 × 5
//!   square (so it is at least 5 pixels wide and high everywhere: strokes, glyph shapes and thin
//!   lines do not survive), reached from changed pixels of that colour;
//! - a fill has no holes and no notches: each of its rows and columns is one run (a box, or
//!   boxes overlapping like the line boxes of a selection). Content left standing inside it — the
//!   rest of a page whose background was tinted, text around a deleted word — makes holes;
//! - a fill lies within [`REACH`] pixels of changed pixels: a "box" that is mostly what was
//!   already there (a blank page after its text was deleted, white on white) is not a box drawn;
//! - every changed pixel lies within 2 pixels of a fill (its anti-aliased edge, and the 1-pixel
//!   tolerance at the rectangle's edge).

use std::collections::HashMap;

use pdfcraft_render::content::UprightRaster;

/// The largest difference in a channel (of 255) that is not a change: well below what can be seen.
const NOISE: u8 = 8;
/// A fill contains, at every one of its pixels, a square of this side (an odd number).
const FILL: usize = 5;
/// How far a changed pixel may be from a fill: its anti-aliased edge, and one more pixel.
const EDGE: usize = 2;
/// How far a fill may reach past the changed pixels (content of the fill colour it covered, such
/// as the strokes of redacted text, which do not change).
const REACH: usize = 16;
/// More fill colours than this on one page is not a set of redaction boxes.
const MAX_FILL_COLOURS: usize = 8;

fn channels(image: &UprightRaster, index: usize) -> Option<&[u8]> {
    let at = index.checked_mul(4)?;
    image.rgba.get(at..at.checked_add(4)?)
}

/// Whether pixel `index` differs between the renders by more than noise.
fn differs(base: &UprightRaster, edited: &UprightRaster, index: usize) -> bool {
    match (channels(base, index), channels(edited, index)) {
        (Some(a), Some(b)) => a.iter().zip(b).any(|(x, y)| x.abs_diff(*y) > NOISE),
        _ => true,
    }
}

fn colour(image: &UprightRaster, index: usize) -> Option<u32> {
    let bytes = channels(image, index)?;
    let mut word = [0u8; 4];
    for (slot, byte) in word.iter_mut().zip(bytes) {
        *slot = *byte;
    }
    Some(u32::from_le_bytes(word))
}

/// For each pixel, how many of the `2r + 1` pixels centred on it along a row (or a column) are set.
fn line_counts(mask: &[bool], width: usize, height: usize, radius: usize, horizontal: bool) -> Vec<usize> {
    let mut out = vec![0usize; mask.len()];
    let (lines, length) = if horizontal { (height, width) } else { (width, height) };
    let at = |line: usize, i: usize| if horizontal { line * width + i } else { i * width + line };
    let mut prefix = vec![0usize; length + 1];
    for line in 0..lines {
        let mut sum = 0usize;
        for (i, slot) in prefix.iter_mut().enumerate().skip(1) {
            sum += usize::from(mask.get(at(line, i - 1)).copied().unwrap_or(false));
            *slot = sum;
        }
        for i in 0..length {
            let low = prefix.get(i.saturating_sub(radius)).copied().unwrap_or(0);
            let high = prefix.get((i + radius + 1).min(length)).copied().unwrap_or(0);
            if let Some(slot) = out.get_mut(at(line, i)) {
                *slot = high - low;
            }
        }
    }
    out
}

/// Pixels whose whole `(2r + 1)²` square is set (outside the image counts as unset).
fn erode(mask: &[bool], width: usize, height: usize, radius: usize) -> Vec<bool> {
    let full = 2 * radius + 1;
    let rows: Vec<bool> = line_counts(mask, width, height, radius, true).into_iter().map(|n| n == full).collect();
    line_counts(&rows, width, height, radius, false).into_iter().map(|n| n == full).collect()
}

/// Pixels with a set pixel within their `(2r + 1)²` square.
fn dilate(mask: &[bool], width: usize, height: usize, radius: usize) -> Vec<bool> {
    let rows: Vec<bool> = line_counts(mask, width, height, radius, true).into_iter().map(|n| n > 0).collect();
    line_counts(&rows, width, height, radius, false).into_iter().map(|n| n > 0).collect()
}

/// The 8-connected neighbours of pixel `index` in a `width` × `height` grid.
fn neighbours(index: usize, width: usize, height: usize) -> impl Iterator<Item = usize> {
    let width = width.max(1);
    let (x, y) = (index % width, index / width);
    (-1i64..=1).flat_map(move |dy| (-1i64..=1).map(move |dx| (dx, dy))).filter_map(move |(dx, dy)| {
        if dx == 0 && dy == 0 {
            return None;
        }
        let nx = usize::try_from(x as i64 + dx).ok().filter(|nx| *nx < width)?;
        let ny = usize::try_from(y as i64 + dy).ok().filter(|ny| *ny < height)?;
        Some(ny * width + nx)
    })
}

/// The pixels 8-connected to `start` within `mask`, marking them in `visited`.
fn flood(start: usize, mask: &[bool], visited: &mut [bool], width: usize, height: usize) -> Vec<usize> {
    let mut region = Vec::new();
    let mut stack = vec![start];
    while let Some(index) = stack.pop() {
        let Some(seen) = visited.get_mut(index) else { continue };
        if *seen || !mask.get(index).copied().unwrap_or(false) {
            continue;
        }
        *seen = true;
        region.push(index);
        stack.extend(neighbours(index, width, height).filter(|&n| mask.get(n).copied().unwrap_or(false) && !visited.get(n).copied().unwrap_or(true)));
    }
    region
}

/// Whether every row and every column of `region` is a single run of pixels.
fn without_holes(region: &[usize], width: usize) -> bool {
    let width = width.max(1);
    let mut rows: HashMap<usize, (usize, usize, usize)> = HashMap::new();
    let mut columns: HashMap<usize, (usize, usize, usize)> = HashMap::new();
    for &index in region {
        let (x, y) = (index % width, index / width);
        for (key, value, runs) in [(y, x, &mut rows), (x, y, &mut columns)] {
            let run = runs.entry(key).or_insert((value, value, 0));
            *run = (run.0.min(value), run.1.max(value), run.2 + 1);
        }
    }
    rows.values().chain(columns.values()).all(|(low, high, count)| high - low + 1 == *count)
}

/// Whether the edited render differs from the base render anywhere filled boxes do not explain.
pub(crate) fn changed_outside_filled_rectangles(base: &UprightRaster, edited: &UprightRaster) -> bool {
    if base.width != edited.width || base.height != edited.height || base.rgba.len() != edited.rgba.len() {
        return true;
    }
    let (width, height) = (edited.width as usize, edited.height as usize);
    let total = width.saturating_mul(height);
    let changed: Vec<bool> = (0..total).map(|i| differs(base, edited, i)).collect();
    if !changed.contains(&true) {
        return false;
    }
    // Each changed region's fill colour: what most of its pixels are now.
    let mut fill_of = vec![None::<u32>; total];
    let mut visited = vec![false; total];
    let mut colours: Vec<u32> = Vec::new();
    for start in 0..total {
        if !changed.get(start).copied().unwrap_or(false) || visited.get(start).copied().unwrap_or(true) {
            continue;
        }
        let region = flood(start, &changed, &mut visited, width, height);
        let mut votes: HashMap<u32, usize> = HashMap::new();
        for &index in &region {
            if let Some(c) = colour(edited, index) {
                *votes.entry(c).or_default() += 1;
            }
        }
        let Some(fill) = votes.into_iter().max_by_key(|(c, n)| (*n, *c)).map(|(c, _)| c) else { return true };
        if !colours.contains(&fill) {
            if colours.len() >= MAX_FILL_COLOURS {
                return true;
            }
            colours.push(fill);
        }
        for index in region {
            if let Some(slot) = fill_of.get_mut(index) {
                *slot = Some(fill);
            }
        }
    }
    let near_change = dilate(&changed, width, height, REACH);
    let radius = FILL / 2;
    for fill in colours {
        let painted: Vec<bool> = (0..total).map(|i| colour(edited, i) == Some(fill)).collect();
        let opened = dilate(&erode(&painted, width, height, radius), width, height, radius);
        // The fills this colour's changed regions drew, and whether each is a box.
        let mut boxes = vec![false; total];
        let mut reached = vec![false; total];
        for start in 0..total {
            let seed = fill_of.get(start).copied().flatten() == Some(fill) && opened.get(start).copied().unwrap_or(false);
            if !seed || reached.get(start).copied().unwrap_or(true) {
                continue;
            }
            let region = flood(start, &opened, &mut reached, width, height);
            let drawn = region.iter().all(|i| near_change.get(*i).copied().unwrap_or(false)) && without_holes(&region, width);
            if drawn {
                for index in region {
                    if let Some(slot) = boxes.get_mut(index) {
                        *slot = true;
                    }
                }
            }
        }
        let explained = dilate(&boxes, width, height, EDGE);
        let unexplained = (0..total).any(|i| fill_of.get(i).copied().flatten() == Some(fill) && !explained.get(i).copied().unwrap_or(false));
        if unexplained {
            return true;
        }
    }
    false
}

/// Whether `area` (user space) is, in the edited render, one uniform colour that was not
/// already all that colour in the base render: a redaction mark applied as a box.
pub(crate) fn area_filled(base: &UprightRaster, edited: &UprightRaster, area: [f64; 4]) -> bool {
    if base.width != edited.width || base.height != edited.height {
        return false;
    }
    let scale = f64::from(edited.scale);
    let [cx, cy] = [f64::from(edited.crop[0]), f64::from(edited.crop[1])];
    let height_pt = f64::from(edited.height_pt);
    let x0 = (area[0] - cx) * scale;
    let x1 = (area[2] - cx) * scale;
    let top = (height_pt - (area[3] - cy)) * scale;
    let bottom = (height_pt - (area[1] - cy)) * scale;
    if ![x0, x1, top, bottom].iter().all(|v| v.is_finite()) {
        return false;
    }
    // Inside the box, away from its anti-aliased edge.
    let pixel = |v: f64, limit: u32| v.clamp(0.0, f64::from(limit)) as usize;
    let (px0, px1) = (pixel(x0.ceil() + 1.0, edited.width), pixel(x1.floor() - 1.0, edited.width));
    let (py0, py1) = (pixel(top.ceil() + 1.0, edited.height), pixel(bottom.floor() - 1.0, edited.height));
    if px1 <= px0 || py1 <= py0 {
        return false;
    }
    let width = edited.width as usize;
    let mut fill = None;
    let mut covered_something = false;
    for y in py0..py1 {
        for x in px0..px1 {
            let index = y * width + x;
            let here = colour(edited, index);
            match fill {
                None => fill = here,
                Some(c) if here != Some(c) => return false,
                Some(_) => {}
            }
            covered_something |= differs(base, edited, index);
        }
    }
    fill.is_some() && covered_something
}
