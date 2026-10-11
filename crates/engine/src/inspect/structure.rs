//! Embed build: one file's structure as `pdfcraft_cos` reads it, for the save inspector.
//!
//! Every object is read through [`Reader::get`], which checks that the object sits where the
//! cross-reference data says (header, generation, a stream's declared length, its position in an
//! object stream) and that `pdfcraft_cos` did not fall back to scanning the file for it. Objects
//! are compared by canonical SHA-256 digests that do not depend on object numbers (a rewrite
//! renumbers every object): references are replaced by the digest of what they point to.
//! Content streams other than a page's own (form XObjects, Type 3 glyph procedures, annotation
//! appearances) are digested by their decoded data, so a save that only re-encodes one does not
//! change it; images and every other stream by their encoded bytes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use pdfcraft_cos::{Dict, Document, Lexer, ObjRef, Object, Stream, XrefEntry, parse_indirect};
use pdfcraft_render::content::{FormDraw, ImageDraw, ImageSource, ObjectPlace, PageContent, PageGlyphs, Resolved};

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

/// Stream-dictionary entries that say how the data is encoded, not what it is.
const CODING_KEYS: [&[u8]; 4] = [b"Filter", b"DecodeParms", b"Length", b"DL"];

/// A form's dictionary entries left out of its key (see [`PageFacts::forms`]).
const FORM_KEY_LEFT_OUT: [&[u8]; 5] = [b"Filter", b"DecodeParms", b"Length", b"DL", b"Resources"];

/// Dictionary entries whose values are content streams, or dictionaries of them (Type 3 glyph
/// procedures, annotation appearances): digested by their decoded data.
const CONTENT_KEYS: [&[u8]; 2] = [b"CharProcs", b"AP"];

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
    /// The dictionary without its appearance (`/AP`): two annotations alike but for their
    /// appearance have the same `bare` digest.
    pub bare: Digest,
    /// `/Rect`, normalised (`None`: missing or not four finite numbers).
    pub rect: Option<[f64; 4]>,
}

/// What one page is, as far as the comparison goes.
pub(crate) struct PageFacts {
    /// `/Rotate` (inherited), 0, 90, 180 or 270.
    pub rotation: i64,
    /// The page dictionary's object.
    pub object: ObjRef,
    /// The decoded content streams, in order.
    pub content: Digest,
    /// The resources the page's content is drawn with: its own and each ancestor's.
    pub resources: Digest,
    /// Every other page-dictionary entry, with the inherited media and crop boxes.
    pub other: Digest,
    /// The images drawn (directly, inside form XObjects, inline), as a sorted multiset.
    pub images: Vec<Digest>,
    /// Each image hayro drew, where (for a rewrite's redaction check): filled from hayro's
    /// interpretation (see [`Known::image_uses`]), not from this scan.
    pub image_uses: Vec<ImageUse>,
    /// What hayro must draw, as this scan reads the content (see [`Required`]).
    pub required: Required,
    /// Every form XObject the content draws (through `Do`, in forms, in tiling patterns and as
    /// soft masks' groups, shown or not): the form, and its key: its dictionary but for its encoding and resources, and its
    /// decoded content (what it draws itself; what it draws with its resources is compared on
    /// its own).
    pub forms: Vec<(ObjRef, Digest)>,
    /// Each form hayro drew, where (for a rewrite's redaction check).
    pub form_draws: Vec<FormDraw>,
    /// The glyph procedures of the Type 3 fonts in the resources the content is drawn with (the
    /// page's, and each drawn form's and pattern's own), each by its name and decoded content.
    pub type3: BTreeSet<Digest>,
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

/// One use of an image on a page, as hayro's interpreter drew it.
#[derive(Clone, Debug)]
pub(crate) struct ImageUse {
    /// The raw encoded stream's digest (with its dictionary).
    pub digest: Digest,
    /// Image space (the unit square) to user space.
    pub matrix: Matrix,
    /// Drawn in a tiling pattern's cell: `matrix` is in the cell's space, not the page's.
    pub tiled: bool,
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

/// An image as both readers name it: an image XObject by its object, or an inline image (which
/// the two readers delimit each their own way: counted, not identified).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ImageKey {
    Object(ObjRef),
    Inline,
}

/// What hayro must draw for a page, by this scan of its content: every image and form the
/// content draws, unless it is optional content that is off in the document's default
/// configuration (see [`OptionalContent`]). Images in tiling patterns' cells only once each
/// (a cell is drawn once per paint that uses it).
#[derive(Clone, Debug, Default)]
pub(crate) struct Required {
    /// Images drawn directly or in forms: how many times each.
    pub plain: BTreeMap<ImageKey, usize>,
    /// Images drawn in tiling patterns' cells.
    pub patterned: BTreeSet<ImageKey>,
    /// Forms drawn directly or in forms.
    pub forms: BTreeSet<ObjRef>,
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

/// What a content stream draws that the comparison follows: which images, forms and patterns,
/// and which optional content it lies in; not where (placements are hayro's, see
/// [`Known::image_uses`]).
enum Item {
    /// `/Name Do`.
    Draw(Vec<u8>),
    /// An inline image: its digest and pixel count.
    Inline(Digest, u64),
    /// `/Name scn` or `/Name SCN`: a pattern chosen as the colour.
    Pattern(Vec<u8>),
    /// `BMC` or `BDC`: optional content when the tag is `/OC` and the property list a name (the
    /// name), any other marked content otherwise (`None`).
    Begin(Option<Vec<u8>>),
    /// `EMC`.
    End,
    /// `/Name gs`: a graphics state, which may set a soft mask.
    State(Vec<u8>),
}

/// A transformation matrix `[a b c d e f]` (ISO 32000-2 §8.3.3).
pub(crate) type Matrix = [f64; 6];

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
    pixels: u64,
    interpreted: usize,
    required: Required,
    forms: Vec<(ObjRef, Digest)>,
    type3: BTreeSet<Digest>,
    /// Tiling patterns and soft masks' groups already followed on this page.
    patterns: BTreeSet<ObjRef>,
    masks: BTreeSet<ObjRef>,
}

fn require(required: &mut Required, key: ImageKey, patterned: bool) {
    if patterned {
        required.patterned.insert(key);
    } else {
        let count = required.plain.entry(key).or_default();
        *count = count.saturating_add(1);
    }
}

/// Where in a page's content a stream is drawn.
#[derive(Clone, Copy)]
struct Context {
    depth: usize,
    /// Inside optional content that is off in the default configuration.
    hidden: bool,
    /// Inside a tiling pattern's cell.
    patterned: bool,
    /// Inside a soft mask's group: drawn only when something is painted through the mask, so
    /// nothing in it is required of hayro; its forms are still compared.
    masked: bool,
}

/// The document's default optional-content configuration (`/OCProperties /D`), as far as it is
/// certain: which groups are off, and which groups' state cannot be told without a viewer's
/// choices (automatic states, intents other than viewing).
#[derive(Default)]
pub(crate) struct OptionalContent {
    off: BTreeSet<ObjRef>,
    on: BTreeSet<ObjRef>,
    /// Groups whose state is not certain.
    uncertain: BTreeSet<ObjRef>,
    /// The configuration's state for groups listed nowhere: `Some(true)` on (`/BaseState /ON`, or
    /// none given), `None` not certain.
    default: Option<bool>,
    /// Whether the configuration is read for viewing: its `/Intent` is absent, `/View` or `/All`
    /// (or an array holding one).
    viewing: bool,
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
    /// Digests by object and whether it is digested as content (see [`CONTENT_KEYS`]).
    digests: HashMap<(ObjRef, bool), Digest>,
    /// The references being digested, outermost first.
    stack: Vec<ObjRef>,
    /// Content streams scanned, by their references and whether `/Contents` is an array.
    contents: HashMap<(Vec<ObjRef>, bool), Arc<Scan>>,
    forms: HashMap<ObjRef, Arc<Scan>>,
    images: HashMap<ObjRef, Arc<ImageInfo>>,
    /// Type 3 fonts' glyph procedure keys, by font.
    type3: HashMap<ObjRef, Arc<Vec<Digest>>>,
    optional: Arc<OptionalContent>,
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
        self.encode(o, 0, false, &mut out)?;
        Ok(sha256(&[&out]))
    }

    /// The digest of a dictionary without some of its entries.
    fn digest_without(&mut self, d: &Dict, without: &[&[u8]]) -> Result<Digest, Fail> {
        let kept: Dict = d.iter().filter(|(key, _)| !without.contains(&key.as_slice())).map(|(k, v)| (k.clone(), v.clone())).collect();
        self.digest(&Object::Dict(kept))
    }

    /// The digest of object `r` (as content: see [`CONTENT_KEYS`]) and the lowest position on the
    /// reference stack its subtree referred back to (`usize::MAX`: none above it), which decides
    /// whether it can be reused.
    fn digest_ref(&mut self, r: ObjRef, content: bool) -> Result<(Digest, usize), Fail> {
        if let Some(found) = self.digests.get(&(r, content)) {
            return Ok((*found, usize::MAX));
        }
        if self.stack.len() >= MAX_REFERENCE_DEPTH {
            return Err(Fail::TooComplex);
        }
        let obj = self.get(r)?;
        let position = self.stack.len();
        self.stack.push(r);
        let mut out = Vec::new();
        let encoded = self.encode(&obj, 0, content, &mut out);
        self.stack.pop();
        let lowest = encoded?;
        let digest = sha256(&[&out]);
        if lowest >= position {
            // Refers back to nothing outside itself: the same wherever it is reached from.
            self.digests.insert((r, content), digest);
            Ok((digest, usize::MAX))
        } else {
            Ok((digest, lowest))
        }
    }

    /// Append the canonical encoding of `o` to `out`; returns the lowest reference-stack position
    /// a reference in it pointed back to. `content`: `o` is reached through [`CONTENT_KEYS`], so a
    /// stream is a content stream, and a dictionary's values are too.
    fn encode(&mut self, o: &Object, depth: usize, content: bool, out: &mut Vec<u8>) -> Result<usize, Fail> {
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
                    lowest = lowest.min(self.encode(item, depth.saturating_add(1), false, out)?);
                }
            }
            Object::Dict(d) => lowest = self.encode_dict(d, depth, content, out)?,
            Object::Stream(stream) if content || stream.dict.name(b"Subtype") == Some(b"Form") => {
                // A content stream: what it draws is its decoded data, however it is encoded.
                out.push(b'D');
                let kept: Dict =
                    stream.dict.iter().filter(|(key, _)| !CODING_KEYS.contains(&key.as_slice())).map(|(k, v)| (k.clone(), v.clone())).collect();
                lowest = self.encode_dict(&kept, depth, false, out)?;
                let data = self.decode(stream)?;
                self.budget.hashed(data.len())?;
                out.extend_from_slice(&sha256(&[&data]));
            }
            Object::Stream(stream) => {
                out.push(b'S');
                lowest = self.encode_dict(&stream.dict, depth, false, out)?;
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
                    let (digest, below) = self.digest_ref(*r, content)?;
                    out.push(b'R');
                    out.extend_from_slice(&digest);
                    lowest = below;
                }
            }
        }
        Ok(lowest)
    }

    fn encode_dict(&mut self, d: &Dict, depth: usize, content: bool, out: &mut Vec<u8>) -> Result<usize, Fail> {
        let mut entries: Vec<(&Vec<u8>, &Object)> = d.iter().filter(|(key, _)| !BACK_REFERENCES.contains(&key.as_slice())).collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        out.push(b'<');
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        let mut lowest = usize::MAX;
        for (key, value) in entries {
            out.extend_from_slice(&(key.len() as u64).to_le_bytes());
            out.extend_from_slice(key);
            let content = content || CONTENT_KEYS.contains(&key.as_slice());
            lowest = lowest.min(self.encode(value, depth.saturating_add(1), content, out)?);
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

    /// The page's content streams, decoded, joined and scanned (once per distinct list). Joined
    /// as hayro joins them, so the two readers' decoded content can be compared: an array's
    /// streams each followed by a space, a single stream as it is.
    fn page_content(&mut self, page: &Dict) -> Result<Arc<Scan>, Fail> {
        let refs_of = |items: &[Object]| -> Result<Vec<ObjRef>, Fail> {
            items.iter().map(|item| item.as_ref().ok_or_else(|| read_error("a /Contents entry is not a reference"))).collect()
        };
        let (refs, array): (Vec<ObjRef>, bool) = match page.get(b"Contents") {
            None => (Vec::new(), false),
            Some(Object::Ref(r)) => match &*self.get(*r)? {
                Object::Stream(_) => (vec![*r], false),
                Object::Array(items) => (refs_of(items)?, true),
                Object::Null => (Vec::new(), false),
                _ => return Err(read_error("/Contents is neither a stream nor an array")),
            },
            Some(Object::Array(items)) => (refs_of(items)?, true),
            Some(Object::Null) => (Vec::new(), false),
            Some(_) => return Err(read_error("/Contents is neither a stream nor an array")),
        };
        let key = (refs, array);
        if let Some(found) = self.contents.get(&key) {
            return Ok(found.clone());
        }
        let mut data = Vec::new();
        for r in &key.0 {
            let obj = self.get(*r)?;
            let Object::Stream(stream) = &*obj else { return Err(read_error(format!("content stream {r} is not a stream"))) };
            data.extend_from_slice(&self.decode(stream)?);
            if array {
                // As hayro joins them: at least one whitespace in between.
                data.push(b' ');
            }
        }
        let scan = Arc::new(self.scan(&data)?);
        self.contents.insert(key, scan.clone());
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

    /// What a content stream draws that is followed: `Do` and inline images, for the images'
    /// identities and the work a page asks of the interpreter. Anything the lexer cannot read is
    /// an error.
    fn scan(&mut self, data: &[u8]) -> Result<Scan, Fail> {
        let unreadable = |what: &str| read_error(format!("a content stream cannot be read: {what}"));
        let mut lx = Lexer::new(data, 0);
        // The last operand, and the one before it, when they are names.
        let mut name: Option<Vec<u8>> = None;
        let mut before: Option<Vec<u8>> = None;
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
                before = name.take();
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
                b"Do" => items.push(Item::Draw(name.take().ok_or_else(|| unreadable("Do without a name"))?)),
                b"BI" => {
                    let (digest, pixels, end) = self.inline_image(data, lx.pos)?;
                    items.push(Item::Inline(digest, pixels));
                    lx.pos = end;
                }
                b"scn" | b"SCN" => {
                    if let Some(pattern) = name.take() {
                        items.push(Item::Pattern(pattern));
                    }
                }
                b"BMC" => items.push(Item::Begin(None)),
                b"BDC" => items.push(Item::Begin(if before.as_deref() == Some(b"OC") { name.take() } else { None })),
                b"EMC" => items.push(Item::End),
                b"gs" => {
                    if let Some(state) = name.take() {
                        items.push(Item::State(state));
                    }
                }
                _ => {}
            }
            name = None;
            before = None;
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

    /// The first resource of `kind` named `name` in a resource chain (nearest first), as hayro
    /// looks it up: a reference.
    fn resource(&mut self, chain: &[Arc<Dict>], kind: &[u8], name: &[u8]) -> Result<Option<ObjRef>, Fail> {
        for level in chain {
            let Some(entries) = self.dict_of(level.get(kind), "a resource dictionary")? else { continue };
            if let Some(found) = entries.get(name) {
                return Ok(found.as_ref());
            }
        }
        Ok(None)
    }

    /// Follow what a content stream draws: images (hashed once each), forms (recursively) and
    /// tiling patterns' cells, with the optional content each lies in.
    fn draw(&mut self, items: &[Item], chain: &[Arc<Dict>], at: Context, drawn: &mut Drawn) -> Result<(), Fail> {
        // Marked content opened in this stream, each level hidden or not; it ends with the stream.
        let mut levels: Vec<bool> = Vec::new();
        for item in items {
            self.budget.uses()?;
            let hidden = levels.last().copied().unwrap_or(at.hidden);
            match item {
                Item::Begin(None) => levels.push(hidden),
                Item::Begin(Some(name)) => {
                    let group = self.resource(chain, b"Properties", name)?;
                    let off = match group {
                        Some(group) => self.optional_off(&Object::Ref(group))?,
                        None => false,
                    };
                    levels.push(hidden || off);
                }
                Item::End => {
                    levels.pop();
                }
                Item::Inline(digest, pixels) => {
                    drawn.images.push(*digest);
                    drawn.pixels = drawn.pixels.saturating_add(*pixels);
                    if !hidden && !at.masked {
                        require(&mut drawn.required, ImageKey::Inline, at.patterned);
                    }
                }
                Item::Draw(name) => {
                    let Some(r) = self.xobject(chain, name)? else { continue };
                    let obj = self.get(r)?;
                    let Object::Stream(stream) = &*obj else { continue };
                    let hidden = match stream.dict.get(b"OC") {
                        Some(oc) => hidden || self.optional_off(oc)?,
                        None => hidden,
                    };
                    match stream.dict.name(b"Subtype") {
                        Some(b"Image") => {
                            let (digest, _) = self.digest_ref(r, false)?;
                            let info = self.image_info(r, &stream.dict)?;
                            drawn.images.push(digest);
                            drawn.pixels = drawn.pixels.saturating_add(u64::from(info.width).saturating_mul(u64::from(info.height)));
                            if !hidden && !at.masked {
                                require(&mut drawn.required, ImageKey::Object(r), at.patterned);
                            }
                        }
                        Some(b"Form") => {
                            if at.depth >= self.budget.limits.form_depth {
                                return Err(Fail::TooComplex);
                            }
                            let scan = self.form_content(r, stream)?;
                            let key = sha256(&[b"form", &self.digest_without(&stream.dict, &FORM_KEY_LEFT_OUT)?, &scan.digest]);
                            drawn.forms.push((r, key));
                            if !hidden && !at.patterned && !at.masked {
                                drawn.required.forms.insert(r);
                            }
                            drawn.interpreted = drawn.interpreted.saturating_add(scan.len);
                            let own = self.dict_of(stream.dict.get(b"Resources"), "a form's /Resources")?.unwrap_or_default();
                            self.type3_procedures(&own, drawn)?;
                            let mut inner = Vec::with_capacity(chain.len().saturating_add(1));
                            inner.push(Arc::new(own));
                            inner.extend(chain.iter().cloned());
                            self.draw(&scan.items, &inner, Context { depth: at.depth.saturating_add(1), hidden, ..at }, drawn)?;
                        }
                        _ => {}
                    }
                }
                Item::Pattern(name) => {
                    let Some(r) = self.resource(chain, b"Pattern", name)? else { continue };
                    let obj = self.get(r)?;
                    // Shading patterns (dictionaries) draw no images; a tiling pattern is a stream.
                    let Object::Stream(stream) = &*obj else { continue };
                    if stream.dict.int(b"PatternType") != Some(1) || !drawn.patterns.insert(r) {
                        continue;
                    }
                    if at.depth >= self.budget.limits.form_depth {
                        return Err(Fail::TooComplex);
                    }
                    let scan = self.form_content(r, stream)?;
                    drawn.interpreted = drawn.interpreted.saturating_add(scan.len);
                    let own = self.dict_of(stream.dict.get(b"Resources"), "a pattern's /Resources")?.unwrap_or_default();
                    self.type3_procedures(&own, drawn)?;
                    let mut inner = Vec::with_capacity(chain.len().saturating_add(1));
                    inner.push(Arc::new(own));
                    inner.extend(chain.iter().cloned());
                    self.draw(&scan.items, &inner, Context { depth: at.depth.saturating_add(1), hidden, patterned: true, ..at }, drawn)?;
                }
                Item::State(name) => {
                    let Some(group) = self.soft_mask_group(chain, name)? else { continue };
                    if !drawn.masks.insert(group) {
                        continue;
                    }
                    let obj = self.get(group)?;
                    let Object::Stream(stream) = &*obj else { continue };
                    if at.depth >= self.budget.limits.form_depth {
                        return Err(Fail::TooComplex);
                    }
                    let scan = self.form_content(group, stream)?;
                    let key = sha256(&[b"form", &self.digest_without(&stream.dict, &FORM_KEY_LEFT_OUT)?, &scan.digest]);
                    drawn.forms.push((group, key));
                    drawn.interpreted = drawn.interpreted.saturating_add(scan.len);
                    let own = self.dict_of(stream.dict.get(b"Resources"), "a soft mask's /Resources")?.unwrap_or_default();
                    self.type3_procedures(&own, drawn)?;
                    let mut inner = Vec::with_capacity(chain.len().saturating_add(1));
                    inner.push(Arc::new(own));
                    inner.extend(chain.iter().cloned());
                    self.draw(&scan.items, &inner, Context { depth: at.depth.saturating_add(1), hidden, masked: true, ..at }, drawn)?;
                }
            }
        }
        Ok(())
    }

    /// The group of the soft mask the graphics state `name` sets (a form XObject), if it sets one:
    /// the state is looked up as hayro looks it up (the first level of the chain naming it).
    fn soft_mask_group(&mut self, chain: &[Arc<Dict>], name: &[u8]) -> Result<Option<ObjRef>, Fail> {
        for level in chain {
            let Some(states) = self.dict_of(level.get(b"ExtGState"), "/ExtGState")? else { continue };
            let Some(state) = states.get(name) else { continue };
            let Some(state) = self.dict_of(Some(state), "a graphics state")? else { return Ok(None) };
            let Some(mask) = state.get(b"SMask") else { return Ok(None) };
            let Object::Dict(mask) = &*self.resolve(mask)? else { return Ok(None) };
            return Ok(mask.get(b"G").and_then(Object::as_ref));
        }
        Ok(None)
    }

    /// The glyph procedures of the Type 3 fonts in `resources`' `/Font`, each by its name and
    /// decoded content.
    fn type3_procedures(&mut self, resources: &Dict, drawn: &mut Drawn) -> Result<(), Fail> {
        let Some(fonts) = self.dict_of(resources.get(b"Font"), "/Font")? else { return Ok(()) };
        for (_, font) in fonts.iter() {
            self.budget.tick()?;
            let keys = match font {
                Object::Ref(r) => match self.type3.get(r) {
                    Some(found) => found.clone(),
                    None => {
                        let keys = match &*self.get(*r)? {
                            Object::Dict(dict) => Arc::new(self.procedure_keys(dict)?),
                            _ => Arc::new(Vec::new()),
                        };
                        self.type3.insert(*r, keys.clone());
                        keys
                    }
                },
                Object::Dict(dict) => Arc::new(self.procedure_keys(dict)?),
                _ => continue,
            };
            drawn.type3.extend(keys.iter().copied());
        }
        Ok(())
    }

    fn procedure_keys(&mut self, font: &Dict) -> Result<Vec<Digest>, Fail> {
        if font.name(b"Subtype") != Some(b"Type3") {
            return Ok(Vec::new());
        }
        let Some(procedures) = self.dict_of(font.get(b"CharProcs"), "/CharProcs")? else { return Ok(Vec::new()) };
        let mut keys = Vec::with_capacity(procedures.len());
        for (name, procedure) in procedures.iter() {
            let obj = self.resolve(procedure)?;
            let Object::Stream(stream) = &*obj else { return Err(read_error("a Type 3 glyph procedure is not a stream")) };
            let data = self.decode(stream)?;
            self.budget.hashed(data.len())?;
            let length = (name.len() as u64).to_le_bytes();
            keys.push(sha256(&[b"type3", &length, name, &sha256(&[&data])]));
        }
        Ok(keys)
    }

    /// Whether the optional content `oc` (a group or a membership dictionary, as `/OC` or a
    /// marked-content property list gives it) is certainly off in the default configuration.
    fn optional_off(&mut self, oc: &Object) -> Result<bool, Fail> {
        let resolved = self.resolve(oc)?;
        let Object::Dict(dict) = &*resolved else { return Ok(false) };
        let config = self.optional.clone();
        let shown = match dict.name(b"Type") {
            Some(b"OCG") => match oc.as_ref() {
                Some(group) => self.group_shown(&config, group, dict)?,
                None => None,
            },
            Some(b"OCMD") => self.membership_shown(&config, dict)?,
            _ => None,
        };
        Ok(shown == Some(false))
    }

    /// A group's state: shown, hidden, or not certain (`None`).
    fn group_shown(&mut self, config: &OptionalContent, group: ObjRef, dict: &Dict) -> Result<Option<bool>, Fail> {
        if !config.viewing || config.uncertain.contains(&group) || dict.name(b"Type") != Some(b"OCG") {
            return Ok(None);
        }
        // A group not meant for viewing is not governed by the viewing configuration.
        let intent = match dict.get(b"Intent") {
            None => true,
            Some(intent) => match &*self.resolve(intent)? {
                Object::Name(name) => name.as_slice() == b"View",
                Object::Array(names) => names.iter().any(|name| name.as_name() == Some(b"View")),
                _ => false,
            },
        };
        if !intent {
            return Ok(None);
        }
        Ok(match (config.on.contains(&group), config.off.contains(&group)) {
            (true, true) => None,
            (true, false) => Some(true),
            (false, true) => Some(false),
            (false, false) => config.default,
        })
    }

    /// A membership dictionary's state (ISO 32000-2 §8.11.2.2), by its `/OCGs` and `/P`; one with
    /// a visibility expression (`/VE`) is not certain.
    fn membership_shown(&mut self, config: &OptionalContent, dict: &Dict) -> Result<Option<bool>, Fail> {
        if dict.contains(b"VE") {
            return Ok(None);
        }
        let groups = match dict.get(b"OCGs") {
            None => Vec::new(),
            Some(entry) => match &*self.resolve(entry)? {
                Object::Null => Vec::new(),
                Object::Array(groups) => groups.clone(),
                Object::Dict(_) => vec![entry.clone()],
                _ => return Ok(None),
            },
        };
        let mut states = Vec::with_capacity(groups.len());
        for group in &groups {
            let Object::Ref(r) = group else { return Ok(None) };
            let Some(dict) = self.get(*r)?.as_dict().cloned() else { return Ok(None) };
            states.push(self.group_shown(config, *r, &dict)?);
        }
        if states.is_empty() {
            return Ok(Some(true));
        }
        let any = |state: bool| states.contains(&Some(state));
        let all = |state: bool| states.iter().all(|s| *s == Some(state));
        let policy = match dict.get(b"P") {
            None => b"AnyOn".to_vec(),
            Some(p) => match &*self.resolve(p)? {
                Object::Name(name) => name.clone(),
                _ => return Ok(None),
            },
        };
        Ok(match policy.as_slice() {
            b"AllOn" if all(true) => Some(true),
            b"AllOn" if any(false) => Some(false),
            b"AnyOn" if any(true) => Some(true),
            b"AnyOn" if all(false) => Some(false),
            b"AnyOff" if any(false) => Some(true),
            b"AnyOff" if all(true) => Some(false),
            b"AllOff" if all(false) => Some(true),
            b"AllOff" if any(true) => Some(false),
            _ => None,
        })
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

    /// The default optional-content configuration (`/OCProperties /D`, ISO 32000-2 §8.11.4.3).
    fn optional_content(&mut self, catalog: &Dict) -> Result<OptionalContent, Fail> {
        let Some(properties) = self.dict_of(catalog.get(b"OCProperties"), "/OCProperties")? else {
            // No configuration: optional content is shown.
            return Ok(OptionalContent { default: Some(true), viewing: true, ..OptionalContent::default() });
        };
        let config = self.dict_of(properties.get(b"D"), "/OCProperties /D")?.unwrap_or_default();
        let refs = |items: Vec<Object>| items.iter().filter_map(Object::as_ref).collect::<BTreeSet<ObjRef>>();
        let listed = refs(self.array_of(properties.get(b"OCGs"), "/OCGs")?);
        let on = refs(self.array_of(config.get(b"ON"), "/ON")?);
        let mut off = refs(self.array_of(config.get(b"OFF"), "/OFF")?);
        let base = match config.get(b"BaseState") {
            None => Some(b"ON".to_vec()),
            Some(base) => self.resolve(base)?.as_name().map(<[u8]>::to_vec),
        };
        let default = match base.as_deref() {
            Some(b"ON") => Some(true),
            Some(b"OFF") => {
                off.extend(listed.difference(&on).copied());
                // A group the document does not list: whether the base state reaches it is not
                // certain.
                None
            }
            _ => None,
        };
        // Groups an automatic state (`/AS`) may switch when the document is viewed.
        let mut uncertain = BTreeSet::new();
        for usage in self.array_of(config.get(b"AS"), "/AS")? {
            if let Some(usage) = self.dict_of(Some(&usage), "an /AS entry")? {
                uncertain.extend(refs(self.array_of(usage.get(b"OCGs"), "/AS /OCGs")?));
            }
        }
        let viewing = match config.get(b"Intent") {
            None => true,
            Some(intent) => match &*self.resolve(intent)? {
                Object::Name(name) => matches!(name.as_slice(), b"View" | b"All"),
                Object::Array(names) => names.iter().any(|name| matches!(name.as_name(), Some(b"View" | b"All"))),
                _ => false,
            },
        };
        Ok(OptionalContent { off, on, uncertain, default, viewing })
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
        for level in &chain {
            self.type3_procedures(level, &mut drawn)?;
        }
        self.draw(&scan.items, &chain, Context { depth: 0, hidden: false, patterned: false, masked: false }, &mut drawn)?;
        drawn.images.sort_unstable();
        let (annots, annots_in_array) = self.annotations(node, widgets, annotation_ids)?;
        Ok(PageFacts {
            rotation,
            object: node.obj,
            content: scan.digest,
            resources,
            other,
            images: drawn.images,
            image_pixels: drawn.pixels,
            image_uses: Vec::new(),
            required: drawn.required,
            forms: drawn.forms,
            form_draws: Vec::new(),
            type3: drawn.type3,
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
                Some(r) => self.digest_ref(r, false)?.0,
                None => self.digest(&Object::Dict(dict.clone()))?,
            };
            let bare = self.digest_without(&dict, &[b"AP"])?;
            let rect = self.array_of(dict.get(b"Rect"), "/Rect")?;
            let rect = match rect.iter().map(Object::as_f64).collect::<Option<Vec<f64>>>().as_deref() {
                Some(&[x0, y0, x1, y1]) if [x0, y0, x1, y1].iter().all(|v| v.is_finite()) => Some([x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]),
                _ => None,
            };
            let subtype = dict.name(b"Subtype").unwrap_or_default().to_vec();
            let widget = subtype == b"Widget" && id.is_some_and(|id| widgets.contains(&id));
            let silent_popup = subtype == b"Popup" && !dict.contains(b"AP");
            let areas = if subtype == b"Redact" { self.marked_areas(&dict)? } else { Vec::new() };
            annots.push(Annot { id, subtype, digest, tracked: !widget && !silent_popup, areas, bare, rect });
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
                widget_digests.extend_from_slice(&self.digest_ref(kid_ref, false)?.0);
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

/// What [`read`] established about objects, kept for identifying the images hayro draws: which
/// objects were checked to sit where the cross-reference data says, the digests made, and the
/// image XObjects read.
pub(crate) struct Known {
    checked: HashMap<u32, u16>,
    object_streams: HashMap<u32, Arc<Vec<u32>>>,
    digests: HashMap<(ObjRef, bool), Digest>,
    images: HashMap<ObjRef, Arc<ImageInfo>>,
    /// Content streams' decoded data as `pdfcraft_cos` decodes it, by digest.
    decoded: HashMap<ObjRef, Digest>,
}

impl Known {
    /// The images hayro drew on a page, each identified by `pdfcraft_cos` (an image XObject is the
    /// object hayro names, read and digested here like any other; an inline image is its bytes
    /// as hayro's parser delimited them) and placed where hayro drew it. An object hayro names
    /// that is not an image here is an error: the readers disagree.
    pub fn image_uses(&mut self, doc: &Document, budget: &mut Budget, draws: &[ImageDraw]) -> Result<Vec<ImageUse>, Fail> {
        let mut reader = self.reader(doc, budget);
        // Inline images by the copies hayro's draws share (alive as long as `draws`): one digest
        // for the uses of an inline image in a form drawn many times.
        let mut inline = HashMap::new();
        let uses = draws.iter().map(|draw| reader.image_use(draw, &mut inline)).collect::<Result<Vec<_>, _>>();
        self.keep(reader);
        uses
    }
}

impl Known {
    /// Check that `pdfcraft_cos` reads what hayro read (see [`Resolved`]): every object at the
    /// same place (the same file offset with the same generation, or the same object stream and
    /// index), read here too with every check [`read`] makes, and every stream's data the same
    /// bytes. Any difference is an error: the two readers would not describe one document.
    pub fn agree(&mut self, doc: &Document, budget: &mut Budget, resolved: &Resolved) -> Result<(), Fail> {
        let disagree = |what: String| read_error(format!("readers disagree: {what}"));
        if resolved.conflicts > 0 {
            return Err(disagree("hayro read an object two ways".into()));
        }
        let data = doc.bytes().as_slice();
        let mut reader = self.reader(doc, budget);
        let mut result = Ok(());
        for ((num, generation), place) in &resolved.objects {
            let r = ObjRef::new(*num, *generation);
            let same = match (doc.xref_entry(*num), place) {
                (Some(XrefEntry::InFile { offset, generation: g }), ObjectPlace::InFile(at)) => offset == *at && g == *generation,
                (Some(XrefEntry::InStream { stream, index }), ObjectPlace::InStream { stream: s, index: i }) => {
                    stream == *s && index == *i && *generation == 0
                }
                _ => false,
            };
            if !same {
                result = Err(disagree(format!("hayro read object {r} from another place")));
                break;
            }
            if let Err(fail) = reader.get(r) {
                result = Err(fail);
                break;
            }
        }
        if result.is_ok() {
            // Content streams hayro interpreted (forms, patterns' cells, Type 3 glyph procedures):
            // decoded to the same data here.
            for ((num, generation), decoded) in &resolved.content {
                let r = ObjRef::new(*num, *generation);
                let theirs = match reader.budget.hashed(decoded.len()) {
                    Ok(()) => sha256(&[decoded]),
                    Err(fail) => {
                        result = Err(fail);
                        break;
                    }
                };
                let ours = match self.decoded.get(&r) {
                    Some(ours) => Ok(*ours),
                    None => reader.content_digest(r),
                };
                match ours {
                    Ok(ours) if ours == theirs => {
                        self.decoded.insert(r, ours);
                    }
                    Ok(_) => {
                        result = Err(disagree(format!("hayro decoded content stream {r} to other data")));
                        break;
                    }
                    Err(fail) => {
                        result = Err(fail);
                        break;
                    }
                }
            }
        }
        if result.is_ok() {
            for ((num, generation), (start, length)) in &resolved.streams {
                let r = ObjRef::new(*num, *generation);
                let read = reader.get(r);
                let theirs = start.checked_add(*length).and_then(|end| data.get(*start..end));
                let same = match (&read, theirs) {
                    (Ok(obj), Some(theirs)) => matches!(&**obj, Object::Stream(stream) if stream.raw.as_slice() == theirs),
                    _ => false,
                };
                if let Err(fail) = reader.budget.hashed(*length) {
                    result = Err(fail);
                    break;
                }
                if !same {
                    result = Err(disagree(format!("hayro read other data for stream {r}")));
                    break;
                }
            }
        }
        self.keep(reader);
        result
    }

    fn reader<'d, 'b>(&mut self, doc: &'d Document, budget: &'b mut Budget) -> Reader<'d, 'b> {
        Reader {
            doc,
            data: doc.bytes().as_slice(),
            budget,
            checked: std::mem::take(&mut self.checked),
            checking: HashSet::new(),
            object_streams: std::mem::take(&mut self.object_streams),
            digests: std::mem::take(&mut self.digests),
            stack: Vec::new(),
            contents: HashMap::new(),
            forms: HashMap::new(),
            images: std::mem::take(&mut self.images),
            type3: HashMap::new(),
            optional: Arc::default(),
        }
    }

    fn keep(&mut self, reader: Reader<'_, '_>) {
        self.checked = reader.checked;
        self.object_streams = reader.object_streams;
        self.digests = reader.digests;
        self.images = reader.images;
    }
}

impl Reader<'_, '_> {
    /// The digest of content stream `r`'s decoded data.
    fn content_digest(&mut self, r: ObjRef) -> Result<Digest, Fail> {
        let obj = self.get(r)?;
        let Object::Stream(stream) = &*obj else { return Err(read_error(format!("readers disagree: hayro interprets object {r} as content"))) };
        let data = self.decode(stream)?;
        self.budget.hashed(data.len())?;
        Ok(sha256(&[&data]))
    }

    fn image_use(&mut self, draw: &ImageDraw, inline: &mut HashMap<(usize, usize), Digest>) -> Result<ImageUse, Fail> {
        self.budget.uses()?;
        let (digest, image) = match &draw.source {
            ImageSource::Object { num, generation } => {
                let r = ObjRef::new(*num, *generation);
                let obj = self.get(r)?;
                let Object::Stream(stream) = &*obj else {
                    return Err(read_error(format!("readers disagree: hayro draws object {r} as an image")));
                };
                if stream.dict.name(b"Subtype") != Some(b"Image") {
                    return Err(read_error(format!("readers disagree: hayro draws object {r} as an image")));
                }
                let (digest, _) = self.digest_ref(r, false)?;
                (digest, Some(self.image_info(r, &stream.dict)?))
            }
            ImageSource::Inline { dict, data } => {
                let key = (dict.as_ptr() as usize, data.as_ptr() as usize);
                let digest = match inline.get(&key) {
                    Some(digest) => *digest,
                    None => {
                        self.budget.hashed(dict.len().saturating_add(data.len()))?;
                        let length = u64::try_from(dict.len()).unwrap_or(u64::MAX).to_le_bytes();
                        let digest = sha256(&[b"inline", &length, dict, data]);
                        inline.insert(key, digest);
                        digest
                    }
                };
                (digest, None)
            }
        };
        Ok(ImageUse { digest, matrix: draw.matrix, tiled: draw.tiled, image })
    }
}

/// Check that hayro drew what the page's content draws (see [`Required`]): each image as often as
/// the content draws it outside optional content that is off, each image of a tiling pattern's
/// cell at least once, and each form. Anything missing is an error: hayro did not draw it, so no
/// fact about it would be certain (another viewer may well draw it).
pub(crate) fn check_drawn(page: &PageFacts, content: &PageContent, number: usize) -> Result<(), Fail> {
    let missing = |what: String| Err(read_error(format!("page {number}: the interpreter did not draw {what} the content draws")));
    let mut plain: BTreeMap<ImageKey, usize> = BTreeMap::new();
    let mut any: BTreeSet<ImageKey> = BTreeSet::new();
    for draw in &content.images {
        let key = match &draw.source {
            ImageSource::Object { num, generation } => ImageKey::Object(ObjRef::new(*num, *generation)),
            ImageSource::Inline { .. } => ImageKey::Inline,
        };
        any.insert(key);
        if !draw.tiled {
            let count = plain.entry(key).or_default();
            *count = count.saturating_add(1);
        }
    }
    for (key, needed) in &page.required.plain {
        if plain.get(key).copied().unwrap_or(0) < *needed {
            return missing(describe(key));
        }
    }
    if let Some(key) = page.required.patterned.iter().find(|key| !any.contains(key)) {
        return missing(describe(key));
    }
    let forms: BTreeSet<ObjRef> = content.forms.iter().map(|form| ObjRef::new(form.num, form.generation)).collect();
    if let Some(form) = page.required.forms.iter().find(|form| !forms.contains(form)) {
        return missing(format!("form {form}"));
    }
    Ok(())
}

fn describe(key: &ImageKey) -> String {
    match key {
        ImageKey::Object(r) => format!("every use of image {r}"),
        ImageKey::Inline => "every inline image".into(),
    }
}

/// Read one file's structure. Fails on anything not read for certain.
pub(crate) fn read(doc: &Document, budget: &mut Budget) -> Result<(Structure, Known), Fail> {
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
        type3: HashMap::new(),
        optional: Arc::default(),
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
    reader.optional = Arc::new(reader.optional_content(&catalog)?);
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
    let known = Known {
        checked: reader.checked,
        object_streams: reader.object_streams,
        digests: reader.digests,
        images: reader.images,
        decoded: HashMap::new(),
    };
    Ok((Structure { pages, fields, form, catalog: entries, layers, sections }, known))
}
