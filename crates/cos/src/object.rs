//! The PDF object model (ISO 32000-2 §7.3).
//!
//! Dictionaries keep their key order so rewritten objects stay recognisable in diffs and
//! byte-stable when re-serialized. Streams keep their *encoded* bytes; decoding is on demand.

use pdfcraft_filters::{Filter, Params};

use crate::{Bytes, CosError};

/// An indirect reference: object number and generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjRef {
    pub num: u32,
    pub generation: u16,
}

impl ObjRef {
    pub const fn new(num: u32, generation: u16) -> Self {
        Self { num, generation }
    }
}

impl std::fmt::Display for ObjRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} R", self.num, self.generation)
    }
}

/// A string object. `hex` records how it was written, so it is re-emitted the same way.
#[derive(Clone, Debug, PartialEq)]
pub struct PdfString {
    pub bytes: Vec<u8>,
    pub hex: bool,
}

impl PdfString {
    pub fn literal(bytes: impl Into<Vec<u8>>) -> Self {
        Self { bytes: bytes.into(), hex: false }
    }

    /// A text string (ISO 32000-2 §7.9.2.2): PDFDocEncoding when possible, else UTF-16BE with BOM.
    pub fn text(s: &str) -> Self {
        if s.chars().all(|c| (' '..='~').contains(&c) || c == '\n' || c == '\r' || c == '\t') {
            return Self::literal(s.as_bytes().to_vec());
        }
        let mut bytes = vec![0xFE, 0xFF];
        for u in s.encode_utf16() {
            bytes.extend_from_slice(&u.to_be_bytes());
        }
        Self { bytes, hex: false }
    }

    /// Decode a text string: UTF-16BE (BOM), UTF-8 (BOM, PDF 2.0) or PDFDocEncoding.
    ///
    /// Lenient beyond the spec, as readers are: invalid sequences after a BOM decode lossily
    /// instead of dropping the string, a BOM-less string whose bytes are valid UTF-8 with
    /// non-ASCII bytes (written by some producers) is read as UTF-8 rather than as
    /// PDFDocEncoding mojibake, and stray leading U+FEFF (doubled BOMs) are removed.
    pub fn to_text(&self) -> String {
        let s = match self.bytes.as_slice() {
            [0xFE, 0xFF, rest @ ..] => {
                let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
                String::from_utf16_lossy(&units)
            }
            [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
            b => match std::str::from_utf8(b) {
                // Pure ASCII is identical either way; only non-ASCII UTF-8 needs the switch.
                Ok(utf8) if !b.is_ascii() => utf8.to_owned(),
                _ => b.iter().map(|&c| pdfcraft_crypt::pdfdoc_char(c)).collect(),
            },
        };
        if s.starts_with('\u{FEFF}') { s.trim_start_matches('\u{FEFF}').to_owned() } else { s }
    }
}

/// A name object (without the leading `/`, with `#xx` escapes already decoded).
pub type Name = Vec<u8>;

/// An ordered dictionary.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dict {
    entries: Vec<(Name, Object)>,
}

impl Dict {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &[u8]) -> Option<&Object> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut Object> {
        self.entries.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Insert or replace, keeping the position of an existing key.
    pub fn set(&mut self, key: impl Into<Name>, value: impl Into<Object>) {
        let key = key.into();
        let value = value.into();
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => *v = value,
            None => self.entries.push((key, value)),
        }
    }

    pub fn remove(&mut self, key: &[u8]) -> Option<Object> {
        let i = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(i).1)
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Name, &Object)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn name(&self, key: &[u8]) -> Option<&[u8]> {
        self.get(key).and_then(Object::as_name)
    }

    pub fn int(&self, key: &[u8]) -> Option<i64> {
        self.get(key).and_then(Object::as_int)
    }

    pub fn reference(&self, key: &[u8]) -> Option<ObjRef> {
        self.get(key).and_then(Object::as_ref)
    }
}

impl FromIterator<(Name, Object)> for Dict {
    fn from_iter<T: IntoIterator<Item = (Name, Object)>>(iter: T) -> Self {
        // Embed build: a later entry with an earlier key replaces its value in place, as `set`
        // does, found through an index: building a dictionary of n keys costs n, not n² (a
        // crafted dictionary of a hundred thousand keys took minutes to parse).
        let mut entries: Vec<(Name, Object)> = Vec::new();
        let mut index: std::collections::HashMap<Name, usize> = std::collections::HashMap::new();
        for (k, v) in iter {
            match index.get(&k).and_then(|&at| entries.get_mut(at)) {
                Some((_, slot)) => *slot = v,
                None => {
                    index.insert(k.clone(), entries.len());
                    entries.push((k, v));
                }
            }
        }
        Dict { entries }
    }
}

/// A stream: dictionary plus its data *as stored* (still filtered).
#[derive(Clone, Debug, PartialEq)]
pub struct Stream {
    pub dict: Dict,
    pub raw: Bytes,
}

/// Upper bound for a single decoded stream (decompression-bomb defence).
pub const MAX_DECODED: usize = 1 << 30;

impl Stream {
    /// A new stream from already-encoded bytes (`/Length` is set at write time).
    pub fn from_raw(dict: Dict, raw: Vec<u8>) -> Self {
        Self { dict, raw: raw.into() }
    }

    /// A new stream holding `data` compressed with Flate.
    pub fn flate(mut dict: Dict, data: &[u8]) -> Self {
        dict.set(b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec()));
        dict.remove(b"DecodeParms");
        Self { dict, raw: pdfcraft_filters::encode_flate(data).into() }
    }

    /// Embed build: replace an indirect `/Filter` or `/DecodeParms` (or an indirect entry of
    /// either's array) with the object it refers to, as readers apply them (ISO 32000-2 §7.3.8.2
    /// lets any value be indirect). `get` loads a reference (`None`: it cannot be loaded, and the
    /// reference stays). Only that one level is resolved: a reference to a reference stays, and
    /// [`Stream::decoded_strict`] refuses it.
    pub fn resolve_coding(dict: &mut Dict, mut get: impl FnMut(ObjRef) -> Option<Object>) {
        for key in [b"Filter".as_slice(), b"DecodeParms".as_slice()] {
            let resolved = match dict.get(key) {
                Some(Object::Ref(r)) => get(*r),
                Some(Object::Array(items)) if items.iter().any(|item| matches!(item, Object::Ref(_))) => Some(Object::Array(
                    items.iter().map(|item| if let Object::Ref(r) = item { get(*r).unwrap_or_else(|| item.clone()) } else { item.clone() }).collect(),
                )),
                _ => None,
            };
            if let Some(resolved) = resolved {
                dict.set(key.to_vec(), resolved);
            }
        }
    }

    /// Embed build: the coding entries' shape, for strict decoding: `/Filter` a name or an array
    /// of names, `/DecodeParms` a dictionary or an array of dictionaries and nulls (either may be
    /// absent or null). [`Stream::filters`] skips anything else, which would take the data for
    /// something it is not.
    /// A `/Crypt` filter is refused too: what it applies depends on the security handler and
    /// its crypt filters, which readers apply differently (and a file without encryption has
    /// none to apply).
    fn coding_is_well_formed(&self) -> bool {
        let filter = match self.dict.get(b"Filter") {
            None | Some(Object::Null) => true,
            Some(Object::Name(name)) => name.as_slice() != b"Crypt",
            Some(Object::Array(items)) => items.iter().all(|item| matches!(item, Object::Name(name) if name.as_slice() != b"Crypt")),
            _ => false,
        };
        let parms = match self.dict.get(b"DecodeParms") {
            None | Some(Object::Null | Object::Dict(_)) => true,
            Some(Object::Array(items)) => items.iter().all(|item| matches!(item, Object::Null | Object::Dict(_))),
            _ => false,
        };
        filter && parms
    }

    /// The filter chain declared in the dictionary.
    pub fn filters(&self) -> Vec<(Filter, Params)> {
        let names: Vec<&[u8]> = match self.dict.get(b"Filter") {
            Some(Object::Name(n)) => vec![n],
            Some(Object::Array(a)) => a.iter().filter_map(Object::as_name).collect(),
            _ => Vec::new(),
        };
        let parms: Vec<Option<&Dict>> = match self.dict.get(b"DecodeParms") {
            Some(Object::Dict(d)) => vec![Some(d)],
            Some(Object::Array(a)) => a.iter().map(Object::as_dict).collect(),
            _ => Vec::new(),
        };
        names
            .into_iter()
            .enumerate()
            // `/Crypt` is applied by the security handler when the object is loaded (§7.4.10).
            .filter(|(_, n)| *n != b"Crypt")
            .map(|(i, n)| {
                let p = parms.get(i).copied().flatten();
                let get = |k: &[u8], d: i64| p.and_then(|p| p.int(k)).unwrap_or(d);
                let params = Params {
                    predictor: get(b"Predictor", 1),
                    colors: get(b"Colors", 1),
                    bits_per_component: get(b"BitsPerComponent", 8),
                    columns: get(b"Columns", 1),
                    early_change: get(b"EarlyChange", 1),
                };
                (Filter::from_name(n), params)
            })
            .collect()
    }

    /// Embed build: the encoded bytes past where the first filter stops reading (after a Flate
    /// stream's end and Adler-32, an ASCII filter's or run-length end-of-data marker, a JPEG's
    /// last end-of-image marker; `None`: nothing after it, or a filter whose end is not told),
    /// and the bytes inflated to find it (for a caller that charges the work). `Err` when Flate
    /// data cannot be read to its end within `max` bytes made, or its Adler-32 is wrong.
    pub fn trailing_data(&self, max: usize) -> Result<(Option<&[u8]>, usize), CosError> {
        let raw: &[u8] = &self.raw;
        let (end, made) = match self.filters().first().map(|(filter, _)| filter) {
            Some(Filter::Flate) => pdfcraft_filters::flate_end(raw, max).map_err(|e| CosError::Filter(e.to_string()))?,
            // Their end-of-data markers: `>`, `~>`, a length byte of 128, a JPEG's last
            // end-of-image marker.
            Some(Filter::AsciiHex) => (raw.iter().position(|byte| *byte == b'>').map_or(raw.len(), |at| at + 1), 0),
            Some(Filter::Ascii85) => (raw.windows(2).position(|pair| pair == b"~>").map_or(raw.len(), |at| at + 2), 0),
            Some(Filter::RunLength) => {
                let mut at = 0usize;
                loop {
                    match raw.get(at) {
                        None => break (raw.len(), 0),
                        Some(128) => break (at + 1, 0),
                        Some(&length) if length < 128 => at = at.saturating_add(usize::from(length) + 2),
                        Some(_) => at = at.saturating_add(2),
                    }
                }
            }
            Some(Filter::Dct) => (raw.windows(2).rposition(|pair| pair == [0xFF, 0xD9]).map_or(raw.len(), |at| at + 2), 0),
            _ => return Ok((None, 0)),
        };
        Ok((raw.get(end..).filter(|rest| !rest.is_empty()), made))
    }

    /// Decoded data, tolerating truncated or corrupt encodings the way viewers do (the data
    /// decoded before the damage is returned). Streams with image codecs fail with
    /// `CosError::Filter` (keep them encoded). Use `decoded_strict` to detect damage.
    pub fn decoded(&self) -> Result<Vec<u8>, CosError> {
        self.decoded_within(MAX_DECODED)
    }

    /// Like [`Stream::decoded`], for a stream whose decoded size has a tighter bound than
    /// [`MAX_DECODED`]: decoding past `max` bytes is an error.
    pub fn decoded_within(&self, max: usize) -> Result<Vec<u8>, CosError> {
        let chain = self.filters();
        if chain.is_empty() {
            return self.raw_within(max);
        }
        pdfcraft_filters::decode_tolerant(&chain, &self.raw, max.min(MAX_DECODED)).map(|(v, _)| v).map_err(|e| CosError::Filter(e.to_string()))
    }

    /// Decoded data, bounded by [`MAX_DECODED`]; any corruption is an error.
    pub fn decoded_strict(&self) -> Result<Vec<u8>, CosError> {
        self.decoded_strict_within(MAX_DECODED)
    }

    /// Embed build: like [`Stream::decoded_strict`], and decoding past `max` bytes is an error too
    /// (a caller with a work budget, such as a save inspector).
    pub fn decoded_strict_within(&self, max: usize) -> Result<Vec<u8>, CosError> {
        self.decoded_strict_counted(max).map(|(data, _)| data)
    }

    /// Embed build: like [`Stream::decoded_strict_within`], also returning the bytes every filter
    /// of the chain made together (at most `max`, which bounds them all together: see
    /// `pdfcraft_filters::decode`), for a caller that charges the work.
    pub fn decoded_strict_counted(&self, max: usize) -> Result<(Vec<u8>, usize), CosError> {
        if !self.coding_is_well_formed() {
            return Err(CosError::Filter(
                "the stream's /Filter or /DecodeParms is not a name, a dictionary or an array of them, or is /Crypt".into(),
            ));
        }
        let chain = self.filters();
        if chain.is_empty() {
            let data = self.raw_within(max)?;
            let made = data.len();
            return Ok((data, made));
        }
        pdfcraft_filters::decode_counted(&chain, &self.raw, max.min(MAX_DECODED)).map_err(|e| CosError::Filter(e.to_string()))
    }

    /// Data after the first filter alone, bounded by `max`: the rest of the chain stays encoded.
    /// Any corruption in that first filter is an error.
    pub fn decoded_first_filter_strict(&self, max: usize) -> Result<Vec<u8>, CosError> {
        let mut chain = self.filters();
        chain.truncate(1);
        if chain.is_empty() {
            return self.raw_within(max);
        }
        pdfcraft_filters::decode(&chain, &self.raw, max.min(MAX_DECODED)).map_err(|e| CosError::Filter(e.to_string()))
    }

    fn raw_within(&self, max: usize) -> Result<Vec<u8>, CosError> {
        let max = max.min(MAX_DECODED);
        // An empty decoder chain still produces an owned output buffer: check its limit
        // before cloning, including streams whose /Crypt filter was applied on load.
        if self.raw.len() > max {
            return Err(CosError::Filter(pdfcraft_filters::FilterError::LimitExceeded(max).to_string()));
        }
        Ok(self.raw.to_vec())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Object {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    String(PdfString),
    Name(Name),
    Array(Vec<Object>),
    Dict(Dict),
    Stream(Stream),
    Ref(ObjRef),
}

impl Object {
    pub fn name(n: &str) -> Self {
        Object::Name(n.as_bytes().to_vec())
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Object::Int(i) => Some(*i),
            Object::Real(r) if r.fract() == 0.0 && r.is_finite() => Some(*r as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Object::Int(i) => Some(*i as f64),
            Object::Real(r) => Some(*r),
            _ => None,
        }
    }

    pub fn as_name(&self) -> Option<&[u8]> {
        match self {
            Object::Name(n) => Some(n),
            _ => None,
        }
    }

    pub fn as_ref(&self) -> Option<ObjRef> {
        match self {
            Object::Ref(r) => Some(*r),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Object::Dict(d) => Some(d),
            Object::Stream(s) => Some(&s.dict),
            _ => None,
        }
    }

    pub fn as_dict_mut(&mut self) -> Option<&mut Dict> {
        match self {
            Object::Dict(d) => Some(d),
            Object::Stream(s) => Some(&mut s.dict),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Object>> {
        match self {
            Object::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&PdfString> {
        match self {
            Object::String(s) => Some(s),
            _ => None,
        }
    }
}

impl From<i64> for Object {
    fn from(v: i64) -> Self {
        Object::Int(v)
    }
}
impl From<i32> for Object {
    fn from(v: i32) -> Self {
        Object::Int(v as i64)
    }
}
impl From<f64> for Object {
    fn from(v: f64) -> Self {
        Object::Real(v)
    }
}
impl From<bool> for Object {
    fn from(v: bool) -> Self {
        Object::Bool(v)
    }
}
impl From<ObjRef> for Object {
    fn from(v: ObjRef) -> Self {
        Object::Ref(v)
    }
}
impl From<Dict> for Object {
    fn from(v: Dict) -> Self {
        Object::Dict(v)
    }
}
impl From<Vec<Object>> for Object {
    fn from(v: Vec<Object>) -> Self {
        Object::Array(v)
    }
}
impl From<PdfString> for Object {
    fn from(v: PdfString) -> Self {
        Object::String(v)
    }
}
impl From<Stream> for Object {
    fn from(v: Stream) -> Self {
        Object::Stream(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_strings_round_trip() {
        for s in ["Plain ASCII", "Café — naïve", "日本語", "emoji 📄"] {
            assert_eq!(PdfString::text(s).to_text(), s);
        }
        assert!(!PdfString::text("abc").bytes.starts_with(&[0xFE, 0xFF]));
    }

    #[test]
    fn text_string_encodings() {
        // UTF-16BE with BOM.
        let mut utf16 = vec![0xFE, 0xFF];
        utf16.extend("第一章 概述".encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(PdfString::literal(utf16).to_text(), "第一章 概述");
        // UTF-8 with BOM (PDF 2.0): the BOM is not part of the text.
        let mut utf8 = vec![0xEF, 0xBB, 0xBF];
        utf8.extend_from_slice("第一章".as_bytes());
        assert_eq!(PdfString::literal(utf8).to_text(), "第一章");
        // Invalid UTF-8 after the BOM decodes lossily instead of vanishing.
        assert_eq!(PdfString::literal(vec![0xEF, 0xBB, 0xBF, b'A', 0xFF, b'B']).to_text(), "A\u{FFFD}B");
        // BOM-less raw UTF-8 from producers that skip the BOM.
        assert_eq!(PdfString::literal("目录".as_bytes()).to_text(), "目录");
        // PDFDocEncoding Latin-1 (not valid UTF-8) stays PDFDocEncoding.
        assert_eq!(PdfString::literal(vec![b'C', b'a', b'f', 0xE9]).to_text(), "Café");
        // Doubled BOMs leave no U+FEFF behind.
        assert_eq!(PdfString::literal(vec![0xFE, 0xFF, 0xFE, 0xFF, 0x00, b'A']).to_text(), "A");
        assert_eq!(PdfString::literal(vec![0xEF, 0xBB, 0xBF, 0xEF, 0xBB, 0xBF, b'A']).to_text(), "A");
        // Odd trailing byte and empty strings never panic.
        assert_eq!(PdfString::literal(vec![0xFE, 0xFF, 0x00, b'A', 0x00]).to_text(), "A");
        assert_eq!(PdfString::literal(vec![0xFE, 0xFF]).to_text(), "");
        assert_eq!(PdfString::literal(Vec::new()).to_text(), "");
    }

    #[test]
    fn pdfdoc_encoding_high_range() {
        assert_eq!(PdfString::literal(vec![0x84, 0x92, 0xA0, b'A']).to_text(), "—™€A");
    }

    #[test]
    fn dict_preserves_order_and_replaces_in_place() {
        let mut d = Dict::new();
        d.set(b"B".to_vec(), 1);
        d.set(b"A".to_vec(), 2);
        d.set(b"B".to_vec(), 3);
        let keys: Vec<_> = d.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, vec![b"B".to_vec(), b"A".to_vec()]);
        assert_eq!(d.int(b"B"), Some(3));
    }

    #[test]
    fn raw_streams_refuse_decoding_past_a_caller_limit() {
        let stream = Stream::from_raw(Dict::new(), b"abc".to_vec());
        for max in [0, 2] {
            assert!(matches!(stream.decoded_within(max), Err(CosError::Filter(_))));
        }
    }

    #[test]
    fn empty_filter_arrays_refuse_decoding_past_a_caller_limit() {
        let mut dict = Dict::new();
        dict.set(b"Filter".to_vec(), Object::Array(Vec::new()));
        let stream = Stream::from_raw(dict, b"abc".to_vec());
        assert!(matches!(stream.decoded_within(2), Err(CosError::Filter(_))));
    }

    #[test]
    fn crypt_only_streams_refuse_decoding_past_a_caller_limit() {
        let mut dict = Dict::new();
        dict.set(b"Filter".to_vec(), Object::name("Crypt"));
        let mut parms = Dict::new();
        parms.set(b"Name".to_vec(), Object::name("Identity"));
        dict.set(b"DecodeParms".to_vec(), Object::Dict(parms));
        let stream = Stream::from_raw(dict, b"abc".to_vec());
        assert!(matches!(stream.decoded_within(2), Err(CosError::Filter(_))));
    }

    #[test]
    fn identity_streams_preserve_bytes_within_a_caller_limit() {
        let mut array = Dict::new();
        array.set(b"Filter".to_vec(), Object::Array(Vec::new()));
        let mut crypt = Dict::new();
        crypt.set(b"Filter".to_vec(), Object::name("Crypt"));
        for dict in [Dict::new(), array, crypt] {
            let stream = Stream::from_raw(dict.clone(), b"abc".to_vec());
            assert_eq!(stream.decoded_within(3).unwrap(), b"abc");
            assert_eq!(stream.decoded_within(usize::MAX).unwrap(), b"abc");
            assert_eq!(stream.decoded().unwrap(), b"abc");
            // Embed build: strict decoding refuses `/Crypt` (`coding_is_well_formed`).
            if dict.name(b"Filter") == Some(b"Crypt".as_slice()) {
                assert!(stream.decoded_strict().is_err());
            } else {
                assert_eq!(stream.decoded_strict().unwrap(), b"abc");
            }
            assert!(Stream::from_raw(dict, Vec::new()).decoded_within(0).unwrap().is_empty());
        }
    }

    #[test]
    fn filtered_streams_preserve_the_caller_limit() {
        let stream = Stream::flate(Dict::new(), b"abc");
        assert!(matches!(stream.decoded_within(2), Err(CosError::Filter(_))));
        assert_eq!(stream.decoded_within(3).unwrap(), b"abc");
        assert_eq!(stream.decoded_strict().unwrap(), b"abc");
    }

    #[test]
    fn first_filter_decodes_only_the_outer_wrapper() {
        let data = "0 0 m 100 100 l S\n".repeat(64).into_bytes();
        let inner = Stream::flate(Dict::new(), &data).raw.to_vec();
        for (outer, name, corrupt) in [(Filter::Ascii85, "ASCII85Decode", &b"{~>"[..]), (Filter::AsciiHex, "ASCIIHexDecode", &b"G0>"[..])] {
            let encoded = pdfcraft_filters::encode(&outer, &Params::default(), &inner).unwrap();
            let mut dict = Dict::new();
            dict.set(b"Filter".to_vec(), Object::Array(vec![Object::name(name), Object::name("FlateDecode")]));
            let stream = Stream::from_raw(dict.clone(), encoded);
            assert_eq!(stream.decoded_first_filter_strict(usize::MAX).unwrap(), inner);
            assert_eq!(stream.decoded_strict().unwrap(), data);
            assert!(matches!(stream.decoded_first_filter_strict(inner.len() - 1), Err(CosError::Filter(_))));
            assert!(matches!(Stream::from_raw(dict, corrupt.to_vec()).decoded_first_filter_strict(usize::MAX), Err(CosError::Filter(_))));
        }
    }
}
