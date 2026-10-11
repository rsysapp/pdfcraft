//! Embed build: one file's structure as `pdfcraft_cos` reads it, for the save inspector.
//!
//! Every object is read through [`Reader::get`], which checks that the object sits where the
//! cross-reference data says (header, generation, a stream's declared length, its position in an
//! object stream) and that `pdfcraft_cos` did not fall back to scanning the file for it. Objects
//! are compared by canonical SHA-256 digests that do not depend on object numbers (a rewrite
//! renumbers every object): references are replaced by the digest of what they point to.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use pdfcraft_cos::{Dict, Document, Lexer, ObjRef, Object, Stream, XrefEntry, parse_indirect};
use pdfcraft_render::content::PageGlyphs;

use super::{Budget, Fail, MAX_PAGES};

pub(crate) type Digest = [u8; 32];

pub(crate) fn sha256(parts: &[&[u8]]) -> Digest {
    let mut out = [0u8; 32];
    for (slot, byte) in out.iter_mut().zip(pdfcraft_sign::DigestAlg::Sha256.digest(parts)) {
        *slot = byte;
    }
    out
}

/// Entries that point back up a tree (page → page tree, widget → field, pop-up → its markup,
/// annotation → page): never followed when digesting, at any depth.
const BACK_REFERENCES: [&[u8]; 3] = [b"Parent", b"P", b"Popup"];

/// Page-dictionary entries compared on their own (or resolved through inheritance).
const PAGE_OWN_KEYS: [&[u8]; 7] = [b"Parent", b"Annots", b"Rotate", b"Contents", b"Resources", b"MediaBox", b"CropBox"];

/// Catalog entries the document-level check leaves out; a change to any other entry (actions,
/// names, outlines, page labels, permissions, …) fails the inspection, since no fact reports it.
/// - `/Pages`, `/AcroForm`, `/OCProperties`: compared on their own (pages, form fields, and the
///   layers, which count every page as changed);
/// - `/Metadata` (the XMP packet), `/StructTreeRoot` and `/MarkInfo` (tags), `/Version` (the PDF
///   version): nothing a viewer draws, and editors update them on ordinary saves (a redaction
///   cleans tags; a save may restamp metadata). Left out deliberately, so a changed `/Metadata`
///   can carry different document information with no trace in the result.
const CATALOG_REPORTED: [&[u8]; 7] = [b"Pages", b"AcroForm", b"OCProperties", b"Metadata", b"StructTreeRoot", b"MarkInfo", b"Version"];

/// The deepest chain of references followed while digesting one object.
const MAX_REFERENCE_DEPTH: usize = 256;
/// The deepest page tree and field tree read.
const MAX_TREE_DEPTH: usize = 64;

/// One annotation of a page.
pub(crate) struct Annot {
    /// The object it is (`None`: a dictionary written directly in `/Annots`).
    pub id: Option<ObjRef>,
    pub subtype: Vec<u8>,
    /// The dictionary (without `/P`, `/Popup`, `/Parent`) and everything it references,
    /// appearance streams included.
    pub digest: Digest,
    /// Compared as an annotation: not a widget of a form field (those are the form), not a
    /// pop-up without an appearance (part of its markup).
    pub tracked: bool,
    /// For `Redact` marks: the marked areas in user space (each quadrilateral's box, or `/Rect`).
    pub areas: Vec<[f64; 4]>,
}

/// What one page is, as far as the comparison goes.
pub(crate) struct PageFacts {
    /// `/Rotate` (inherited), 0, 90, 180 or 270.
    pub rotation: i64,
    /// The decoded content streams, in order.
    pub content: Digest,
    /// The resources the page's content is drawn with: its own and each ancestor's.
    pub resources: Digest,
    /// Every other page-dictionary entry, with the inherited media and crop boxes.
    pub other: Digest,
    /// The images drawn (directly, inside form XObjects, inline), as a sorted multiset.
    pub images: Vec<Digest>,
    /// Each image drawn, where (for a rewrite's redaction check).
    pub image_uses: Vec<ImageUse>,
    /// The image pixels drawn, summed over every use.
    pub image_pixels: u64,
    /// Content bytes the interpreter reads for the page (forms counted per use).
    pub interpreted: usize,
    pub annots: Vec<Annot>,
    /// How many `/Annots` entries are dictionaries (hayro must count the same).
    pub annots_in_array: usize,
    /// Filled from hayro: the upright page size in points, the glyphs drawn, the fingerprint.
    pub size: (f32, f32),
    pub glyphs: PageGlyphs,
    pub fingerprint: Digest,
}

/// One use of an image on a page.
#[derive(Clone, Debug)]
pub(crate) struct ImageUse {
    /// The raw encoded stream's digest (with its dictionary).
    pub digest: Digest,
    /// Image space (the unit square) to user space.
    pub matrix: Matrix,
    /// The image XObject (`None`: an inline image).
    pub image: Option<Arc<ImageInfo>>,
}

/// An image XObject, for a comparison of its samples.
#[derive(Debug)]
pub(crate) struct ImageInfo {
    pub obj: ObjRef,
    /// The dictionary without its filters, decode parameters and length: what the samples mean.
    pub meaning: Digest,
    pub width: u32,
    pub height: u32,
}

/// One cross-reference section of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Section {
    pub offset: u64,
    pub prev: Option<u64>,
    pub hybrid: bool,
}

pub(crate) struct Structure {
    pub pages: Vec<PageFacts>,
    /// Each terminal field's full name → its dictionaries' (with its ancestors') and widgets'
    /// digest.
    pub fields: BTreeMap<String, Digest>,
    /// The `/AcroForm` dictionary without `/Fields`.
    pub form: Option<Digest>,
    /// Catalog entries not reported otherwise, by key.
    pub catalog: BTreeMap<Vec<u8>, Digest>,
    /// `/OCProperties`: which layers show.
    pub layers: Option<Digest>,
    /// Cross-reference sections, oldest first.
    pub sections: Vec<Section>,
}

/// The fingerprint pages are matched by: content, resources, images and text.
pub(crate) fn fingerprint(page: &PageFacts) -> Digest {
    let text: String = page.glyphs.chars.iter().collect();
    let images: Vec<u8> = page.images.iter().flatten().copied().collect();
    sha256(&[&page.content, &page.resources, &images, text.as_bytes()])
}

/// The first catalog entry (not reported otherwise) that differs between the files.
pub(crate) fn catalog_change(before: &Structure, after: &Structure) -> Option<String> {
    let keys: BTreeSet<&Vec<u8>> = before.catalog.keys().chain(after.catalog.keys()).collect();
    keys.into_iter().find(|key| before.catalog.get(*key) != after.catalog.get(*key)).map(|key| String::from_utf8_lossy(key).into_owned())
}

/// The edited file's cross-reference chain: an appended update must chain onto the base's
/// sections exactly, every section it adds pointing backwards; a rewrite's sections all point
/// backwards.
pub(crate) fn check_chain(sections: &[Section], base_len: usize, base: &[Section], base_kept: bool) -> Result<(), Fail> {
    let refuse = |what: &str| Err(Fail::Read(format!("the cross-reference chain is not sound: {what}")));
    let backwards = |section: &&Section| section.prev.is_none_or(|prev| prev < section.offset);
    if !base_kept {
        return if sections.iter().all(|section| backwards(&section)) { Ok(()) } else { refuse("a /Prev does not point backwards") };
    }
    // The base's own sections were read with the base (a linearized base points forwards once).
    let base_len = base_len as u64;
    let (kept, added): (Vec<&Section>, Vec<&Section>) = sections.iter().partition(|section| section.offset < base_len);
    if !added.iter().all(backwards) {
        return refuse("a /Prev does not point backwards");
    }
    if kept.len() != base.len() || kept.iter().zip(base).any(|(a, b)| a.offset != b.offset) {
        return refuse("the update does not chain onto the base's sections");
    }
    // Oldest first: the first added section continues from the base's newest one.
    if let Some(first) = added.first()
        && first.prev != base.last().map(|newest| newest.offset)
    {
        return refuse("the update's /Prev is not the base's cross-reference section");
    }
    if added.iter().any(|section| section.hybrid) {
        return refuse("an appended section is a hybrid-reference section");
    }
    // The chain visits the added sections newest first, then the base's.
    if sections.len() != kept.len().saturating_add(added.len()) || sections.iter().skip(kept.len()).any(|section| section.offset < base_len) {
        return refuse("the sections are out of order");
    }
    Ok(())
}

/// A content stream, decoded and scanned once.
struct Scan {
    digest: Digest,
    items: Vec<Item>,
    len: usize,
}

/// What a content stream draws that the comparison follows.
enum Item {
    /// `/Name Do`, under this transformation (relative to the stream's start).
    Draw(Vec<u8>, Matrix),
    /// An inline image: its digest, pixel count and transformation.
    Inline(Digest, u64, Matrix),
}

/// A transformation matrix `[a b c d e f]` (ISO 32000-2 §8.3.3).
pub(crate) type Matrix = [f64; 6];

const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `m` then `then` (`m × then`, as `cm` concatenates onto the CTM).
fn concat(m: &Matrix, then: &Matrix) -> Matrix {
    [
        m[0] * then[0] + m[1] * then[2],
        m[0] * then[1] + m[1] * then[3],
        m[2] * then[0] + m[3] * then[2],
        m[2] * then[1] + m[3] * then[3],
        m[4] * then[0] + m[5] * then[2] + then[4],
        m[4] * then[1] + m[5] * then[3] + then[5],
    ]
}

/// The box an image drawn under `m` covers (the unit square), in the space `m` maps into. A
/// matrix that does not give finite numbers gives a box that contains nothing.
pub(crate) fn unit_square(m: &Matrix) -> [f64; 4] {
    let corners = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)].map(|(x, y)| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]));
    let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (x, y) in corners {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    if [x0, y0, x1, y1].iter().all(|v| v.is_finite()) { [x0, y0, x1, y1] } else { [f64::NAN; 4] }
}

/// A leaf of the page tree with what it inherits.
struct PageNode {
    obj: ObjRef,
    dict: Dict,
    /// `/Resources` of the page and each ancestor, nearest first.
    resources: Vec<Object>,
    media: Option<Object>,
    crop: Option<Object>,
    rotate: Option<Object>,
}

/// What a page draws.
#[derive(Default)]
struct Drawn {
    images: Vec<Digest>,
    /// Each image drawn, where.
    placements: Vec<ImageUse>,
    pixels: u64,
    interpreted: usize,
}

struct Reader<'d, 'b> {
    doc: &'d Document,
    data: &'d [u8],
    budget: &'b mut Budget,
    /// Objects whose location was checked: number → generation.
    checked: HashMap<u32, u16>,
    checking: HashSet<u32>,
    /// Object streams: the object numbers they hold, in order.
    object_streams: HashMap<u32, Arc<Vec<u32>>>,
    digests: HashMap<ObjRef, Digest>,
    /// The references being digested, outermost first.
    stack: Vec<ObjRef>,
    contents: HashMap<Vec<ObjRef>, Arc<Scan>>,
    forms: HashMap<ObjRef, Arc<Scan>>,
    images: HashMap<ObjRef, Arc<ImageInfo>>,
}

fn read_error(what: impl std::fmt::Display) -> Fail {
    Fail::Read(what.to_string())
}

/// A decode failure: the budget's bound, or damaged data.
fn decode_error(error: &pdfcraft_cos::CosError) -> Fail {
    let message = error.to_string();
    if message.contains("exceeds the limit") { Fail::TooComplex } else { Fail::Read(format!("a stream cannot be decoded: {message}")) }
}

fn ws(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | b'\x0C' | b'\r' | b' ')
}

fn delimiter(byte: u8) -> bool {
    matches!(byte, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

/// The number written for a PDF number, so that `20`, `20.0` and a rewrite's `20` agree.
fn encode_number(value: f64, out: &mut Vec<u8>) {
    let value = if value == 0.0 { 0.0 } else { value };
    out.push(b'#');
    out.extend_from_slice(&value.to_bits().to_le_bytes());
}

impl<'d, 'b> Reader<'d, 'b> {
    /// Load object `r`, checking where it was found.
    fn get(&mut self, r: ObjRef) -> Result<Arc<Object>, Fail> {
        self.budget.object()?;
        self.check(r)?;
        self.doc.try_get(r.num).map_err(|error| read_error(format!("object {r} cannot be read: {error}")))
    }

    /// Follow `o` if it is a reference.
    fn resolve(&mut self, o: &Object) -> Result<Arc<Object>, Fail> {
        match o {
            Object::Ref(r) => self.get(*r),
            other => Ok(Arc::new(other.clone())),
        }
    }

    /// `o` resolved to a dictionary; `None` when absent or null; an error for anything else.
    fn dict_of(&mut self, o: Option<&Object>, what: &str) -> Result<Option<Dict>, Fail> {
        let Some(o) = o else { return Ok(None) };
        match &*self.resolve(o)? {
            Object::Null => Ok(None),
            Object::Dict(d) => Ok(Some(d.clone())),
            _ => Err(read_error(format!("{what} is not a dictionary"))),
        }
    }

    /// `o` resolved to an array; empty when absent or null; an error for anything else.
    fn array_of(&mut self, o: Option<&Object>, what: &str) -> Result<Vec<Object>, Fail> {
        let Some(o) = o else { return Ok(Vec::new()) };
        match &*self.resolve(o)? {
            Object::Null => Ok(Vec::new()),
            Object::Array(a) => Ok(a.clone()),
            _ => Err(read_error(format!("{what} is not an array"))),
        }
    }

    fn int_of(&mut self, o: Option<&Object>) -> Result<Option<i64>, Fail> {
        let Some(o) = o else { return Ok(None) };
        Ok(self.resolve(o)?.as_int())
    }

    /// The object must be where the cross-reference data says, as `pdfcraft_cos` read it.
    fn check(&mut self, r: ObjRef) -> Result<(), Fail> {
        if let Some(generation) = self.checked.get(&r.num) {
            return if *generation == r.generation { Ok(()) } else { Err(read_error(format!("object {r} is referenced with another generation"))) };
        }
        if !self.checking.insert(r.num) {
            return Err(read_error(format!("object {r} refers to itself while loading")));
        }
        let result = match self.doc.xref_entry(r.num) {
            None | Some(XrefEntry::Free { .. }) => Ok(()),
            Some(XrefEntry::InFile { offset, generation }) => {
                if generation == r.generation {
                    self.check_in_file(r, offset)
                } else {
                    Err(read_error(format!("object {r} is referenced with another generation")))
                }
            }
            Some(XrefEntry::InStream { stream, index }) => {
                if r.generation != 0 {
                    Err(read_error(format!("object {r} is in an object stream but has a generation")))
                } else {
                    self.object_stream(stream).and_then(|numbers| {
                        if usize::try_from(index).ok().and_then(|index| numbers.get(index)) == Some(&r.num) {
                            Ok(())
                        } else {
                            Err(read_error(format!("object {r} is not where its object stream says")))
                        }
                    })
                }
            }
        };
        self.checking.remove(&r.num);
        result?;
        self.checked.insert(r.num, r.generation);
        Ok(())
    }

    /// The object at `offset` is `r` itself, and `pdfcraft_cos` loaded it from there (a stream's
    /// declared length is its real length: no search for `endstream`).
    fn check_in_file(&mut self, r: ObjRef, offset: u64) -> Result<(), Fail> {
        let misplaced = || read_error(format!("object {r} is not where the cross-reference data says"));
        let at = usize::try_from(offset).map_err(|_| misplaced())?;
        let mut lx = Lexer::new(self.data, at);
        lx.skip_ws();
        let number = |token: &[u8]| std::str::from_utf8(token).ok().and_then(|s| s.parse::<u64>().ok());
        let num = number(lx.token());
        lx.skip_ws();
        let generation = number(lx.token());
        if num != Some(u64::from(r.num)) || generation != Some(u64::from(r.generation)) || !lx.eat_keyword(b"obj") {
            return Err(misplaced());
        }
        let header = lx.object().map_err(|_| misplaced())?;
        let is_stream = matches!(header, Object::Dict(_)) && lx.eat_keyword(b"stream");
        let loaded = self.doc.try_get(r.num).map_err(|error| read_error(format!("object {r} cannot be read: {error}")))?;
        match (&header, &*loaded) {
            (Object::Dict(declared), Object::Stream(stream)) if is_stream => {
                let length = match declared.get(b"Length") {
                    Some(Object::Int(length)) => Some(*length),
                    Some(Object::Ref(length)) => self.get(*length)?.as_int(),
                    _ => None,
                };
                let without_length =
                    |d: &Dict| d.iter().filter(|(key, _)| key.as_slice() != b"Length").map(|(k, v)| (k.clone(), v.clone())).collect::<Dict>();
                if length != i64::try_from(stream.raw.len()).ok() {
                    return Err(read_error(format!("stream {r} does not have the length it declares")));
                }
                if without_length(declared) != without_length(&stream.dict) {
                    return Err(misplaced());
                }
                Ok(())
            }
            (header, loaded) if !is_stream && header == loaded => Ok(()),
            _ => Err(misplaced()),
        }
    }

    /// The object numbers an object stream holds, in order (decoded strictly).
    fn object_stream(&mut self, num: u32) -> Result<Arc<Vec<u32>>, Fail> {
        if let Some(found) = self.object_streams.get(&num) {
            return Ok(found.clone());
        }
        let Some(XrefEntry::InFile { generation, .. }) = self.doc.xref_entry(num) else {
            return Err(read_error(format!("object stream {num} is not in the file")));
        };
        let obj = self.get(ObjRef::new(num, generation))?;
        let Object::Stream(stream) = &*obj else { return Err(read_error(format!("object {num} is not an object stream"))) };
        if stream.dict.name(b"Type") != Some(b"ObjStm") {
            return Err(read_error(format!("object {num} is not an object stream")));
        }
        let count = stream.dict.int(b"N").unwrap_or(0).clamp(0, 1_000_000);
        let data = stream.decoded_strict_within(self.budget.content_room()).map_err(|error| decode_error(&error))?;
        self.budget.content(data.len())?;
        let mut lx = Lexer::new(&data, 0);
        let mut numbers = Vec::new();
        for _ in 0..count {
            let (Ok(Object::Int(inner)), Ok(Object::Int(_))) = (lx.object(), lx.object()) else { break };
            numbers.push(u32::try_from(inner.max(0)).unwrap_or(u32::MAX));
        }
        let numbers = Arc::new(numbers);
        self.object_streams.insert(num, numbers.clone());
        Ok(numbers)
    }

    // ── digests ─────────────────────────────────────────────────────────────────────────────

    /// The digest of `o` and everything it references.
    fn digest(&mut self, o: &Object) -> Result<Digest, Fail> {
        let mut out = Vec::new();
        self.encode(o, 0, &mut out)?;
        Ok(sha256(&[&out]))
    }

    /// The digest of a dictionary without some of its entries.
    fn digest_without(&mut self, d: &Dict, without: &[&[u8]]) -> Result<Digest, Fail> {
        let kept: Dict = d.iter().filter(|(key, _)| !without.contains(&key.as_slice())).map(|(k, v)| (k.clone(), v.clone())).collect();
        self.digest(&Object::Dict(kept))
    }

    /// The digest of object `r` and the lowest position on the reference stack its subtree
    /// referred back to (`usize::MAX`: none above it), which decides whether it can be reused.
    fn digest_ref(&mut self, r: ObjRef) -> Result<(Digest, usize), Fail> {
        if let Some(found) = self.digests.get(&r) {
            return Ok((*found, usize::MAX));
        }
        if self.stack.len() >= MAX_REFERENCE_DEPTH {
            return Err(Fail::TooComplex);
        }
        let obj = self.get(r)?;
        let position = self.stack.len();
        self.stack.push(r);
        let mut out = Vec::new();
        let encoded = self.encode(&obj, 0, &mut out);
        self.stack.pop();
        let lowest = encoded?;
        let digest = sha256(&[&out]);
        if lowest >= position {
            // Refers back to nothing outside itself: the same wherever it is reached from.
            self.digests.insert(r, digest);
            Ok((digest, usize::MAX))
        } else {
            Ok((digest, lowest))
        }
    }

    /// Append the canonical encoding of `o` to `out`; returns the lowest reference-stack position
    /// a reference in it pointed back to.
    fn encode(&mut self, o: &Object, depth: usize, out: &mut Vec<u8>) -> Result<usize, Fail> {
        if depth > 512 {
            return Err(Fail::TooComplex);
        }
        let mut lowest = usize::MAX;
        match o {
            Object::Null => out.push(b'n'),
            Object::Bool(value) => out.extend_from_slice(&[b'b', u8::from(*value)]),
            Object::Int(value) => {
                if value.unsigned_abs() < (1u64 << 53) {
                    encode_number(*value as f64, out);
                } else {
                    out.push(b'I');
                    out.extend_from_slice(&value.to_le_bytes());
                }
            }
            Object::Real(value) => encode_number(*value, out),
            Object::String(s) => {
                out.push(b's');
                out.extend_from_slice(&(s.bytes.len() as u64).to_le_bytes());
                out.extend_from_slice(&s.bytes);
            }
            Object::Name(name) => {
                out.push(b'/');
                out.extend_from_slice(&(name.len() as u64).to_le_bytes());
                out.extend_from_slice(name);
            }
            Object::Array(items) => {
                out.push(b'[');
                out.extend_from_slice(&(items.len() as u64).to_le_bytes());
                for item in items {
                    lowest = lowest.min(self.encode(item, depth.saturating_add(1), out)?);
                }
            }
            Object::Dict(d) => lowest = self.encode_dict(d, depth, out)?,
            Object::Stream(stream) => {
                out.push(b'S');
                lowest = self.encode_dict(&stream.dict, depth, out)?;
                self.budget.hashed(stream.raw.len())?;
                out.extend_from_slice(&sha256(&[&stream.raw]));
            }
            Object::Ref(r) => {
                self.budget.tick()?;
                if let Some(position) = self.stack.iter().position(|outer| outer == r) {
                    // A cycle: say how far up it goes, not which number it has.
                    out.push(b'C');
                    out.extend_from_slice(&(self.stack.len().saturating_sub(position) as u64).to_le_bytes());
                    lowest = position;
                } else {
                    let (digest, below) = self.digest_ref(*r)?;
                    out.push(b'R');
                    out.extend_from_slice(&digest);
                    lowest = below;
                }
            }
        }
        Ok(lowest)
    }

    fn encode_dict(&mut self, d: &Dict, depth: usize, out: &mut Vec<u8>) -> Result<usize, Fail> {
        let mut entries: Vec<(&Vec<u8>, &Object)> = d.iter().filter(|(key, _)| !BACK_REFERENCES.contains(&key.as_slice())).collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        out.push(b'<');
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        let mut lowest = usize::MAX;
        for (key, value) in entries {
            out.extend_from_slice(&(key.len() as u64).to_le_bytes());
            out.extend_from_slice(key);
            lowest = lowest.min(self.encode(value, depth.saturating_add(1), out)?);
        }
        Ok(lowest)
    }

    // ── page tree ───────────────────────────────────────────────────────────────────────────

    fn page_tree(&mut self, root: &Dict) -> Result<Vec<PageNode>, Fail> {
        let Some(Object::Ref(tree)) = root.get(b"Pages") else { return Err(read_error("the catalog has no page tree")) };
        let tree = *tree;
        let dict = self.get(tree)?.as_dict().cloned().ok_or_else(|| read_error("the page tree is not a dictionary"))?;
        let mut pages = Vec::new();
        let mut nodes = BTreeSet::from([tree]);
        let mut seen = BTreeSet::new();
        let inherited = PageNode { obj: tree, dict: Dict::new(), resources: Vec::new(), media: None, crop: None, rotate: None };
        self.walk(&dict, &inherited, 0, &mut nodes, &mut seen, &mut pages)?;
        Ok(pages)
    }

    fn walk(
        &mut self,
        node: &Dict,
        inherited: &PageNode,
        depth: usize,
        nodes: &mut BTreeSet<ObjRef>,
        seen: &mut BTreeSet<ObjRef>,
        pages: &mut Vec<PageNode>,
    ) -> Result<(), Fail> {
        if depth > MAX_TREE_DEPTH {
            return Err(read_error("the page tree is too deep"));
        }
        let here = with_inherited(node, inherited);
        let Some(kids) = node.get(b"Kids") else { return Err(read_error("a page tree node has no /Kids")) };
        let kids = self.array_of(Some(kids), "/Kids")?;
        for kid in kids {
            self.budget.tick()?;
            let Object::Ref(r) = kid else { return Err(read_error("a page tree entry is not a reference")) };
            let dict = self.get(r)?.as_dict().cloned().ok_or_else(|| read_error(format!("page tree entry {r} is not a dictionary")))?;
            if dict.name(b"Type") == Some(b"Pages") {
                if !nodes.insert(r) {
                    return Err(read_error("the page tree loops"));
                }
                self.walk(&dict, &here, depth.saturating_add(1), nodes, seen, pages)?;
            } else {
                if !seen.insert(r) {
                    return Err(read_error(format!("page {r} appears twice in the page tree")));
                }
                if pages.len() >= MAX_PAGES {
                    return Err(Fail::TooComplex);
                }
                let mut page = with_inherited(&dict, &here);
                page.obj = r;
                page.dict = dict;
                pages.push(page);
            }
        }
        Ok(())
    }

    // ── content ─────────────────────────────────────────────────────────────────────────────

    fn decode(&mut self, stream: &Stream) -> Result<Vec<u8>, Fail> {
        let data = stream.decoded_strict_within(self.budget.content_room()).map_err(|error| decode_error(&error))?;
        self.budget.content(data.len())?;
        Ok(data)
    }

    /// The page's content streams, decoded, joined and scanned (once per distinct list).
    fn page_content(&mut self, page: &Dict) -> Result<Arc<Scan>, Fail> {
        let refs: Vec<ObjRef> = match page.get(b"Contents") {
            None => Vec::new(),
            Some(Object::Ref(r)) => match &*self.get(*r)? {
                Object::Stream(_) => vec![*r],
                Object::Array(items) => items
                    .iter()
                    .map(|item| item.as_ref().ok_or_else(|| read_error("a /Contents entry is not a reference")))
                    .collect::<Result<_, _>>()?,
                Object::Null => Vec::new(),
                _ => return Err(read_error("/Contents is neither a stream nor an array")),
            },
            Some(Object::Array(items)) => {
                items.iter().map(|item| item.as_ref().ok_or_else(|| read_error("a /Contents entry is not a reference"))).collect::<Result<_, _>>()?
            }
            Some(Object::Null) => Vec::new(),
            Some(_) => return Err(read_error("/Contents is neither a stream nor an array")),
        };
        if let Some(found) = self.contents.get(&refs) {
            return Ok(found.clone());
        }
        let mut data = Vec::new();
        let single = refs.len() == 1;
        for r in &refs {
            let obj = self.get(*r)?;
            let Object::Stream(stream) = &*obj else { return Err(read_error(format!("content stream {r} is not a stream"))) };
            data.extend_from_slice(&self.decode(stream)?);
            if !single {
                // As hayro joins them: at least one whitespace in between.
                data.push(b' ');
            }
        }
        let scan = Arc::new(self.scan(&data)?);
        self.contents.insert(refs, scan.clone());
        Ok(scan)
    }

    /// A form XObject's content, decoded and scanned once.
    fn form_content(&mut self, r: ObjRef, stream: &Stream) -> Result<Arc<Scan>, Fail> {
        if let Some(found) = self.forms.get(&r) {
            return Ok(found.clone());
        }
        let data = self.decode(stream)?;
        let scan = Arc::new(self.scan(&data)?);
        self.forms.insert(r, scan.clone());
        Ok(scan)
    }

    /// What a content stream draws that is followed: `Do` and inline images. Anything the lexer
    /// cannot read is an error.
    fn scan(&mut self, data: &[u8]) -> Result<Scan, Fail> {
        let unreadable = |what: &str| read_error(format!("a content stream cannot be read: {what}"));
        let mut lx = Lexer::new(data, 0);
        let mut name: Option<Vec<u8>> = None;
        let mut numbers: Vec<f64> = Vec::new();
        let mut ctm = IDENTITY;
        let mut saved: Vec<Matrix> = Vec::new();
        let mut items = Vec::new();
        loop {
            self.budget.tick()?;
            lx.skip_ws();
            let Some(&byte) = data.get(lx.pos) else { break };
            let at = lx.pos;
            if matches!(byte, b'/' | b'(' | b'<' | b'[' | b'+' | b'-' | b'.') || byte.is_ascii_digit() {
                let operand = lx.object().map_err(|error| unreadable(&error.to_string()))?;
                if lx.pos <= at {
                    return Err(unreadable("no progress"));
                }
                if let Some(number) = operand.as_f64() {
                    numbers.push(number);
                }
                name = match operand {
                    Object::Name(n) => Some(n),
                    _ => None,
                };
                continue;
            }
            if delimiter(byte) {
                return Err(unreadable("a stray delimiter"));
            }
            let keyword = lx.token();
            if keyword.is_empty() {
                return Err(unreadable("an unknown byte"));
            }
            match keyword {
                b"Do" => items.push(Item::Draw(name.take().ok_or_else(|| unreadable("Do without a name"))?, ctm)),
                b"BI" => {
                    let (digest, pixels, end) = self.inline_image(data, lx.pos)?;
                    items.push(Item::Inline(digest, pixels, ctm));
                    lx.pos = end;
                }
                b"q" => saved.push(ctm),
                b"Q" => ctm = saved.pop().unwrap_or(ctm),
                b"cm" => {
                    if let Some(m) = numbers.len().checked_sub(6).and_then(|from| numbers.get(from..)) {
                        let m: Matrix = [m[0], m[1], m[2], m[3], m[4], m[5]];
                        ctm = concat(&m, &ctm);
                    }
                }
                _ => {}
            }
            numbers.clear();
            name = None;
        }
        Ok(Scan { digest: sha256(&[data]), items, len: data.len() })
    }

    /// An inline image after `BI`: its digest and pixel count, and where it ends (after `EI`).
    fn inline_image(&mut self, data: &[u8], pos: usize) -> Result<(Digest, u64, usize), Fail> {
        let unreadable = || read_error("an inline image cannot be read");
        let mut lx = Lexer::new(data, pos);
        let mut dict = Dict::new();
        loop {
            self.budget.tick()?;
            lx.skip_ws();
            if lx.eat_keyword(b"ID") {
                break;
            }
            let Ok(Object::Name(key)) = lx.object() else { return Err(unreadable()) };
            let value = lx.object().map_err(|_| unreadable())?;
            dict.set(key, value);
        }
        // One whitespace byte separates ID from the data, which ends at whitespace + `EI` +
        // (whitespace, a delimiter or the end).
        let begin = lx.pos.saturating_add(1).min(data.len());
        let mut i = begin;
        let end = loop {
            let (Some(&e), Some(&next)) = (data.get(i), data.get(i.saturating_add(1))) else { return Err(unreadable()) };
            if e == b'E'
                && next == b'I'
                && (i == begin || data.get(i.wrapping_sub(1)).copied().is_some_and(ws))
                && data.get(i.saturating_add(2)).is_none_or(|b| ws(*b) || delimiter(*b))
            {
                break i.saturating_add(2);
            }
            i = i.saturating_add(1);
        };
        let bytes = data.get(pos..end).ok_or_else(unreadable)?;
        self.budget.hashed(bytes.len())?;
        let side =
            |a: &[u8], b: &[u8]| dict.get(a).or_else(|| dict.get(b)).and_then(Object::as_int).map_or(0, |v| u64::try_from(v.max(0)).unwrap_or(0));
        let pixels = side(b"W", b"Width").saturating_mul(side(b"H", b"Height"));
        Ok((sha256(&[b"inline", bytes]), pixels, end))
    }

    /// The XObject `name` resolves to through a resource chain (nearest first), as hayro looks it
    /// up: the first level whose `/XObject` entry is a stream.
    fn xobject(&mut self, chain: &[Arc<Dict>], name: &[u8]) -> Result<Option<ObjRef>, Fail> {
        for level in chain {
            let Some(xobjects) = self.dict_of(level.get(b"XObject"), "/XObject")? else { continue };
            if let Some(Object::Ref(r)) = xobjects.get(name)
                && matches!(&*self.get(*r)?, Object::Stream(_))
            {
                return Ok(Some(*r));
            }
        }
        Ok(None)
    }

    /// Follow what a content stream draws: images (hashed once each) and forms (recursively).
    /// `ctm` maps the stream's space to user space.
    fn draw(&mut self, items: &[Item], chain: &[Arc<Dict>], depth: usize, ctm: &Matrix, drawn: &mut Drawn) -> Result<(), Fail> {
        for item in items {
            self.budget.uses()?;
            match item {
                Item::Inline(digest, pixels, at) => {
                    drawn.images.push(*digest);
                    drawn.placements.push(ImageUse { digest: *digest, matrix: concat(at, ctm), image: None });
                    drawn.pixels = drawn.pixels.saturating_add(*pixels);
                }
                Item::Draw(name, at) => {
                    let placed = concat(at, ctm);
                    let Some(r) = self.xobject(chain, name)? else { continue };
                    let obj = self.get(r)?;
                    let Object::Stream(stream) = &*obj else { continue };
                    match stream.dict.name(b"Subtype") {
                        Some(b"Image") => {
                            let (digest, _) = self.digest_ref(r)?;
                            let info = self.image_info(r, &stream.dict)?;
                            drawn.images.push(digest);
                            drawn.pixels = drawn.pixels.saturating_add(u64::from(info.width).saturating_mul(u64::from(info.height)));
                            drawn.placements.push(ImageUse { digest, matrix: placed, image: Some(info) });
                        }
                        Some(b"Form") => {
                            if depth >= self.budget.limits.form_depth {
                                return Err(Fail::TooComplex);
                            }
                            let scan = self.form_content(r, stream)?;
                            drawn.interpreted = drawn.interpreted.saturating_add(scan.len);
                            let own = self.dict_of(stream.dict.get(b"Resources"), "a form's /Resources")?.unwrap_or_default();
                            let mut inner = Vec::with_capacity(chain.len().saturating_add(1));
                            inner.push(Arc::new(own));
                            inner.extend(chain.iter().cloned());
                            let numbers = self.array_of(stream.dict.get(b"Matrix"), "a form's /Matrix")?;
                            let matrix = match numbers.iter().map(Object::as_f64).collect::<Option<Vec<f64>>>().as_deref() {
                                Some([a, b, c, d, e, f]) => [*a, *b, *c, *d, *e, *f],
                                _ => IDENTITY,
                            };
                            self.draw(&scan.items, &inner, depth.saturating_add(1), &concat(&matrix, &placed), drawn)?;
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// What image `r` (with dictionary `dict`) is, once per image.
    fn image_info(&mut self, r: ObjRef, dict: &Dict) -> Result<Arc<ImageInfo>, Fail> {
        if let Some(found) = self.images.get(&r) {
            return Ok(found.clone());
        }
        let side = |value: Option<i64>| u32::try_from(value.unwrap_or(0).max(0)).unwrap_or(u32::MAX);
        let width = side(self.int_of(dict.get(b"Width"))?);
        let height = side(self.int_of(dict.get(b"Height"))?);
        let meaning = self.digest_without(dict, &[b"Filter", b"DecodeParms", b"Length", b"DL"])?;
        let info = Arc::new(ImageInfo { obj: r, meaning, width, height });
        self.images.insert(r, info.clone());
        Ok(info)
    }

    // ── pages ───────────────────────────────────────────────────────────────────────────────

    fn page(&mut self, node: &PageNode, widgets: &BTreeSet<ObjRef>, annotation_ids: &mut BTreeSet<ObjRef>) -> Result<PageFacts, Fail> {
        let rotation = match self.int_of(node.rotate.as_ref())? {
            None if node.rotate.is_some() => return Err(read_error(format!("page {} has a /Rotate that is not a whole number", node.obj))),
            value => value.unwrap_or(0).rem_euclid(360),
        };
        if rotation % 90 != 0 {
            return Err(read_error(format!("page {} is rotated by {rotation}°, not a multiple of 90°", node.obj)));
        }
        // Every other entry, and the boxes as inherited.
        let mut other: Dict =
            node.dict.iter().filter(|(key, _)| !PAGE_OWN_KEYS.contains(&key.as_slice())).map(|(k, v)| (k.clone(), v.clone())).collect();
        if let Some(media) = &node.media {
            other.set(b"MediaBox".to_vec(), media.clone());
        }
        if let Some(crop) = &node.crop {
            other.set(b"CropBox".to_vec(), crop.clone());
        }
        let other = self.digest(&Object::Dict(other))?;
        // Resources: every level a lookup can reach, nearest first; empty levels change nothing.
        let mut chain = Vec::new();
        let mut levels = Vec::new();
        for level in &node.resources {
            let Some(dict) = self.dict_of(Some(level), "/Resources")? else { continue };
            if dict.is_empty() {
                continue;
            }
            levels.extend_from_slice(&self.digest(level)?);
            chain.push(Arc::new(dict));
        }
        let resources = sha256(&[&levels]);
        let scan = self.page_content(&node.dict)?;
        let mut drawn = Drawn { interpreted: scan.len, ..Drawn::default() };
        self.draw(&scan.items, &chain, 0, &IDENTITY, &mut drawn)?;
        drawn.images.sort_unstable();
        let (annots, annots_in_array) = self.annotations(node, widgets, annotation_ids)?;
        Ok(PageFacts {
            rotation,
            content: scan.digest,
            resources,
            other,
            images: drawn.images,
            image_pixels: drawn.pixels,
            image_uses: drawn.placements,
            interpreted: drawn.interpreted,
            annots,
            annots_in_array,
            size: (0.0, 0.0),
            glyphs: PageGlyphs::default(),
            fingerprint: [0; 32],
        })
    }

    fn annotations(&mut self, node: &PageNode, widgets: &BTreeSet<ObjRef>, ids: &mut BTreeSet<ObjRef>) -> Result<(Vec<Annot>, usize), Fail> {
        let entries = self.array_of(node.dict.get(b"Annots"), "/Annots")?;
        let mut annots = Vec::new();
        let mut in_array = 0usize;
        for entry in entries {
            self.budget.tick()?;
            let (id, dict) = match &entry {
                Object::Ref(r) => (Some(*r), self.get(*r)?.as_dict().cloned()),
                Object::Dict(d) => (None, Some(d.clone())),
                _ => (None, None),
            };
            // As hayro counts them: entries that are dictionaries.
            let Some(dict) = dict else { continue };
            in_array = in_array.saturating_add(1);
            if let Some(id) = id
                && !ids.insert(id)
            {
                return Err(read_error(format!("annotation {id} is listed more than once")));
            }
            let digest = match id {
                Some(r) => self.digest_ref(r)?.0,
                None => self.digest(&Object::Dict(dict.clone()))?,
            };
            let subtype = dict.name(b"Subtype").unwrap_or_default().to_vec();
            let widget = subtype == b"Widget" && id.is_some_and(|id| widgets.contains(&id));
            let silent_popup = subtype == b"Popup" && !dict.contains(b"AP");
            let areas = if subtype == b"Redact" { self.marked_areas(&dict)? } else { Vec::new() };
            annots.push(Annot { id, subtype, digest, tracked: !widget && !silent_popup, areas });
        }
        Ok((annots, in_array))
    }

    /// A redaction mark's areas: each quadrilateral's box, or the rectangle.
    fn marked_areas(&mut self, dict: &Dict) -> Result<Vec<[f64; 4]>, Fail> {
        let numbers = |items: Vec<Object>| items.iter().map(Object::as_f64).collect::<Option<Vec<f64>>>().filter(|v| v.iter().all(|x| x.is_finite()));
        let boxed = |points: &[f64]| {
            let xs = points.iter().step_by(2);
            let ys = points.iter().skip(1).step_by(2);
            let (x0, x1) = xs.clone().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
            let (y0, y1) = ys.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), y| (lo.min(*y), hi.max(*y)));
            [x0, y0, x1, y1]
        };
        let quads = self.array_of(dict.get(b"QuadPoints"), "/QuadPoints")?;
        if let Some(points) = numbers(quads).filter(|v| !v.is_empty() && v.len() % 8 == 0) {
            return Ok(points.chunks(8).map(boxed).collect());
        }
        let rect = self.array_of(dict.get(b"Rect"), "/Rect")?;
        Ok(numbers(rect).filter(|v| v.len() == 4).map(|v| vec![boxed(&v)]).unwrap_or_default())
    }

    // ── form fields ─────────────────────────────────────────────────────────────────────────

    fn fields(&mut self, form: &Dict, widgets: &mut BTreeSet<ObjRef>) -> Result<BTreeMap<String, Digest>, Fail> {
        let mut fields = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for entry in self.array_of(form.get(b"Fields"), "/Fields")? {
            self.field(&entry, "", &[], 0, &mut seen, widgets, &mut fields)?;
        }
        Ok(fields)
    }

    #[allow(clippy::too_many_arguments)]
    fn field(
        &mut self,
        entry: &Object,
        parent: &str,
        ancestors: &[Digest],
        depth: usize,
        seen: &mut BTreeSet<ObjRef>,
        widgets: &mut BTreeSet<ObjRef>,
        fields: &mut BTreeMap<String, Digest>,
    ) -> Result<(), Fail> {
        self.budget.tick()?;
        if depth > MAX_TREE_DEPTH {
            return Err(read_error("the form's field tree is too deep"));
        }
        let Object::Ref(r) = entry else { return Err(read_error("a form field is not a reference")) };
        if !seen.insert(*r) {
            return Err(read_error("the form's field tree loops"));
        }
        let dict = self.get(*r)?.as_dict().cloned().ok_or_else(|| read_error(format!("form field {r} is not a dictionary")))?;
        let name = match dict.get(b"T").map(|t| self.resolve(t)).transpose()?.as_deref() {
            Some(Object::String(t)) if parent.is_empty() => t.to_text(),
            Some(Object::String(t)) => format!("{parent}.{}", t.to_text()),
            _ => parent.to_string(),
        };
        if dict.name(b"Subtype") == Some(b"Widget") {
            widgets.insert(*r);
        }
        let own = self.digest_without(&dict, &[b"Kids"])?;
        let mut children = Vec::new();
        let mut widget_digests = Vec::new();
        for kid in self.array_of(dict.get(b"Kids"), "/Kids")? {
            let Object::Ref(kid_ref) = kid else { return Err(read_error("a form field's kid is not a reference")) };
            let kid_dict =
                self.get(kid_ref)?.as_dict().cloned().ok_or_else(|| read_error(format!("form field kid {kid_ref} is not a dictionary")))?;
            if kid_dict.contains(b"T") || kid_dict.contains(b"Kids") {
                children.push(kid);
            } else {
                if !seen.insert(kid_ref) {
                    return Err(read_error("the form's field tree loops"));
                }
                widgets.insert(kid_ref);
                widget_digests.extend_from_slice(&self.digest_ref(kid_ref)?.0);
            }
        }
        let mut path = ancestors.to_vec();
        path.push(own);
        if children.is_empty() {
            let flat: Vec<u8> = path.iter().flatten().copied().collect();
            let digest = sha256(&[&flat, &widget_digests]);
            let combined = match fields.get(&name) {
                Some(earlier) => sha256(&[earlier, &digest]),
                None => digest,
            };
            fields.insert(name, combined);
            return Ok(());
        }
        for child in &children {
            self.field(child, &name, &path, depth.saturating_add(1), seen, widgets, fields)?;
        }
        Ok(())
    }
}

/// A page-tree node's attributes over what it inherits.
fn with_inherited(node: &Dict, inherited: &PageNode) -> PageNode {
    let mut resources = vec![node.get(b"Resources").cloned().unwrap_or(Object::Null)];
    resources.extend(inherited.resources.iter().cloned());
    PageNode {
        obj: inherited.obj,
        dict: Dict::new(),
        resources,
        media: node.get(b"MediaBox").cloned().or_else(|| inherited.media.clone()),
        crop: node.get(b"CropBox").cloned().or_else(|| inherited.crop.clone()),
        rotate: node.get(b"Rotate").cloned().or_else(|| inherited.rotate.clone()),
    }
}

/// The trailer of the cross-reference section at `offset`.
fn section(data: &[u8], offset: u64, is_stream: bool) -> Result<Section, Fail> {
    let unreadable = || read_error(format!("the cross-reference section at {offset} cannot be read"));
    let at = usize::try_from(offset).map_err(|_| unreadable())?;
    let trailer = if is_stream {
        let (_, obj) = parse_indirect(data, at, &|_| None).map_err(|_| unreadable())?;
        obj.as_dict().cloned().ok_or_else(unreadable)?
    } else {
        let rest = data.get(at..).ok_or_else(unreadable)?;
        let found = rest.windows(7).position(|w| w == b"trailer").ok_or_else(unreadable)?;
        let mut lx = Lexer::new(data, at.saturating_add(found).saturating_add(7));
        lx.object().ok().and_then(|o| o.as_dict().cloned()).ok_or_else(unreadable)?
    };
    let prev = match trailer.get(b"Prev") {
        None => None,
        Some(Object::Int(prev)) if *prev >= 0 => Some(prev.unsigned_abs()),
        Some(_) => return Err(read_error("a /Prev is not an offset")),
    };
    if trailer.get(b"Root").is_some_and(|root| !matches!(root, Object::Ref(_))) {
        return Err(read_error("a trailer's /Root is not a reference"));
    }
    Ok(Section { offset, prev, hybrid: trailer.contains(b"XRefStm") })
}

/// Read one file's structure. Fails on anything not read for certain.
pub(crate) fn read(doc: &Document, budget: &mut Budget) -> Result<Structure, Fail> {
    let data = doc.bytes().as_slice();
    let sections = doc.revisions().iter().map(|revision| section(data, revision.xref_offset, revision.is_stream)).collect::<Result<Vec<_>, _>>()?;
    if sections.is_empty() {
        return Err(read_error("the file has no cross-reference data"));
    }
    let mut reader = Reader {
        doc,
        data,
        budget,
        checked: HashMap::new(),
        checking: HashSet::new(),
        object_streams: HashMap::new(),
        digests: HashMap::new(),
        stack: Vec::new(),
        contents: HashMap::new(),
        forms: HashMap::new(),
        images: HashMap::new(),
    };
    let Some(root) = doc.root() else { return Err(read_error("the trailer's /Root is not a reference")) };
    let catalog = reader.get(root)?.as_dict().cloned().ok_or_else(|| read_error("the catalog is not a dictionary"))?;
    // The page tree first: the page bound is checked before any per-page work.
    let nodes = reader.page_tree(&catalog)?;
    let mut widgets = BTreeSet::new();
    let (fields, form) = match reader.dict_of(catalog.get(b"AcroForm"), "/AcroForm")? {
        Some(form) => (reader.fields(&form, &mut widgets)?, Some(reader.digest_without(&form, &[b"Fields"])?)),
        None => (BTreeMap::new(), None),
    };
    let layers = catalog.get(b"OCProperties").map(|layers| reader.digest(layers)).transpose()?;
    let mut entries = BTreeMap::new();
    for (key, value) in catalog.iter() {
        if !CATALOG_REPORTED.contains(&key.as_slice()) {
            entries.insert(key.clone(), reader.digest(value)?);
        }
    }
    let mut pages = Vec::with_capacity(nodes.len());
    let mut annotation_ids = BTreeSet::new();
    for node in &nodes {
        reader.budget.time()?;
        pages.push(reader.page(node, &widgets, &mut annotation_ids)?);
    }
    Ok(Structure { pages, fields, form, catalog: entries, layers, sections })
}
