//! Embed build: is a rewritten page the base page with redactions applied, and nothing else?
//!
//! Both pages' content is rendered upright, without annotations, at the same scale. A pixel has
//! changed when a channel differs by more than [`NOISE`] (a rewritten text run lands a rounding
//! error away from where the original glyph-by-glyph advances put it, which moves an anti-aliased
//! edge by a level or two). Every connected region of changed pixels must be explained by
//! uniformly filled rectangles of the edited render:
//! - the region's fill colour is the colour most of its changed pixels now have, and it must be
//!   the editor's redaction fill: opaque and in [`REDACTION_FILLS`] (pdfcraft's Redact look fills
//!   black). A box of another colour, or one with transparency (which blends with what lies under
//!   it), is not an applied redaction: a deletion covered by a coloured box can change the meaning
//!   of what is left (drop "not"), and only the editor's own fill is accepted as a redaction;
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

/// The fills a redaction is applied with, as rendered (premultiplied RGBA): opaque black, the
/// Redact look's fill (`pdfcraft_annot`, `Style::default_for` of `Shape::Redact`). An embed that
/// offered other fill colours would have its redactions refused until they are listed here.
pub(crate) const REDACTION_FILLS: [u32; 1] = [u32::from_le_bytes([0, 0, 0, 255])];

/// The largest difference in a channel (of 255) that is not a change: well below what can be seen.
const NOISE: u8 = 8;
/// A fill contains, at every one of its pixels, a square of this side (an odd number).
const FILL: usize = 5;
/// How far a changed pixel may be from a fill: its anti-aliased edge, and one more pixel.
const EDGE: usize = 2;
/// How far a fill may reach past the changed pixels (content of the fill colour it covered, such
/// as the strokes of redacted text, which do not change).
const REACH: usize = 16;

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

/// A rectangle of render pixels: columns `x0..x1`, rows `y0..y1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PixelRect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

/// What the visual comparison found on a page.
#[derive(Debug, Default)]
pub(crate) struct Verdict {
    /// Something changed that the redaction boxes do not explain.
    pub outside: bool,
    /// The redaction boxes that explain the changes (empty when `outside`), each box's area as
    /// rectangles: boxes that overlap (the line boxes of one mark) are one area, split into the
    /// fewest row bands.
    pub boxes: Vec<PixelRect>,
}

const OUTSIDE: Verdict = Verdict { outside: true, boxes: Vec::new() };

/// A hole-free region (each row one run) as row bands of equal runs.
fn bands(region: &[usize], width: usize) -> Vec<PixelRect> {
    let width = width.max(1);
    let mut rows: std::collections::BTreeMap<usize, (usize, usize)> = std::collections::BTreeMap::new();
    for &index in region {
        let (x, y) = (index % width, index / width);
        let run = rows.entry(y).or_insert((x, x));
        *run = (run.0.min(x), run.1.max(x));
    }
    let mut out: Vec<PixelRect> = Vec::new();
    for (y, (x0, x1)) in rows {
        match out.last_mut() {
            Some(last) if last.y1 == y && last.x0 == x0 && last.x1 == x1 + 1 => last.y1 = y + 1,
            _ => out.push(PixelRect { x0, y0: y, x1: x1 + 1, y1: y + 1 }),
        }
    }
    out
}

/// Whether the edited render differs from the base render anywhere the editor's redaction boxes
/// do not explain, and which boxes explain the rest.
pub(crate) fn redaction_boxes(base: &UprightRaster, edited: &UprightRaster) -> Verdict {
    if base.width != edited.width || base.height != edited.height || base.rgba.len() != edited.rgba.len() {
        return OUTSIDE;
    }
    let (width, height) = (edited.width as usize, edited.height as usize);
    let total = width.saturating_mul(height);
    let changed: Vec<bool> = (0..total).map(|i| differs(base, edited, i)).collect();
    if !changed.contains(&true) {
        return Verdict::default();
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
        let Some(fill) = votes.into_iter().max_by_key(|(c, n)| (*n, *c)).map(|(c, _)| c) else { return OUTSIDE };
        if !REDACTION_FILLS.contains(&fill) {
            return OUTSIDE;
        }
        if !colours.contains(&fill) {
            colours.push(fill);
        }
        for index in region {
            if let Some(slot) = fill_of.get_mut(index) {
                *slot = Some(fill);
            }
        }
    }
    let near_change = dilate(&changed, width, height, REACH);
    let mut verdict = Verdict::default();
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
                verdict.boxes.extend(bands(&region, width));
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
            return OUTSIDE;
        }
    }
    verdict
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

/// Follow-up 9 (R6b-54, N10): whether the base render shows anything but blank paper inside
/// `area` (user space, away from its edge): what a box over it covered was drawn content (a
/// path, a shading), so a redaction there removed some.
pub(crate) fn drawn_under(base: &UprightRaster, area: [f64; 4]) -> bool {
    let scale = f64::from(base.scale);
    let [cx, cy] = [f64::from(base.crop[0]), f64::from(base.crop[1])];
    let height_pt = f64::from(base.height_pt);
    let x0 = (area[0] - cx) * scale;
    let x1 = (area[2] - cx) * scale;
    let top = (height_pt - (area[3] - cy)) * scale;
    let bottom = (height_pt - (area[1] - cy)) * scale;
    if ![x0, x1, top, bottom].iter().all(|v| v.is_finite()) {
        return false;
    }
    let pixel = |v: f64, limit: u32| v.clamp(0.0, f64::from(limit)) as usize;
    let (px0, px1) = (pixel(x0.ceil() + 1.0, base.width), pixel(x1.floor() - 1.0, base.width));
    let (py0, py1) = (pixel(top.ceil() + 1.0, base.height), pixel(bottom.floor() - 1.0, base.height));
    let width = base.width as usize;
    (py0..py1).any(|y| (px0..px1).any(|x| colour(base, y * width + x).is_some_and(|c| c != u32::MAX)))
}

/// A comparison render reduced to blocks of [`BLOCK`] × [`BLOCK`] pixels, for pairing changed
/// pages that share no glyph or image (R6b-38): each block's mean luminance, in quarters of a
/// level, and whether it holds a pixel of what could be a redaction box: a pixel of the fill
/// colour that a [`FILL`] × [`FILL`] square of that colour covers (the fills
/// [`redaction_boxes`] accepts are made of such pixels; glyph strokes and thin lines are not).
pub(crate) struct Thumb {
    width: u32,
    height: u32,
    columns: usize,
    rows: usize,
    mean: Vec<u16>,
    filled: Vec<bool>,
}

/// The side of a [`Thumb`]'s blocks, in render pixels: more than [`EDGE`], so that a changed
/// pixel's fill lies in its block or a neighbouring one. A 1,600-pixel render gives about 124
/// blocks a side.
pub(crate) const BLOCK: usize = 13;

impl Thumb {
    /// The bytes it holds, for the pairing's memory bound.
    pub fn bytes(&self) -> usize {
        self.mean.len().saturating_mul(3)
    }

    /// Its blocks.
    pub fn blocks(&self) -> usize {
        self.mean.len()
    }
}

#[cfg(test)]
impl Thumb {
    /// A render's blocks that tell page `id` from any other page, for tests of the pairing.
    pub fn for_tests(id: usize) -> Self {
        let columns = usize::BITS as usize;
        let mean = (0..columns).map(|bit| if id >> bit & 1 == 1 { 800 } else { 0 }).collect();
        Thumb { width: 100, height: 100, columns, rows: 1, mean, filled: vec![false; columns] }
    }
}

/// The render reduced to a [`Thumb`].
pub(crate) fn thumb(raster: &UprightRaster) -> Thumb {
    let (width, height) = (raster.width as usize, raster.height as usize);
    let columns = width.div_ceil(BLOCK);
    let rows = height.div_ceil(BLOCK);
    let blocks = columns.saturating_mul(rows);
    // Luminance in thousandths of a level, summed per block.
    let mut sums = vec![0u64; blocks];
    let mut counts = vec![0u64; blocks];
    let mut filled = vec![false; blocks];
    let total = width.saturating_mul(height);
    let painted: Vec<bool> = (0..total).map(|index| colour(raster, index).is_some_and(|c| REDACTION_FILLS.contains(&c))).collect();
    let radius = FILL / 2;
    let boxed = dilate(&erode(&painted, width, height, radius), width, height, radius);
    for y in 0..height {
        for x in 0..width {
            let index = y.saturating_mul(width).saturating_add(x);
            let block = (y / BLOCK).saturating_mul(columns).saturating_add(x / BLOCK);
            let Some(pixel) = channels(raster, index) else { continue };
            let (r, g, b) = (pixel.first(), pixel.get(1), pixel.get(2));
            let (Some(r), Some(g), Some(b)) = (r, g, b) else { continue };
            let luminance = u64::from(*r) * 299 + u64::from(*g) * 587 + u64::from(*b) * 114;
            if let (Some(sum), Some(count)) = (sums.get_mut(block), counts.get_mut(block)) {
                *sum = sum.saturating_add(luminance);
                *count = count.saturating_add(1);
            }
            if boxed.get(index).copied().unwrap_or(false)
                && let Some(slot) = filled.get_mut(block)
            {
                *slot = true;
            }
        }
    }
    // Quarters of a level (at most 1,020), rounded down.
    let mean = sums.iter().zip(&counts).map(|(sum, count)| if *count == 0 { 0 } else { (sum.saturating_mul(4) / 1_000 / count) as u16 }).collect();
    Thumb { width: raster.width, height: raster.height, columns, rows, mean, filled }
}

/// How many blocks of `base` and `edited` certainly differ (their mean luminance by more than
/// [`NOISE`]) with no box of the redaction fill in the edited render in them or beside them; `None` when the
/// renders' sizes differ. Zero is necessary for [`redaction_boxes`] to find nothing outside the
/// boxes: a mean differing by more than `NOISE` means a pixel whose luminance, so one of whose
/// channels, differs by more than `NOISE`, and every such pixel lies within [`EDGE`] pixels of a
/// box, made of fill pixels a [`FILL`]-pixel square covers, in its block or a neighbouring one.
/// Black text of the edited page is no box: content that differs beside it is not explained. So an edited page could be
/// a redaction of a base page only where this is zero.
pub(crate) fn unexplained(base: &Thumb, edited: &Thumb) -> Option<usize> {
    if base.width != edited.width || base.height != edited.height || base.mean.len() != edited.mean.len() {
        return None;
    }
    // Rounding down loses less than a quarter of a level on each side.
    let threshold = u16::from(NOISE) * 4 + 1;
    let near_fill = |row: usize, column: usize| {
        (row.saturating_sub(1)..=row.saturating_add(1).min(edited.rows.saturating_sub(1))).any(|r| {
            (column.saturating_sub(1)..=column.saturating_add(1).min(edited.columns.saturating_sub(1)))
                .any(|c| edited.filled.get(r.saturating_mul(edited.columns).saturating_add(c)).copied().unwrap_or(false))
        })
    };
    let mut count = 0usize;
    for (block, (a, b)) in base.mean.iter().zip(&edited.mean).enumerate() {
        if a.abs_diff(*b) <= threshold {
            continue;
        }
        let (row, column) = (block / edited.columns.max(1), block % edited.columns.max(1));
        if !near_fill(row, column) {
            count = count.saturating_add(1);
        }
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A white 130 × 130 render with black `rects` (x0, y0, x1, y1 in pixels, ends excluded).
    fn raster(rects: &[(usize, usize, usize, usize)]) -> UprightRaster {
        let side = 130usize;
        let mut rgba = vec![255u8; side * side * 4];
        for &(x0, y0, x1, y1) in rects {
            for y in y0..y1 {
                for x in x0..x1 {
                    rgba[(y * side + x) * 4..(y * side + x) * 4 + 3].copy_from_slice(&[0, 0, 0]);
                }
            }
        }
        UprightRaster { width: side as u32, height: side as u32, rgba, scale: 1.0, crop: [0.0, 0.0, 130.0, 130.0], height_pt: 130.0 }
    }

    #[test]
    fn content_that_differs_beside_black_text_is_not_explained() {
        // Thin black strokes (text): the base page has two in each block of a row, the edited
        // page one. They differ, and no redaction box explains it, though black pixels of the
        // edited page lie in every one of those blocks (which explained them before: the
        // edited page could pair with a base page it is not a redaction of).
        let strokes = |offsets: &[usize]| {
            (0..8).flat_map(|i| offsets.iter().map(move |offset| (13 + i * 13 + offset, 20, 15 + i * 13 + offset, 60))).collect::<Vec<_>>()
        };
        let (base, edited) = (raster(&strokes(&[0, 6])), raster(&strokes(&[0])));
        assert!(unexplained(&thumb(&base), &thumb(&edited)).is_some_and(|count| count > 0));
        assert!(redaction_boxes(&base, &edited).outside);
        // A box drawn over where they differ explains it.
        let mut boxed = strokes(&[0]);
        boxed.push((10, 15, 120, 65));
        assert_eq!(unexplained(&thumb(&base), &thumb(&raster(&boxed))), Some(0));
        assert!(!redaction_boxes(&base, &raster(&boxed)).outside);
    }
}
