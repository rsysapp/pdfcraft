//! Clearing the pixels of an image under redaction regions. The image is decoded, every pixel
//! that reaches a region is set to black (or, for an image mask, to no ink), and the result is
//! written as a new Flate image; the original object is untouched (other pages may use it).
//! Images whose codec PdfCraft can't decode (DCT, JPX, JBIG2, CCITT) return `Err`, and the
//! caller removes the whole image instead, or refuses the redaction; that is fail-closed.

use pdfcraft_content::{Matrix, overlaps};
use pdfcraft_cos::{Document, Object, Stream};

/// Most pixels an image may have to be cleared in place (larger ones are removed).
const MAX_PIXELS: u64 = 64 << 20;

/// Embed build (R6b-54, N11): what a cleared sample is written as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// An image drawn in colour: the colour space's black, through `/Decode`.
    Colour,
    /// An image's soft mask (`/SMask`): opaque, so the cleared (black) pixels show as black.
    SoftMask,
    /// An image's stencil mask (`/Mask` holding an image mask): painted, so the cleared (black)
    /// pixels of the image it masks show.
    PaintedMask,
    /// An image mask drawn on its own (a stencil painted in the current colour): no ink.
    Stencil,
}

/// What clearing an image gave.
pub(crate) struct Cleared {
    pub stream: Stream,
    /// Embed build (R6b-54, N6): the larger side of one of its pixels in user space, the most
    /// the cleared pixels reach past the regions (every pixel a region reaches is cleared whole).
    pub pixel: f64,
    /// Embed build (R6b-54, N6): the box (user space) of the pixels cleared.
    pub bounds: [f64; 4],
}

/// A colour space's black, one value per component in the space's own range (an index for an
/// indexed space): `None` where the redactor knows no black it can write (Lab, separations,
/// DeviceN, patterns, an indexed space over one of those).
fn black(doc: &Document, space: &Object, depth: usize) -> Option<Vec<f64>> {
    let space = doc.resolve(space);
    let (name, rest): (&[u8], &[Object]) = match &*space {
        Object::Name(n) => (n.as_slice(), &[]),
        Object::Array(a) => (a.first()?.as_name()?, a.get(1..).unwrap_or_default()),
        _ => return None,
    };
    Some(match name {
        b"DeviceGray" | b"G" | b"CalGray" => vec![0.0],
        b"DeviceRGB" | b"RGB" | b"CalRGB" => vec![0.0; 3],
        b"DeviceCMYK" | b"CMYK" => vec![0.0, 0.0, 0.0, 1.0],
        b"ICCBased" => match &*doc.resolve(rest.first()?) {
            Object::Stream(icc) => match icc.dict.int(b"N")? {
                1 => vec![0.0],
                3 => vec![0.0; 3],
                4 => vec![0.0, 0.0, 0.0, 1.0],
                _ => return None,
            },
            _ => return None,
        },
        b"Indexed" | b"I" if depth == 0 => {
            // The darkest entry of the palette, by luminance in its base space.
            let base = rest.first()?;
            let components = match black(doc, base, depth + 1)?.len() {
                n @ (1 | 3 | 4) => n,
                _ => return None,
            };
            let base_name = match &*doc.resolve(base) {
                Object::Name(n) => n.clone(),
                Object::Array(a) => a.first()?.as_name()?.to_vec(),
                _ => return None,
            };
            let high = usize::try_from(doc.resolve(rest.get(1)?).as_int()?).ok()?.min(255);
            let table = match &*doc.resolve(rest.get(2)?) {
                Object::String(s) => s.bytes.clone(),
                Object::Stream(s) => s.decoded().ok()?,
                _ => return None,
            };
            let cmyk = components == 4 && !base_name.starts_with(b"ICC");
            let luminance = |entry: &[u8]| -> f64 {
                let v: Vec<f64> = entry.iter().map(|b| f64::from(*b) / 255.0).collect();
                match v.as_slice() {
                    [g] => *g,
                    [r, g, b] => 0.299 * r + 0.587 * g + 0.114 * b,
                    [c, m, y, k] if cmyk => (1.0 - c) * (1.0 - k) * 0.299 + (1.0 - m) * (1.0 - k) * 0.587 + (1.0 - y) * (1.0 - k) * 0.114,
                    _ => 1.0,
                }
            };
            let best = (0..=high)
                .filter_map(|index| table.get(index * components..(index + 1) * components).map(|entry| (index, luminance(entry))))
                .min_by(|a, b| a.1.total_cmp(&b.1))?;
            vec![best.0 as f64]
        }
        _ => return None,
    })
}

/// Embed build (R6b-54, N11): the sample value that decodes (`/Decode`, or the default range)
/// to `target` for component `index`.
fn sample_for(target: f64, decode: Option<&[f64]>, index: usize, maximum: f64, indexed: bool) -> u32 {
    let (low, high) = match decode.and_then(|d| Some((*d.get(index * 2)?, *d.get(index * 2 + 1)?))) {
        Some(range) => range,
        None if indexed => (0.0, maximum),
        None => (0.0, 1.0),
    };
    if (high - low).abs() < f64::EPSILON {
        return 0;
    }
    let sample = ((target - low) / (high - low) * maximum).round().clamp(0.0, maximum);
    sample as u32
}

/// `Ok(Some(cleared))` with every pixel a region reaches cleared, `Ok(None)` when no pixel
/// reaches one, `Err(())` when the image can't be cleared (the caller removes it, or refuses).
#[allow(clippy::result_unit_err)]
pub(crate) fn clear(doc: &Document, s: &Stream, ctm: &Matrix, rects: &[[f64; 4]], role: Role) -> Result<Option<Cleared>, ()> {
    decode_and_clear(doc, s, ctm, rects, role).ok_or(())
}

fn decode_and_clear(doc: &Document, s: &Stream, ctm: &Matrix, rects: &[[f64; 4]], role: Role) -> Option<Option<Cleared>> {
    let supported = |n: &[u8]| {
        matches!(
            n,
            b"FlateDecode" | b"Fl" | b"LZWDecode" | b"LZW" | b"ASCII85Decode" | b"A85" | b"ASCIIHexDecode" | b"AHx" | b"RunLengthDecode" | b"RL"
        )
    };
    let ok = match s.dict.get(b"Filter") {
        None => true,
        Some(Object::Name(n)) => supported(n),
        Some(Object::Array(a)) => a.iter().all(|f| f.as_name().is_some_and(supported)),
        _ => false,
    };
    if !ok {
        return None;
    }
    let (w, h) = (s.dict.int(b"Width")?, s.dict.int(b"Height")?);
    if w <= 0 || h <= 0 || (w as u64) * (h as u64) > MAX_PIXELS {
        return None;
    }
    let (w, h) = (w as usize, h as usize);
    let mask = matches!(s.dict.get(b"ImageMask"), Some(Object::Bool(true)));
    let decode: Option<Vec<f64>> =
        s.dict.get(b"Decode").and_then(|d| doc.resolve(d).as_array().map(|a| a.iter().filter_map(Object::as_f64).collect()));
    // Embed build (R6b-54, N11): each component's cleared sample.
    let (ncomp, bpc, samples): (usize, usize, Vec<u32>) = if mask {
        // Image masks: a sample of 0 paints unless /Decode is [1 0].
        let inverted = decode.as_deref().and_then(|d| d.first()).copied() == Some(1.0);
        let paint = matches!(role, Role::PaintedMask);
        (1, 1, vec![u32::from(paint == inverted)])
    } else {
        let bpc = usize::try_from(s.dict.int(b"BitsPerComponent")?).ok()?;
        if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
            return None;
        }
        let maximum = f64::from((1u32 << bpc) - 1);
        let (target, indexed) = match role {
            Role::SoftMask => (vec![1.0], false),
            _ => {
                let space = s.dict.get(b"ColorSpace")?;
                let indexed = match &*doc.resolve(space) {
                    Object::Array(a) => a.first().and_then(Object::as_name).is_some_and(|n| n == b"Indexed" || n == b"I"),
                    _ => false,
                };
                (black(doc, space, 0)?, indexed)
            }
        };
        let samples: Vec<u32> = target.iter().enumerate().map(|(index, t)| sample_for(*t, decode.as_deref(), index, maximum, indexed)).collect();
        (samples.len(), bpc, samples)
    };
    let mut data = s.decoded().ok()?;
    let row = (w * ncomp * bpc).div_ceil(8);
    if data.len() < row * h {
        return None;
    }
    data.truncate(row * h);
    let bits_per_pixel = ncomp * bpc;
    let mut cleared = 0usize;
    let mut bounds = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for j in 0..h {
        // Image space: (0,0) is the bottom-left of the unit square; row 0 is the top. Embed
        // build (R6b-54, N6): a pixel is cleared whole when any of it reaches a region.
        let (v0, v1) = (1.0 - (j as f64 + 1.0) / h as f64, 1.0 - j as f64 / h as f64);
        let row_box = ctm.bbox([0.0, v0, 1.0, v1]);
        if !rects.iter().any(|r| overlaps(*r, row_box, 0.0)) {
            continue;
        }
        for i in 0..w {
            let pixel_box = ctm.bbox([i as f64 / w as f64, v0, (i as f64 + 1.0) / w as f64, v1]);
            if !rects.iter().any(|r| overlaps(*r, pixel_box, 0.0)) {
                continue;
            }
            cleared += 1;
            bounds = [bounds[0].min(pixel_box[0]), bounds[1].min(pixel_box[1]), bounds[2].max(pixel_box[2]), bounds[3].max(pixel_box[3])];
            let start = j * row * 8 + i * bits_per_pixel;
            for (component, sample) in samples.iter().enumerate() {
                for bit in 0..bpc {
                    let at = start + component * bpc + bit;
                    let (byte, shift) = (at / 8, 7 - at % 8);
                    let on = (sample >> (bpc - 1 - bit)) & 1 == 1;
                    if on {
                        data[byte] |= 1 << shift;
                    } else {
                        data[byte] &= !(1 << shift);
                    }
                }
            }
        }
    }
    if cleared == 0 {
        return Some(None);
    }
    let mut dict = s.dict.clone();
    dict.remove(b"Length");
    dict.remove(b"DecodeParms");
    // Embed build (R6b-50): nothing of the original but its samples: not its alternate
    // versions, XMP, private or prepress data.
    for key in [&b"Alternates"[..], b"Metadata", b"PieceInfo", b"LastModified", b"OPI"] {
        dict.remove(key);
    }
    let unit = ctm.bbox([0.0, 0.0, 1.0 / w as f64, 1.0 / h as f64]);
    Some(Some(Cleared { stream: Stream::flate(dict, &data), pixel: (unit[2] - unit[0]).max(unit[3] - unit[1]), bounds }))
}
