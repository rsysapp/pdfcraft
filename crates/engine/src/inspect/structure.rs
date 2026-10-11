//! Embed build: one file's structure as `pdfcraft_cos` reads it, for the save inspector.
//!
//! Every object is read through [`Reader::get`], which checks that the object sits where the
//! cross-reference data says (header, generation, a stream's declared length, its position in an
//! object stream) and that `pdfcraft_cos` did not fall back to scanning the file for it. Objects
//! are compared by canonical SHA-256 digests that do not depend on object numbers (a rewrite
//! renumbers every object): references are replaced by the digest of what they point to.
//! A stream is its dictionary, with an indirect `/Filter` or `/DecodeParms` resolved, and its data:
//! decoded where hayro decoded the same stream to interpret it (a form XObject, a tiling pattern's
//! cell, a Type 3 glyph procedure, a soft mask's group it drew) and `pdfcraft_cos` decodes it to
//! the same bytes, so a save that only re-encodes one does not change it; everywhere else (images,
//! annotation appearances, glyph procedures no page draws, …) its encoded bytes and how they are
//! encoded, since no second reader checks what they decode to.

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
/// names, page labels, permissions, …) fails the inspection, since no fact reports it.
/// - `/Pages`, `/AcroForm`, `/OCProperties`, `/Outlines`: compared on their own (pages, form
///   fields, the layers, which count every page as changed, and the outline entry by entry);
/// - `/Metadata` (the XMP packet), `/StructTreeRoot` and `/MarkInfo` (tags), `/Version` (the PDF
///   version): nothing a viewer draws, and editors update them on ordinary saves (a redaction
///   cleans tags; a save may restamp metadata). Left out deliberately, so a changed `/Metadata`
///   can carry different document information with no trace in the result.
const CATALOG_REPORTED: [&[u8]; 8] = [b"Pages", b"AcroForm", b"OCProperties", b"Outlines", b"Metadata", b"StructTreeRoot", b"MarkInfo", b"Version"];

/// Stream-dictionary entries that say how the data is encoded, not what it is.
const CODING_KEYS: [&[u8]; 4] = [b"Filter", b"DecodeParms", b"Length", b"DL"];

/// A form's dictionary entries left out of its key (see [`PageFacts::forms`]): its data is keyed
/// on its own (see `Reader::content_key`), and so are its resources.
const FORM_KEY_LEFT_OUT: [&[u8]; 5] = [b"Filter", b"DecodeParms", b"Length", b"DL", b"Resources"];

/// The most outline entries read (see [`OutlineEntry`]).
const MAX_OUTLINE_ENTRIES: usize = 100_000;

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
    /// soft masks' groups): see [`FormUse`]. In a rewrite, the forms each drawn exactly once in
    /// the file (one page, one placement) are taken out after reading (see
    /// [`retain_shared_forms`]): the page's glyph, image and pixel rules judge them.
    pub forms: Vec<FormUse>,
    /// Each form hayro drew, where (for a rewrite's redaction check).
    pub form_draws: Vec<FormDraw>,
    /// The Type 3 fonts in the resources the content is drawn with (the page's, and each drawn
    /// form's, pattern's and mask group's own), each keyed with everything its glyph procedures
    /// draw (see `Reader::type3_key`).
    pub type3: BTreeSet<Digest>,
    /// Something the content draws with that hayro renders otherwise than other viewers, or
    /// that viewers may ignore (see `Reader::render_risk`), described: in a rewrite, a page
    /// drawn differently from its base that draws with one counts outside the redactions, since
    /// a change hidden in hayro's pixels may show in another viewer.
    pub render_risk: Option<String>,
    /// The image pixels drawn, summed over every use.
    pub image_pixels: u64,
    /// Content bytes the interpreter reads for the page (forms counted per use).
    pub interpreted: usize,
    pub annots: Vec<Annot>,
    /// How many `/Annots` entries are dictionaries (hayro must count the same).
    pub annots_in_array: usize,
    /// Filled from hayro: the upright page size in points, the glyphs drawn, the fingerprint.
    pub size: (f32, f32),
    pub glyphs: Glyphs,
    pub fingerprint: Digest,
}

/// One use of a form XObject in a page's content, as this scan reads it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FormUse {
    pub obj: ObjRef,
    /// Its dictionary but for its encoding and resources, and its data (see
    /// `Reader::content_key`): what it draws itself (what it draws with its resources is compared
    /// on its own).
    pub key: Digest,
    /// Drawn through `Do` in the page's content or in forms, not in a tiling pattern's cell or as
    /// a soft mask's group.
    pub placed: bool,
}

/// The glyphs a page draws, each named by its text, or (without a Unicode value) by its font and
/// glyph.
#[derive(Clone, Debug, Default)]
pub(crate) struct Glyphs {
    pub page: PageGlyphs,
    /// Glyph index → (the digest of its font's object, its glyph ID in the font), for the glyphs
    /// without text. The digest, not the object number: a rewrite renumbers objects.
    pub unnamed: HashMap<u32, (Digest, u32)>,
}

/// How a glyph is named: its text, or its font and glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GlyphName<'a> {
    Text(&'a [char]),
    Font(&'a Digest, u32),
}

impl Glyphs {
    pub fn len(&self) -> usize {
        self.page.len()
    }

    /// Glyph `index`'s name and em box.
    pub fn glyph(&self, index: usize) -> Option<(GlyphName<'_>, [f32; 4])> {
        let (text, rect) = self.page.glyph(index)?;
        let name = match u32::try_from(index).ok().and_then(|index| self.unnamed.get(&index)) {
            Some((font, id)) => GlyphName::Font(font, *id),
            None => GlyphName::Text(text),
        };
        Some((name, rect))
    }
}

/// One entry of the document outline (bookmarks), in the outline's order (depth first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutlineEntry {
    /// 0 for the outline's top level.
    pub depth: usize,
    /// `/Title`, as written (`None`: absent or not a string).
    pub title: Option<Vec<u8>>,
    /// Where it goes: `/Dest`, or a `/GoTo` action's `/D`.
    pub destination: Destination,
    /// Every other entry (colour, style, structure element, any other action) but the tree's own
    /// links and `/Count`: what no fact reports.
    pub rest: Digest,
    /// It has an action other than going to a destination in this file.
    pub acts: bool,
}

/// An outline entry's destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    None,
    /// An explicit destination to one of this file's pages: the page's index and the view (the
    /// rest of the array: the fit, position and zoom).
    Page(usize, Digest),
    /// A named destination (a name or a string), by its digest: what it names is the catalog's
    /// `/Dests` or `/Names`, compared whole.
    Named(Digest),
    /// Anything else (an array whose first entry is not one of this file's pages, a page given
    /// by number, …), by its digest.
    Other(Digest),
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
/// content draws, in every optional-content layer (hayro interprets them all as visible), but
/// not what a soft mask's group draws (drawn only when something is painted through the mask).
/// Images in tiling patterns' cells only once each (a cell is drawn once per paint that uses it).
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
    /// Its trailer names an encryption dictionary (`/Encrypt`).
    pub encrypted: bool,
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
    /// The outline's entries, depth first.
    pub outline: Vec<OutlineEntry>,
    /// The outline's root dictionary but for `/Type`, its links to the entries and `/Count` (an
    /// empty dictionary's digest when there is no outline).
    pub outline_root: Digest,
    /// Cross-reference sections, oldest first.
    pub sections: Vec<Section>,
}

/// The fingerprint pages are matched by: content, resources, images and text.
pub(crate) fn fingerprint(page: &PageFacts) -> Digest {
    let text: String = page.glyphs.page.chars.iter().collect();
    let images: Vec<u8> = page.images.iter().flatten().copied().collect();
    // Glyphs without text, by their place in the page's glyphs and their font and glyph.
    let mut unnamed: Vec<(&u32, &(Digest, u32))> = page.glyphs.unnamed.iter().collect();
    unnamed.sort_unstable();
    let unnamed: Vec<u8> =
        unnamed.into_iter().flat_map(|(index, (font, id))| index.to_le_bytes().into_iter().chain(*font).chain(id.to_le_bytes())).collect();
    sha256(&[&page.content, &page.resources, &images, text.as_bytes(), &unnamed])
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
/// and which other resources it names; not where (placements are hayro's, see
/// [`Known::image_uses`]).
enum Item {
    /// `/Name Do`.
    Draw(Vec<u8>),
    /// An inline image: its digest and pixel count.
    Inline(Digest, u64),
    /// `/Name scn` or `/Name SCN`: a pattern chosen as the colour.
    Pattern(Vec<u8>),
    /// `/Name gs`: a graphics state, which may set a soft mask.
    State(Vec<u8>),
    /// Any other resource named: a font (`Tf`), a shading (`sh`), a colour space (`cs`, `CS`),
    /// a property list (`BDC`, `DP`); the resource dictionary's key, and the name.
    Named(&'static [u8], Vec<u8>),
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
    forms: Vec<FormUse>,
    type3: BTreeSet<Digest>,
    render_risk: Option<String>,
    /// Tiling patterns and soft masks' groups already followed on this page, each with whether it
    /// was in a tiling pattern's cell and in a soft mask's group there (what it draws is required
    /// differently in each).
    patterns: BTreeSet<(ObjRef, bool, bool)>,
    masks: BTreeSet<(ObjRef, bool, bool)>,
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
    /// Inside a tiling pattern's cell.
    patterned: bool,
    /// Inside a soft mask's group: drawn only when something is painted through the mask, so
    /// nothing in it is required of hayro; its forms are still compared.
    masked: bool,
    /// The digest of the resource chain the content is drawn with (see `Reader::type3_key`).
    chain: Digest,
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
    /// Digests by object.
    digests: HashMap<ObjRef, Digest>,
    /// The references being digested, outermost first.
    stack: Vec<ObjRef>,
    /// Content streams scanned, by their references and whether `/Contents` is an array.
    contents: HashMap<(Vec<ObjRef>, bool), Arc<Scan>>,
    forms: HashMap<ObjRef, Arc<Scan>>,
    images: HashMap<ObjRef, Arc<ImageInfo>>,
    /// Type 3 fonts' keys, by the font's digest and the resource chain's (see `type3_key`).
    type3: HashMap<(Digest, Digest), Digest>,
    /// What a form, tiling pattern or soft mask's group a glyph procedure draws names, by the
    /// stream and the resource chain's digest (see `used_key`).
    used: HashMap<(ObjRef, Digest), Digest>,
    /// The content streams hayro decoded to interpret them, each with the digest of its data as
    /// hayro decoded it: keyed by their decoded data (see `content_key`).
    hayro_decoded: Arc<HashMap<ObjRef, Digest>>,
    /// The digest of each content stream's data as `pdfcraft_cos` decodes it, once computed.
    decoded: HashMap<ObjRef, Digest>,
    /// Streams whose `/Length` is not the length of their data (`pdfcraft_cos` found their end at
    /// `endstream`): certain only where hayro read the same data (see [`Known::agree`]), and
    /// tolerated only in objects of the base revision (see `kept_until`).
    misdeclared: BTreeSet<ObjRef>,
    /// How many bytes at the start of this file are the base's own: an object that lies wholly
    /// within them is byte for byte the base's object at the same offset (the base itself: all
    /// of it; an appended update: the base's length; a rewrite: none).
    kept_until: usize,
}

/// How far after a stream's data its `endobj` is looked for, to say where the object ends.
const OBJECT_END_SEARCH: usize = 256;

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
        // Where the data starts, as `pdfcraft_cos` takes it: after the end of line that follows
        // `stream` (CRLF, LF or a lone CR).
        let data_start = match (self.data.get(lx.pos), self.data.get(lx.pos.saturating_add(1))) {
            (Some(b'\r'), Some(b'\n')) => lx.pos.saturating_add(2),
            (Some(b'\n' | b'\r'), _) => lx.pos.saturating_add(1),
            _ => lx.pos,
        };
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
                    // `pdfcraft_cos` ended the data at `endstream`: a fact only where hayro read
                    // the same data (checked once hayro has read the file), and only for an
                    // object of the base revision. Other readers end such data elsewhere (at
                    // the first `endobj`, say), and the editor's writer always writes the exact
                    // length: an object the edited file brings must declare its length (R6b-30).
                    if !self.of_the_base(data_start.saturating_add(stream.raw.len())) {
                        return Err(read_error(format!("stream {r} does not have the length it declares, and is not the base's own object")));
                    }
                    self.misdeclared.insert(r);
                }
                // `pdfcraft_cos` applies an indirect /Filter or /DecodeParms as written directly:
                // so does the declaration compared with it, each reference read here.
                let mut declared = declared.clone();
                let mut failed = None;
                Stream::resolve_coding(&mut declared, |coding| match self.get(coding) {
                    Ok(obj) => Some((*obj).clone()),
                    Err(fail) => {
                        failed = Some(fail);
                        None
                    }
                });
                if let Some(fail) = failed {
                    return Err(fail);
                }
                if without_length(&declared) != without_length(&stream.dict) {
                    return Err(misplaced());
                }
                Ok(())
            }
            (header, loaded) if !is_stream && header == loaded => Ok(()),
            _ => Err(misplaced()),
        }
    }

    /// Whether the stream object whose data ends at `data_end` lies wholly within the bytes this
    /// file keeps of the base (see `kept_until`), through its `endobj`. An object whose end is
    /// not found nearby is not.
    fn of_the_base(&self, data_end: usize) -> bool {
        let window = self.data.get(data_end..data_end.saturating_add(OBJECT_END_SEARCH).min(self.data.len())).unwrap_or_default();
        let Some(at) = window.windows(6).position(|w| w == b"endobj") else { return false };
        data_end.saturating_add(at).saturating_add(6) <= self.kept_until
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
        let (data, made) = stream.decoded_strict_counted(self.budget.content_room()).map_err(|error| decode_error(&error))?;
        self.budget.content(made)?;
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
        self.encode(o, 0, None, &mut out)?;
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
        let encoded = self.encode(&obj, 0, Some(r), &mut out);
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

    /// The digest of content stream `r`'s data as `pdfcraft_cos` decodes it (strictly), once.
    fn decoded_digest(&mut self, r: ObjRef, stream: &Stream) -> Result<Digest, Fail> {
        if let Some(found) = self.decoded.get(&r) {
            return Ok(*found);
        }
        let digest = match self.forms.get(&r) {
            // Scanned already: its digest is its decoded data's.
            Some(scan) => scan.digest,
            None => {
                let data = self.decode(stream)?;
                self.budget.hashed(data.len())?;
                sha256(&[&data])
            }
        };
        self.decoded.insert(r, digest);
        Ok(digest)
    }

    /// Whether hayro decoded stream `r` to interpret it; if so, `pdfcraft_cos` must decode it to
    /// the same data, and that data's digest is returned.
    fn agreed_decoding(&mut self, r: ObjRef, stream: &Stream) -> Result<Option<Digest>, Fail> {
        let Some(theirs) = self.hayro_decoded.get(&r).copied() else { return Ok(None) };
        let ours = self.decoded_digest(r, stream)?;
        if ours != theirs {
            return Err(read_error(format!("readers disagree: hayro decoded content stream {r} to other data")));
        }
        Ok(Some(ours))
    }

    /// The key of stream `r`'s data (see the module documentation): decoded where hayro decoded
    /// it and the readers agree, else its encoded bytes with the entries saying how they are
    /// encoded (`/Filter`, `/DecodeParms`, `/DL`, as `pdfcraft_cos` resolved them).
    fn content_key(&mut self, r: ObjRef, stream: &Stream) -> Result<Digest, Fail> {
        if let Some(decoded) = self.agreed_decoding(r, stream)? {
            return Ok(sha256(&[b"decoded", &decoded]));
        }
        let coding: Dict = stream
            .dict
            .iter()
            .filter(|(key, _)| matches!(key.as_slice(), b"Filter" | b"DecodeParms" | b"DL"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let coding = self.digest(&Object::Dict(coding))?;
        self.budget.hashed(stream.raw.len())?;
        Ok(sha256(&[b"encoded", &coding, &sha256(&[&stream.raw])]))
    }

    /// Append the canonical encoding of `o` to `out`; returns the lowest reference-stack position
    /// a reference in it pointed back to. `own`: the object `o` is (a stream is only ever one).
    fn encode(&mut self, o: &Object, depth: usize, own: Option<ObjRef>, out: &mut Vec<u8>) -> Result<usize, Fail> {
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
                    lowest = lowest.min(self.encode(item, depth.saturating_add(1), None, out)?);
                }
            }
            Object::Dict(d) => lowest = self.encode_dict(d, depth, out)?,
            Object::Stream(stream) => {
                // Decoded where both readers decoded it alike; else as encoded. `/Length` says
                // nothing the data's digest does not.
                let decoded = match own {
                    Some(r) if depth == 0 => self.agreed_decoding(r, stream)?,
                    _ => None,
                };
                match decoded {
                    Some(decoded) => {
                        out.push(b'D');
                        let kept: Dict = stream
                            .dict
                            .iter()
                            .filter(|(key, _)| !CODING_KEYS.contains(&key.as_slice()))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        lowest = self.encode_dict(&kept, depth, out)?;
                        out.extend_from_slice(&decoded);
                    }
                    None => {
                        out.push(b'S');
                        let kept: Dict =
                            stream.dict.iter().filter(|(key, _)| key.as_slice() != b"Length").map(|(k, v)| (k.clone(), v.clone())).collect();
                        lowest = self.encode_dict(&kept, depth, out)?;
                        self.budget.hashed(stream.raw.len())?;
                        out.extend_from_slice(&sha256(&[&stream.raw]));
                    }
                }
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
            lowest = lowest.min(self.encode(value, depth.saturating_add(1), None, out)?);
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

    /// A stream's data, decoded strictly. Every filter of its chain may make only what the
    /// budget has left, all of them together, and what they made is charged.
    fn decode(&mut self, stream: &Stream) -> Result<Vec<u8>, Fail> {
        let (data, made) = stream.decoded_strict_counted(self.budget.content_room()).map_err(|error| decode_error(&error))?;
        self.budget.content(made)?;
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
                b"gs" => {
                    if let Some(state) = name.take() {
                        items.push(Item::State(state));
                    }
                }
                // The font is the operand before the size.
                b"Tf" => {
                    if let Some(font) = before.take() {
                        items.push(Item::Named(b"Font", font));
                    }
                }
                b"sh" | b"cs" | b"CS" | b"BDC" | b"DP" => {
                    let kind: &'static [u8] = match keyword {
                        b"sh" => b"Shading",
                        b"cs" | b"CS" => b"ColorSpace",
                        _ => b"Properties",
                    };
                    if let Some(resource) = name.take() {
                        items.push(Item::Named(kind, resource));
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

    /// The resource of `kind` named `name` in a resource chain (nearest first): the first level
    /// naming it, whatever it is.
    fn resource_entry(&mut self, chain: &[Arc<Dict>], kind: &[u8], name: &[u8]) -> Result<Option<Object>, Fail> {
        for level in chain {
            let Some(entries) = self.dict_of(level.get(kind), "a resource dictionary")? else { continue };
            if let Some(found) = entries.get(name) {
                return Ok(Some(found.clone()));
            }
        }
        Ok(None)
    }

    /// `chain` with `own` (a stream's `/Resources`, as hayro puts it in front of its parent's)
    /// first, and the new chain's digest: `own`'s, after `key`, the parent chain's.
    fn inner_chain(&mut self, own: Option<&Object>, what: &str, chain: &[Arc<Dict>], key: &Digest) -> Result<(Vec<Arc<Dict>>, Digest), Fail> {
        let dict = self.dict_of(own, what)?.unwrap_or_default();
        let digest = match own {
            Some(own) => self.digest(own)?,
            None => sha256(&[b"no resources"]),
        };
        let mut inner = Vec::with_capacity(chain.len().saturating_add(1));
        inner.push(Arc::new(dict));
        inner.extend(chain.iter().cloned());
        Ok((inner, sha256(&[&digest, key])))
    }

    /// A form's key (see [`FormUse::key`]).
    fn form_key(&mut self, r: ObjRef, stream: &Stream) -> Result<Digest, Fail> {
        let dict = self.digest_without(&stream.dict, &FORM_KEY_LEFT_OUT)?;
        let data = self.content_key(r, stream)?;
        Ok(sha256(&[b"form", &dict, &data]))
    }

    /// Follow what a content stream draws: images (hashed once each), forms (recursively), tiling
    /// patterns' cells and soft masks' groups, and the Type 3 fonts of each resource chain.
    fn draw(&mut self, items: &[Item], chain: &[Arc<Dict>], at: Context, drawn: &mut Drawn) -> Result<(), Fail> {
        for item in items {
            self.budget.uses()?;
            match item {
                Item::Named(..) => {}
                Item::Inline(digest, pixels) => {
                    drawn.images.push(*digest);
                    drawn.pixels = drawn.pixels.saturating_add(*pixels);
                    if !at.masked {
                        require(&mut drawn.required, ImageKey::Inline, at.patterned);
                    }
                }
                Item::Draw(name) => {
                    let Some(r) = self.xobject(chain, name)? else { continue };
                    let obj = self.get(r)?;
                    let Object::Stream(stream) = &*obj else { continue };
                    match stream.dict.name(b"Subtype") {
                        Some(b"Image") => {
                            let (digest, _) = self.digest_ref(r)?;
                            let info = self.image_info(r, &stream.dict)?;
                            drawn.images.push(digest);
                            drawn.pixels = drawn.pixels.saturating_add(u64::from(info.width).saturating_mul(u64::from(info.height)));
                            if !at.masked {
                                require(&mut drawn.required, ImageKey::Object(r), at.patterned);
                            }
                        }
                        Some(b"Form") => {
                            if at.depth >= self.budget.limits.form_depth {
                                return Err(Fail::TooComplex);
                            }
                            let scan = self.form_content(r, stream)?;
                            let key = self.form_key(r, stream)?;
                            self.group_risk(&stream.dict, drawn)?;
                            let placed = !at.patterned && !at.masked;
                            drawn.forms.push(FormUse { obj: r, key, placed });
                            if placed {
                                drawn.required.forms.insert(r);
                            }
                            drawn.interpreted = drawn.interpreted.saturating_add(scan.len);
                            let (inner, key) = self.inner_chain(stream.dict.get(b"Resources"), "a form's /Resources", chain, &at.chain)?;
                            self.type3_fonts(&inner, &key, drawn)?;
                            self.draw(&scan.items, &inner, Context { depth: at.depth.saturating_add(1), chain: key, ..at }, drawn)?;
                        }
                        _ => {}
                    }
                }
                Item::Pattern(name) => {
                    let Some(r) = self.resource(chain, b"Pattern", name)? else { continue };
                    let obj = self.get(r)?;
                    // Shading patterns (dictionaries) draw no images, but may set a graphics
                    // state; a tiling pattern is a stream.
                    if let Object::Dict(pattern) = &*obj
                        && let Some(state) = self.dict_of(pattern.get(b"ExtGState"), "a pattern's /ExtGState")?
                    {
                        self.state_risk(&state, drawn)?;
                    }
                    let Object::Stream(stream) = &*obj else { continue };
                    if stream.dict.int(b"PatternType") != Some(1) || !drawn.patterns.insert((r, at.patterned, at.masked)) {
                        continue;
                    }
                    if at.depth >= self.budget.limits.form_depth {
                        return Err(Fail::TooComplex);
                    }
                    let scan = self.form_content(r, stream)?;
                    drawn.interpreted = drawn.interpreted.saturating_add(scan.len);
                    let (inner, key) = self.inner_chain(stream.dict.get(b"Resources"), "a pattern's /Resources", chain, &at.chain)?;
                    self.type3_fonts(&inner, &key, drawn)?;
                    self.draw(&scan.items, &inner, Context { depth: at.depth.saturating_add(1), patterned: true, chain: key, ..at }, drawn)?;
                }
                Item::State(name) => {
                    if let Some(entry) = self.resource_entry(chain, b"ExtGState", name)?
                        && let Some(state) = self.dict_of(Some(&entry), "a graphics state")?
                    {
                        self.state_risk(&state, drawn)?;
                    }
                    let Some(group) = self.soft_mask_group(chain, name)? else { continue };
                    if !drawn.masks.insert((group, at.patterned, at.masked)) {
                        continue;
                    }
                    let obj = self.get(group)?;
                    let Object::Stream(stream) = &*obj else { continue };
                    if at.depth >= self.budget.limits.form_depth {
                        return Err(Fail::TooComplex);
                    }
                    let scan = self.form_content(group, stream)?;
                    let key = self.form_key(group, stream)?;
                    self.group_risk(&stream.dict, drawn)?;
                    drawn.forms.push(FormUse { obj: group, key, placed: false });
                    drawn.interpreted = drawn.interpreted.saturating_add(scan.len);
                    let (inner, key) = self.inner_chain(stream.dict.get(b"Resources"), "a soft mask's /Resources", chain, &at.chain)?;
                    self.type3_fonts(&inner, &key, drawn)?;
                    self.draw(&scan.items, &inner, Context { depth: at.depth.saturating_add(1), masked: true, chain: key, ..at }, drawn)?;
                }
            }
        }
        Ok(())
    }

    /// Note a graphics state hayro may render otherwise than other viewers: a blend mode other
    /// than `/Normal` (hayro composes every group as isolated, and does not knock out), or a
    /// transfer function (`/TR`, `/TR2`; PDF 2.0 deprecates them, and viewers may ignore them).
    fn state_risk(&mut self, state: &Dict, drawn: &mut Drawn) -> Result<(), Fail> {
        if drawn.render_risk.is_some() {
            return Ok(());
        }
        let normal = |name: &[u8]| matches!(name, b"Normal" | b"Compatible");
        if let Some(mode) = state.get(b"BM") {
            let plain = match &*self.resolve(mode)? {
                Object::Name(name) => normal(name),
                Object::Array(names) => names.iter().all(|name| name.as_name().is_some_and(normal)),
                _ => false,
            };
            if !plain {
                drawn.render_risk = Some("a blend mode other than /Normal".into());
                return Ok(());
            }
        }
        for key in [b"TR".as_slice(), b"TR2".as_slice()] {
            if let Some(transfer) = state.get(key) {
                let identity = matches!(&*self.resolve(transfer)?, Object::Name(name) if matches!(name.as_slice(), b"Identity" | b"Default"));
                if !identity {
                    drawn.render_risk = Some("a transfer function".into());
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Note a knockout transparency group (`/Group << /K true >>`), which hayro draws as an
    /// ordinary group.
    fn group_risk(&mut self, dict: &Dict, drawn: &mut Drawn) -> Result<(), Fail> {
        if drawn.render_risk.is_none()
            && let Some(group) = self.dict_of(dict.get(b"Group"), "a /Group")?
            && let Some(knockout) = group.get(b"K")
            && matches!(&*self.resolve(knockout)?, Object::Bool(true))
        {
            drawn.render_risk = Some("a knockout group".into());
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

    /// Key every Type 3 font a resource chain holds, drawn with that chain (see
    /// [`Reader::type3_key`]).
    fn type3_fonts(&mut self, chain: &[Arc<Dict>], key: &Digest, drawn: &mut Drawn) -> Result<(), Fail> {
        for level in chain {
            let Some(fonts) = self.dict_of(level.get(b"Font"), "/Font")? else { continue };
            for (_, font) in fonts.iter() {
                self.budget.tick()?;
                if let Some(found) = self.type3_key(font, chain, key, 0, &mut Vec::new())? {
                    drawn.type3.insert(found);
                }
            }
        }
        Ok(())
    }

    /// A Type 3 font's key (`None`: `font` is not a Type 3 font), drawn with resource chain
    /// `chain` (digest `key`): its dictionary and everything it references (its glyph procedures
    /// keyed like any stream, its own resources), and, procedure by procedure, every resource the
    /// procedure names as it is looked up when drawn (the procedure's own `/Resources`, the
    /// font's, then the chain's), with what each draws in turn: forms, tiling patterns, soft
    /// masks' groups and nested Type 3 fonts, recursively. A procedure draws unclipped wherever
    /// its glyph is, so any change to this key is a change of the page.
    fn type3_key(
        &mut self,
        font: &Object,
        chain: &[Arc<Dict>],
        key: &Digest,
        depth: usize,
        visiting: &mut Vec<Digest>,
    ) -> Result<Option<Digest>, Fail> {
        let resolved = self.resolve(font)?;
        let Object::Dict(dict) = &*resolved else { return Ok(None) };
        if dict.name(b"Subtype") != Some(b"Type3") {
            return Ok(None);
        }
        let font_digest = self.digest(font)?;
        if let Some(found) = self.type3.get(&(font_digest, *key)) {
            return Ok(Some(*found));
        }
        if visiting.contains(&font_digest) {
            // A procedure showing a glyph of a font it is drawing: named, not followed again.
            return Ok(Some(sha256(&[b"type3 again", &font_digest])));
        }
        if depth >= self.budget.limits.form_depth {
            return Err(Fail::TooComplex);
        }
        visiting.push(font_digest);
        let found = self.procedures_key(dict, &font_digest, chain, key, depth, visiting);
        visiting.pop();
        let found = found?;
        // Only a key computed from the top is the same whichever font is reached first.
        if visiting.is_empty() {
            self.type3.insert((font_digest, *key), found);
        }
        Ok(Some(found))
    }

    fn procedures_key(
        &mut self,
        font: &Dict,
        font_digest: &Digest,
        chain: &[Arc<Dict>],
        key: &Digest,
        depth: usize,
        visiting: &mut Vec<Digest>,
    ) -> Result<Digest, Fail> {
        let (fonts, fonts_key) = self.inner_chain(font.get(b"Resources"), "a Type 3 font's /Resources", chain, key)?;
        let procedures = self.dict_of(font.get(b"CharProcs"), "/CharProcs")?.unwrap_or_default();
        let mut names: Vec<(&Vec<u8>, &Object)> = procedures.iter().collect();
        names.sort_by(|a, b| a.0.cmp(b.0));
        let mut parts = vec![b"type3".to_vec(), font_digest.to_vec()];
        for (name, procedure) in names {
            self.budget.tick()?;
            let Object::Ref(r) = procedure else { return Err(read_error("a Type 3 glyph procedure is not a stream")) };
            let obj = self.get(*r)?;
            let Object::Stream(stream) = &*obj else { return Err(read_error("a Type 3 glyph procedure is not a stream")) };
            let scan = self.form_content(*r, stream)?;
            let (inner, inner_key) = self.inner_chain(stream.dict.get(b"Resources"), "a glyph procedure's /Resources", &fonts, &fonts_key)?;
            let used = self.used_key(&scan.items, &inner, &inner_key, depth.saturating_add(1), visiting)?;
            parts.push((name.len() as u64).to_le_bytes().to_vec());
            parts.push(name.clone());
            parts.push(used.to_vec());
        }
        let parts: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        Ok(sha256(&parts))
    }

    /// Every resource `items` name, looked up in `chain` (digest `key`), each by its digest, and
    /// what it draws in turn (see [`Reader::type3_key`]).
    fn used_key(&mut self, items: &[Item], chain: &[Arc<Dict>], key: &Digest, depth: usize, visiting: &mut Vec<Digest>) -> Result<Digest, Fail> {
        if depth >= self.budget.limits.form_depth {
            return Err(Fail::TooComplex);
        }
        let mut parts: Vec<u8> = Vec::new();
        for item in items {
            self.budget.uses()?;
            let (kind, name): (&[u8], &[u8]) = match item {
                Item::Draw(name) => (b"XObject", name),
                Item::Pattern(name) => (b"Pattern", name),
                Item::State(name) => (b"ExtGState", name),
                Item::Named(kind, name) => (kind, name),
                // Its data is the procedure's own.
                Item::Inline(..) => continue,
            };
            parts.extend_from_slice(&(kind.len() as u64).to_le_bytes());
            parts.extend_from_slice(kind);
            parts.extend_from_slice(&(name.len() as u64).to_le_bytes());
            parts.extend_from_slice(name);
            let entry = match item {
                Item::Draw(name) => self.xobject(chain, name)?.map(Object::Ref),
                _ => self.resource_entry(chain, kind, name)?,
            };
            let Some(entry) = entry else {
                parts.push(b'-');
                continue;
            };
            let digest = self.digest(&entry)?;
            parts.extend_from_slice(&digest);
            // What it draws, looked up through the chain it is drawn with.
            let drawn_stream = match (item, &entry) {
                (Item::Draw(_) | Item::Pattern(_), Object::Ref(r)) => Some(*r),
                (Item::State(name), _) => self.soft_mask_group(chain, name)?,
                _ => None,
            };
            if let Some(r) = drawn_stream {
                let obj = self.get(r)?;
                if let Object::Stream(stream) = &*obj
                    && (stream.dict.name(b"Subtype") == Some(b"Form") || stream.dict.int(b"PatternType") == Some(1))
                {
                    let used = match self.used.get(&(r, *key)) {
                        Some(found) => *found,
                        None => {
                            let scan = self.form_content(r, stream)?;
                            let (inner, inner_key) = self.inner_chain(stream.dict.get(b"Resources"), "a form's /Resources", chain, key)?;
                            let found = self.used_key(&scan.items, &inner, &inner_key, depth.saturating_add(1), visiting)?;
                            self.used.insert((r, *key), found);
                            found
                        }
                    };
                    parts.extend_from_slice(&used);
                }
            }
            if kind == b"Font"
                && let Some(nested) = self.type3_key(&entry, chain, key, depth.saturating_add(1), visiting)?
            {
                parts.extend_from_slice(&nested);
            }
        }
        Ok(sha256(&[&parts]))
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
        self.group_risk(&node.dict, &mut drawn)?;
        self.type3_fonts(&chain, &resources, &mut drawn)?;
        self.draw(&scan.items, &chain, Context { depth: 0, patterned: false, masked: false, chain: resources }, &mut drawn)?;
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
            render_risk: drawn.render_risk,
            image_uses: Vec::new(),
            required: drawn.required,
            forms: drawn.forms,
            form_draws: Vec::new(),
            type3: drawn.type3,
            interpreted: drawn.interpreted,
            annots,
            annots_in_array,
            size: (0.0, 0.0),
            glyphs: Glyphs::default(),
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

impl Reader<'_, '_> {
    // ── outline ─────────────────────────────────────────────────────────────────────────────

    /// The document outline's entries, depth first (ISO 32000-2 §12.3.3). `pages`: each page's
    /// object and index.
    fn outline(&mut self, catalog: &Dict, pages: &HashMap<ObjRef, usize>) -> Result<Vec<OutlineEntry>, Fail> {
        let Some(root) = self.dict_of(catalog.get(b"Outlines"), "/Outlines")? else { return Ok(Vec::new()) };
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        // Entries still to read, each with its depth: the next one on top.
        let mut stack: Vec<(Object, usize)> = root.get(b"First").map(|first| vec![(first.clone(), 0)]).unwrap_or_default();
        while let Some((item, depth)) = stack.pop() {
            self.budget.tick()?;
            let r = match item {
                Object::Null => continue,
                Object::Ref(r) => r,
                _ => return Err(read_error("an outline entry is not a reference")),
            };
            if !seen.insert(r) {
                return Err(read_error("the outline loops"));
            }
            if depth > MAX_TREE_DEPTH {
                return Err(read_error("the outline is too deep"));
            }
            if entries.len() >= MAX_OUTLINE_ENTRIES {
                return Err(Fail::TooComplex);
            }
            let dict = self.get(r)?.as_dict().cloned().ok_or_else(|| read_error(format!("outline entry {r} is not a dictionary")))?;
            entries.push(self.outline_entry(&dict, depth, pages)?);
            // The entry's children come before its next sibling.
            if let Some(next) = dict.get(b"Next") {
                stack.push((next.clone(), depth));
            }
            if let Some(first) = dict.get(b"First") {
                stack.push((first.clone(), depth.saturating_add(1)));
            }
        }
        Ok(entries)
    }

    fn outline_entry(&mut self, dict: &Dict, depth: usize, pages: &HashMap<ObjRef, usize>) -> Result<OutlineEntry, Fail> {
        let title = match dict.get(b"Title") {
            Some(title) => match &*self.resolve(title)? {
                Object::String(title) => Some(title.bytes.clone()),
                _ => None,
            },
            None => None,
        };
        const TREE: [&[u8]; 9] = [b"Title", b"Parent", b"Prev", b"Next", b"First", b"Last", b"Count", b"Dest", b"A"];
        let mut rest: Dict = dict.iter().filter(|(key, _)| !TREE.contains(&key.as_slice())).map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut destination = match dict.get(b"Dest") {
            Some(dest) => self.destination(dest, pages)?,
            None => Destination::None,
        };
        let mut acts = false;
        if let Some(action) = dict.get(b"A") {
            // A plain go-to action is a destination; any other action (or one that chains
            // others) is not described, only compared.
            let goto = match &*self.resolve(action)? {
                Object::Dict(a) if a.name(b"S") == Some(b"GoTo") && a.iter().all(|(key, _)| matches!(key.as_slice(), b"Type" | b"S" | b"D")) => {
                    a.get(b"D").cloned()
                }
                _ => None,
            };
            match goto {
                Some(dest) if !dict.contains(b"Dest") => destination = self.destination(&dest, pages)?,
                _ => {
                    rest.set(b"A".to_vec(), action.clone());
                    acts = true;
                }
            }
        }
        let rest = self.digest(&Object::Dict(rest))?;
        Ok(OutlineEntry { depth, title, destination, rest, acts })
    }

    /// An explicit destination by its page's index and its view; anything else by its digest.
    fn destination(&mut self, dest: &Object, pages: &HashMap<ObjRef, usize>) -> Result<Destination, Fail> {
        let resolved = self.resolve(dest)?;
        if let Object::Array(items) = &*resolved
            && let Some(Object::Ref(page)) = items.first()
            && let Some(index) = pages.get(page)
        {
            let view = self.digest(&Object::Array(items.iter().skip(1).cloned().collect()))?;
            return Ok(Destination::Page(*index, view));
        }
        Ok(match &*resolved {
            Object::Null => Destination::None,
            named @ (Object::Name(_) | Object::String(_)) => Destination::Named(self.digest(named)?),
            other => Destination::Other(self.digest(other)?),
        })
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

/// The file's cross-reference sections, oldest first (see [`Section`]).
pub(crate) fn sections(doc: &Document) -> Result<Vec<Section>, Fail> {
    let data = doc.bytes().as_slice();
    let sections = doc.revisions().iter().map(|revision| section(data, revision.xref_offset, revision.is_stream)).collect::<Result<Vec<_>, _>>()?;
    if sections.is_empty() {
        return Err(read_error("the file has no cross-reference data"));
    }
    Ok(sections)
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
    Ok(Section { offset, prev, hybrid: trailer.contains(b"XRefStm"), encrypted: trailer.contains(b"Encrypt") })
}

/// What [`read`] established about objects, kept for identifying the images hayro draws: which
/// objects were checked to sit where the cross-reference data says, the digests made, and the
/// image XObjects read.
pub(crate) struct Known {
    checked: HashMap<u32, u16>,
    object_streams: HashMap<u32, Arc<Vec<u32>>>,
    digests: HashMap<ObjRef, Digest>,
    images: HashMap<ObjRef, Arc<ImageInfo>>,
    /// Content streams' decoded data as `pdfcraft_cos` decodes it, by digest.
    decoded: HashMap<ObjRef, Digest>,
    /// The same as hayro decoded them (see `Reader::hayro_decoded`).
    hayro_decoded: Arc<HashMap<ObjRef, Digest>>,
    /// Streams whose `/Length` is not their data's (see `Reader::misdeclared`).
    misdeclared: BTreeSet<ObjRef>,
    /// See `Reader::kept_until`.
    kept_until: usize,
    /// Streams whose data hayro read as the same bytes.
    agreed: BTreeSet<ObjRef>,
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

    /// The digest of the object hayro names as a glyph's font (a glyph without text is named by
    /// it: see [`Glyphs`]).
    pub fn font_digest(&mut self, doc: &Document, budget: &mut Budget, font: ObjRef) -> Result<Digest, Fail> {
        let mut reader = self.reader(doc, budget);
        let digest = reader.digest_ref(font).map(|(digest, _)| digest);
        self.keep(reader);
        digest
    }

    /// Every object read here that the cross-reference data places in the file or in an object
    /// stream (the others read as null), in order.
    pub fn objects_read(&self, doc: &Document) -> Vec<(u32, u16)> {
        let mut read: Vec<(u32, u16)> = self
            .checked
            .iter()
            .filter(|(num, _)| matches!(doc.xref_entry(**num), Some(XrefEntry::InFile { .. } | XrefEntry::InStream { .. })))
            .map(|(num, generation)| (*num, *generation))
            .collect();
        read.sort_unstable();
        read
    }

    /// The first stream whose `/Length` is not its data's length that hayro did not read as the
    /// same data (`None`: there is none): what its data is depends on the reader.
    pub fn unconfirmed_length(&self) -> Option<ObjRef> {
        self.misdeclared.difference(&self.agreed).next().copied()
    }

    /// Check that `pdfcraft_cos` decodes every content stream hayro decoded to interpret it (see
    /// `Reader::hayro_decoded`) to the same data. Most were checked as they were keyed; this
    /// checks the rest.
    pub fn agree_decoded(&mut self, doc: &Document, budget: &mut Budget) -> Result<(), Fail> {
        let hayro_decoded = self.hayro_decoded.clone();
        let mut reader = self.reader(doc, budget);
        let mut result = Ok(());
        for (r, theirs) in hayro_decoded.iter() {
            let ours = match reader.decoded.get(r) {
                Some(ours) => Ok(*ours),
                None => reader.content_digest(*r),
            };
            match ours {
                Ok(ours) if ours == *theirs => {
                    reader.decoded.insert(*r, ours);
                }
                Ok(_) => {
                    result = Err(read_error(format!("readers disagree: hayro decoded content stream {r} to other data")));
                    break;
                }
                Err(fail) => {
                    result = Err(fail);
                    break;
                }
            }
        }
        self.keep(reader);
        result
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
            // Content streams hayro interpreted (forms, patterns' cells, Type 3 glyph procedures)
            // whose data is still in the log: decoded to the same data here.
            for ((num, generation), decoded) in &resolved.content {
                let r = ObjRef::new(*num, *generation);
                let theirs = match reader.budget.hashed(decoded.len()) {
                    Ok(()) => sha256(&[decoded]),
                    Err(fail) => {
                        result = Err(fail);
                        break;
                    }
                };
                let ours = match reader.decoded.get(&r) {
                    Some(ours) => Ok(*ours),
                    None => reader.content_digest(r),
                };
                match ours {
                    Ok(ours) if ours == theirs => {
                        reader.decoded.insert(r, ours);
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
                self.agreed.insert(r);
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
            used: HashMap::new(),
            hayro_decoded: self.hayro_decoded.clone(),
            decoded: std::mem::take(&mut self.decoded),
            misdeclared: std::mem::take(&mut self.misdeclared),
            kept_until: self.kept_until,
        }
    }

    fn keep(&mut self, reader: Reader<'_, '_>) {
        self.checked = reader.checked;
        self.object_streams = reader.object_streams;
        self.digests = reader.digests;
        self.images = reader.images;
        self.decoded = reader.decoded;
        self.misdeclared = reader.misdeclared;
    }
}

impl Reader<'_, '_> {
    /// The digest of content stream `r`'s decoded data.
    fn content_digest(&mut self, r: ObjRef) -> Result<Digest, Fail> {
        let obj = self.get(r)?;
        let Object::Stream(stream) = &*obj else { return Err(read_error(format!("readers disagree: hayro interprets object {r} as content"))) };
        self.decoded_digest(r, stream)
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
                let (digest, _) = self.digest_ref(r)?;
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

/// In a rewrite, take out of each page's forms those drawn exactly once in the file: one use in
/// one page's content (directly or in forms, not in a tiling pattern's cell or as a soft mask's
/// group) and one placement where hayro drew it. What such a form draws is part of its page's
/// glyphs, images and pixels, so the page's rules judge it: a redaction inside a page-wide form
/// (as imposition and merging tools write pages) changes the form, and must not count as a
/// change outside the boxes for that alone. A form drawn more than once keeps its key (one change
/// alters every use).
pub(crate) fn retain_shared_forms(pages: &mut [PageFacts]) {
    // Per form: uses in content (and whether all were placed), and placements hayro drew (a
    // tiled one counting as many).
    let mut uses: HashMap<ObjRef, (usize, bool)> = HashMap::new();
    let mut draws: HashMap<ObjRef, usize> = HashMap::new();
    for page in pages.iter() {
        for form in &page.forms {
            let entry = uses.entry(form.obj).or_insert((0, true));
            entry.0 = entry.0.saturating_add(1);
            entry.1 &= form.placed;
        }
        for draw in &page.form_draws {
            let count = draws.entry(ObjRef::new(draw.num, draw.generation)).or_default();
            *count = count.saturating_add(if draw.tiled { 2 } else { 1 });
        }
    }
    let once = |form: &FormUse| uses.get(&form.obj) == Some(&(1, true)) && draws.get(&form.obj) == Some(&1);
    for page in pages.iter_mut() {
        page.forms.retain(|form| !once(form));
    }
}

/// Check that hayro drew what the page's content draws (see [`Required`]): each image as often as
/// the content draws it (outside soft masks' groups), each image of a tiling pattern's cell at
/// least once, and each form. Anything missing is an error: hayro did not draw it, so no
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

/// Read one file's structure. Fails on anything not read for certain. `hayro_decoded`: the
/// content streams hayro decoded to interpret the file's pages, each with its decoded data's
/// digest (see `Reader::hayro_decoded`).
pub(crate) fn read(
    doc: &Document,
    budget: &mut Budget,
    hayro_decoded: Arc<HashMap<ObjRef, Digest>>,
    sections: Vec<Section>,
    kept_until: usize,
) -> Result<(Structure, Known), Fail> {
    let data = doc.bytes().as_slice();
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
        used: HashMap::new(),
        hayro_decoded: hayro_decoded.clone(),
        decoded: HashMap::new(),
        misdeclared: BTreeSet::new(),
        kept_until,
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
    let page_index: HashMap<ObjRef, usize> = nodes.iter().enumerate().map(|(index, node)| (node.obj, index)).collect();
    let outline = reader.outline(&catalog, &page_index)?;
    let root = reader.dict_of(catalog.get(b"Outlines"), "/Outlines")?.unwrap_or_default();
    let outline_root = reader.digest_without(&root, &[b"Type", b"First", b"Last", b"Count"])?;
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
        decoded: reader.decoded,
        hayro_decoded,
        misdeclared: reader.misdeclared,
        kept_until,
        agreed: BTreeSet::new(),
    };
    Ok((Structure { pages, fields, form, catalog: entries, layers, outline, outline_root, sections }, known))
}
