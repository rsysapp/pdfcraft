//! Embed build: a baseline JPEG read to its quantised DCT coefficients (ISO/IEC 10918-1, Annex F
//! sequential Huffman decoding), for the save inspector's comparison of a re-encoded image with
//! the one it replaced. Nothing is reconstructed or colour converted here: each component keeps
//! its own block grid at its own sampling, with the quantisation table it was coded with.
//!
//! Only what can be read for certain is accepted: 8-bit baseline or extended sequential Huffman
//! frames (SOF0, SOF1) with one to four components. Progressive, lossless, hierarchical and
//! arithmetic-coded frames, a height given later by DNL, a block that runs past its 64
//! coefficients, a code no table has, or entropy-coded data that ends early are errors; the caller
//! then treats the image as one it cannot compare.

use super::{Budget, Fail};

/// The natural-order index of each zig-zag position.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43,
    36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// One component of a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Component {
    pub id: u8,
    pub h: u8,
    pub v: u8,
    /// The quantisation table the frame names for it.
    pub table: u8,
    /// That table, natural order (as in force when a scan coded the component).
    pub quant: [u16; 64],
    /// Blocks across and down the component's grid (whole MCUs).
    pub blocks_wide: usize,
    pub blocks_high: usize,
    /// Quantised coefficients, natural order, 64 per block, row by row.
    pub coefficients: Vec<i16>,
    /// Whether a scan coded the component.
    pub coded: bool,
}

impl Component {
    /// Block (`x`, `y`)'s 64 coefficients.
    pub fn block(&self, x: usize, y: usize) -> Option<&[i16]> {
        let at = y.checked_mul(self.blocks_wide)?.checked_add(x)?.checked_mul(64)?;
        self.coefficients.get(at..at.checked_add(64)?)
    }
}

/// A decoded frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub width: usize,
    pub height: usize,
    pub components: Vec<Component>,
    /// The Adobe marker's colour transform (APP14), if present.
    pub adobe_transform: Option<u8>,
    pub max_h: u8,
    pub max_v: u8,
}

fn unreadable(what: &str) -> Fail {
    Fail::Read(format!("a JPEG image cannot be compared: {what}"))
}

/// A Huffman table as ISO/IEC 10918-1 F.2.2.3 decodes it.
#[derive(Clone)]
struct Huffman {
    max_code: [i32; 17],
    val_ptr: [i32; 17],
    min_code: [i32; 17],
    values: Vec<u8>,
}

impl Huffman {
    fn new(counts: &[u8; 16], values: Vec<u8>) -> Result<Self, Fail> {
        let mut table = Huffman { max_code: [-1; 17], val_ptr: [0; 17], min_code: [0; 17], values };
        let (mut code, mut k) = (0i32, 0i32);
        for length in 1..=16usize {
            let count = i32::from(counts[length - 1]);
            if count > 0 {
                table.val_ptr[length] = k;
                table.min_code[length] = code;
                code += count;
                k += count;
                table.max_code[length] = code - 1;
            }
            if code > (1 << length) {
                return Err(unreadable("a Huffman table has more codes than its lengths allow"));
            }
            code <<= 1;
        }
        Ok(table)
    }
}

/// Entropy-coded data: bits after byte unstuffing, stopping at a marker.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    byte: u8,
    left: u8,
}

impl Bits<'_> {
    fn bit(&mut self) -> Result<u8, Fail> {
        if self.left == 0 {
            let byte = *self.data.get(self.pos).ok_or_else(|| unreadable("the coded data ends early"))?;
            if byte == 0xFF {
                match self.data.get(self.pos.saturating_add(1)) {
                    Some(0x00) => self.pos = self.pos.saturating_add(2),
                    _ => return Err(unreadable("the coded data ends early")),
                }
            } else {
                self.pos = self.pos.saturating_add(1);
            }
            self.byte = byte;
            self.left = 8;
        }
        self.left -= 1;
        Ok((self.byte >> self.left) & 1)
    }

    fn bits(&mut self, count: u8) -> Result<i32, Fail> {
        let mut value = 0i32;
        for _ in 0..count {
            value = (value << 1) | i32::from(self.bit()?);
        }
        Ok(value)
    }

    fn decode(&mut self, table: &Huffman) -> Result<u8, Fail> {
        let mut code = i32::from(self.bit()?);
        for length in 1..=16usize {
            if code <= table.max_code[length] {
                let index = table.val_ptr[length] + code - table.min_code[length];
                return usize::try_from(index)
                    .ok()
                    .and_then(|i| table.values.get(i))
                    .copied()
                    .ok_or_else(|| unreadable("a Huffman code has no value"));
            }
            code = (code << 1) | i32::from(self.bit()?);
        }
        Err(unreadable("a Huffman code is not in its table"))
    }

    /// RECEIVE and EXTEND (F.2.2.1).
    fn extend(&mut self, size: u8) -> Result<i32, Fail> {
        if size == 0 {
            return Ok(0);
        }
        if size > 15 {
            return Err(unreadable("a coefficient is too large"));
        }
        let value = self.bits(size)?;
        Ok(if value < (1 << (size - 1)) { value - (1 << size) + 1 } else { value })
    }

    /// Skip to the next byte and past an `RSTn` marker.
    fn restart(&mut self) -> Result<(), Fail> {
        self.left = 0;
        match (self.data.get(self.pos), self.data.get(self.pos.saturating_add(1))) {
            (Some(0xFF), Some(0xD0..=0xD7)) => {
                self.pos = self.pos.saturating_add(2);
                Ok(())
            }
            _ => Err(unreadable("a restart marker is missing")),
        }
    }
}

fn u16_at(data: &[u8], at: usize) -> Result<usize, Fail> {
    let high = *data.get(at).ok_or_else(|| unreadable("a marker segment is cut"))?;
    let low = *data.get(at.saturating_add(1)).ok_or_else(|| unreadable("a marker segment is cut"))?;
    Ok(usize::from(u16::from_be_bytes([high, low])))
}

/// Decode `data` to its coefficients. Every block is charged to `budget` (64 coefficients a block,
/// two bytes each), and a frame larger than `max_bytes` of coefficients is too complex.
pub(crate) fn decode(data: &[u8], max_bytes: usize, budget: &mut Budget) -> Result<Frame, Fail> {
    if data.get(..2) != Some(&[0xFF, 0xD8]) {
        return Err(unreadable("no start of image"));
    }
    let mut pos = 2usize;
    let mut quant: [Option<[u16; 64]>; 4] = [None; 4];
    let mut dc: [Option<Huffman>; 4] = [None, None, None, None];
    let mut ac: [Option<Huffman>; 4] = [None, None, None, None];
    let mut frame: Option<Frame> = None;
    let mut restart_interval = 0usize;
    let mut adobe_transform = None;
    loop {
        budget.tick()?;
        // Markers, after any fill bytes.
        if data.get(pos) != Some(&0xFF) {
            return Err(unreadable("a marker is missing"));
        }
        while data.get(pos) == Some(&0xFF) {
            pos = pos.saturating_add(1);
        }
        let marker = *data.get(pos).ok_or_else(|| unreadable("the image ends without an end marker"))?;
        pos = pos.saturating_add(1);
        match marker {
            0xD9 => break,
            0xD0..=0xD7 | 0x01 => continue,
            _ => {}
        }
        let length = u16_at(data, pos)?;
        if length < 2 {
            return Err(unreadable("a marker segment is too short"));
        }
        let segment = data.get(pos.saturating_add(2)..pos.saturating_add(length)).ok_or_else(|| unreadable("a marker segment is cut"))?;
        let next = pos.saturating_add(length);
        match marker {
            0xDB => read_quant(segment, &mut quant)?,
            0xC4 => read_huffman(segment, &mut dc, &mut ac)?,
            0xC0 | 0xC1 => {
                if frame.is_some() {
                    return Err(unreadable("the image has two frames"));
                }
                frame = Some(read_frame(segment, max_bytes)?);
            }
            0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return Err(unreadable("not a baseline or sequential Huffman image")),
            0xDC => return Err(unreadable("the height is given later (DNL)")),
            0xDD => restart_interval = u16_at(segment, 0)?,
            0xEE if segment.starts_with(b"Adobe") => adobe_transform = segment.get(11).copied(),
            0xDA => {
                let Some(frame) = frame.as_mut() else { return Err(unreadable("a scan comes before the frame")) };
                let end = read_scan(segment, data, next, frame, &quant, (&dc, &ac), restart_interval, budget)?;
                pos = end;
                continue;
            }
            _ => {}
        }
        pos = next;
    }
    let mut frame = frame.ok_or_else(|| unreadable("no frame"))?;
    if frame.components.iter().any(|component| !component.coded) {
        return Err(unreadable("a component is never coded"));
    }
    frame.adobe_transform = adobe_transform;
    Ok(frame)
}

fn read_quant(mut segment: &[u8], quant: &mut [Option<[u16; 64]>; 4]) -> Result<(), Fail> {
    while let Some((&info, rest)) = segment.split_first() {
        let (precision, id) = (info >> 4, usize::from(info & 15));
        let size = if precision == 0 { 64 } else { 128 };
        let values = rest.get(..size).ok_or_else(|| unreadable("a quantisation table is cut"))?;
        let mut table = [0u16; 64];
        for (k, natural) in ZIGZAG.iter().enumerate() {
            let value = if precision == 0 {
                u16::from(values.get(k).copied().unwrap_or(0))
            } else {
                u16::from_be_bytes([values.get(2 * k).copied().unwrap_or(0), values.get(2 * k + 1).copied().unwrap_or(0)])
            };
            if value == 0 {
                return Err(unreadable("a quantisation step is zero"));
            }
            if let Some(slot) = table.get_mut(*natural) {
                *slot = value;
            }
        }
        *quant.get_mut(id).ok_or_else(|| unreadable("a quantisation table number is out of range"))? = Some(table);
        segment = rest.get(size..).unwrap_or_default();
    }
    Ok(())
}

fn read_huffman(mut segment: &[u8], dc: &mut [Option<Huffman>; 4], ac: &mut [Option<Huffman>; 4]) -> Result<(), Fail> {
    while let Some((&info, rest)) = segment.split_first() {
        let (class, id) = (info >> 4, usize::from(info & 15));
        let counts: [u8; 16] = rest.get(..16).and_then(|c| c.try_into().ok()).ok_or_else(|| unreadable("a Huffman table is cut"))?;
        let total: usize = counts.iter().map(|c| usize::from(*c)).sum();
        let values = rest.get(16..16 + total).ok_or_else(|| unreadable("a Huffman table is cut"))?.to_vec();
        let table = Huffman::new(&counts, values)?;
        let slot = match class {
            0 => dc.get_mut(id),
            1 => ac.get_mut(id),
            _ => None,
        };
        *slot.ok_or_else(|| unreadable("a Huffman table number is out of range"))? = Some(table);
        segment = rest.get(16 + total..).unwrap_or_default();
    }
    Ok(())
}

fn read_frame(segment: &[u8], max_bytes: usize) -> Result<Frame, Fail> {
    let precision = *segment.first().ok_or_else(|| unreadable("the frame header is cut"))?;
    if precision != 8 {
        return Err(unreadable("not 8 bits a sample"));
    }
    let height = u16_at(segment, 1)?;
    let width = u16_at(segment, 3)?;
    let count = usize::from(*segment.get(5).ok_or_else(|| unreadable("the frame header is cut"))?);
    if width == 0 || height == 0 || !(1..=4).contains(&count) {
        return Err(unreadable("the frame's size or component count is not supported"));
    }
    let mut components = Vec::with_capacity(count);
    for index in 0..count {
        let at = 6 + 3 * index;
        let spec = segment.get(at..at + 3).ok_or_else(|| unreadable("the frame header is cut"))?;
        let (h, v) = (spec[1] >> 4, spec[1] & 15);
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) || components.iter().any(|c: &Component| c.id == spec[0]) {
            return Err(unreadable("a component's sampling is not supported"));
        }
        if spec[2] > 3 {
            return Err(unreadable("a quantisation table number is out of range"));
        }
        components.push(Component {
            id: spec[0],
            h,
            v,
            table: spec[2],
            quant: [0; 64],
            blocks_wide: 0,
            blocks_high: 0,
            coefficients: Vec::new(),
            coded: false,
        });
    }
    let max_h = components.iter().map(|c| c.h).max().unwrap_or(1);
    let max_v = components.iter().map(|c| c.v).max().unwrap_or(1);
    let mcus_wide = width.div_ceil(8 * usize::from(max_h));
    let mcus_high = height.div_ceil(8 * usize::from(max_v));
    let mut bytes = 0usize;
    for component in &mut components {
        component.blocks_wide = mcus_wide.saturating_mul(usize::from(component.h));
        component.blocks_high = mcus_high.saturating_mul(usize::from(component.v));
        let values = component.blocks_wide.saturating_mul(component.blocks_high).saturating_mul(64);
        bytes = bytes.saturating_add(values.saturating_mul(2));
        if bytes > max_bytes {
            return Err(Fail::TooComplex);
        }
        component.coefficients = vec![0; values];
    }
    Ok(Frame { width, height, components, adobe_transform: None, max_h, max_v })
}

/// Decode one scan whose header is `segment` and whose data starts at `start`; returns where the
/// data ends (the next marker).
#[allow(clippy::too_many_arguments)]
fn read_scan(
    segment: &[u8],
    data: &[u8],
    start: usize,
    frame: &mut Frame,
    quant: &[Option<[u16; 64]>; 4],
    (dc, ac): (&[Option<Huffman>; 4], &[Option<Huffman>; 4]),
    restart_interval: usize,
    budget: &mut Budget,
) -> Result<usize, Fail> {
    let count = usize::from(*segment.first().ok_or_else(|| unreadable("a scan header is cut"))?);
    if !(1..=4).contains(&count) {
        return Err(unreadable("a scan has no components"));
    }
    // (component index, DC table, AC table)
    let mut members = Vec::with_capacity(count);
    for index in 0..count {
        let spec = segment.get(1 + 2 * index..3 + 2 * index).ok_or_else(|| unreadable("a scan header is cut"))?;
        let component = frame.components.iter().position(|c| c.id == spec[0]).ok_or_else(|| unreadable("a scan names an unknown component"))?;
        let dc_table = dc.get(usize::from(spec[1] >> 4)).and_then(Option::as_ref).ok_or_else(|| unreadable("a DC table is missing"))?;
        let ac_table = ac.get(usize::from(spec[1] & 15)).and_then(Option::as_ref).ok_or_else(|| unreadable("an AC table is missing"))?;
        members.push((component, dc_table, ac_table));
    }
    let selection = segment.get(1 + 2 * count..4 + 2 * count).ok_or_else(|| unreadable("a scan header is cut"))?;
    if selection != [0, 63, 0] {
        return Err(unreadable("a scan is not sequential"));
    }
    // The quantisation table in force for each component of this scan.
    for &(component, _, _) in &members {
        let Some(c) = frame.components.get_mut(component) else { continue };
        if c.coded {
            return Err(unreadable("a component is coded twice"));
        }
        c.quant = quant.get(usize::from(c.table)).copied().flatten().ok_or_else(|| unreadable("a quantisation table is missing"))?;
        c.coded = true;
    }
    let interleaved = count > 1;
    let (mcus_wide, mcus_high) = if interleaved {
        (frame.width.div_ceil(8 * usize::from(frame.max_h)), frame.height.div_ceil(8 * usize::from(frame.max_v)))
    } else {
        let c = frame.components.get(members.first().map_or(0, |m| m.0)).ok_or_else(|| unreadable("a scan names an unknown component"))?;
        let samples_wide = (frame.width * usize::from(c.h)).div_ceil(usize::from(frame.max_h));
        let samples_high = (frame.height * usize::from(c.v)).div_ceil(usize::from(frame.max_v));
        (samples_wide.div_ceil(8), samples_high.div_ceil(8))
    };
    let mut bits = Bits { data, pos: start, byte: 0, left: 0 };
    let mut predictors = [0i32; 4];
    let total = mcus_wide.saturating_mul(mcus_high);
    let mut block = [0i32; 64];
    for mcu in 0..total {
        if restart_interval > 0 && mcu > 0 && mcu % restart_interval == 0 {
            bits.restart()?;
            predictors = [0; 4];
        }
        let (mx, my) = (mcu % mcus_wide.max(1), mcu / mcus_wide.max(1));
        for (slot, &(component, dc_table, ac_table)) in members.iter().enumerate() {
            let (h, v) = match frame.components.get(component) {
                Some(c) if interleaved => (usize::from(c.h), usize::from(c.v)),
                Some(_) => (1, 1),
                None => continue,
            };
            for by in 0..v {
                for bx in 0..h {
                    budget.tick()?;
                    block.fill(0);
                    let size = bits.decode(dc_table)?;
                    let predictor = predictors.get_mut(slot).ok_or_else(|| unreadable("too many scan components"))?;
                    *predictor = predictor.saturating_add(bits.extend(size)?);
                    block[0] = *predictor;
                    let mut k = 1usize;
                    while k < 64 {
                        let rs = bits.decode(ac_table)?;
                        let (run, size) = (usize::from(rs >> 4), rs & 15);
                        if size == 0 {
                            if run == 15 {
                                k += 16;
                                continue;
                            }
                            break;
                        }
                        k += run;
                        let natural = *ZIGZAG.get(k).ok_or_else(|| unreadable("a block has more than 64 coefficients"))?;
                        block[natural] = bits.extend(size)?;
                        k += 1;
                    }
                    if k > 64 {
                        return Err(unreadable("a block has more than 64 coefficients"));
                    }
                    let Some(c) = frame.components.get_mut(component) else { continue };
                    let (x, y) = (mx * h + bx, my * v + by);
                    let at = y.saturating_mul(c.blocks_wide).saturating_add(x).saturating_mul(64);
                    let Some(target) = c.coefficients.get_mut(at..at.saturating_add(64)) else {
                        return Err(unreadable("a block lies outside its component"));
                    };
                    for (slot, value) in target.iter_mut().zip(block) {
                        *slot = i16::try_from(value).unwrap_or(if value < 0 { i16::MIN } else { i16::MAX });
                    }
                }
            }
        }
    }
    // What follows the data must be a marker (not RSTn: no more MCUs remain).
    let mut end = bits.pos;
    while data.get(end) == Some(&0xFF) && data.get(end.saturating_add(1)) == Some(&0xFF) {
        end = end.saturating_add(1);
    }
    match (data.get(end), data.get(end.saturating_add(1))) {
        (Some(0xFF), Some(marker)) if !(0xD0..=0xD7).contains(marker) && *marker != 0 => Ok(end),
        _ => Err(unreadable("the coded data does not end at a marker")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::inspect::Limits;

    fn encoded(width: u32, height: u32, rgb: bool) -> Vec<u8> {
        let channels = if rgb { 3 } else { 1 };
        let pixels: Vec<u8> = (0..(width * height * channels) as usize).map(|i| ((i * 7 + i / 97) % 251) as u8).collect();
        let kind = if rgb { image::ExtendedColorType::Rgb8 } else { image::ExtendedColorType::L8 };
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 75).encode(&pixels, width, height, kind).unwrap();
        out
    }

    #[test]
    fn reads_what_an_encoder_wrote() {
        let mut budget = Budget::new(Limits::default());
        let data = encoded(37, 21, false);
        let frame = decode(&data, 1 << 20, &mut budget).unwrap();
        assert_eq!((frame.width, frame.height, frame.components.len()), (37, 21, 1));
        // The reconstruction agrees with another decoder's to within its rounding.
        let theirs = image::load_from_memory_with_format(&data, image::ImageFormat::Jpeg).unwrap().to_luma8();
        let component = &frame.components[0];
        let mut worst = 0.0f64;
        for by in 0..3 {
            for bx in 0..5 {
                let block = component.block(bx, by).unwrap();
                let values: [f64; 64] = std::array::from_fn(|k| f64::from(block[k]) * f64::from(component.quant[k]));
                let samples = crate::inspect::images::shown(&values);
                for y in 0..8 {
                    for x in 0..8 {
                        let (px, py) = (bx * 8 + x, by * 8 + y);
                        if px < 37 && py < 21 {
                            worst = worst.max((samples[y * 8 + x] - f64::from(theirs.get_pixel(px as u32, py as u32).0[0])).abs());
                        }
                    }
                }
            }
        }
        assert!(worst <= 1.5, "{worst}");
        // Colour, with subsampled chroma or not: three components.
        let frame = decode(&encoded(40, 24, true), 1 << 20, &mut budget).unwrap();
        assert_eq!(frame.components.len(), 3);
    }

    #[test]
    fn never_panics_on_damaged_data() {
        let data = encoded(40, 24, true);
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3_000 {
            let mut damaged = data.clone();
            let at = (next() % damaged.len() as u64) as usize;
            if next() % 4 == 0 {
                damaged.truncate(at);
            } else {
                damaged[at] ^= 1u8 << (next() % 8);
            }
            let mut budget = Budget::new(Limits::default());
            let _ = decode(&damaged, 1 << 20, &mut budget);
        }
        // Too many coefficients for the room given.
        let mut budget = Budget::new(Limits::default());
        assert!(matches!(decode(&data, 1_000, &mut budget), Err(Fail::TooComplex)));
    }
}
