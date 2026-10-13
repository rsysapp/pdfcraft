//! Embed build: in a rewrite, what happened to a page's images, checked against the redaction
//! areas at the images' own resolution.
//!
//! Uses of the same image (encoded stream) at the same placement are unchanged. Of the rest, a
//! base use and an edited use at the same placement are a replacement, compared decoded, sample
//! by sample: every pixel whose centre (mapped into user space through the placement) lies
//! outside every area must equal the base exactly when the edited image is losslessly coded, or,
//! when both are baseline JPEGs, differ by no more than requantisation with the edited image's
//! quantisation tables allows (see [`dct_block_ok`]). A replacement that cannot be compared that
//! way (another size or meaning, another codec, an inline image, a coding this reader does not
//! decode) is accepted only when both uses lie wholly under the areas. A base use with no
//! replacement must lie wholly under the areas; an edited use with no base use is an added image.
//!
//! Uses are where hayro's interpreter drew each image (the interpretation the page render
//! shows). Images drawn in a tiling pattern's cell repeat over what the pattern paints; they are
//! compared by identity only, and any difference counts as a change outside the areas. An
//! image's optional-content membership is part of its identity: one drawn in other optional
//! content than the base's at its place is a change outside the areas (R6b-29).

use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::OnceLock;

use pdfcraft_cos::{Document, Object, Stream};

use super::structure::{Digest, ImageUse, Matrix, unit_square};
use super::{AREA_TOLERANCE, Budget, Fail, jpeg};

/// How far (PDF points) the corners of two placements of an image may lie apart for them to be one
/// placement (a rewrite writes the same `cm` with rounding).
const PLACEMENT_TOLERANCE: f64 = 0.5;

/// What the redaction areas of one page explain, and which of them were used.
pub(crate) struct Areas {
    rects: Vec<[f64; 4]>,
    pub used: Vec<bool>,
}

impl Areas {
    pub fn new(rects: &[[f32; 4]]) -> Self {
        let tolerance = f64::from(AREA_TOLERANCE);
        let rects: Vec<[f64; 4]> = rects
            .iter()
            .map(|r| [f64::from(r[0]) - tolerance, f64::from(r[1]) - tolerance, f64::from(r[2]) + tolerance, f64::from(r[3]) + tolerance])
            .collect();
        let used = vec![false; rects.len()];
        Self { rects, used }
    }

    /// Whether `point` lies in an area (within the tolerance); marks the first such area used.
    pub fn take_point(&mut self, x: f64, y: f64) -> bool {
        let Some(index) = self.rects.iter().position(|r| x >= r[0] && x <= r[2] && y >= r[1] && y <= r[3]) else { return false };
        if let Some(slot) = self.used.get_mut(index) {
            *slot = true;
        }
        true
    }

    /// Whether `point` lies in an area, without marking it.
    fn contains(&self, x: f64, y: f64) -> bool {
        self.rects.iter().any(|r| x >= r[0] && x <= r[2] && y >= r[1] && y <= r[3])
    }

    fn touching(&self, b: &[f64; 4]) -> bool {
        self.rects.iter().any(|r| b[0] <= r[2] && b[2] >= r[0] && b[1] <= r[3] && b[3] >= r[1])
    }

    /// Whether the union of the areas covers the rectangle `b` wholly (a rectangle that is not
    /// finite is not covered); marks every area that touches it used when it does.
    pub fn take_rect(&mut self, b: &[f64; 4]) -> bool {
        if !b.iter().all(|v| v.is_finite()) || b[0] > b[2] || b[1] > b[3] {
            return false;
        }
        let mut xs: Vec<f64> = vec![b[0], b[2]];
        for r in &self.rects {
            for x in [r[0], r[2]] {
                if x > b[0] && x < b[2] {
                    xs.push(x);
                }
            }
        }
        xs.sort_by(f64::total_cmp);
        xs.dedup();
        let slabs: Vec<f64> = if xs.len() < 2 { vec![b[0]] } else { xs.windows(2).map(|w| (w[0] + w[1]) / 2.0).collect() };
        for x in slabs {
            let mut spans: Vec<(f64, f64)> = self.rects.iter().filter(|r| x >= r[0] && x <= r[2]).map(|r| (r[1], r[3])).collect();
            spans.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut reached = b[1];
            for (low, high) in spans {
                if low > reached {
                    break;
                }
                reached = reached.max(high);
            }
            if reached < b[3] {
                return false;
            }
        }
        for (r, slot) in self.rects.iter().zip(self.used.iter_mut()) {
            if b[0] <= r[2] && b[2] >= r[0] && b[1] <= r[3] && b[3] >= r[1] {
                *slot = true;
            }
        }
        true
    }
}

/// What a page's images did.
#[derive(Default, Debug)]
pub(crate) struct ImageOutcome {
    /// A base image is gone or lost pixels.
    pub removed: bool,
    /// An image was added (not a replacement of a base image).
    pub added: bool,
    /// An image changed outside the areas.
    pub outside: bool,
}

/// One file's cos document, opened again for its images when a comparison needs them.
pub(crate) struct Images {
    bytes: std::sync::Arc<Vec<u8>>,
    doc: OnceLock<Option<Document>>,
}

impl Images {
    pub fn new(bytes: std::sync::Arc<Vec<u8>>) -> Self {
        Self { bytes, doc: OnceLock::new() }
    }

    fn stream(&self, num: u32) -> Option<Stream> {
        let doc = self.doc.get_or_init(|| Document::open(self.bytes.clone()).ok()).as_ref()?;
        match &*doc.try_get(num).ok()? {
            Object::Stream(stream) => Some(stream.clone()),
            _ => None,
        }
    }

    fn resolve(&self, o: &Object) -> Option<std::sync::Arc<Object>> {
        let doc = self.doc.get_or_init(|| Document::open(self.bytes.clone()).ok()).as_ref()?;
        Some(doc.resolve(o))
    }
}

/// The corners of the unit square under `m`.
fn corners(m: &Matrix) -> [(f64, f64); 4] {
    [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)].map(|(u, v)| (m[0] * u + m[2] * v + m[4], m[1] * u + m[3] * v + m[5]))
}

pub(crate) fn same_placement(a: &Matrix, b: &Matrix) -> bool {
    corners(a).iter().zip(corners(b)).all(|(p, q)| (p.0 - q.0).abs() <= PLACEMENT_TOLERANCE && (p.1 - q.1).abs() <= PLACEMENT_TOLERANCE)
}

/// Whether two multisets of digests differ: (`a` has one `b` lacks, `b` has one `a` lacks).
fn differ(a: &[Digest], b: &[Digest]) -> (bool, bool) {
    let mut counts: HashMap<&Digest, i64> = HashMap::new();
    for digest in a {
        *counts.entry(digest).or_default() += 1;
    }
    for digest in b {
        *counts.entry(digest).or_default() -= 1;
    }
    (counts.values().any(|n| *n > 0), counts.values().any(|n| *n < 0))
}

/// A placement's grid cell (its origin to the nearest point), for finding the same placement.
fn place_key(m: &Matrix) -> Option<(i64, i64)> {
    (m[4].is_finite() && m[5].is_finite()).then(|| (m[4].floor() as i64, m[5].floor() as i64))
}

/// Compare a rewritten page's image uses with the base page's, against `areas`.
pub(crate) fn compare(
    base: &[ImageUse],
    edited: &[ImageUse],
    files: (&Images, &Images),
    areas: &mut Areas,
    budget: &mut Budget,
) -> Result<ImageOutcome, Fail> {
    let mut outcome = ImageOutcome::default();
    // Images in pattern cells: by identity (with their optional-content membership, R6b-29).
    let tiled = |uses: &[ImageUse]| {
        uses.iter()
            .filter(|used| used.tiled)
            .map(|used| match &used.layer {
                Some(layer) => super::structure::sha256(&[&used.digest, &layer.digest]),
                None => used.digest,
            })
            .collect::<Vec<_>>()
    };
    let (lost, gained) = differ(&tiled(base), &tiled(edited));
    if lost || gained {
        outcome.removed |= lost;
        outcome.added |= gained;
        outcome.outside = true;
    }
    let base: Vec<&ImageUse> = base.iter().filter(|used| !used.tiled).collect();
    let edited: Vec<&ImageUse> = edited.iter().filter(|used| !used.tiled).collect();
    // Base uses not yet accounted for, by placement cell.
    let mut left: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    let mut taken = vec![false; base.len()];
    for (index, used) in base.iter().enumerate() {
        budget.tick()?;
        if let Some(key) = place_key(&used.matrix) {
            left.entry(key).or_default().push(index);
        }
    }
    let find = |left: &HashMap<(i64, i64), Vec<usize>>, taken: &[bool], use_: &ImageUse, same_image: bool| -> Option<usize> {
        let (x, y) = place_key(&use_.matrix)?;
        for dx in -1..=1 {
            for dy in -1..=1 {
                let Some(list) = left.get(&(x.saturating_add(dx), y.saturating_add(dy))) else { continue };
                for &index in list {
                    let Some(candidate) = base.get(index).copied() else { continue };
                    if !taken.get(index).copied().unwrap_or(true)
                        && (!same_image || (candidate.digest == use_.digest && candidate.layer == use_.layer))
                        && same_placement(&candidate.matrix, &use_.matrix)
                    {
                        return Some(index);
                    }
                }
            }
        }
        None
    };
    // The same image at the same place: unchanged.
    let mut rest = Vec::new();
    for used in edited.iter().copied() {
        budget.tick()?;
        match find(&left, &taken, used, true) {
            Some(index) => {
                if let Some(slot) = taken.get_mut(index) {
                    *slot = true;
                }
            }
            None => rest.push(used),
        }
    }
    // Another image at the same place: a replacement.
    for used in rest {
        budget.tick()?;
        let Some(index) = find(&left, &taken, used, false) else {
            outcome.added = true;
            continue;
        };
        if let Some(slot) = taken.get_mut(index) {
            *slot = true;
        }
        let Some(original) = base.get(index).copied() else { continue };
        // Another membership in optional content is not a redaction, whatever the boxes cover.
        let replaced = if original.layer == used.layer { replacement(original, used, files, areas, budget)? } else { Replaced::Outside };
        match replaced {
            Replaced::Same => {}
            Replaced::Redacted => outcome.removed = true,
            Replaced::Outside => {
                outcome.removed = true;
                outcome.added = true;
                outcome.outside = true;
            }
        }
    }
    // Base uses gone: wholly under the areas.
    for (index, used) in base.iter().enumerate() {
        if taken.get(index).copied().unwrap_or(true) {
            continue;
        }
        outcome.removed = true;
        if !areas.take_rect(&unit_square(&used.matrix)) {
            outcome.outside = true;
        }
    }
    Ok(outcome)
}

/// What a replacement did.
enum Replaced {
    /// Every sample is as it was.
    Same,
    /// Samples changed, all under the areas.
    Redacted,
    /// Something changed outside the areas, or could not be shown not to.
    Outside,
}

/// A replacement that cannot be compared sample by sample: both uses wholly under the areas.
fn under_areas(base: &ImageUse, edited: &ImageUse, areas: &mut Areas) -> Replaced {
    if areas.take_rect(&unit_square(&base.matrix)) && areas.take_rect(&unit_square(&edited.matrix)) { Replaced::Redacted } else { Replaced::Outside }
}

/// How the samples of an image are coded, as far as this comparison reads them.
enum Coding {
    /// Lossless filters only (or none).
    Lossless,
    /// One `DCTDecode` filter.
    Dct,
    Other,
}

fn coding(stream: &Stream) -> Coding {
    // Decode parameters must be written out where they are used: a reference could be read one
    // way here and another way by a viewer.
    let direct = match stream.dict.get(b"DecodeParms") {
        None | Some(Object::Null | Object::Dict(_)) => true,
        Some(Object::Array(items)) => items.iter().all(|item| matches!(item, Object::Null | Object::Dict(_))),
        Some(_) => false,
    };
    if !direct || matches!(stream.dict.get(b"Filter"), Some(Object::Ref(_))) {
        return Coding::Other;
    }
    let names: Vec<&[u8]> = match stream.dict.get(b"Filter") {
        None => Vec::new(),
        Some(Object::Name(name)) => vec![name.as_slice()],
        Some(Object::Array(items)) => match items.iter().map(Object::as_name).collect::<Option<Vec<_>>>() {
            Some(names) => names,
            None => return Coding::Other,
        },
        Some(_) => return Coding::Other,
    };
    let lossless = |name: &[u8]| {
        matches!(
            name,
            b"FlateDecode" | b"Fl" | b"LZWDecode" | b"LZW" | b"ASCIIHexDecode" | b"AHx" | b"ASCII85Decode" | b"A85" | b"RunLengthDecode" | b"RL"
        )
    };
    match names.as_slice() {
        [b"DCTDecode" | b"DCT"] => Coding::Dct,
        names if names.iter().all(|name| lossless(name)) => Coding::Lossless,
        _ => Coding::Other,
    }
}

fn replacement(base: &ImageUse, edited: &ImageUse, files: (&Images, &Images), areas: &mut Areas, budget: &mut Budget) -> Result<Replaced, Fail> {
    let (Some(b), Some(e)) = (&base.image, &edited.image) else { return Ok(under_areas(base, edited, areas)) };
    // Follow-up 9 (R6b-54, N6): what a redaction takes off a cleared copy may only go.
    let extras_kept = e.extras.is_none() || e.extras == b.extras;
    if b.meaning != e.meaning || b.width != e.width || b.height != e.height || b.width == 0 || b.height == 0 || !extras_kept {
        return Ok(under_areas(base, edited, areas));
    }
    let Some(mut replaced) = samples(files, (b.obj.num, e.obj.num), &base.matrix, areas, budget)? else {
        return Ok(under_areas(base, edited, areas));
    };
    // Follow-up 9 (R6b-54, N6): its masks, each at its own resolution: present in both or in
    // neither, and every sample outside the areas as it was.
    for (b_mask, e_mask) in b.masks.iter().zip(&e.masks) {
        let compared = match (b_mask, e_mask) {
            (None, None) => Some(Replaced::Same),
            // Audit (follow-up 9): only a mask meaning what the base's meant compares by its
            // samples.
            (Some((b_mask, b_meaning)), Some((e_mask, e_meaning))) if b_meaning == e_meaning => {
                samples(files, (b_mask.num, e_mask.num), &base.matrix, areas, budget)?
            }
            _ => None,
        };
        replaced = match (replaced, compared) {
            (_, None) => return Ok(under_areas(base, edited, areas)),
            (Replaced::Outside, _) | (_, Some(Replaced::Outside)) => Replaced::Outside,
            (Replaced::Redacted, _) | (_, Some(Replaced::Redacted)) => Replaced::Redacted,
            (Replaced::Same, Some(Replaced::Same)) => Replaced::Same,
        };
    }
    // Audit (follow-up 9): an image redacted in place keeps no copy of what it showed: its
    // alternates (printed by some viewers), XMP, private and prepress data must have gone.
    if matches!(replaced, Replaced::Redacted) && e.extras.is_some() {
        return Ok(Replaced::Outside);
    }
    Ok(replaced)
}

/// Two images' samples (objects `nums` of the base and the edited file), placed by `matrix`,
/// compared against `areas` (`None`: not comparable sample by sample).
fn samples(files: (&Images, &Images), nums: (u32, u32), matrix: &Matrix, areas: &mut Areas, budget: &mut Budget) -> Result<Option<Replaced>, Fail> {
    let (Some(b_stream), Some(e_stream)) = (files.0.stream(nums.0), files.1.stream(nums.1)) else { return Ok(None) };
    let side = |stream: &Stream, key: &[u8]| stream.dict.int(key).and_then(|v| usize::try_from(v).ok()).filter(|v| *v > 0);
    let (Some(width), Some(height)) = (side(&b_stream, b"Width"), side(&b_stream, b"Height")) else { return Ok(None) };
    if side(&e_stream, b"Width") != Some(width) || side(&e_stream, b"Height") != Some(height) {
        return Ok(None);
    }
    let grid = Grid { matrix: *matrix, width, height };
    Ok(match (coding(&b_stream), coding(&e_stream)) {
        (Coding::Lossless, Coding::Lossless) => lossless(&b_stream, &e_stream, files.0, &grid, areas, budget)?,
        (Coding::Dct, Coding::Dct) => dct(&b_stream, &e_stream, &grid, areas, budget)?,
        _ => None,
    })
}

/// An image's pixel grid placed on the page.
struct Grid {
    matrix: Matrix,
    width: usize,
    height: usize,
}

impl Grid {
    /// The centre of pixel (`i`, `j`) (row 0 at the top) in user space.
    fn centre(&self, i: usize, j: usize) -> (f64, f64) {
        let u = (i as f64 + 0.5) / self.width as f64;
        let v = 1.0 - (j as f64 + 0.5) / self.height as f64;
        let m = &self.matrix;
        (m[0] * u + m[2] * v + m[4], m[1] * u + m[3] * v + m[5])
    }

    /// The box (user space) of the centres of pixels `x0..x1` × `y0..y1`.
    fn centres_box(&self, (x0, x1): (usize, usize), (y0, y1): (usize, usize)) -> [f64; 4] {
        let points = [
            self.centre(x0, y0),
            self.centre(x1.saturating_sub(1), y0),
            self.centre(x0, y1.saturating_sub(1)),
            self.centre(x1.saturating_sub(1), y1.saturating_sub(1)),
        ];
        let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for (x, y) in points {
            b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
        }
        b
    }
}

/// Decode a stream's samples for the comparison, within the budget.
fn decode(stream: &Stream, budget: &mut Budget) -> Result<Option<Vec<u8>>, Fail> {
    let room = budget.image_room();
    // Every filter of the chain together within the room, all of it charged.
    match stream.decoded_strict_counted(room) {
        Ok((data, made)) => {
            budget.image_bytes(made)?;
            Ok(Some(data))
        }
        Err(error) if error.to_string().contains("exceeds the limit") => Err(Fail::TooComplex),
        Err(_) => Ok(None),
    }
}

/// Components a sample of an image with this dictionary has (`None`: not known for certain).
fn components(stream: &Stream, file: &Images) -> Option<usize> {
    if matches!(stream.dict.get(b"ImageMask"), Some(Object::Bool(true))) {
        return Some(1);
    }
    let space = file.resolve(stream.dict.get(b"ColorSpace")?)?;
    let family = |name: &[u8]| match name {
        b"DeviceGray" | b"G" | b"CalGray" | b"Indexed" | b"I" | b"Separation" => Some(1),
        b"DeviceRGB" | b"RGB" | b"CalRGB" | b"Lab" => Some(3),
        b"DeviceCMYK" | b"CMYK" => Some(4),
        _ => None,
    };
    match &*space {
        Object::Name(name) => family(name),
        Object::Array(items) => match items.first()?.as_name()? {
            b"ICCBased" => match &*file.resolve(items.get(1)?)? {
                Object::Stream(icc) => icc.dict.int(b"N").and_then(|n| usize::try_from(n).ok()).filter(|n| matches!(n, 1 | 3 | 4)),
                _ => None,
            },
            b"DeviceN" => match &*file.resolve(items.get(1)?)? {
                Object::Array(names) => Some(names.len()).filter(|n| (1..=32).contains(n)),
                _ => None,
            },
            name => family(name),
        },
        _ => None,
    }
}

/// Losslessly coded images: every pixel outside the areas equal.
fn lossless(b: &Stream, e: &Stream, file: &Images, grid: &Grid, areas: &mut Areas, budget: &mut Budget) -> Result<Option<Replaced>, Fail> {
    let Some(components) = components(b, file) else { return Ok(None) };
    let depth = if matches!(b.dict.get(b"ImageMask"), Some(Object::Bool(true))) { 1 } else { b.dict.int(b"BitsPerComponent").unwrap_or(0) };
    if !matches!(depth, 1 | 2 | 4 | 8 | 16) {
        return Ok(None);
    }
    let bits = components.saturating_mul(depth as usize);
    let row = grid.width.saturating_mul(bits).div_ceil(8);
    let need = row.saturating_mul(grid.height);
    if need > budget.image_room() {
        return Err(Fail::TooComplex);
    }
    let (Some(base), Some(edited)) = (decode(b, budget)?, decode(e, budget)?) else { return Ok(None) };
    if base.len() < need || edited.len() < need {
        return Ok(None);
    }
    let bit = |data: &[u8], at: usize| data.get(at / 8).is_some_and(|byte| byte & (0x80 >> (at % 8)) != 0);
    let mut changed = false;
    for j in 0..grid.height {
        budget.tick()?;
        let (Some(base_row), Some(edited_row)) = (base.get(j * row..(j + 1) * row), edited.get(j * row..(j + 1) * row)) else { return Ok(None) };
        if base_row == edited_row {
            continue;
        }
        budget.compared(grid.width)?;
        for i in 0..grid.width {
            let first = i * bits;
            if (first..first + bits).all(|at| bit(base_row, at) == bit(edited_row, at)) {
                continue;
            }
            changed = true;
            let (x, y) = grid.centre(i, j);
            if !areas.take_point(x, y) {
                return Ok(Some(Replaced::Outside));
            }
        }
    }
    Ok(Some(if changed { Replaced::Redacted } else { Replaced::Same }))
}

// ── DCT ──────────────────────────────────────────────────────────────────────────────────────

/// How far (sample levels) a decoder's reconstruction of a sample may differ from the exact one:
/// half a level of rounding, and an inexact integer inverse DCT (conforming ones stay within one).
const DECODE_SLACK: f64 = 1.5;
/// How far (coefficient units) an encoder's forward DCT may differ from the exact one (an integer
/// forward DCT is accurate to within a unit).
const ENCODE_SLACK: f64 = 1.0;

/// `C(u) cos((2x + 1) u π / 16) / 2` for sample `x` and frequency `u`: the 8-point DCT basis
/// (ISO/IEC 10918-1 A.3.3), so that `F(u, v) = Σ basis[x][u] basis[y][v] s(x, y)` and back.
fn basis() -> &'static [[f64; 8]; 8] {
    static BASIS: OnceLock<[[f64; 8]; 8]> = OnceLock::new();
    BASIS.get_or_init(|| {
        let mut table = [[0.0; 8]; 8];
        for (x, row) in table.iter_mut().enumerate() {
            for (u, value) in row.iter_mut().enumerate() {
                let c = if u == 0 { std::f64::consts::FRAC_1_SQRT_2 } else { 1.0 };
                *value = c * ((2 * x + 1) as f64 * u as f64 * PI / 16.0).cos() / 2.0;
            }
        }
        table
    })
}

/// For each coefficient (natural order `v·8 + u`), the most a change of one level in every sample
/// can move it: `Σ_x |basis[x][u]| · Σ_y |basis[y][v]|`.
fn spread() -> &'static [f64; 64] {
    static SPREAD: OnceLock<[f64; 64]> = OnceLock::new();
    SPREAD.get_or_init(|| {
        let b = basis();
        let column = |u: usize| (0..8).map(|x| b[x][u].abs()).sum::<f64>();
        let mut out = [0.0; 64];
        for (k, value) in out.iter_mut().enumerate() {
            *value = column(k % 8) * column(k / 8);
        }
        out
    })
}

/// Samples (level shifted back, not clamped) of a block of dequantised coefficients.
fn inverse(coefficients: &[f64; 64]) -> [f64; 64] {
    let b = basis();
    let mut out = [0.0; 64];
    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0.0;
            for v in 0..8 {
                for u in 0..8 {
                    sum += b[x][u] * b[y][v] * coefficients[v * 8 + u];
                }
            }
            out[y * 8 + x] = sum + 128.0;
        }
    }
    out
}

fn forward(samples: &[f64; 64]) -> [f64; 64] {
    let b = basis();
    let mut out = [0.0; 64];
    for v in 0..8 {
        for u in 0..8 {
            let mut sum = 0.0;
            for y in 0..8 {
                for x in 0..8 {
                    sum += b[x][u] * b[y][v] * (samples[y * 8 + x] - 128.0);
                }
            }
            out[v * 8 + u] = sum;
        }
    }
    out
}

fn dequantised(block: &[i16], quant: &[u16; 64]) -> [f64; 64] {
    let mut out = [0.0; 64];
    for (k, slot) in out.iter_mut().enumerate() {
        *slot = f64::from(block.get(k).copied().unwrap_or(0)) * f64::from(quant[k]);
    }
    out
}

/// The samples a decoder shows: reconstructed and clamped to 0…255.
pub(crate) fn shown(coefficients: &[f64; 64]) -> [f64; 64] {
    inverse(coefficients).map(|s| s.clamp(0.0, 255.0))
}

/// A block wholly outside the areas and wholly inside the image: its coefficients are the base
/// block's requantised with the edited table. For each coefficient `k`, the edited value
/// `n_e · Q_e` lies within `Q_e / 2` (rounding to the nearest step) plus the encoder's and the
/// redactor's decoder's inexactness of either
/// - the forward DCT of the base block as shown (a redactor that decoded the base to samples and
///   coded them again), or
/// - the base coefficient itself (one that requantised the coefficients).
fn dct_block_ok(base: &[f64; 64], edited: &[f64; 64], quant: &[u16; 64]) -> bool {
    let spread = spread();
    let again = forward(&shown(base));
    (0..64).all(|k| {
        let step = f64::from(quant[k]) / 2.0 + ENCODE_SLACK;
        (edited[k] - again[k]).abs() <= step + DECODE_SLACK * spread[k] || (edited[k] - base[k]).abs() <= step
    })
}

/// For a block that is partly under the areas or cut by the image's edge, the most requantising
/// the block can move sample (`x`, `y`) whatever the rest of the block held (what the areas cover,
/// the encoder's padding past the edge): `Σ_k (Q_e[k] / 2 + slack) |basis_k(x, y)|`, plus the
/// decoder's inexactness.
fn sample_bounds(quant: &[u16; 64]) -> [f64; 64] {
    let b = basis();
    let mut out = [0.0; 64];
    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0.0;
            for (k, q) in quant.iter().enumerate() {
                sum += (f64::from(*q) / 2.0 + ENCODE_SLACK) * (b[x][k % 8] * b[y][k / 8]).abs();
            }
            out[y * 8 + x] = sum + DECODE_SLACK;
        }
    }
    out
}

/// The `/ColorTransform` of a DCT stream's decode parameters.
fn color_transform(stream: &Stream) -> Option<i64> {
    match stream.dict.get(b"DecodeParms") {
        Some(Object::Dict(d)) => d.int(b"ColorTransform"),
        Some(Object::Array(items)) => items.first().and_then(Object::as_dict).and_then(|d| d.int(b"ColorTransform")),
        _ => None,
    }
}

/// Baseline JPEGs: each block outside the areas within requantisation of the base.
fn dct(b: &Stream, e: &Stream, grid: &Grid, areas: &mut Areas, budget: &mut Budget) -> Result<Option<Replaced>, Fail> {
    if color_transform(b) != color_transform(e) {
        return Ok(None);
    }
    let room = budget.image_room();
    let read = |stream: &Stream, budget: &mut Budget| match jpeg::decode(&stream.raw, room, budget) {
        Ok(frame) => Ok(Some(frame)),
        Err(Fail::TooComplex) => Err(Fail::TooComplex),
        Err(_) => Ok(None),
    };
    let (Some(base), Some(edited)) = (read(b, budget)?, read(e, budget)?) else { return Ok(None) };
    let layout = |f: &jpeg::Frame| f.components.iter().map(|c| (c.id, c.h, c.v)).collect::<Vec<_>>();
    if (base.width, base.height) != (grid.width, grid.height)
        || (edited.width, edited.height) != (grid.width, grid.height)
        || layout(&base) != layout(&edited)
        || base.adobe_transform != edited.adobe_transform
    {
        return Ok(None);
    }
    let bytes = base.components.iter().chain(&edited.components).map(|c| c.coefficients.len().saturating_mul(2)).sum();
    budget.image_bytes(bytes)?;
    // Whether an area explains a change (requantisation alone redacts nothing).
    let mut redacted = false;
    for (bc, ec) in base.components.iter().zip(&edited.components) {
        // Image pixels a sample covers, across and down.
        let (h, v) = (usize::from(base.max_h / bc.h.max(1)), usize::from(base.max_v / bc.v.max(1)));
        if usize::from(bc.h) * h != usize::from(base.max_h) || usize::from(bc.v) * v != usize::from(base.max_v) {
            return Ok(None);
        }
        let samples_wide = (grid.width * usize::from(bc.h)).div_ceil(usize::from(base.max_h));
        let samples_high = (grid.height * usize::from(bc.v)).div_ceil(usize::from(base.max_v));
        let bounds = sample_bounds(&ec.quant);
        for by in 0..samples_high.div_ceil(8) {
            for bx in 0..samples_wide.div_ceil(8) {
                budget.tick()?;
                let (Some(base_block), Some(edited_block)) = (bc.block(bx, by), ec.block(bx, by)) else { return Ok(None) };
                if base_block == edited_block && bc.quant == ec.quant {
                    continue;
                }
                budget.compared(64 * h * v)?;
                // The block's samples and the image pixels they cover.
                let (sx0, sx1) = (bx * 8, (bx * 8 + 8).min(samples_wide));
                let (sy0, sy1) = (by * 8, (by * 8 + 8).min(samples_high));
                let pixels_x = (sx0 * h, (sx1 * h).min(grid.width));
                let pixels_y = (sy0 * v, (sy1 * v).min(grid.height));
                let reach = grid.centres_box(pixels_x, pixels_y);
                let inside_image = sx1 - sx0 == 8 && sy1 - sy0 == 8;
                let base_values = dequantised(base_block, &bc.quant);
                let edited_values = dequantised(edited_block, &ec.quant);
                if !areas.touching(&reach) {
                    if inside_image {
                        if !dct_block_ok(&base_values, &edited_values, &ec.quant) {
                            return Ok(Some(Replaced::Outside));
                        }
                        continue;
                    }
                } else if areas.take_rect(&reach) {
                    // Wholly under the areas.
                    redacted = true;
                    continue;
                }
                // Partly under the areas, or cut by the edge: each sample not wholly under them
                // within the per-sample bound.
                let (shown_base, shown_edited) = (shown(&base_values), shown(&edited_values));
                for sy in sy0..sy1 {
                    for sx in sx0..sx1 {
                        let k = (sy - sy0) * 8 + (sx - sx0);
                        if (shown_edited[k] - shown_base[k]).abs() <= bounds[k] {
                            continue;
                        }
                        let covered = (sy * v..((sy + 1) * v).min(grid.height)).all(|j| {
                            (sx * h..((sx + 1) * h).min(grid.width)).all(|i| {
                                let (x, y) = grid.centre(i, j);
                                areas.contains(x, y)
                            })
                        });
                        if !covered {
                            return Ok(Some(Replaced::Outside));
                        }
                        let (x, y) = grid.centre(sx * h, sy * v);
                        areas.take_point(x, y);
                        redacted = true;
                    }
                }
            }
        }
    }
    Ok(Some(if redacted { Replaced::Redacted } else { Replaced::Same }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dct_round_trips() {
        let samples: [f64; 64] = std::array::from_fn(|i| ((i * 37) % 256) as f64);
        let back = inverse(&forward(&samples));
        for (a, b) in samples.iter().zip(back) {
            assert!((a - b).abs() < 1e-9);
        }
        // A flat block of 255 is a DC of 8 · 127.
        let dc = forward(&[255.0; 64])[0];
        assert!((dc - 1016.0).abs() < 1e-9, "{dc}");
    }

    fn jpeg(pixels: &[u8], quality: u8) -> Stream {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode(pixels, 64, 64, image::ExtendedColorType::L8).unwrap();
        let mut dict = pdfcraft_cos::Dict::new();
        dict.set(b"Filter".to_vec(), Object::Name(b"DCTDecode".to_vec()));
        Stream { dict, raw: out.into() }
    }

    /// A 64 × 64 grey JPEG placed on 64 × 64 points at the origin, against itself with pixels
    /// 19–43 × 19–43 cleared and coded again (cutting through blocks), and `areas`.
    fn redacted(change: Option<(usize, usize)>, areas: &[[f32; 4]]) -> Option<Replaced> {
        let pixels: Vec<u8> = (0..64 * 64).map(|i| (128.0 + 50.0 * ((i % 64) as f64 / 9.0).sin() * ((i / 64) as f64 / 7.0).cos()) as u8).collect();
        let base = jpeg(&pixels, 85);
        let mut cleared = image::load_from_memory_with_format(&base.raw, image::ImageFormat::Jpeg).unwrap().to_luma8().into_raw();
        for y in 19..43 {
            for x in 19..43 {
                cleared[y * 64 + x] = 0;
            }
        }
        if let Some((x, y)) = change {
            cleared[y * 64 + x] = cleared[y * 64 + x].wrapping_add(128);
        }
        let edited = jpeg(&cleared, 85);
        let grid = Grid { matrix: [64.0, 0.0, 0.0, 64.0, 0.0, 0.0], width: 64, height: 64 };
        let mut budget = Budget::new(crate::inspect::Limits::default());
        dct(&base, &edited, &grid, &mut Areas::new(areas), &mut budget).unwrap()
    }

    #[test]
    fn partly_covered_jpeg_blocks_are_held_to_the_sample_bound() {
        // Rows 19–43 from the top are y 21–45 in user space.
        let box_ = [19.0, 21.0, 43.0, 45.0];
        assert!(matches!(redacted(None, &[box_]), Some(Replaced::Redacted)));
        // A sample changed far beyond the bound, outside the box, in a block the box cuts.
        assert!(matches!(redacted(Some((45, 20)), &[box_]), Some(Replaced::Outside)));
        // In a block the box does not touch.
        assert!(matches!(redacted(Some((60, 60)), &[box_]), Some(Replaced::Outside)));
        // No box at all.
        assert!(matches!(redacted(None, &[]), Some(Replaced::Outside)));
    }

    #[test]
    fn rectangles_are_covered_by_the_union_of_areas() {
        let mut areas = Areas::new(&[[0.0, 0.0, 10.0, 10.0], [10.0, 0.0, 20.0, 5.0]]);
        assert!(areas.take_rect(&[2.0, 2.0, 8.0, 8.0]));
        assert!(areas.take_rect(&[2.0, 1.0, 19.0, 4.0]));
        // The notch of the L is not covered (beyond the tolerance of 1 point).
        assert!(!areas.take_rect(&[2.0, 2.0, 19.0, 8.0]));
        assert!(!areas.take_rect(&[f64::NAN, 0.0, 1.0, 1.0]));
    }
}
