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
use pdfcraft_render::content::{FormDraw, ImageDraw, ImageSource, LayerMark, ObjectPlace, PageContent, PageGlyphs, Resolved};

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
/// annotation → page): never followed when digesting, at any depth (while they are references).
const BACK_REFERENCES: [&[u8]; 3] = [b"Parent", b"P", b"Popup"];

/// Page-dictionary entries compared on their own (or resolved through inheritance).
const PAGE_OWN_KEYS: [&[u8]; 7] = [b"Parent", b"Annots", b"Rotate", b"Contents", b"Resources", b"MediaBox", b"CropBox"];

// ── Every key of every dictionary drawn, by kind (the completeness audit) ───────────────────
//
// Each key of a dictionary the comparison reads is (a) compared, (b) ignored, for a stated
// reason, or (c) any change to it is a change: a key in neither of a kind's lists below is (c),
// so a key unknown here can never pass unseen. In an appended update every dictionary below is
// compared whole (a page's entries, its resources and everything they reach, by digest: any
// change is reported). In a rewrite a page's content is judged by its renders, glyphs and
// images, which compare what hayro draws: a (c) key of a dictionary a changed page draws with
// (one hayro does not apply, which another viewer may) makes the page a render risk, so the
// page counts outside the redactions on either side; text for assistive technology and copying
// in property lists may be taken away, never added, changed or moved (see [`PageFacts::text`]); the
// page dictionary's own (c) entries must stay as they were (see [`PageFacts::entries`]).

/// Page entries ignored (b): what no viewer draws on the page (`/Type`; `/LastModified`,
/// `/PieceInfo`, `/Metadata`: an application's data and the XMP packet, which a sanitising
/// rewrite removes; `/StructParents`: the tags' link, compared with the structure tree; `/ID`,
/// `/PZ`, `/Tabs`, `/B`, `/Dur`, `/Trans`, `/PresSteps`: navigation, zoom, tab order, article
/// beads and presentation timing). Every other entry not in [`PAGE_OWN_KEYS`] is (c).
const PAGE_IGNORED: [&[u8]; 12] =
    [b"Type", b"LastModified", b"PieceInfo", b"Metadata", b"StructParents", b"ID", b"PZ", b"Tabs", b"B", b"Dur", b"Trans", b"PresSteps"];
/// Page entries a rewrite may remove (and only remove): its actions (`/AA`) and its thumbnail
/// (`/Thumb`, which would show the page as it was); an editor's sanitising removes them.
const PAGE_REMOVABLE: [&[u8]; 2] = [b"AA", b"Thumb"];

/// The keys of each kind of dictionary a page draws with that its renders, glyphs and images
/// compare (a: hayro draws by them), or that are compared on their own (a), or ignored (b, with
/// the reason in the kind's comment). Any other key is (c).
fn drawn_keys(kind: &[u8]) -> &'static [&'static [u8]] {
    match kind {
        // A resource dictionary's categories hayro looks things up in; `/ProcSet` (b) is
        // obsolete and no viewer reads it.
        b"Resources" => &[b"Font", b"XObject", b"ExtGState", b"Pattern", b"Shading", b"ColorSpace", b"Properties", b"ProcSet"],
        // A font: what draws and names its glyphs (glyph shapes in the renders, text and place
        // in the glyphs).
        b"Font" => &[
            b"Type",
            b"Subtype",
            b"BaseFont",
            b"FirstChar",
            b"LastChar",
            b"Widths",
            b"FontDescriptor",
            b"Encoding",
            b"ToUnicode",
            b"DescendantFonts",
            b"CIDSystemInfo",
            b"DW",
            b"W",
            b"DW2",
            b"W2",
            b"CIDToGIDMap",
            b"FontBBox",
            b"FontMatrix",
            b"CharProcs",
            b"Resources",
            b"Name",
        ],
        // A graphics state: what hayro applies (blend modes and transfer functions are render
        // risks, see `state_risk`). Overprint, halftones, black generation and undercolour
        // removal are (c): hayro ignores them, other viewers may not.
        b"ExtGState" => &[
            b"Type", b"LW", b"LC", b"LJ", b"ML", b"D", b"RI", b"FL", b"SM", b"SA", b"BM", b"SMask", b"CA", b"ca", b"AIS", b"TK", b"Font", b"TR",
            b"TR2",
        ],
        // A form XObject: what draws it (its optional content is evaluated, see `Known::layer`);
        // ignored (b): its application data, metadata, tags' link, name, prepress (`/OPI`) and
        // measurement data, which no viewer draws. `/Ref` (a reference XObject) is (c).
        b"Form" => &[
            b"Type",
            b"Subtype",
            b"FormType",
            b"BBox",
            b"Matrix",
            b"Resources",
            b"Group",
            b"OC",
            b"Filter",
            b"DecodeParms",
            b"Length",
            b"DL",
            b"Metadata",
            b"PieceInfo",
            b"LastModified",
            b"StructParent",
            b"StructParents",
            b"Name",
            b"OPI",
            b"Measure",
            b"PtData",
        ],
        b"Pattern" => &[
            b"Type",
            b"PatternType",
            b"PaintType",
            b"TilingType",
            b"BBox",
            b"XStep",
            b"YStep",
            b"Resources",
            b"Matrix",
            b"Shading",
            b"ExtGState",
            b"Filter",
            b"DecodeParms",
            b"Length",
            b"DL",
        ],
        b"Shading" => &[
            b"ShadingType",
            b"ColorSpace",
            b"Background",
            b"BBox",
            b"AntiAlias",
            b"Coords",
            b"Domain",
            b"Function",
            b"Extend",
            b"Matrix",
            b"Decode",
            b"BitsPerCoordinate",
            b"BitsPerComponent",
            b"BitsPerFlag",
            b"VerticesPerRow",
            b"Filter",
            b"DecodeParms",
            b"Length",
            b"DL",
        ],
        // A marked-content property list: optional content's entries are evaluated (see
        // `Reader::evaluate_mark`); `/MCID` (b) links it to the structure tree, compared on its
        // own. Its text for assistive technology and copying is kept span by span (see
        // [`Span`]).
        b"Properties" => &[b"Type", b"OCGs", b"P", b"VE", b"Name", b"Intent", b"Usage", b"MCID", b"ActualText", b"Alt", b"E", b"Lang", b"T"],
        _ => &[],
    }
}

/// One use of a marked-content property list in a page's content (`BDC` or `DP`, the list named
/// in the resources or written in the content), in drawing order (R6b-41): what it is but for
/// its text, and its text for assistive technology and copying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    /// The tag and the list's entries but its text (its `/MCID` and optional content among them).
    pub identity: Digest,
    /// Each text entry (`/ActualText`, `/Alt`, `/E`, `/Lang`, `/T`) present: the key's place in
    /// [`STRUCT_TEXT_KEYS`] and the value's digest, in that key order.
    pub text: Vec<(u8, Digest)>,
}

/// Whether a rewrite kept a page's property-list text as a redaction may keep it, and if so how
/// many text entries it took away: the edited page's spans, in drawing order, are the base
/// page's with some spans taken away whole and some text taken away from others; none added,
/// changed or moved to another span (R6b-41).
fn spans_kept(base: &[Span], edited: &[Span]) -> Option<usize> {
    let mut rest = base.iter();
    let matched = edited
        .iter()
        .all(|span| rest.any(|candidate| candidate.identity == span.identity && span.text.iter().all(|entry| candidate.text.contains(entry))));
    let total = |spans: &[Span]| spans.iter().map(|span| span.text.len()).sum::<usize>();
    matched.then(|| total(base).saturating_sub(total(edited)))
}

/// Catalog entries the document-level check leaves out; a change to any other entry (actions,
/// names, page labels, permissions, …) fails the inspection, since no fact reports it.
/// - `/Pages`, `/AcroForm`, `/OCProperties`, `/Outlines`, `/Dests`: compared on their own (pages,
///   form fields, the layers, which count every page as changed, the outline entry by entry, and
///   named destinations name by name; `/Names` is compared whole but for its `/Dests` tree, which
///   is compared name by name too);
/// - `/StructTreeRoot` (tags): compared on its own (see [`StructFacts`]): a redaction cleans
///   the tags of what it removed;
/// - `/PageLabels`: compared page by page (see [`PageLabels`]): each page's label must follow
///   its page (R6b-46).
///
/// `/Metadata` (the XMP packet), `/MarkInfo` and `/Version` are compared like any other entry:
/// the document information a viewer or assistive technology reads, and what the file says it
/// uses. `/Metadata` may be removed by a rewrite (an editor's sanitising removes it), never
/// changed (see [`catalog_change`]).
const CATALOG_REPORTED: [&[u8]; 7] = [b"Pages", b"AcroForm", b"OCProperties", b"Outlines", b"Dests", b"StructTreeRoot", b"PageLabels"];

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
    /// For a link going to a destination in this file (its `/Dest`, or a plain go-to `/A`):
    /// that destination. It is left out of `digest` and `bare`, and compared on its own: it may
    /// change only by following its page (R6b-32).
    pub link: Option<Destination>,
    /// Follow-up 8 (P6-33): a `Redact` mark carrying overlay entries (see
    /// [`super::Inspection::marks_with_overlay`]).
    pub overlay: bool,
    /// Follow-up 9 (R6b-55, P6-40 (d)): the annotation this one belongs to: its `/IRT` (a
    /// reply), or for a pop-up its `/Parent`. A redaction removing that one removes this one too.
    pub reply_to: Option<ObjRef>,
    /// Follow-up 9 (R6b-54, N8): the dictionary (as `digest`) without its text (`/Contents`,
    /// `/RC`, `/T`), and that text by key: a redaction may take text holding a removed word away.
    pub untexted: Digest,
    pub texts: Vec<(Vec<u8>, Digest)>,
}

/// Follow-up 9 (R6b-54, N8): an annotation's text a redaction may clear.
pub(crate) const ANNOTATION_TEXT_KEYS: [&[u8]; 3] = [b"Contents", b"RC", b"T"];

/// What one page is, as far as the comparison goes.
pub(crate) struct PageFacts {
    /// `/Rotate` (inherited), 0, 90, 180 or 270.
    pub rotation: i64,
    /// `/MediaBox` and `/CropBox` (inherited; the crop box `None` when there is none), each four
    /// numbers, normalised: hayro must have inherited the same (see `inspect::read_side`).
    pub boxes: ([f64; 4], Option<[f64; 4]>),
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
    /// Each of `form_draws`' optional-content membership (R6b-29).
    pub form_layers: Vec<Option<LayerFact>>,
    /// The optional content the page's content is inside (R6b-36): a changed page is rendered
    /// in every combination of its groups' states.
    pub layers: PageLayers,
    /// The property lists the page's content draws with, in drawing order, each with its text
    /// for assistive technology and copying (see [`Span`]): a rewrite may take spans or text
    /// away, never add, change or move any.
    pub text: Vec<Span>,
    /// The levels of marked content the page's own content and the forms it draws open, in
    /// drawing order (see [`Level`], R6b-47).
    pub levels: Vec<Level>,
    /// hayro opened the same levels (each with its tag, inside the same level) around the glyphs
    /// it drew, and marked content was balanced in every content stream, for both: which
    /// glyph is in which level is then known (set in `inspect::read_side`).
    pub levels_agree: bool,
    /// A level closed that its content stream did not open, or left open at its end.
    pub levels_unbalanced: bool,
    /// Embed build (follow-up 7 audit, item 7): the entries of the resource dictionaries the
    /// page and what it draws use (by category and content, not name) that nothing looks up:
    /// a rewrite may keep the base's own unused entries, never leave one the base used, or add
    /// one (it could hold what the rewrite took away, drawn by nothing). The editor's standard
    /// fonts (a name-only Type 1 dictionary of the standard 14) are not counted.
    pub unused_resources: BTreeSet<Digest>,
    /// Follow-up 8 (R6b-51, I1): the standard fonts (name-only Type 1 dictionaries of the
    /// standard 14, holding no data) the resources offer and nothing draws, by name, and every
    /// entry the resources offer, by name and content: an unused standard font may stay only
    /// under a name the base page's resources had (its name could spell out text), or under
    /// one of the editor's names.
    pub unused_standard_fonts: BTreeSet<Digest>,
    pub offered_resources: BTreeSet<Digest>,
    /// The page dictionary's own entries (not in [`PAGE_OWN_KEYS`], nor `/Type`), by
    /// their values' digests: in a rewrite they must be the same, but for [`PAGE_REMOVABLE`]
    /// ones removed.
    pub entries: BTreeMap<Vec<u8>, Digest>,
    /// The Type 3 fonts in the resources the content is drawn with (the page's, and each drawn
    /// form's, pattern's and mask group's own), each keyed with everything its glyph procedures
    /// draw (see `Reader::type3_key`).
    pub type3: BTreeSet<Digest>,
    /// Something the content draws with that hayro renders otherwise than other viewers, or
    /// that viewers may ignore (see `Reader::render_risk`), described: in a rewrite, a page
    /// drawn differently from its base that draws with one counts outside the redactions, since
    /// a change hidden in hayro's pixels may show in another viewer.
    pub render_risk: Option<String>,
    /// Follow-up 8 (R6b-51, C1): a content stream the page draws (its own, a form's, a tiling
    /// pattern's cell, a soft mask's group) that a rewrite brings is not written as the
    /// redactor writes (see `operators`): what it holds that draws nothing is compared by
    /// nothing, so the page counts outside its redactions.
    pub content_violation: Option<String>,
    /// Follow-up 8 (R6b-51; the redaction re-review's C2): the resources the page draws with
    /// but forms and images (fonts, patterns, shadings, colour spaces, graphics states, property
    /// lists without their text), by content: a rewrite may draw only with what the base page
    /// drew with, since what they hold under the boxes no comparison reads.
    pub drawing_resources: BTreeSet<Digest>,
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

#[cfg(test)]
impl PageFacts {
    /// A page of the given glyphs and fingerprint, nothing else (for tests of the page pairing).
    pub fn for_tests(glyphs: Glyphs, fingerprint: Digest) -> Self {
        Self {
            rotation: 0,
            boxes: ([0.0, 0.0, 200.0, 300.0], None),
            object: ObjRef::new(1, 0),
            content: fingerprint,
            resources: [0; 32],
            other: [0; 32],
            images: Vec::new(),
            image_uses: Vec::new(),
            required: Required::default(),
            forms: Vec::new(),
            form_draws: Vec::new(),
            form_layers: Vec::new(),
            layers: PageLayers::default(),
            text: Vec::new(),
            levels: Vec::new(),
            levels_agree: true,
            levels_unbalanced: false,
            unused_resources: BTreeSet::new(),
            unused_standard_fonts: BTreeSet::new(),
            offered_resources: BTreeSet::new(),
            entries: BTreeMap::new(),
            type3: BTreeSet::new(),
            render_risk: None,
            content_violation: None,
            drawing_resources: BTreeSet::new(),
            image_pixels: 0,
            interpreted: 0,
            annots: Vec::new(),
            annots_in_array: 0,
            size: (200.0, 300.0),
            glyphs,
            fingerprint,
        }
    }
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
    /// Glyph index → its optional-content membership, for the glyphs drawn inside optional
    /// content: part of a glyph's identity (R6b-29).
    pub layers: HashMap<u32, LayerFact>,
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

    /// Glyph `index`'s optional-content membership (`None`: it is not inside any).
    pub fn layer(&self, index: usize) -> Option<&LayerFact> {
        u32::try_from(index).ok().and_then(|index| self.layers.get(&index))
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
    /// An explicit destination whose page is given by its number (`[2 /Fit]`): PDFium and pdf.js
    /// read a whole number there as a page index in this file (R6b-43). The index, which names
    /// one of this file's pages, and the view, as for `Page`. Kept apart from `Page`: a
    /// destination may follow its page, not change how it names it.
    Index(usize, Digest),
    /// An explicit destination whose page is a reference that names no page of this file (a page
    /// deleted and freed, or an object outside the page tree), or a number that names none (below
    /// 0, or past the last page): it goes nowhere. The view, as for `Page`.
    Unpaged(Digest),
    /// An explicit destination whose page is `null` (R6b-37: an editor writes a deleted page's
    /// named destination so): it goes nowhere. The view, as for `Page`. Kept apart from
    /// `Unpaged`: only a destination whose base page was deleted may become one.
    NullPage(Digest),
    /// A named destination (a name or a string), by its digest: what it names is the catalog's
    /// `/Dests` or the `/Names /Dests` tree, compared name by name (see
    /// [`Structure::named_destinations`]).
    Named(Digest),
    /// Anything else (an array whose first entry is neither a page, a page number nor `null`, …),
    /// by its digest.
    Other(Digest),
}

/// A named destination's name: where it is defined (the catalog's `/Dests` dictionary, or the
/// `/Names /Dests` tree) and the name's bytes.
pub(crate) type NamedKey = (NameSource, Vec<u8>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum NameSource {
    Dests,
    Names,
}

/// The named destinations by name, and the name tree's nodes' other entries (see
/// `Reader::named_destinations`).
type NamedDestinations = (BTreeMap<NamedKey, Destination>, Vec<(Vec<u8>, Digest)>);

/// The most nodes and entries a name tree may have.
const MAX_NAME_TREE_ENTRIES: usize = 100_000;

/// The page labels (the catalog's `/PageLabels`: what a viewer shows for a page, "iii", "A-2"),
/// page by page (R6b-46).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PageLabels {
    /// Each page's label, in page order: `None` where no range covers the page (and for every
    /// page of a file without labels: a viewer shows its number).
    Read(Vec<Option<PageLabel>>),
    /// A tree or a label dictionary readers could read differently (keys out of order, `/Limits`
    /// that do not bound a node, a style no reader shares, a start below 1, an entry no label
    /// has, …): compared whole, by the catalog entry's digest.
    Unreadable(Digest),
}

/// One page's label as every viewer forms it: the range's numbering style, its prefix and the
/// page's number in the range (its `/St` plus the page's place in the range). Equal labels show
/// alike in any viewer, however the ranges that give them are written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PageLabel {
    /// `/S`: `D`, `R`, `r`, `A` or `a`; 0 for a prefix alone.
    pub style: u8,
    /// `/P`'s bytes (`None`: no prefix, or an empty one).
    pub prefix: Option<Digest>,
    /// The number shown (0 for a prefix alone, which shows none).
    pub number: u64,
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
    /// Its optional-content membership (R6b-29).
    pub layer: Option<LayerFact>,
}

/// An image XObject, for a comparison of its samples.
#[derive(Debug)]
pub(crate) struct ImageInfo {
    pub obj: ObjRef,
    /// The dictionary without its filters, decode parameters and length, its masks (compared
    /// sample by sample on their own) and what a redaction takes off a cleared copy (follow-up
    /// 9, R6b-54: compared as [`ImageInfo::extras`]): what the samples mean.
    pub meaning: Digest,
    pub width: u32,
    pub height: u32,
    /// Follow-up 9 (R6b-54, N6): its soft mask (`/SMask`) and its stencil mask (`/Mask`, an
    /// image mask), by object, with what the mask's samples mean (its dictionary without its
    /// filters, decode parameters and length: a mask read otherwise, by its `/Decode`, depth or
    /// `/Matte`, is another mask whatever its bytes); a colour-key `/Mask` is part of `meaning`.
    pub masks: [Option<(ObjRef, Digest)>; 2],
    /// Follow-up 9 (R6b-54, N6): its alternates, XMP packet, private and prepress data, which a
    /// redaction may only take away (`None`: it has none).
    pub extras: Option<Digest>,
}

/// Follow-up 9 (R6b-54, N6): what a redaction takes off a cleared image's copy.
pub(crate) const IMAGE_EXTRAS: [&[u8]; 5] = [b"Alternates", b"Metadata", b"PieceInfo", b"LastModified", b"OPI"];

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
    /// Follow-up 9 (R6b-54, N4): the same without `/XFA` either, and whether it has `/XFA`.
    pub form_without_xfa: Option<Digest>,
    pub xfa: bool,
    /// Follow-up 9 (R6b-54, N8), rewrites: each optional-content group's `/Name` (its digest),
    /// in `/OCGs` order (compared apart from the groups, which a rewrite digests without it).
    pub layer_names: Vec<Option<(Digest, bool)>>,
    /// Each terminal field's full name, in the order `/Fields` and the fields' `/Kids` give.
    pub field_order: Vec<String>,
    /// Catalog entries not reported otherwise, by key.
    pub catalog: BTreeMap<Vec<u8>, Digest>,
    /// `/OCProperties`: which layers show.
    pub layers: Option<Digest>,
    /// The outline's entries, depth first.
    pub outline: Vec<OutlineEntry>,
    /// The outline's root dictionary but for `/Type`, its links to the entries and `/Count` (an
    /// empty dictionary's digest when there is no outline).
    pub outline_root: Digest,
    /// Named destinations: the catalog's `/Dests` dictionary (by name) and the `/Names /Dests`
    /// name tree (by string), each name's destination (a dictionary value's `/D`; one with other
    /// entries as a whole, as `Other`).
    pub named_destinations: BTreeMap<NamedKey, Destination>,
    /// Cross-reference sections, oldest first.
    pub sections: Vec<Section>,
    /// The structure tree (see [`StructFacts`]).
    pub structure_tree: StructFacts,
    /// Where the structure tree's marked content is (see [`StructLinks`]).
    pub structure_links: StructLinks,
    /// The named destinations' tree nodes' entries other than `/Kids`, `/Names` and `/Limits`,
    /// in the tree's order: no fact reports them, so they may not change.
    pub name_tree_keys: Vec<(Vec<u8>, Digest)>,
    /// The stream objects read whose `/Length` is not their data's (both readers read the same
    /// data), each with the offset the cross-reference data names for it: in the base, the only
    /// objects an appended update may keep with such a length (see [`Tolerated`]).
    pub misdeclared: BTreeSet<(ObjRef, u64)>,
    /// The document information (the trailer's `/Info`): each key and its value's digest (empty
    /// when there is none), see `Reader::document_information` (R6b-42).
    pub info: BTreeMap<Vec<u8>, Digest>,
    /// An object read for a fact holds an explicit destination that gives its page by number
    /// (`[2 /Fit]`) where no destination rule follows it: a catalog's `/OpenAction`, an action
    /// chained after another, a form field's action, a named destination with other entries, …
    /// Compared by digest, it would go to another page, unseen, once pages are deleted, moved or
    /// inserted (R6b-43). Filled in after reading (see `inspect::read_side`).
    pub numbered_destinations: bool,
    /// The page labels, page by page (R6b-46).
    pub page_labels: PageLabels,
    /// The catalog's `/PageLabels` entry's digest (`None`: there is none).
    pub page_labels_digest: Option<Digest>,
    /// Follow-up 8 (R6b-51, M5): the page tree nodes' entries but their kids, count, parent and
    /// type (see [`NodeEntries`]). Those no page takes are compared nowhere else.
    pub page_tree_entries: NodeEntries,
    /// Follow-up 8 (R6b-51, I1): the page labels' ranges starting past the last page (their
    /// label dictionaries' digests, sorted), which label no page; and every range's.
    pub labels_past_the_end: Vec<Digest>,
    pub label_values: Vec<Digest>,
    /// Follow-up 8 (R6b-51, C1): the content streams read and their marked content, gathered
    /// for the base of a rewrite (see [`ContentRule::Gather`]).
    pub content_base: ContentBase,
    /// Follow-up 8 (P6-33): each terminal field's widgets that are on a page: (page index, its
    /// `/Rect` normalised).
    pub field_places: BTreeMap<String, Vec<WidgetPlace>>,
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

/// The first catalog entry (not reported otherwise) that differs between the files: in a rewrite
/// (`base_kept` false), `/Metadata` gone is no difference (an editor's sanitising removes it).
pub(crate) fn catalog_change(before: &Structure, after: &Structure, base_kept: bool) -> Option<String> {
    let keys: BTreeSet<&Vec<u8>> = before.catalog.keys().chain(after.catalog.keys()).collect();
    keys.into_iter()
        .find(|key| {
            let (was, is) = (before.catalog.get(*key), after.catalog.get(*key));
            was != is && !(!base_kept && matches!(key.as_slice(), b"Metadata" | b"NeedsRendering") && is.is_none())
        })
        .map(|key| String::from_utf8_lossy(key).into_owned())
}

/// The document's structure tree (tags), as far as the comparison goes. Its elements are taken
/// in the tree's order (depth first through `/K`), each named by its place in that order and
/// its parent's place (R6b-44). Kept: the text each element gives assistive technology and
/// copying (`/ActualText`, `/Alt`, `/E`, `/Lang`, `/T`), element by element; each marked-content
/// kid (an MCID) with its element, in order; a digest of everything (each element's entries but
/// its kids, parent and page, with its parent's place and its marked-content kids; the root's
/// but for its parent tree and ID tree); and the same without the text and the marked-content
/// kids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StructFacts {
    /// (element, the key's place in [`STRUCT_TEXT_KEYS`], the value's digest), in the tree's
    /// order and, within an element, in that key order.
    pub text: Vec<(u32, u8, Digest)>,
    /// (element, or [`STRUCT_ROOT`] for a kid of the root, the MCID), in the tree's order.
    pub marked: Vec<(u32, i64)>,
    /// (element, the digest of the object an object reference names), in the tree's order.
    pub objects: Vec<(u32, Digest)>,
    pub digest: Digest,
    /// The same without the text and the marked-content kids: what a rewrite may not change
    /// (a redaction takes away text and the marked content it removed).
    pub shape: Digest,
}

/// Where the structure tree's elements and marked content are (R6b-47): with the shape the same
/// in both files (see [`tags_kept`]), the base's say which elements a glyph belongs to.
#[derive(Clone, Debug, Default)]
pub(crate) struct StructLinks {
    /// Each element's parent (by its place in the tree's order, as [`StructFacts::text`] names
    /// it; [`STRUCT_ROOT`]: the root), in that order.
    pub parents: Vec<u32>,
    /// Each marked-content kid (an MCID, or a marked-content reference), in the tree's order.
    pub kids: Vec<MarkedKid>,
    /// Each object reference (`/Type /OBJR`): the element it belongs to and the object it
    /// names (an annotation), in the tree's order (follow-up 7 audit, item 10).
    pub objects: Vec<(u32, ObjRef)>,
    /// Follow-up 8 (R6b-51, I2): each element's page (its `/Pg`, else the nearest ancestor's),
    /// in the tree's order, and each object reference's (its own `/Pg`, else its element's),
    /// in the order of [`StructLinks::objects`].
    pub element_pages: Vec<Option<ObjRef>>,
    pub object_pages: Vec<Option<ObjRef>>,
}

/// One marked-content kid of the structure tree (R6b-47).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MarkedKid {
    /// The element it belongs to ([`STRUCT_ROOT`]: the root).
    pub element: u32,
    /// The page it is on: a marked-content reference's own `/Pg`, else its element's or the
    /// nearest ancestor's (`None`: none names one).
    pub page: Option<ObjRef>,
    /// The form XObject whose content holds it (a marked-content reference's `/Stm`; `None`:
    /// the page's own content).
    pub form: Option<ObjRef>,
    pub mcid: i64,
}

/// Whether a rewrite kept the structure tree as a redaction may keep it, and if so how many
/// text entries it took away: the same elements in the same places (the shape), each with its
/// text but for entries taken away (none added, changed or moved to another element), and each
/// element's marked-content kids in their order but for kids taken away (R6b-44).
pub(crate) fn tags_kept(base: &StructFacts, edited: &StructFacts) -> Option<usize> {
    let kept = base.shape == edited.shape
        && subsequence(&edited.text, &base.text)
        && subsequence(&edited.marked, &base.marked)
        && subsequence(&edited.objects, &base.objects);
    kept.then(|| base.text.len().saturating_sub(edited.text.len()))
}

/// Follow-up 8 (R6b-51, I2): whether every element, object reference and marked-content kid of
/// the edited structure tree is on the page its base counterpart was on (each kid's below), followed through the pages' pairing (`pairs`: base page, edited
/// page), and in the same form (the same object in an appended update, which keeps numbers; a
/// form or none alike in a rewrite). A kid's page is its own `/Pg`, else its element's or the
/// nearest ancestor's: moved, the tree gives assistive technology and copying another page's
/// text. A base kid on a deleted page may name no page of the edited file. Kids are aligned as
/// [`tags_kept`] aligns them (each edited kid with the next base kid of the same element and
/// MCID on the page it follows).
pub(crate) fn marked_pages_followed(
    before: &Structure,
    after: &Structure,
    pairs: &[(usize, usize)],
    base_kept: bool,
    budget: &mut Budget,
) -> Result<bool, Fail> {
    let index =
        |structure: &Structure| -> HashMap<ObjRef, usize> { structure.pages.iter().enumerate().map(|(at, page)| (page.object, at)).collect() };
    let (base_pages, edited_pages) = (index(before), index(after));
    let followed: HashMap<usize, usize> = pairs.iter().copied().collect();
    let follows = |base: &MarkedKid, edited: &MarkedKid| {
        let page = match (base.page.and_then(|page| base_pages.get(&page)), edited.page.and_then(|page| edited_pages.get(&page))) {
            (Some(old), Some(new)) => followed.get(old) == Some(new),
            (Some(old), None) => !followed.contains_key(old),
            (None, None) => true,
            (None, Some(_)) => false,
        };
        let form = if base_kept { base.form == edited.form } else { base.form.is_some() == edited.form.is_some() };
        page && form
    };
    // Every element, by its place (the shapes are the same in both files), and every object
    // reference, aligned as the tags are.
    let on_page = |base: Option<ObjRef>, edited: Option<ObjRef>| {
        follows(&MarkedKid { element: 0, page: base, form: None, mcid: 0 }, &MarkedKid { element: 0, page: edited, form: None, mcid: 0 })
    };
    let (base_links, edited_links) = (&before.structure_links, &after.structure_links);
    if base_links.element_pages.len() != edited_links.element_pages.len() {
        return Ok(false);
    }
    for (base, edited) in base_links.element_pages.iter().zip(&edited_links.element_pages) {
        budget.tick()?;
        if !on_page(*base, *edited) {
            return Ok(false);
        }
    }
    let base_objects: Vec<((u32, Digest), Option<ObjRef>)> =
        before.structure_tree.objects.iter().copied().zip(base_links.object_pages.iter().copied()).collect();
    let mut rest = base_objects.iter();
    for (object, page) in after.structure_tree.objects.iter().zip(&edited_links.object_pages) {
        budget.tick()?;
        if !rest.by_ref().any(|(candidate, base_page)| candidate == object && on_page(*base_page, *page)) {
            return Ok(false);
        }
    }
    let mut rest = before.structure_links.kids.iter();
    for kid in &after.structure_links.kids {
        budget.tick()?;
        let mut matched = false;
        for candidate in rest.by_ref() {
            budget.tick()?;
            if candidate.element == kid.element && candidate.mcid == kid.mcid && follows(candidate, kid) {
                matched = true;
                break;
            }
        }
        if !matched {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The digest a destination naming `name` (a name, or a string) has (see `Reader::encode`).
fn name_digest(name: &[u8], string: bool) -> Digest {
    let mut out = vec![if string { b's' } else { b'/' }];
    out.extend_from_slice(&(name.len() as u64).to_le_bytes());
    out.extend_from_slice(name);
    sha256(&[&out])
}

/// Follow-up 8 (R6b-50): the named destinations a redaction renamed (a name holding a removed
/// word, written again as `PCRedactedDest` and a number, the destination the same), paired in
/// order: the edited file's names, and every destination naming them (bookmarks', links'), are
/// read back as the base's, so the comparisons that follow judge them as unchanged. How many.
pub(crate) fn rename_destinations(before: &Structure, after: &mut Structure) -> usize {
    let opaque = |name: &[u8]| name.strip_prefix(b"PCRedactedDest").is_some_and(|digits| !digits.is_empty() && digits.iter().all(u8::is_ascii_digit));
    let gone: Vec<&NamedKey> = before.named_destinations.keys().filter(|key| !after.named_destinations.contains_key(*key)).collect();
    let fresh: Vec<NamedKey> =
        after.named_destinations.keys().filter(|key| !before.named_destinations.contains_key(*key) && opaque(&key.1)).cloned().collect();
    let mut renames: HashMap<Digest, Digest> = HashMap::new();
    let mut used: HashSet<usize> = HashSet::new();
    for new in fresh {
        let Some(destination) = after.named_destinations.get(&new).cloned() else { continue };
        let Some((at, old)) = gone
            .iter()
            .enumerate()
            .find(|(at, old)| !used.contains(at) && old.0 == new.0 && before.named_destinations.get(**old) == Some(&destination))
        else {
            continue;
        };
        used.insert(at);
        let old = (*old).clone();
        after.named_destinations.remove(&new);
        after.named_destinations.insert(old.clone(), destination);
        for string in [false, true] {
            renames.insert(name_digest(&new.1, string), name_digest(&old.1, string));
        }
    }
    if renames.is_empty() {
        return 0;
    }
    let back = |destination: &mut Destination| {
        if let Destination::Named(digest) = destination
            && let Some(old) = renames.get(digest)
        {
            *digest = *old;
        }
    };
    for entry in &mut after.outline {
        back(&mut entry.destination);
    }
    for page in &mut after.pages {
        for annot in &mut page.annots {
            if let Some(link) = annot.link.as_mut() {
                back(link);
            }
        }
    }
    used.len()
}

/// Whether `part` is `whole` with some items taken away, the rest in their order.
fn subsequence<T: PartialEq>(part: &[T], whole: &[T]) -> bool {
    let mut rest = whole.iter();
    part.iter().all(|item| rest.any(|candidate| candidate == item))
}

/// The entries of a structure element that hold text for assistive technology and copying.
const STRUCT_TEXT_KEYS: [&[u8]; 5] = [b"ActualText", b"Alt", b"E", b"Lang", b"T"];
/// The most structure elements read, and the most text entries kept from them.
const MAX_STRUCT_ELEMENTS: usize = 1_000_000;
/// The most marked-content kids kept from the structure tree.
const MAX_STRUCT_MARKED: usize = 4_000_000;
/// The owner of a marked-content kid of the structure tree's root (no element).
const STRUCT_ROOT: u32 = u32::MAX;

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
    /// Follow-up 8 (R6b-51, C1): a content stream (or, for a page, one of its streams) the
    /// rewrite brings: the resources it names must all exist.
    new: bool,
    /// Follow-up 8 audit (operators): a form's, pattern's or group's content stream the rewrite
    /// brings shows text.
    shows_text: bool,
    /// Follow-up 8 (R6b-51, C1): why a content stream the rewrite brings is not written as the
    /// redactor writes (see `operators`).
    violation: Option<String>,
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
    /// Any other resource named: a font (`Tf`), a shading (`sh`), a colour space (`cs`, `CS`);
    /// the resource dictionary's key, and the name.
    Named(&'static [u8], Vec<u8>),
    /// A marked-content property list (`BDC`, `DP`): its tag, the list, and whether it opens a
    /// level of marked content (`BDC`).
    Span(Option<Vec<u8>>, PropertyList, bool),
    /// A level of marked content opened without a property list (`BMC`, or a `BDC` whose operand
    /// is neither a name nor a dictionary): its tag (R6b-47).
    Level(Option<Vec<u8>>),
    /// `EMC`: the innermost level closed (R6b-47).
    End,
}

/// A marked-content property list: named in the resources (`/Properties`), or written in the
/// content.
enum PropertyList {
    Named(Vec<u8>),
    Inline(Dict),
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

/// Follow-up 8 (P6-33): where a widget is: its page's index and its `/Rect` normalised.
pub(crate) type WidgetPlace = (usize, Option<[f64; 4]>);

/// The names the editor gives the standard fonts it adds (its stamps and overlays).
const EDITOR_FONTS: [&[u8]; 3] = [b"PCHelv", b"PCTimes", b"PCCour"];

/// The procedure sets a resource dictionary's `/ProcSet` may name (ISO 32000-2 §14.2).
const PROCEDURE_SETS: [&[u8]; 5] = [b"PDF", b"Text", b"ImageB", b"ImageC", b"ImageI"];

/// The attributes a page inherits from the page tree (ISO 32000-2 §7.7.3.4).
const INHERITED: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

/// Follow-up 8 (R6b-51, M5): page tree nodes' entries, each by its key and value digest,
/// sorted: those no page takes from its node (its own entries, and inheritable attributes no
/// page under the node takes), and all of them.
#[derive(Clone, Debug, Default)]
pub(crate) struct NodeEntries {
    pub untaken: Vec<(Vec<u8>, Digest)>,
    pub all: Vec<(Vec<u8>, Digest)>,
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
    /// Follow-up 8 (R6b-51, C1): see [`PageFacts::content_violation`].
    violation: Option<String>,
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
    /// See [`PageFacts::text`].
    text: Vec<Span>,
    /// See [`PageFacts::levels`].
    levels: Vec<Level>,
    /// The levels open now (indices in `levels`), innermost last, and how many of them the
    /// content stream being drawn may not close (opened outside it).
    open: Vec<u32>,
    floor: usize,
    /// A level closed that its content stream did not open, or left open at its end.
    unbalanced: bool,
    /// Embed build (follow-up 7 audit, item 7): the resource entries the page's and its forms'
    /// resource dictionaries hold, and those the drawing looked up (see
    /// [`PageFacts::unused_resources`]).
    /// Each offered entry: its digest by content (as `used` holds them), and by name and
    /// content (follow-up 8, R6b-51 I1: a name nothing draws can spell out text too).
    offered: BTreeSet<(Digest, Digest)>,
    /// The standard fonts offered (see [`PageFacts::unused_standard_fonts`]), the same way.
    standard: BTreeSet<(Digest, Digest)>,
    used: BTreeSet<Digest>,
    /// See [`PageFacts::drawing_resources`].
    drawing: BTreeSet<Digest>,
}

/// One level of marked content (`BDC`, `BMC`) the page's own content or a form it draws opens
/// (not a tiling pattern's cell or a soft mask's group, whose glyphs are not the page's text), in
/// drawing order (R6b-47): what a glyph drawn inside it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Level {
    /// The tag (`None`: the scan found no name before the operator).
    pub tag: Option<Vec<u8>>,
    /// The level it opened inside (an index in the page's levels; [`NO_LEVEL`]: none).
    pub parent: u32,
    /// Its property list's span (an index in [`PageFacts::text`]; `None`: no property list, or
    /// one that could not be found).
    pub span: Option<u32>,
    /// Its property list's `/MCID`, written as a whole number.
    pub mcid: Option<i64>,
    /// The form XObject whose content opened it (`None`: the page's own content): where its
    /// `/MCID` belongs (a form's marked content is the structure tree's through `/Stm`).
    pub form: Option<ObjRef>,
}

/// No level of marked content.
pub(crate) const NO_LEVEL: u32 = u32::MAX;

impl Drawn {
    /// Open a level (in the page's own content or a form it draws).
    fn open_level(&mut self, tag: Option<Vec<u8>>, span: Option<u32>, mcid: Option<i64>, form: Option<ObjRef>) -> Result<(), Fail> {
        let index = u32::try_from(self.levels.len()).ok().filter(|index| *index != NO_LEVEL).ok_or(Fail::TooComplex)?;
        let parent = self.open.last().copied().unwrap_or(NO_LEVEL);
        self.levels.push(Level { tag, parent, span, mcid, form });
        self.open.push(index);
        Ok(())
    }

    fn close_level(&mut self) {
        if self.open.len() > self.floor {
            self.open.pop();
        } else {
            self.unbalanced = true;
        }
    }
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
    /// The form XObject whose content this is (`None`: the page's own content), for the
    /// marked-content levels it opens (R6b-47).
    form: Option<ObjRef>,
    /// Follow-up 8 (R6b-51, C1): the content a rewrite brings: every resource it names exists.
    new: bool,
}

struct Reader<'d, 'b> {
    doc: &'d Document,
    data: &'d [u8],
    budget: &'b mut Budget,
    /// Objects whose location was checked: number → generation.
    checked: HashMap<u32, u16>,
    /// Objects read for a fact (not only to check that hayro read them as `pdfcraft_cos` does).
    facts: HashSet<u32>,
    /// Reading only to check what hayro read (see [`Known::agree`]): nothing read is a fact's.
    agreeing: bool,
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
    /// tolerated only in objects of the base revision (see [`Tolerated`]).
    misdeclared: BTreeSet<ObjRef>,
    tolerated: Arc<Tolerated>,
    /// An object read for a fact holds a destination that gives its page by number (see
    /// [`Structure::numbered_destinations`]).
    numbered: bool,
    /// See [`Structure::labels_past_the_end`]: (past the end, all), as read.
    label_values: (Vec<Digest>, Vec<Digest>),
    /// Follow-up 8 (R6b-51, C1): what is done with each content stream decoded (see
    /// [`ContentRule`]), and the base's content as gathered.
    content_rule: ContentRule,
    content_base: ContentBase,
    /// Whether the content stream last checked shows text (see `operators::Brought`).
    shows_text: bool,
    /// Each terminal field's widgets (see [`Structure::field_places`]).
    field_widgets: BTreeMap<String, Vec<ObjRef>>,
    /// Follow-up 9 (R6b-54, N8): a rewrite's comparison: an optional-content group is digested
    /// without its `/Name` (a redaction may clear it; the names are compared apart, see
    /// [`Structure::layer_names`]).
    names_apart: bool,
    /// Follow-up 9 (R6b-54, N8): each group's place in `/OCGs`, its identity where names are
    /// compared apart.
    ocg_index: Arc<HashMap<ObjRef, usize>>,
}

/// Follow-up 8 (R6b-51, C1): the base's content streams, by their decoded data's digest, and
/// the marked-content operators they hold (see `operators`).
#[derive(Clone, Debug, Default)]
pub(crate) struct ContentBase {
    pub streams: HashSet<Digest>,
    pub marked: HashSet<Digest>,
    /// Every operator, with its operands (see `operators::gather`).
    pub operators: HashSet<Digest>,
    /// The inline images' data.
    pub inline: HashSet<Digest>,
}

/// What the reader does with each content stream it decodes.
#[derive(Clone, Debug)]
pub(crate) enum ContentRule {
    /// Nothing.
    None,
    /// Gather them (the base of a rewrite).
    Gather,
    /// Check each that is no content stream of the base's (a rewrite).
    Check(Arc<ContentBase>),
}

/// Which stream objects may declare a `/Length` other than their data's (R6b-30, R6b-35).
#[derive(Debug)]
pub(crate) enum Tolerated {
    /// Any: this file is the base, and every object it reads is at the offset its own
    /// cross-reference data names for it (an object of the base revision).
    Base,
    /// Only the base's own objects that the base read with a misdeclared length, each at the
    /// offset the base revision's cross-reference data names for it and lying wholly within the
    /// first `kept_until` bytes, which are the base's (an appended update keeps them; a rewrite
    /// keeps none). An update that points an object at any other offset (an older copy still in
    /// the base's bytes, which the base revision never read) brings a new object, and the
    /// editor's writer declares every length exactly.
    Kept { kept_until: usize, objects: BTreeSet<(ObjRef, u64)> },
}

/// How far after a stream's data its `endobj` is looked for, to say where the object ends.
const OBJECT_END_SEARCH: usize = 256;

fn read_error(what: impl std::fmt::Display) -> Fail {
    Fail::Read(what.to_string())
}

/// A decode failure: the budget's bound, or damaged data.
pub(crate) fn decode_error(error: &pdfcraft_cos::CosError) -> Fail {
    let message = error.to_string();
    if message.contains("exceeds the limit") { Fail::TooComplex } else { Fail::Read(format!("a stream cannot be decoded: {message}")) }
}

fn ws(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | b'\x0C' | b'\r' | b' ')
}

fn delimiter(byte: u8) -> bool {
    matches!(byte, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

/// The views an explicit destination names after its page (ISO 32000-2 §12.3.2.2).
const VIEW_KINDS: [&[u8]; 8] = [b"XYZ", b"Fit", b"FitH", b"FitV", b"FitR", b"FitB", b"FitBH", b"FitBV"];

/// Whether `items` is an explicit destination that gives its page by number (`[2 /Fit]`).
fn names_page_by_number(items: &[Object]) -> bool {
    matches!(items.first(), Some(Object::Int(_) | Object::Real(_)))
        && items.get(1).and_then(Object::as_name).is_some_and(|kind| VIEW_KINDS.contains(&kind))
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
        if !self.agreeing {
            self.facts.insert(r.num);
        }
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
                    if !self.of_the_base(r, offset, data_start.saturating_add(stream.raw.len())) {
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

    /// Whether stream object `r`, read at `offset` with its data ending at `data_end`, is an
    /// object of the base revision (see [`Tolerated`]): in the base, any; in the edited file,
    /// only one the base read at that same offset, lying wholly (through its `endobj`) within
    /// the bytes this file keeps of the base. An object whose end is not found nearby is not.
    fn of_the_base(&self, r: ObjRef, offset: u64, data_end: usize) -> bool {
        let Tolerated::Kept { kept_until, objects } = &*self.tolerated else { return true };
        if !objects.contains(&(r, offset)) {
            return false;
        }
        let window = self.data.get(data_end..data_end.saturating_add(OBJECT_END_SEARCH).min(self.data.len())).unwrap_or_default();
        let Some(at) = window.windows(6).position(|w| w == b"endobj") else { return false };
        data_end.saturating_add(at).saturating_add(6) <= *kept_until
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
    /// Embed build (follow-up 7 audit, item 4): [`Reader::digest_without`] for entries that
    /// link (to a parent, a page, a tree, a count): each left out only while it is a reference
    /// or a plain value (a number, name, boolean or null). Written as anything else (a dictionary,
    /// array or string in place of a link) it is digested like any entry: no value can sit
    /// uncompared under a key the comparison takes for a link.
    fn digest_without_links(&mut self, d: &Dict, links: &[&[u8]]) -> Result<Digest, Fail> {
        let plain =
            |value: &Object| matches!(value, Object::Ref(_) | Object::Int(_) | Object::Real(_) | Object::Name(_) | Object::Bool(_) | Object::Null);
        let kept: Dict =
            d.iter().filter(|(key, value)| !(links.contains(&key.as_slice()) && plain(value))).map(|(k, v)| (k.clone(), v.clone())).collect();
        self.digest(&Object::Dict(kept))
    }

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
                // R6b-43: a destination naming its page by number, compared here by its digest,
                // goes to another page once pages are deleted, moved or inserted.
                if !self.agreeing && names_page_by_number(items) {
                    self.numbered = true;
                }
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
        // Embed build (follow-up 7 audit, item 4): a back reference is left out only while it is
        // one (a reference, or a plain value); written as a dictionary, array or string it is
        // digested like any entry, so nothing can sit uncompared under its key.
        let back_reference = |key: &[u8], value: &Object| {
            BACK_REFERENCES.contains(&key)
                && matches!(value, Object::Ref(_) | Object::Int(_) | Object::Real(_) | Object::Name(_) | Object::Bool(_) | Object::Null)
        };
        // Follow-up 9 (R6b-54, N8): in a rewrite, a layer's name is compared apart; the group
        // is known by its place in `/OCGs` in its stead (so groups alike but for their names
        // stay apart).
        let name_key = b"Name".to_vec();
        let ocg_place = (self.names_apart && d.name(b"Type") == Some(b"OCG"))
            .then(|| self.stack.last().and_then(|r| self.ocg_index.get(r)).map_or(Object::Null, |place| Object::Int(*place as i64)));
        let mut entries: Vec<(&Vec<u8>, &Object)> =
            d.iter().filter(|(key, value)| !(back_reference(key, value) || ocg_place.is_some() && key.as_slice() == b"Name")).collect();
        if let Some(place) = &ocg_place {
            entries.push((&name_key, place));
        }
        entries.sort_by(|a, b| a.0.cmp(b.0));
        // Follow-up 8 (R6b-51, M4): PDFium takes a number first in a destination for a page
        // index whatever view follows (`[2]`, `[2 /Unknown]`): any such array where a
        // destination stands (a go-to action's or a destination dictionary's `/D`, any `/Dest`,
        // the opening action) gives its page by number.
        if !self.agreeing {
            // A border style's `/D` is a dash array (`[3 2]`), not a destination.
            let border = d.contains(b"W") || matches!(d.name(b"Type"), Some(b"Border" | b"ExtGState"));
            let go_to = d.name(b"S") == Some(b"GoTo") || (!d.contains(b"S") && !border);
            for (key, value) in &entries {
                let destination = match key.as_slice() {
                    b"D" => go_to,
                    b"Dest" | b"OpenAction" => true,
                    _ => false,
                };
                if !destination {
                    continue;
                }
                let resolved = match value {
                    Object::Ref(r) => Some(self.get(*r)?),
                    _ => None,
                };
                let items = match (value, resolved.as_deref()) {
                    (Object::Array(items), _) | (_, Some(Object::Array(items))) => Some(items),
                    _ => None,
                };
                if items.is_some_and(|items| matches!(items.first(), Some(Object::Int(_) | Object::Real(_)))) {
                    self.numbered = true;
                }
            }
        }
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

    /// The pages, in order, and the page tree nodes' entries no page inherits (see
    /// [`Structure::page_tree_entries`]).
    fn page_tree(&mut self, root: &Dict) -> Result<(Vec<PageNode>, NodeEntries), Fail> {
        let Some(Object::Ref(tree)) = root.get(b"Pages") else { return Err(read_error("the catalog has no page tree")) };
        let tree = *tree;
        let dict = self.get(tree)?.as_dict().cloned().ok_or_else(|| read_error("the page tree is not a dictionary"))?;
        // Follow-up 8 (R6b-51, C1): the root has no parent; PDFium and pdf.js would inherit
        // through one.
        if dict.get(b"Parent").is_some_and(|parent| !matches!(parent, Object::Null)) {
            return Err(read_error("the page tree's root names a /Parent, through which viewers inherit attributes"));
        }
        let mut pages = Vec::new();
        let mut entries = NodeEntries::default();
        let mut nodes = BTreeSet::from([tree]);
        let mut seen = BTreeSet::new();
        let inherited = PageNode { obj: tree, dict: Dict::new(), resources: Vec::new(), media: None, crop: None, rotate: None };
        self.walk(tree, &dict, &inherited, 0, &mut nodes, &mut seen, &mut pages, &mut entries)?;
        entries.untaken.sort_unstable();
        entries.all.sort_unstable();
        Ok((pages, entries))
    }

    /// Follow-up 8 (R6b-51, M5): a page tree node's entries other than its kids, count, parent,
    /// type and the attributes its pages take from it (each by its key and value): what no
    /// viewer draws but any tool reading the file finds. An attribute pages inherit is compared
    /// through the pages that take it; one no page takes (every page under the node has its
    /// own, or a nearer node gives it) is compared here (follow-up 8 audit). `taken`: the
    /// inheritable attributes some page under the node takes from it or above.
    fn node_entries(&mut self, node: &Dict, taken: &[&[u8]], entries: &mut NodeEntries) -> Result<(), Fail> {
        const NODE_KEYS: [&[u8]; 4] = [b"Type", b"Kids", b"Count", b"Parent"];
        for (key, value) in node.iter() {
            self.budget.tick()?;
            if NODE_KEYS.contains(&key.as_slice()) {
                continue;
            }
            let digest = self.digest(value)?;
            if !taken.contains(&key.as_slice()) {
                entries.untaken.push((key.clone(), digest));
            }
            entries.all.push((key.clone(), digest));
        }
        Ok(())
    }

    /// Embed build (follow-up 7 audit, item 6): in an edited file, a page tree node's `/Count`
    /// must be the number of pages under it. PDFium takes the root's `/Count` for the page count
    /// (where it is plausible), hayro and this reader count the pages: a `/Count` that lies shows
    /// another document in one of them. (A base's own tree is read as it is.)
    fn check_count(&mut self, node: &Dict, leaves: usize) -> Result<(), Fail> {
        if matches!(*self.tolerated, Tolerated::Base) {
            return Ok(());
        }
        match self.int_of(node.get(b"Count"))? {
            Some(count) if usize::try_from(count).ok() == Some(leaves) => Ok(()),
            _ => Err(read_error(format!("a page tree node's /Count is not the {leaves} page(s) under it, which viewers count differently"))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &mut self,
        node_ref: ObjRef,
        node: &Dict,
        inherited: &PageNode,
        depth: usize,
        nodes: &mut BTreeSet<ObjRef>,
        seen: &mut BTreeSet<ObjRef>,
        pages: &mut Vec<PageNode>,
        entries: &mut NodeEntries,
    ) -> Result<Vec<&'static [u8]>, Fail> {
        if depth > MAX_TREE_DEPTH {
            return Err(read_error("the page tree is too deep"));
        }
        let before = pages.len();
        let here = with_inherited(node, inherited);
        // The inheritable attributes some page under this node takes from it or above: a page's
        // boxes and rotation where it has none of its own, its resources always (a page draws
        // with its ancestors' resources too, see [`PageFacts::resources`]).
        let mut taking: Vec<&'static [u8]> = Vec::new();
        let mut take = |keys: Vec<&'static [u8]>| {
            for key in keys {
                if !taking.contains(&key) {
                    taking.push(key);
                }
            }
        };
        let Some(kids) = node.get(b"Kids") else { return Err(read_error("a page tree node has no /Kids")) };
        let kids = self.array_of(Some(kids), "/Kids")?;
        for kid in kids {
            self.budget.tick()?;
            let Object::Ref(r) = kid else { return Err(read_error("a page tree entry is not a reference")) };
            let dict = self.get(r)?.as_dict().cloned().ok_or_else(|| read_error(format!("page tree entry {r} is not a dictionary")))?;
            // Embed build (follow-up 7 audit, item 6): a node is `/Type /Pages`, a page has no
            // `/Kids`; PDFium and pdf.js take an entry with `/Kids` for a node whatever its type.
            if dict.name(b"Type") != Some(b"Pages") && dict.contains(b"Kids") {
                return Err(read_error(format!("page tree entry {r} has /Kids but is not a /Pages node, which viewers read differently")));
            }
            // Follow-up 8 (R6b-51, C1): PDFium and pdf.js inherit a page's rotation, boxes and
            // resources through its own /Parent chain; hayro and this reader along the path
            // walked. They agree only where every entry's /Parent is the node listing it.
            if dict.get(b"Parent") != Some(&Object::Ref(node_ref)) {
                return Err(read_error(format!(
                    "page tree entry {r}'s /Parent is not the node listing it, so viewers would inherit another page tree's attributes"
                )));
            }
            if dict.name(b"Type") == Some(b"Pages") {
                if !nodes.insert(r) {
                    return Err(read_error("the page tree loops"));
                }
                take(self.walk(r, &dict, &here, depth.saturating_add(1), nodes, seen, pages, entries)?);
            } else {
                take(INHERITED.iter().copied().filter(|key| *key == b"Resources".as_slice() || !dict.contains(key)).collect());
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
        self.check_count(node, pages.len().saturating_sub(before))?;
        self.node_entries(node, &taking, entries)?;
        // What this node's ancestors give its pages: what they take that it does not give.
        Ok(taking.into_iter().filter(|key| *key == b"Resources".as_slice() || !node.contains(key)).collect())
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
        let mut new = false;
        let mut violation = None;
        for r in &key.0 {
            let obj = self.get(*r)?;
            let Object::Stream(stream) = &*obj else { return Err(read_error(format!("content stream {r} is not a stream"))) };
            let decoded = self.decode(stream)?;
            let (brought, why) = self.content_stream(&decoded)?;
            new |= brought;
            violation = violation.or(why);
            data.extend_from_slice(&decoded);
            if array {
                // As hayro joins them: at least one whitespace in between.
                data.push(b' ');
            }
        }
        let mut scan = self.scan(&data)?;
        scan.new = new;
        scan.violation = violation;
        let scan = Arc::new(scan);
        self.contents.insert(key, scan.clone());
        Ok(scan)
    }

    /// A form XObject's content, decoded and scanned once.
    fn form_content(&mut self, r: ObjRef, stream: &Stream) -> Result<Arc<Scan>, Fail> {
        if let Some(found) = self.forms.get(&r) {
            return Ok(found.clone());
        }
        let data = self.decode(stream)?;
        self.shows_text = false;
        let (new, violation) = self.content_stream(&data)?;
        let mut scan = self.scan(&data)?;
        scan.new = new;
        scan.violation = violation;
        scan.shows_text = std::mem::take(&mut self.shows_text);
        let scan = Arc::new(scan);
        self.forms.insert(r, scan.clone());
        Ok(scan)
    }

    /// Follow-up 8 (R6b-51, C1): apply the [`ContentRule`] to one content stream's decoded
    /// data; whether it is one the rewrite brings, and why it is not written as the redactor
    /// writes (the page drawing it counts outside its redactions).
    fn content_stream(&mut self, data: &[u8]) -> Result<(bool, Option<String>), Fail> {
        match &self.content_rule {
            ContentRule::None => Ok((false, None)),
            ContentRule::Gather => {
                self.budget.hashed(data.len())?;
                if self.content_base.streams.insert(sha256(&[data])) {
                    super::operators::gather(data, &mut self.content_base, self.budget)?;
                }
                Ok((false, None))
            }
            ContentRule::Check(base) => {
                let base = base.clone();
                self.budget.hashed(data.len())?;
                if base.streams.contains(&sha256(&[data])) {
                    return Ok((false, None));
                }
                match super::operators::check(data, &base, self.budget) {
                    Ok(brought) => {
                        self.shows_text = brought.shows_text;
                        Ok((true, None))
                    }
                    Err(Fail::Read(why)) => Ok((true, Some(why))),
                    Err(other) => Err(other),
                }
            }
        }
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
        // The last operand, when it is a dictionary (an inline property list).
        let mut dict: Option<Dict> = None;
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
                dict = None;
                name = match operand {
                    Object::Name(n) => Some(n),
                    Object::Dict(d) => {
                        dict = Some(d);
                        None
                    }
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
                b"sh" | b"cs" | b"CS" => {
                    let kind: &'static [u8] = if keyword == b"sh" { b"Shading" } else { b"ColorSpace" };
                    if let Some(resource) = name.take() {
                        items.push(Item::Named(kind, resource));
                    }
                }
                // The tag is the operand before the property list (a name, or a dictionary).
                b"BDC" | b"DP" => {
                    let level = keyword == b"BDC";
                    if let Some(resource) = name.take() {
                        items.push(Item::Span(before.take(), PropertyList::Named(resource), level));
                    } else if let Some(list) = dict.take() {
                        items.push(Item::Span(before.take(), PropertyList::Inline(list), level));
                    } else if level {
                        // Embed build: a level all the same (R6b-47).
                        items.push(Item::Level(before.take()));
                    }
                }
                b"BMC" => items.push(Item::Level(name.take())),
                b"EMC" => items.push(Item::End),
                _ => {}
            }
            name = None;
            before = None;
            dict = None;
        }
        Ok(Scan { digest: sha256(&[data]), items, len: data.len(), new: false, violation: None, shows_text: false })
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
            // Follow-up 8 (R6b-51, C1): content a rewrite brings names only resources that
            // exist (a name drawing nothing could spell out text).
            if at.new && drawn.violation.is_none() {
                const DEVICE: [&[u8]; 7] = [b"DeviceGray", b"DeviceRGB", b"DeviceCMYK", b"Pattern", b"G", b"RGB", b"CMYK"];
                let missing = match item {
                    Item::Named(b"ColorSpace", name) => {
                        !DEVICE.contains(&name.as_slice()) && self.resource_entry(chain, b"ColorSpace", name)?.is_none()
                    }
                    Item::Named(kind, name) => self.resource_entry(chain, kind, name)?.is_none(),
                    Item::Draw(name) => self.xobject(chain, name)?.is_none(),
                    Item::Pattern(name) => self.resource_entry(chain, b"Pattern", name)?.is_none(),
                    Item::State(name) => self.resource_entry(chain, b"ExtGState", name)?.is_none(),
                    Item::Span(_, PropertyList::Named(name), _) => self.resource_entry(chain, b"Properties", name)?.is_none(),
                    _ => false,
                };
                if missing {
                    drawn.violation = Some("content the rewrite brings names a resource that does not exist".into());
                }
            }
            match item {
                Item::Named(kind, name) => {
                    let kind: &'static [u8] = match *kind {
                        b"Font" => b"Font",
                        b"Shading" => b"Shading",
                        // Colour spaces are names or arrays: what they mean, hayro draws. Only
                        // noted as used.
                        _ => {
                            if let Some(entry) = self.resource_entry(chain, b"ColorSpace", name)? {
                                self.use_resource(b"ColorSpace", &entry, drawn)?;
                            }
                            continue;
                        }
                    };
                    if let Some(entry) = self.resource_entry(chain, kind, name)? {
                        self.use_resource(kind, &entry, drawn)?;
                        match &*self.resolve(&entry)? {
                            Object::Dict(dict) => self.note_keys(kind, dict, drawn)?,
                            Object::Stream(stream) => self.note_keys(kind, &stream.dict, drawn)?,
                            _ => {}
                        }
                    }
                }
                Item::Span(tag, list, level) => {
                    let found = match list {
                        PropertyList::Named(name) => match self.resource_entry(chain, b"Properties", name)? {
                            Some(entry) => match &*{
                                self.use_resource(b"Properties", &entry, drawn)?;
                                self.resolve(&entry)?
                            } {
                                Object::Dict(dict) => Some(dict.clone()),
                                Object::Stream(stream) => Some(stream.dict.clone()),
                                _ => None,
                            },
                            None => None,
                        },
                        PropertyList::Inline(dict) => Some(dict.clone()),
                    };
                    let span = u32::try_from(drawn.text.len()).ok();
                    let mcid = found.as_ref().and_then(|dict| match dict.get(b"MCID") {
                        Some(Object::Int(mcid)) => Some(*mcid),
                        _ => None,
                    });
                    if let Some(dict) = &found {
                        self.note_keys(b"Properties", dict, drawn)?;
                        self.note_span(tag.as_deref(), dict, drawn)?;
                    }
                    // Embed build: a level glyphs are drawn in (R6b-47).
                    if *level && !at.patterned && !at.masked {
                        drawn.open_level(tag.clone(), found.is_some().then_some(span).flatten(), mcid, at.form)?;
                    }
                }
                Item::Level(tag) => {
                    if !at.patterned && !at.masked {
                        drawn.open_level(tag.clone(), None, None, at.form)?;
                    }
                }
                Item::End => {
                    if !at.patterned && !at.masked {
                        drawn.close_level();
                    }
                }
                Item::Inline(digest, pixels) => {
                    drawn.images.push(*digest);
                    drawn.pixels = drawn.pixels.saturating_add(*pixels);
                    if !at.masked {
                        require(&mut drawn.required, ImageKey::Inline, at.patterned);
                    }
                }
                Item::Draw(name) => {
                    let Some(r) = self.xobject(chain, name)? else { continue };
                    self.use_resource(b"XObject", &Object::Ref(r), drawn)?;
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
                            if drawn.violation.is_none() {
                                drawn.violation.clone_from(&scan.violation);
                            }
                            let key = self.form_key(r, stream)?;
                            self.group_risk(&stream.dict, drawn)?;
                            self.note_keys(b"Form", &stream.dict, drawn)?;
                            self.note_resources(stream.dict.get(b"Resources"), drawn)?;
                            let placed = !at.patterned && !at.masked;
                            drawn.forms.push(FormUse { obj: r, key, placed });
                            if placed {
                                drawn.required.forms.insert(r);
                            }
                            drawn.interpreted = drawn.interpreted.saturating_add(scan.len);
                            let (inner, key) = self.inner_chain(stream.dict.get(b"Resources"), "a form's /Resources", chain, &at.chain)?;
                            if let Some(own) = inner.first() {
                                self.offer_resources(own, drawn)?;
                            }
                            self.type3_fonts(&inner, &key, drawn)?;
                            // Embed build: the form's content may close only the levels it opened.
                            let floor = std::mem::replace(&mut drawn.floor, drawn.open.len());
                            self.draw(
                                &scan.items,
                                &inner,
                                Context { depth: at.depth.saturating_add(1), chain: key, form: Some(r), new: scan.new, ..at },
                                drawn,
                            )?;
                            if !at.patterned && !at.masked {
                                drawn.unbalanced |= drawn.open.len() != drawn.floor;
                                drawn.open.truncate(drawn.floor);
                            }
                            drawn.floor = floor;
                        }
                        _ => {}
                    }
                }
                Item::Pattern(name) => {
                    if let Some(entry) = self.resource_entry(chain, b"Pattern", name)? {
                        self.use_resource(b"Pattern", &entry, drawn)?;
                    }
                    let Some(r) = self.resource(chain, b"Pattern", name)? else { continue };
                    let obj = self.get(r)?;
                    // Shading patterns (dictionaries) draw no images, but may set a graphics
                    // state; a tiling pattern is a stream.
                    if let Object::Dict(pattern) = &*obj {
                        self.note_keys(b"Pattern", pattern, drawn)?;
                        if let Some(state) = self.dict_of(pattern.get(b"ExtGState"), "a pattern's /ExtGState")? {
                            self.state_risk(&state, drawn)?;
                            self.note_keys(b"ExtGState", &state, drawn)?;
                        }
                        match pattern.get(b"Shading").map(|shading| self.resolve(shading)).transpose()?.as_deref() {
                            Some(Object::Dict(shading)) => self.note_keys(b"Shading", shading, drawn)?,
                            Some(Object::Stream(shading)) => self.note_keys(b"Shading", &shading.dict, drawn)?,
                            _ => {}
                        }
                    }
                    let Object::Stream(stream) = &*obj else { continue };
                    self.note_keys(b"Pattern", &stream.dict, drawn)?;
                    self.note_resources(stream.dict.get(b"Resources"), drawn)?;
                    if stream.dict.int(b"PatternType") != Some(1) || !drawn.patterns.insert((r, at.patterned, at.masked)) {
                        continue;
                    }
                    if at.depth >= self.budget.limits.form_depth {
                        return Err(Fail::TooComplex);
                    }
                    let scan = self.form_content(r, stream)?;
                    if drawn.violation.is_none() {
                        drawn.violation.clone_from(&scan.violation);
                    }
                    // Follow-up 8 audit (operators): no glyph in a cell is extracted, so a cell
                    // the rewrite brings may show no text.
                    if scan.new && scan.shows_text && drawn.violation.is_none() {
                        drawn.violation = Some("a tiling pattern's cell the rewrite brings shows text no comparison reads".into());
                    }
                    drawn.interpreted = drawn.interpreted.saturating_add(scan.len);
                    let (inner, key) = self.inner_chain(stream.dict.get(b"Resources"), "a pattern's /Resources", chain, &at.chain)?;
                    if let Some(own) = inner.first() {
                        self.offer_resources(own, drawn)?;
                    }
                    self.type3_fonts(&inner, &key, drawn)?;
                    self.draw(
                        &scan.items,
                        &inner,
                        Context { depth: at.depth.saturating_add(1), patterned: true, chain: key, new: scan.new, ..at },
                        drawn,
                    )?;
                }
                Item::State(name) => {
                    if let Some(entry) = self.resource_entry(chain, b"ExtGState", name)? {
                        self.use_resource(b"ExtGState", &entry, drawn)?;
                    }
                    if let Some(entry) = self.resource_entry(chain, b"ExtGState", name)?
                        && let Some(state) = self.dict_of(Some(&entry), "a graphics state")?
                    {
                        self.state_risk(&state, drawn)?;
                        self.note_keys(b"ExtGState", &state, drawn)?;
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
                    if drawn.violation.is_none() {
                        drawn.violation.clone_from(&scan.violation);
                    }
                    if scan.new && scan.shows_text && drawn.violation.is_none() {
                        drawn.violation = Some("a soft mask's group the rewrite brings shows text no comparison reads".into());
                    }
                    let key = self.form_key(group, stream)?;
                    self.group_risk(&stream.dict, drawn)?;
                    self.note_keys(b"Form", &stream.dict, drawn)?;
                    self.note_resources(stream.dict.get(b"Resources"), drawn)?;
                    drawn.forms.push(FormUse { obj: group, key, placed: false });
                    drawn.interpreted = drawn.interpreted.saturating_add(scan.len);
                    let (inner, key) = self.inner_chain(stream.dict.get(b"Resources"), "a soft mask's /Resources", chain, &at.chain)?;
                    if let Some(own) = inner.first() {
                        self.offer_resources(own, drawn)?;
                    }
                    self.type3_fonts(&inner, &key, drawn)?;
                    self.draw(
                        &scan.items,
                        &inner,
                        Context { depth: at.depth.saturating_add(1), masked: true, chain: key, new: scan.new, ..at },
                        drawn,
                    )?;
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

    /// Check the keys of `dict` (a dictionary of `kind` the page draws with, see
    /// [`drawn_keys`]): a key neither compared nor ignored makes the page a render risk.
    fn note_keys(&mut self, kind: &'static [u8], dict: &Dict, drawn: &mut Drawn) -> Result<(), Fail> {
        let known = drawn_keys(kind);
        for (key, _) in dict.iter() {
            self.budget.tick()?;
            if !known.contains(&key.as_slice()) && drawn.render_risk.is_none() {
                drawn.render_risk = Some(format!(
                    "/{} in a {} dictionary, which this comparison does not know",
                    String::from_utf8_lossy(key),
                    String::from_utf8_lossy(kind)
                ));
            }
        }
        Ok(())
    }

    /// Embed build (follow-up 7 audit, item 7): a resource entry by its category and content.
    fn resource_digest(&mut self, kind: &[u8], entry: &Object) -> Result<Digest, Fail> {
        let digest = if kind == b"Font" { self.font_digest(entry)? } else { self.digest(entry)? };
        Ok(sha256(&[b"resource", kind, &digest]))
    }

    /// Follow-up 9 (R6b-54, N6): a font by its content. A Type 3 font's glyph procedures are
    /// content streams, keyed by their data as `pdfcraft_cos` decodes them strictly, whether or
    /// not hayro drew a glyph of them in this file: a redaction that removes a font's last
    /// glyph drawn from one procedure leaves that procedure undrawn, and its key may not change
    /// for that (where hayro draws one, the readers' decodings are compared elsewhere). Any
    /// other font, by [`Reader::digest`].
    fn font_digest(&mut self, entry: &Object) -> Result<Digest, Fail> {
        let resolved = self.resolve(entry)?;
        let Object::Dict(font) = &*resolved else { return self.digest(entry) };
        if font.name(b"Subtype") != Some(b"Type3") {
            return self.digest(entry);
        }
        let mut parts = vec![b"type3 font".to_vec(), self.digest_without(font, &[b"CharProcs"])?.to_vec()];
        let procedures = self.dict_of(font.get(b"CharProcs"), "/CharProcs")?.unwrap_or_default();
        let mut names: Vec<(&Vec<u8>, &Object)> = procedures.iter().collect();
        names.sort_by(|a, b| a.0.cmp(b.0));
        for (name, procedure) in names {
            self.budget.tick()?;
            let Object::Ref(r) = procedure else { return Err(read_error("a Type 3 glyph procedure is not a stream")) };
            let obj = self.get(*r)?;
            let Object::Stream(stream) = &*obj else { return Err(read_error("a Type 3 glyph procedure is not a stream")) };
            let kept: Dict =
                stream.dict.iter().filter(|(key, _)| !CODING_KEYS.contains(&key.as_slice())).map(|(k, v)| (k.clone(), v.clone())).collect();
            parts.push((name.len() as u64).to_le_bytes().to_vec());
            parts.push(name.clone());
            parts.push(self.digest(&Object::Dict(kept))?.to_vec());
            parts.push(self.decoded_digest(*r, stream)?.to_vec());
        }
        let parts: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        Ok(sha256(&parts))
    }

    fn use_resource(&mut self, kind: &[u8], entry: &Object, drawn: &mut Drawn) -> Result<(), Fail> {
        let digest = self.resource_digest(kind, entry)?;
        drawn.used.insert(digest);
        // Follow-up 8 (R6b-51; the redaction re-review's C2): what a page draws with but forms
        // and images, whose data under the boxes no comparison reads (a pattern's cell, a
        // shading's samples, a colour profile, a font's names and maps): a property list
        // without its text, which is compared span by span.
        if kind != b"XObject" {
            let drawing = match (kind, &*self.resolve(entry)?) {
                (b"Properties", Object::Dict(list)) => {
                    let untexted: Dict =
                        list.iter().filter(|(key, _)| !STRUCT_TEXT_KEYS.contains(&key.as_slice())).map(|(k, v)| (k.clone(), v.clone())).collect();
                    let digest = self.digest(&Object::Dict(untexted))?;
                    sha256(&[b"resource", kind, &digest])
                }
                // A tiling pattern's cell is content, judged as content the rewrite brings is
                // (written as the redactor writes, showing no text, its images compared by
                // identity, its pixels by the renders): the pattern but for its cell.
                (b"Pattern", Object::Stream(pattern)) => {
                    let dict = self.digest_without(&pattern.dict, &FORM_KEY_LEFT_OUT)?;
                    sha256(&[b"resource", kind, b"cell left out", &dict])
                }
                // A soft mask's group is a form, compared as forms are (its glyphs, images and
                // pixels): the state but for it.
                (b"ExtGState", Object::Dict(state)) if state.contains(b"SMask") => {
                    let mut state = state.clone();
                    if let Some(Object::Dict(mask)) = state.get(b"SMask").map(|mask| self.resolve(mask)).transpose()?.as_deref() {
                        let mut mask = mask.clone();
                        mask.remove(b"G");
                        state.set(b"SMask".to_vec(), Object::Dict(mask));
                    }
                    let digest = self.digest(&Object::Dict(state))?;
                    sha256(&[b"resource", kind, &digest])
                }
                _ => digest,
            };
            drawn.drawing.insert(drawing);
        }
        Ok(())
    }

    /// Note every entry `resources` offers (see [`PageFacts::unused_resources`]), but the editor's
    /// standard fonts under its names for them.
    fn offer_resources(&mut self, resources: &Dict, drawn: &mut Drawn) -> Result<(), Fail> {
        for kind in [b"XObject".as_slice(), b"Font", b"ExtGState", b"Pattern", b"Shading", b"Properties", b"ColorSpace"] {
            let Some(entries) = self.dict_of(resources.get(kind), "a resource dictionary")? else { continue };
            for (name, entry) in entries.iter() {
                self.budget.tick()?;
                let digest = self.resource_digest(kind, entry)?;
                let named = sha256(&[b"named resource", kind, &(name.len() as u64).to_le_bytes(), name, &digest]);
                if kind == b"Font" && self.standard_font(entry)? {
                    // The editor's own, under the names it gives them, are not counted.
                    if !EDITOR_FONTS.contains(&name.as_slice()) {
                        drawn.standard.insert((digest, named));
                    }
                    continue;
                }
                drawn.offered.insert((digest, named));
            }
        }
        Ok(())
    }

    /// A Type 1 font dictionary of the standard 14 naming nothing but its type, subtype, base
    /// font and a standard encoding (what the editor adds for its own text): it holds no text.
    fn standard_font(&mut self, entry: &Object) -> Result<bool, Fail> {
        const STANDARD: [&[u8]; 14] = [
            b"Helvetica",
            b"Helvetica-Bold",
            b"Helvetica-Oblique",
            b"Helvetica-BoldOblique",
            b"Times-Roman",
            b"Times-Bold",
            b"Times-Italic",
            b"Times-BoldItalic",
            b"Courier",
            b"Courier-Bold",
            b"Courier-Oblique",
            b"Courier-BoldOblique",
            b"Symbol",
            b"ZapfDingbats",
        ];
        let Object::Dict(font) = &*self.resolve(entry)? else { return Ok(false) };
        Ok(font.iter().all(|(key, value)| match (key.as_slice(), value) {
            (b"Type", Object::Name(name)) => name.as_slice() == b"Font",
            (b"Subtype", Object::Name(name)) => name.as_slice() == b"Type1",
            (b"BaseFont", Object::Name(name)) => STANDARD.contains(&name.as_slice()),
            (b"Encoding", Object::Name(name)) => matches!(name.as_slice(), b"WinAnsiEncoding" | b"MacRomanEncoding" | b"StandardEncoding"),
            _ => false,
        }))
    }

    /// Keep one use of a property list, tagged `tag`, in drawing order (see [`Span`]).
    fn note_span(&mut self, tag: Option<&[u8]>, list: &Dict, drawn: &mut Drawn) -> Result<(), Fail> {
        let rest = self.digest_without(list, &STRUCT_TEXT_KEYS)?;
        let tag = tag.map_or_else(|| sha256(&[b"no tag"]), |tag| sha256(&[b"tag", tag]));
        let mut text = Vec::new();
        for (key_index, key) in (0u8..).zip(STRUCT_TEXT_KEYS) {
            if let Some(value) = list.get(key) {
                text.push((key_index, self.digest(value)?));
            }
        }
        drawn.text.push(Span { identity: sha256(&[b"span", &tag, &rest]), text });
        Ok(())
    }

    /// [`Reader::note_keys`] for a resource dictionary (`resources`, resolved).
    fn note_resources(&mut self, resources: Option<&Object>, drawn: &mut Drawn) -> Result<(), Fail> {
        if let Some(resources) = self.dict_of(resources, "/Resources")? {
            self.note_keys(b"Resources", &resources, drawn)?;
            // Follow-up 8 (R6b-51, I1): no viewer reads `/ProcSet`; a name other than the
            // standard procedure sets is data nothing else compares.
            if drawn.render_risk.is_none() && !self.procedure_sets(&resources)? {
                drawn.render_risk = Some("a /ProcSet naming other than the standard procedure sets".into());
            }
        }
        Ok(())
    }

    /// Whether `resources`' `/ProcSet` (if any) names only standard procedure sets.
    fn procedure_sets(&mut self, resources: &Dict) -> Result<bool, Fail> {
        let Some(sets) = resources.get(b"ProcSet") else { return Ok(true) };
        Ok(match &*self.resolve(sets)? {
            Object::Array(names) => names.iter().all(|name| name.as_name().is_some_and(|name| PROCEDURE_SETS.contains(&name))),
            Object::Null => true,
            _ => false,
        })
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
        let font_digest = self.font_digest(font)?;
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
                Item::Span(_, PropertyList::Named(name), _) => (b"Properties", name),
                // Its data is the procedure's own; marked-content levels name nothing.
                Item::Inline(..) | Item::Span(_, PropertyList::Inline(_), _) | Item::Level(_) | Item::End => continue,
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
        let stream_mask = matches!(dict.get(b"Mask"), Some(Object::Ref(_)));
        let mut left_out: Vec<&[u8]> = vec![b"Filter", b"DecodeParms", b"Length", b"DL", b"SMask"];
        if stream_mask {
            left_out.push(b"Mask");
        }
        left_out.extend(IMAGE_EXTRAS);
        let meaning = self.digest_without(dict, &left_out)?;
        let mut masks = [None, None];
        for (slot, key) in masks.iter_mut().zip([&b"SMask"[..], b"Mask"]) {
            let Some(mask) = dict.get(key).and_then(Object::as_ref) else { continue };
            if key == b"Mask" && !stream_mask {
                continue;
            }
            let object = self.get(mask)?;
            // A mask that is no stream is no mask a viewer reads alike: its own meaning only.
            let meaning = match &*object {
                Object::Stream(stream) => self.digest_without(&stream.dict, &CODING_KEYS)?,
                other => sha256(&[b"no mask stream", &self.digest(other)?]),
            };
            *slot = Some((mask, meaning));
        }
        let extras: Dict = dict.iter().filter(|(key, _)| IMAGE_EXTRAS.contains(&key.as_slice())).map(|(k, v)| (k.clone(), v.clone())).collect();
        let extras = if extras.is_empty() { None } else { Some(self.digest(&Object::Dict(extras))?) };
        let info = Arc::new(ImageInfo { obj: r, meaning, width, height, masks, extras });
        self.images.insert(r, info.clone());
        Ok(info)
    }

    // ── pages ───────────────────────────────────────────────────────────────────────────────

    fn page(
        &mut self,
        node: &PageNode,
        widgets: &BTreeSet<ObjRef>,
        annotation_ids: &mut BTreeSet<ObjRef>,
        pages: &HashMap<ObjRef, usize>,
    ) -> Result<PageFacts, Fail> {
        let rotation = match self.int_of(node.rotate.as_ref())? {
            None if node.rotate.is_some() => return Err(read_error(format!("page {} has a /Rotate that is not a whole number", node.obj))),
            value => value.unwrap_or(0).rem_euclid(360),
        };
        if rotation % 90 != 0 {
            return Err(read_error(format!("page {} is rotated by {rotation}°, not a multiple of 90°", node.obj)));
        }
        let what = node.obj;
        let media = self.page_box(node.media.as_ref())?.ok_or_else(|| read_error(format!("page {what} has no /MediaBox of four numbers")))?;
        let crop = match &node.crop {
            Some(crop) => Some(self.page_box(Some(crop))?.ok_or_else(|| read_error(format!("page {what} has a /CropBox that is not four numbers")))?),
            None => None,
        };
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
        let mut entries = BTreeMap::new();
        for (key, value) in node.dict.iter() {
            // Embed build (follow-up 7 audit, item 4): the entries no viewer draws are kept here
            // too, so a rewrite may only keep or remove them, never add or change one (a page's
            // `/PieceInfo` or `/Metadata` could carry what the rewrite took away).
            if PAGE_OWN_KEYS.contains(&key.as_slice()) || key.as_slice() == b"Type" {
                continue;
            }
            entries.insert(key.clone(), self.digest(value)?);
        }
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
        let mut drawn = Drawn { interpreted: scan.len, violation: scan.violation.clone(), ..Drawn::default() };
        self.group_risk(&node.dict, &mut drawn)?;
        for level in &chain {
            self.note_keys(b"Resources", level, &mut drawn)?;
            if drawn.render_risk.is_none() && !self.procedure_sets(level)? {
                drawn.render_risk = Some("a /ProcSet naming other than the standard procedure sets".into());
            }
            self.offer_resources(level, &mut drawn)?;
        }
        self.type3_fonts(&chain, &resources, &mut drawn)?;
        self.draw(
            &scan.items,
            &chain,
            Context { depth: 0, patterned: false, masked: false, chain: resources, form: None, new: scan.new },
            &mut drawn,
        )?;
        drawn.unbalanced |= !drawn.open.is_empty();
        drawn.images.sort_unstable();
        let (annots, annots_in_array) = self.annotations(node, widgets, annotation_ids, pages)?;
        Ok(PageFacts {
            rotation,
            boxes: (media, crop),
            object: node.obj,
            content: scan.digest,
            resources,
            other,
            images: drawn.images,
            text: drawn.text,
            levels: drawn.levels,
            levels_agree: false,
            levels_unbalanced: drawn.unbalanced,
            unused_resources: drawn.offered.iter().filter(|(content, _)| !drawn.used.contains(content)).map(|(_, named)| *named).collect(),
            unused_standard_fonts: drawn.standard.iter().filter(|(content, _)| !drawn.used.contains(content)).map(|(_, named)| *named).collect(),
            offered_resources: drawn.offered.iter().chain(&drawn.standard).map(|(_, named)| *named).collect(),
            entries,
            image_pixels: drawn.pixels,
            render_risk: drawn.render_risk,
            content_violation: drawn.violation,
            drawing_resources: drawn.drawing,
            image_uses: Vec::new(),
            required: drawn.required,
            forms: drawn.forms,
            form_draws: Vec::new(),
            form_layers: Vec::new(),
            layers: PageLayers::default(),
            type3: drawn.type3,
            interpreted: drawn.interpreted,
            annots,
            annots_in_array,
            size: (0.0, 0.0),
            glyphs: Glyphs::default(),
            fingerprint: [0; 32],
        })
    }

    /// A page box: four numbers written in the array (not references, which a reader may or may
    /// not follow), normalised. `None` when absent or null, or when it is anything else: hayro
    /// skips such a value and takes the inherited one, so it is refused.
    fn page_box(&mut self, o: Option<&Object>) -> Result<Option<[f64; 4]>, Fail> {
        let Some(o) = o else { return Ok(None) };
        let Object::Array(items) = &*self.resolve(o)? else { return Ok(None) };
        let numbers: Option<Vec<f64>> =
            items.iter().map(|item| matches!(item, Object::Int(_) | Object::Real(_)).then(|| item.as_f64()).flatten()).collect();
        Ok(match numbers.as_deref() {
            Some(&[x0, y0, x1, y1]) if [x0, y0, x1, y1].iter().all(|v| v.is_finite()) => Some([x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]),
            _ => None,
        })
    }

    fn annotations(
        &mut self,
        node: &PageNode,
        widgets: &BTreeSet<ObjRef>,
        ids: &mut BTreeSet<ObjRef>,
        pages: &HashMap<ObjRef, usize>,
    ) -> Result<(Vec<Annot>, usize), Fail> {
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
            let subtype = dict.name(b"Subtype").unwrap_or_default().to_vec();
            // A link's destination in this file names a page object, whose digest would change
            // with the page (rotated, annotated, renumbered): it is compared on its own.
            let found = if subtype == b"Link" { self.link_destination(&dict, pages)? } else { None };
            let (link, dest_key) = match found {
                Some((destination, key)) => (Some(destination), Some(key)),
                None => (None, None),
            };
            let digest = match (id, dest_key) {
                // A link, with or without a destination, alike (an editor drops the destination
                // of a link whose page it deleted).
                _ if subtype == b"Link" => sha256(&[b"link", &self.digest_without(&dict, dest_key.as_slice())?]),
                (Some(r), _) => self.digest_ref(r)?.0,
                (None, _) => self.digest(&Object::Dict(dict.clone()))?,
            };
            let bare = match dest_key {
                Some(key) => self.digest_without(&dict, &[b"AP", key])?,
                None => self.digest_without(&dict, &[b"AP"])?,
            };
            let rect = self.array_of(dict.get(b"Rect"), "/Rect")?;
            let rect = match rect.iter().map(Object::as_f64).collect::<Option<Vec<f64>>>().as_deref() {
                Some(&[x0, y0, x1, y1]) if [x0, y0, x1, y1].iter().all(|v| v.is_finite()) => Some([x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]),
                _ => None,
            };
            let widget = subtype == b"Widget" && id.is_some_and(|id| widgets.contains(&id));
            let silent_popup = subtype == b"Popup" && !dict.contains(b"AP");
            let areas = if subtype == b"Redact" { self.marked_areas(&dict)? } else { Vec::new() };
            // Follow-up 8 (P6-33), follow-up 9 (R6b-55): a mark asking for more than the black
            // fill the redactor gives every area, by the host's rule.
            let overlay = subtype == b"Redact" && self.mark_overlay(&dict)?;
            let reply_to = match dict.get(b"IRT") {
                Some(Object::Ref(to)) => Some(*to),
                _ if subtype == b"Popup" => dict.get(b"Parent").and_then(Object::as_ref),
                _ => None,
            };
            let untexted = match dest_key {
                Some(key) => self.digest_without(&dict, &[b"Contents", b"RC", b"T", key])?,
                None if subtype == b"Link" => sha256(&[b"link", &self.digest_without(&dict, &[b"Contents", b"RC", b"T"])?]),
                None => self.digest_without(&dict, &[b"Contents", b"RC", b"T"])?,
            };
            let mut texts = Vec::new();
            for key in ANNOTATION_TEXT_KEYS {
                if let Some(value) = dict.get(key) {
                    texts.push((key.to_vec(), self.digest(value)?));
                }
            }
            annots.push(Annot {
                id,
                subtype,
                digest,
                tracked: !widget && !silent_popup,
                areas,
                bare,
                rect,
                link,
                overlay,
                reply_to,
                untexted,
                texts,
            });
        }
        Ok((annots, in_array))
    }

    /// The structure tree's facts (see [`StructFacts`]), read through `/K` from the root, every
    /// element once (a tree that loops is refused).
    fn structure_tree(&mut self, catalog: &Dict) -> Result<(StructFacts, StructLinks), Fail> {
        let Some(root) = self.dict_of(catalog.get(b"StructTreeRoot"), "/StructTreeRoot")? else {
            return Ok((StructFacts::default(), StructLinks::default()));
        };
        let mut facts = StructFacts::default();
        let mut links = StructLinks::default();
        let mut parts: Vec<u8> = Vec::new();
        let mut shape: Vec<u8> = Vec::new();
        // `/K` is walked below; the parent tree and the ID tree, indexes into the elements, are
        // compared after it, each element they name by its place (follow-up 7 audit, item 4).
        let root_digest = self.digest_without(&root, &[b"K", b"ParentTree", b"IDTree"])?;
        let mut element_places: HashMap<ObjRef, u32> = HashMap::new();
        parts.extend_from_slice(&root_digest);
        shape.extend_from_slice(&root_digest);
        let mut seen: BTreeSet<ObjRef> = BTreeSet::new();
        // (kid, depth, the element it belongs to, the page that element or its nearest ancestor
        // names), depth first in the tree's order. An element is named by its place in that order
        // and its parent's (R6b-44): with the shape the same in both files, the same place is the
        // same element.
        let mut todo: Vec<(Object, usize, u32, Option<ObjRef>)> =
            root.get(b"K").cloned().map(|kid| (kid, 0, STRUCT_ROOT, None)).into_iter().collect();
        let mut elements: u32 = 0;
        while let Some((kid, depth, owner, page)) = todo.pop() {
            self.budget.tick()?;
            if depth > MAX_REFERENCE_DEPTH || seen.len() > MAX_STRUCT_ELEMENTS {
                return Err(Fail::TooComplex);
            }
            if let Object::Ref(r) = &kid
                && !seen.insert(*r)
            {
                return Err(read_error("the structure tree loops"));
            }
            let kid_ref = if let Object::Ref(r) = &kid { Some(*r) } else { None };
            match &*self.resolve(&kid)? {
                Object::Array(items) => {
                    parts.push(b'[');
                    todo.extend(items.iter().rev().map(|item| (item.clone(), depth.saturating_add(1), owner, page)));
                }
                Object::Int(mcid) => {
                    if facts.marked.len() >= MAX_STRUCT_MARKED {
                        return Err(Fail::TooComplex);
                    }
                    facts.marked.push((owner, *mcid));
                    links.kids.push(MarkedKid { element: owner, page, form: None, mcid: *mcid });
                    parts.push(b'm');
                    parts.extend_from_slice(&owner.to_le_bytes());
                    parts.extend_from_slice(&mcid.to_le_bytes());
                }
                // Follow-up 7 audit (item 10): a marked-content reference is its element's marked
                // content, as an MCID is (on its own `/Pg` or in the form `/Stm` names, R6b-47); an
                // object reference names an annotation (or another object) the element stands
                // for. Each may be taken away by a rewrite, as marked content is; neither may hold
                // an entry of its own.
                Object::Dict(reference) if reference.name(b"Type") == Some(b"MCR") => {
                    if reference.iter().any(|(key, _)| !matches!(key.as_slice(), b"Type" | b"Pg" | b"Stm" | b"StmOwn" | b"MCID")) {
                        return Err(read_error("a marked-content reference holds an entry no reference has"));
                    }
                    let mcid = match reference.get(b"MCID").map(|mcid| self.resolve(mcid)).transpose()?.as_deref() {
                        Some(Object::Int(mcid)) => *mcid,
                        _ => return Err(read_error("a marked-content reference has no whole-number /MCID")),
                    };
                    if facts.marked.len() >= MAX_STRUCT_MARKED {
                        return Err(Fail::TooComplex);
                    }
                    let page = match reference.get(b"Pg") {
                        Some(Object::Ref(own)) => Some(*own),
                        _ => page,
                    };
                    let form = match reference.get(b"Stm") {
                        Some(Object::Ref(form)) => Some(*form),
                        _ => None,
                    };
                    facts.marked.push((owner, mcid));
                    links.kids.push(MarkedKid { element: owner, page, form, mcid });
                    parts.push(b'm');
                    parts.extend_from_slice(&owner.to_le_bytes());
                    parts.extend_from_slice(&mcid.to_le_bytes());
                }
                Object::Dict(reference) if reference.name(b"Type") == Some(b"OBJR") => {
                    if reference.iter().any(|(key, _)| !matches!(key.as_slice(), b"Type" | b"Pg" | b"Obj")) {
                        return Err(read_error("an object reference holds an entry no reference has"));
                    }
                    let Some(Object::Ref(target)) = reference.get(b"Obj") else {
                        return Err(read_error("an object reference does not name an object"));
                    };
                    if facts.objects.len() >= MAX_STRUCT_MARKED {
                        return Err(Fail::TooComplex);
                    }
                    let (digest, _) = self.digest_ref(*target)?;
                    facts.objects.push((owner, digest));
                    links.objects.push((owner, *target));
                    links.object_pages.push(match reference.get(b"Pg") {
                        Some(Object::Ref(own)) => Some(*own),
                        _ => page,
                    });
                    parts.push(b'o');
                    parts.extend_from_slice(&owner.to_le_bytes());
                    parts.extend_from_slice(&digest);
                }
                Object::Dict(element) => {
                    let index = elements;
                    elements = elements.checked_add(1).filter(|count| *count as usize <= MAX_STRUCT_ELEMENTS).ok_or(Fail::TooComplex)?;
                    links.parents.push(owner);
                    if let Some(r) = kid_ref {
                        element_places.insert(r, index);
                    }
                    let page = match element.get(b"Pg") {
                        Some(Object::Ref(own)) => Some(*own),
                        _ => page,
                    };
                    links.element_pages.push(page);
                    // Its text, whatever the value is (a reader may show a value that is not a
                    // string; a value of another kind is still compared).
                    for (key_index, key) in (0u8..).zip(STRUCT_TEXT_KEYS) {
                        if let Some(value) = element.get(key) {
                            if facts.text.len() >= MAX_STRUCT_ELEMENTS {
                                return Err(Fail::TooComplex);
                            }
                            let digest = self.digest(value)?;
                            facts.text.push((index, key_index, digest));
                        }
                    }
                    parts.push(b'{');
                    parts.extend_from_slice(&owner.to_le_bytes());
                    // `/K` is walked (the kids); `/P`, `/Pg` and `/Obj` are links.
                    let unkidded: Dict = element.iter().filter(|(key, _)| key.as_slice() != b"K").map(|(k, v)| (k.clone(), v.clone())).collect();
                    parts.extend_from_slice(&self.digest_without_links(&unkidded, &[b"P", b"Pg", b"Obj"])?);
                    shape.push(b'{');
                    shape.extend_from_slice(&owner.to_le_bytes());
                    let untexted: Dict =
                        unkidded.iter().filter(|(key, _)| !STRUCT_TEXT_KEYS.contains(&key.as_slice())).map(|(k, v)| (k.clone(), v.clone())).collect();
                    shape.extend_from_slice(&self.digest_without_links(&untexted, &[b"P", b"Pg", b"Obj"])?);
                    if let Some(kids) = element.get(b"K") {
                        todo.push((kids.clone(), depth.saturating_add(1), index, page));
                    }
                }
                Object::Null => {}
                _ => return Err(read_error("a structure element is not a dictionary")),
            }
        }
        for key in [b"ParentTree".as_slice(), b"IDTree"] {
            if let Some(index) = root.get(key) {
                let mut encoded = Vec::new();
                self.index_encode(index, &element_places, 0, &mut BTreeSet::new(), &mut encoded)?;
                let digest = sha256(&[key, &encoded]);
                parts.extend_from_slice(&digest);
                shape.extend_from_slice(&digest);
            }
        }
        facts.digest = sha256(&[b"structure", &parts]);
        facts.shape = sha256(&[b"structure shape", &shape]);
        Ok((facts, links))
    }

    /// Embed build (follow-up 7 audit, item 4): the structure tree's parent tree or ID tree as
    /// compared: every value as written, but a reference to a structure element by its place
    /// in the tree (see [`StructFacts`]), so the index of a rewrite that kept the tree's shape
    /// encodes alike; any other reference is followed (read for this fact). A stream, or a loop,
    /// is refused.
    fn index_encode(
        &mut self,
        value: &Object,
        places: &HashMap<ObjRef, u32>,
        depth: usize,
        seen: &mut BTreeSet<ObjRef>,
        out: &mut Vec<u8>,
    ) -> Result<(), Fail> {
        self.budget.tick()?;
        if depth > MAX_REFERENCE_DEPTH {
            return Err(Fail::TooComplex);
        }
        match value {
            Object::Ref(r) => match places.get(r) {
                Some(place) => {
                    out.push(b'E');
                    out.extend_from_slice(&place.to_le_bytes());
                }
                None => {
                    if !seen.insert(*r) {
                        return Err(read_error("the structure tree's index loops"));
                    }
                    let found = self.get(*r)?;
                    out.push(b'R');
                    self.index_encode(&found, places, depth.saturating_add(1), seen, out)?;
                }
            },
            Object::Array(items) => {
                out.push(b'[');
                out.extend_from_slice(&(items.len() as u64).to_le_bytes());
                for item in items {
                    self.index_encode(item, places, depth.saturating_add(1), seen, out)?;
                }
            }
            Object::Dict(dict) => {
                out.push(b'<');
                out.extend_from_slice(&(dict.len() as u64).to_le_bytes());
                let mut entries: Vec<(&Vec<u8>, &Object)> = dict.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                for (key, value) in entries {
                    out.extend_from_slice(&(key.len() as u64).to_le_bytes());
                    out.extend_from_slice(key);
                    self.index_encode(value, places, depth.saturating_add(1), seen, out)?;
                }
            }
            Object::Stream(_) => return Err(read_error("the structure tree's index holds a stream")),
            scalar => {
                out.push(b'v');
                out.extend_from_slice(&self.digest(scalar)?);
            }
        }
        Ok(())
    }

    /// The document information dictionary (the trailer's `/Info`), key by key, each value by
    /// its digest (R6b-42); empty when there is none (or it is `null`). PDFium reads it only
    /// through a reference, other readers directly too, so anything but a reference to a
    /// dictionary (or to `null`) is refused: a stream, a dictionary written in the trailer, ….
    fn document_information(&mut self, trailer: &Dict) -> Result<BTreeMap<Vec<u8>, Digest>, Fail> {
        let refused = || read_error("the document information (/Info) is not a reference to a dictionary, which readers read differently");
        let mut out = BTreeMap::new();
        let info = match trailer.get(b"Info") {
            None | Some(Object::Null) => return Ok(out),
            Some(Object::Ref(r)) => self.get(*r)?,
            Some(_) => return Err(refused()),
        };
        let dict = match &*info {
            Object::Null => return Ok(out),
            Object::Dict(dict) => dict.clone(),
            _ => return Err(refused()),
        };
        for (key, value) in dict.iter() {
            self.budget.tick()?;
            out.insert(key.clone(), self.digest(value)?);
        }
        Ok(out)
    }

    /// The document's default layer configuration (see [`LayerConfig`]).
    fn layer_config(&mut self, catalog: &Dict) -> Result<Option<LayerConfig>, Fail> {
        let Some(properties) = self.dict_of(catalog.get(b"OCProperties"), "/OCProperties")? else { return Ok(None) };
        let mut problem = None;
        let groups_of = |this: &mut Self, o: Option<&Object>, what: &str, problem: &mut Option<String>| -> Result<BTreeSet<ObjRef>, Fail> {
            let mut out = BTreeSet::new();
            for item in this.array_of(o, what)? {
                match item {
                    Object::Ref(r) => {
                        out.insert(r);
                    }
                    _ => *problem = Some(format!("{what} holds something other than a reference")),
                }
            }
            Ok(out)
        };
        let (mut groups, mut order) = (HashMap::new(), Vec::new());
        for item in self.array_of(properties.get(b"OCGs"), "/OCGs")? {
            match item {
                Object::Ref(r) => {
                    if let std::collections::hash_map::Entry::Vacant(slot) = groups.entry(r) {
                        slot.insert(order.len());
                        order.push(r);
                    }
                }
                _ => problem = Some("/OCGs holds something other than a reference".into()),
            }
        }
        let config = self.dict_of(properties.get(b"D"), "/OCProperties /D")?;
        let Some(config) = config else {
            return Ok(Some(LayerConfig {
                groups,
                order,
                base_off: false,
                on: BTreeSet::new(),
                off: BTreeSet::new(),
                problem: Some("no default configuration (/D)".into()),
            }));
        };
        let base_off = match config.get(b"BaseState").map(|state| self.resolve(state)).transpose()?.as_deref() {
            None => false,
            Some(Object::Name(name)) if name.as_slice() == b"ON" => false,
            Some(Object::Name(name)) if name.as_slice() == b"OFF" => true,
            Some(_) => {
                problem = Some("a /BaseState other than /ON or /OFF".into());
                false
            }
        };
        let on = groups_of(self, config.get(b"ON"), "/ON", &mut problem)?;
        let off = groups_of(self, config.get(b"OFF"), "/OFF", &mut problem)?;
        if on.intersection(&off).next().is_some() {
            problem = Some("a group both in /ON and in /OFF".into());
        }
        if !self.array_of(config.get(b"AS"), "/AS")?.is_empty() {
            problem = Some("automatic states (/AS), which viewers apply each their own way".into());
        }
        if let Some(intent) = config.get(b"Intent")
            && !self.only_view(intent)?
        {
            problem = Some("an /Intent other than /View".into());
        }
        Ok(Some(LayerConfig { groups, order, base_off, on, off, problem }))
    }

    /// Whether an `/Intent` is `/View` (or an array of just that).
    fn only_view(&mut self, intent: &Object) -> Result<bool, Fail> {
        Ok(match &*self.resolve(intent)? {
            Object::Name(name) => name.as_slice() == b"View",
            Object::Array(names) => !names.is_empty() && names.iter().all(|name| name.as_name() == Some(b"View".as_slice())),
            _ => false,
        })
    }

    /// One mark (see [`LayerMark`]) against the configuration: `None` when it is not optional
    /// content (a property list of another kind, under another tag); else what it is (see
    /// [`MarkFact`]). Anything readers may evaluate differently fails.
    fn evaluate_mark(&mut self, mark: &LayerMark, config: Option<&LayerConfig>) -> Result<Option<MarkFact>, Fail> {
        let tagged_oc = mark.tag.as_deref().is_none_or(|tag| tag == b"OC");
        if mark.inline {
            return Err(unevaluable("a property list written in the content names optional content"));
        }
        let Some((num, generation)) = mark.object else {
            return if tagged_oc { Err(unevaluable("optional content that is not an object of its own")) } else { Ok(None) };
        };
        let (Ok(num), Ok(generation)) = (u32::try_from(num), u16::try_from(generation)) else {
            return Err(unevaluable("an object number no file has"));
        };
        let r = ObjRef::new(num, generation);
        let obj = self.get(r)?;
        let Object::Dict(dict) = &*obj else {
            return if tagged_oc { Err(unevaluable("optional content that is not a dictionary")) } else { Ok(None) };
        };
        let kind = dict.name(b"Type").map(<[u8]>::to_vec);
        let is_layer = matches!(kind.as_deref(), Some(b"OCG" | b"OCMD"));
        if !tagged_oc {
            // hayro hides content in a group named under another tag; other readers do not.
            return if is_layer { Err(unevaluable("a group named by a marked-content tag other than /OC")) } else { Ok(None) };
        }
        let Some(config) = config else { return Err(unevaluable("optional content in a file without /OCProperties")) };
        if let Some(problem) = &config.problem {
            return Err(unevaluable(problem));
        }
        let mut usage = false;
        let visibility = match kind.as_deref() {
            Some(b"OCG") => self.group(r, config, &mut usage)?,
            Some(b"OCMD") => self.membership(dict, config, &mut usage)?,
            _ => return Err(unevaluable("a /OC that is neither a group nor a membership dictionary")),
        };
        let visible = visibility.shows(&|index| config.shows(index));
        Ok(Some(MarkFact { digest: self.digest_ref(r)?.0, visibility: Arc::new(visibility), visible, usage }))
    }

    /// Group `r`, by its place in `/OCGs`; `usage` is set when it has `/Usage`.
    fn group(&mut self, r: ObjRef, config: &LayerConfig, usage: &mut bool) -> Result<Visibility, Fail> {
        let Some(index) = config.groups.get(&r).copied() else { return Err(unevaluable("a group not listed in /OCProperties /OCGs")) };
        let Object::Dict(group) = &*self.get(r)? else { return Err(unevaluable("a group that is not a dictionary")) };
        if group.name(b"Type") != Some(b"OCG") {
            return Err(unevaluable("a membership names something other than a group"));
        }
        if let Some(intent) = group.get(b"Intent")
            && !self.only_view(intent)?
        {
            return Err(unevaluable("a group with an /Intent other than /View"));
        }
        *usage |= group.contains(b"Usage");
        Ok(Visibility::Group(index))
    }

    /// When content in membership dictionary `dict` shows: by its visibility expression (`/VE`)
    /// where it has one, else by its policy (`/P`) over its groups.
    fn membership(&mut self, dict: &Dict, config: &LayerConfig, usage: &mut bool) -> Result<Visibility, Fail> {
        if let Some(expression) = dict.get(b"VE") {
            return self.expression(expression, config, 0, usage);
        }
        let groups = match dict.get(b"OCGs") {
            Some(Object::Ref(r)) if matches!(&*self.get(*r)?, Object::Dict(_)) => vec![Object::Ref(*r)],
            other => self.array_of(other, "a membership's /OCGs")?,
        };
        if groups.is_empty() {
            return Err(unevaluable("a membership dictionary with no groups"));
        }
        let mut shown = Vec::with_capacity(groups.len());
        for group in groups {
            let Object::Ref(r) = group else { return Err(unevaluable("a membership's /OCGs holds something other than a group")) };
            shown.push(self.group(r, config, usage)?);
        }
        let policy = match dict.get(b"P").map(|p| self.resolve(p)).transpose()?.as_deref() {
            None => b"AnyOn".to_vec(),
            Some(Object::Name(name)) => name.clone(),
            Some(_) => return Err(unevaluable("a membership policy that is not a name")),
        };
        let off = |shown: Vec<Visibility>| shown.into_iter().map(|group| Visibility::Not(Box::new(group))).collect();
        Ok(match policy.as_slice() {
            b"AllOn" => Visibility::All(shown),
            b"AnyOn" => Visibility::Any(shown),
            b"AnyOff" => Visibility::Any(off(shown)),
            b"AllOff" => Visibility::All(off(shown)),
            _ => return Err(unevaluable("an unknown membership policy")),
        })
    }

    /// A visibility expression (ISO 32000-2 §8.11.2.2): `[/And …]`, `[/Or …]`, `[/Not x]` over
    /// groups and expressions.
    fn expression(&mut self, expression: &Object, config: &LayerConfig, depth: usize, usage: &mut bool) -> Result<Visibility, Fail> {
        if depth > MAX_VISIBILITY_DEPTH {
            return Err(unevaluable("a visibility expression nested too deep"));
        }
        if let Object::Ref(r) = expression
            && let Object::Dict(_) = &*self.get(*r)?
        {
            return self.group(*r, config, usage);
        }
        let items = self.array_of(Some(expression), "a visibility expression")?;
        let Some((operator, operands)) = items.split_first() else { return Err(unevaluable("an empty visibility expression")) };
        let mut values = Vec::with_capacity(operands.len());
        for operand in operands {
            self.budget.tick()?;
            values.push(self.expression(operand, config, depth.saturating_add(1), usage)?);
        }
        match (operator.as_name(), values.len()) {
            (Some(b"Not"), 1) => Ok(Visibility::Not(Box::new(values.into_iter().next().ok_or_else(|| unevaluable("an empty /Not"))?))),
            (Some(b"And"), 1..) => Ok(Visibility::All(values)),
            (Some(b"Or"), 1..) => Ok(Visibility::Any(values)),
            _ => Err(unevaluable("a malformed visibility expression")),
        }
    }

    /// An annotation's destination in this file and the entry that gives it: `/Dest`, or `/A`
    /// when it is a plain go-to action (only `/Type`, `/S /GoTo` and `/D`), as for an outline
    /// entry. `None` when it has neither (any other action stays part of its digest).
    fn link_destination(&mut self, dict: &Dict, pages: &HashMap<ObjRef, usize>) -> Result<Option<(Destination, &'static [u8])>, Fail> {
        if let Some(dest) = dict.get(b"Dest") {
            if dict.contains(b"A") {
                return Ok(None);
            }
            return Ok(Some((self.destination(dest, pages)?, b"Dest")));
        }
        let Some(action) = dict.get(b"A") else { return Ok(None) };
        let goto = match &*self.resolve(action)? {
            Object::Dict(a) if a.name(b"S") == Some(b"GoTo") && a.iter().all(|(key, _)| matches!(key.as_slice(), b"Type" | b"S" | b"D")) => {
                a.get(b"D").cloned()
            }
            _ => None,
        };
        match goto {
            Some(dest) => Ok(Some((self.destination(&dest, pages)?, b"A"))),
            None => Ok(None),
        }
    }

    /// A redaction mark's areas, by the rule the redactor reads them with
    /// (`pdfcraft_redact::mark_areas`): each quadrilateral's box, or the rectangle.
    fn marked_areas(&mut self, dict: &Dict) -> Result<Vec<[f64; 4]>, Fail> {
        let numbers = |items: Vec<Object>| {
            items.iter().map(|item| matches!(item, Object::Int(_) | Object::Real(_)).then(|| item.as_f64()).flatten()).collect::<Option<Vec<f64>>>()
        };
        let quads = numbers(self.array_of(dict.get(b"QuadPoints"), "/QuadPoints")?);
        let rect = numbers(self.array_of(dict.get(b"Rect"), "/Rect")?);
        Ok(pdfcraft_redact::mark_areas(quads.as_deref(), rect.as_deref()))
    }

    /// Follow-up 9 (R6b-55, P6-33): whether a `Redact` mark carries an overlay, by the host's
    /// rule: any `/OverlayText`, `/RO`, `/Repeat`, `/DA` or `/Q` entry; an `/IC` that is not
    /// black (absent, `[0]`, `[0 0 0]` and `[0 0 0 1]` are); or an `/AP` that is not plain. Plain
    /// is exactly one `/N` entry, a single stream (not a dictionary of states), whose resources
    /// hold no `/XObject`, `/Font`, `/Shading` or `/Pattern`, and whose content constructs and
    /// paints paths, sets colours, line styles and graphics states only (no text, image, form,
    /// shading or marked content).
    fn mark_overlay(&mut self, dict: &Dict) -> Result<bool, Fail> {
        if [&b"OverlayText"[..], b"RO", b"Repeat", b"DA", b"Q"].iter().any(|key| dict.contains(key)) {
            return Ok(true);
        }
        let black = match dict.get(b"IC").map(|fill| self.resolve(fill)).transpose()?.as_deref() {
            None => true,
            Some(Object::Array(parts)) => {
                let values: Option<Vec<f64>> =
                    parts.iter().map(|part| matches!(part, Object::Int(_) | Object::Real(_)).then(|| part.as_f64()).flatten()).collect();
                matches!(values.as_deref(), Some([0.0] | [0.0, 0.0, 0.0] | [0.0, 0.0, 0.0, 1.0]))
            }
            Some(_) => false,
        };
        if !black {
            return Ok(true);
        }
        let Some(appearance) = dict.get(b"AP") else { return Ok(false) };
        let Some(appearance) = self.dict_of(Some(appearance), "/AP")? else { return Ok(true) };
        if appearance.len() != 1 {
            return Ok(true);
        }
        let Some(Object::Ref(normal)) = appearance.get(b"N") else { return Ok(true) };
        let object = self.get(*normal)?;
        let Object::Stream(stream) = &*object else { return Ok(true) };
        let resources = self.dict_of(stream.dict.get(b"Resources"), "an appearance's /Resources")?.unwrap_or_default();
        if [&b"XObject"[..], b"Font", b"Shading", b"Pattern"].iter().any(|key| resources.contains(key)) {
            return Ok(true);
        }
        let data = self.decode(stream)?;
        Ok(!super::operators::plain_drawing(&data, self.budget)?)
    }

    // ── form fields ─────────────────────────────────────────────────────────────────────────

    fn fields(&mut self, form: &Dict, widgets: &mut BTreeSet<ObjRef>) -> Result<(BTreeMap<String, Digest>, Vec<String>), Fail> {
        let mut fields = BTreeMap::new();
        let mut order = Vec::new();
        let mut seen = BTreeSet::new();
        for entry in self.array_of(form.get(b"Fields"), "/Fields")? {
            self.field(&entry, "", &[], 0, &mut seen, widgets, (&mut fields, &mut order))?;
        }
        Ok((fields, order))
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
        (fields, order): (&mut BTreeMap<String, Digest>, &mut Vec<String>),
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
            // Follow-up 8 (P6-33): its widgets, where a redaction may have taken it away.
            let mut own_widgets: Vec<ObjRef> = Vec::new();
            if dict.name(b"Subtype") == Some(b"Widget") {
                own_widgets.push(*r);
            }
            for kid in self.array_of(dict.get(b"Kids"), "/Kids")? {
                if let Object::Ref(kid_ref) = kid {
                    own_widgets.push(kid_ref);
                }
            }
            self.field_widgets.entry(name.clone()).or_default().extend(own_widgets);
            let flat: Vec<u8> = path.iter().flatten().copied().collect();
            let digest = sha256(&[&flat, &widget_digests]);
            let combined = match fields.get(&name) {
                Some(earlier) => sha256(&[earlier, &digest]),
                None => digest,
            };
            order.push(name.clone());
            fields.insert(name, combined);
            return Ok(());
        }
        for child in &children {
            self.field(child, &name, &path, depth.saturating_add(1), seen, widgets, (&mut *fields, &mut *order))?;
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
        // Embed build (follow-up 7 audit, item 4): a link of the tree written as anything but a
        // reference or a plain value is compared with the rest.
        let link = |key: &[u8], value: &Object| {
            matches!(key, b"Parent" | b"Prev" | b"Next" | b"First" | b"Last" | b"Count")
                && !matches!(value, Object::Ref(_) | Object::Int(_) | Object::Real(_) | Object::Name(_) | Object::Bool(_) | Object::Null)
        };
        let mut rest: Dict =
            dict.iter().filter(|(key, value)| !TREE.contains(&key.as_slice()) || link(key, value)).map(|(k, v)| (k.clone(), v.clone())).collect();
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

    /// The named destinations (see [`Structure::named_destinations`]). A name tree is read as
    /// ISO 32000-2 §7.9.6 has it: hayro never reads it, and viewers search it each their own way
    /// (by `/Limits`, in order, or all of it), so a tree they could resolve differently is
    /// refused: keys out of order or named twice, `/Limits` that do not bound what a node holds,
    /// a node with both `/Kids` and `/Names` or with neither, a key that is not a string.
    ///
    /// A node's entries other than `/Kids`, `/Names` and `/Limits` are returned too, by their
    /// values' digests in the tree's order (no viewer reads them; any change to them is a change).
    fn named_destinations(&mut self, catalog: &Dict, pages: &HashMap<ObjRef, usize>) -> Result<NamedDestinations, Fail> {
        let mut out = BTreeMap::new();
        let mut keys = Vec::new();
        if let Some(dests) = self.dict_of(catalog.get(b"Dests"), "/Dests")? {
            for (name, value) in dests.iter() {
                self.budget.tick()?;
                let destination = self.named_value(value, pages)?;
                out.insert((NameSource::Dests, name.clone()), destination);
            }
        }
        let names = self.dict_of(catalog.get(b"Names"), "/Names")?.unwrap_or_default();
        if let Some(tree) = names.get(b"Dests") {
            let mut entries = Vec::new();
            self.name_node(tree, 0, true, &mut BTreeSet::new(), &mut entries, &mut keys)?;
            if entries.windows(2).any(|pair| pair.first().map(|entry| &entry.0) >= pair.get(1).map(|entry| &entry.0)) {
                return Err(read_error("the named destinations' tree has keys out of order or named twice"));
            }
            for (name, value) in entries {
                self.budget.tick()?;
                let destination = self.named_value(&value, pages)?;
                out.insert((NameSource::Names, name), destination);
            }
        }
        Ok((out, keys))
    }

    /// One node of a name tree and everything under it, its entries appended to `out` in order.
    fn name_node(
        &mut self,
        node: &Object,
        depth: usize,
        root: bool,
        seen: &mut BTreeSet<ObjRef>,
        out: &mut Vec<(Vec<u8>, Object)>,
        keys: &mut Vec<(Vec<u8>, Digest)>,
    ) -> Result<(), Fail> {
        self.budget.tick()?;
        if depth > MAX_TREE_DEPTH {
            return Err(read_error("a name tree is too deep"));
        }
        if let Object::Ref(r) = node
            && !seen.insert(*r)
        {
            return Err(read_error("a name tree loops"));
        }
        let Object::Dict(dict) = &*self.resolve(node)? else { return Err(read_error("a name tree node is not a dictionary")) };
        for (key, value) in dict.iter() {
            if !matches!(key.as_slice(), b"Kids" | b"Names" | b"Limits") {
                let digest = self.digest(value)?;
                keys.push((key.clone(), digest));
            }
        }
        let start = out.len();
        match (dict.get(b"Kids"), dict.get(b"Names")) {
            (Some(_), Some(_)) => return Err(read_error("a name tree node has both /Kids and /Names")),
            (Some(kids), None) => {
                for kid in self.array_of(Some(kids), "a name tree's /Kids")? {
                    self.name_node(&kid, depth.saturating_add(1), false, seen, out, keys)?;
                }
            }
            (None, Some(names)) => {
                let items = self.array_of(Some(names), "a name tree's /Names")?;
                if items.len() % 2 != 0 {
                    return Err(read_error("a name tree's /Names has a key without a value"));
                }
                for pair in items.chunks(2) {
                    let (Some(key), Some(value)) = (pair.first(), pair.get(1)) else { continue };
                    let Object::String(key) = &*self.resolve(key)? else { return Err(read_error("a name tree's key is not a string")) };
                    if out.len() >= MAX_NAME_TREE_ENTRIES {
                        return Err(Fail::TooComplex);
                    }
                    out.push((key.bytes.clone(), value.clone()));
                }
            }
            (None, None) if root => {}
            (None, None) => return Err(read_error("a name tree node has neither /Kids nor /Names")),
        }
        if let Some(limits) = dict.get(b"Limits") {
            let limits = self.array_of(Some(limits), "a name tree's /Limits")?;
            let mut bounds = Vec::with_capacity(2);
            for limit in &limits {
                match &*self.resolve(limit)? {
                    Object::String(limit) => bounds.push(limit.bytes.clone()),
                    _ => return Err(read_error("a name tree's /Limits are not strings")),
                }
            }
            let held = out.get(start..).unwrap_or_default();
            let (Some(first), Some(last)) = (held.first(), held.last()) else { return Err(read_error("a name tree's /Limits bound an empty node")) };
            if bounds.as_slice() != [first.0.clone(), last.0.clone()] {
                return Err(read_error("a name tree's /Limits do not bound what the node holds"));
            }
        }
        Ok(())
    }

    /// The page labels, page by page (see [`PageLabels`], R6b-46): the catalog's `/PageLabels`
    /// number tree (ISO 32000-2 §7.9.7, §12.4.2) read strictly, since viewers search it each
    /// their own way: keys whole numbers (written directly) in increasing order, `/Limits` that
    /// bound each node, no node with both `/Kids` and `/Nums` (or neither, but for an empty
    /// root), no entry other than those; each label dictionary only `/Type /PageLabel`, `/S` one
    /// of the five styles, `/P` a string and `/St` a whole number from 1. Anything else is
    /// [`PageLabels::Unreadable`], compared whole (`digest`, the catalog entry's).
    fn page_labels(&mut self, catalog: &Dict, pages: usize, digest: Option<Digest>) -> Result<PageLabels, Fail> {
        let (Some(tree), Some(digest)) = (catalog.get(b"PageLabels"), digest) else { return Ok(PageLabels::Read(vec![None; pages])) };
        let mut entries = Vec::new();
        if !self.number_node(tree, 0, true, &mut BTreeSet::new(), &mut entries)? {
            return Ok(PageLabels::Unreadable(digest));
        }
        if entries.windows(2).any(|pair| pair.first().map(|entry| entry.0) >= pair.get(1).map(|entry| entry.0)) {
            return Ok(PageLabels::Unreadable(digest));
        }
        // Each range's label dictionary, read once.
        let mut ranges: Vec<(u64, PageLabel)> = Vec::with_capacity(entries.len());
        for (start, value) in entries {
            self.budget.tick()?;
            // Follow-up 8 (R6b-51, I1): ranges starting past the last page label no page, so
            // nothing compares them: kept by value.
            let digest = self.digest(&value)?;
            self.label_values.1.push(digest);
            if usize::try_from(start).is_ok_and(|start| start >= pages) {
                self.label_values.0.push(digest);
            }
            match self.label_spec(&value)? {
                Some(label) => ranges.push((start, label)),
                None => return Ok(PageLabels::Unreadable(digest)),
            }
        }
        let mut labels = Vec::with_capacity(pages);
        let mut range = 0usize;
        for page in 0..pages {
            self.budget.tick()?;
            let page = page as u64;
            while ranges.get(range.saturating_add(1)).is_some_and(|(start, _)| *start <= page) {
                range = range.saturating_add(1);
            }
            let label = match ranges.get(range) {
                Some((start, spec)) if *start <= page => {
                    let number = if spec.style == 0 {
                        0
                    } else {
                        // A start below 2^63 plus a page index cannot overflow; refused, not
                        // wrapped, all the same.
                        match spec.number.checked_add(page.saturating_sub(*start)) {
                            Some(number) => number,
                            None => return Ok(PageLabels::Unreadable(digest)),
                        }
                    };
                    Some(PageLabel { number, ..spec.clone() })
                }
                _ => None,
            };
            labels.push(label);
        }
        Ok(PageLabels::Read(labels))
    }

    /// One node of a number tree and everything under it, its entries appended to `out` in
    /// order. `Ok(false)`: a tree readers could read differently (see [`Reader::page_labels`]).
    fn number_node(
        &mut self,
        node: &Object,
        depth: usize,
        root: bool,
        seen: &mut BTreeSet<ObjRef>,
        out: &mut Vec<(u64, Object)>,
    ) -> Result<bool, Fail> {
        self.budget.tick()?;
        if depth > MAX_TREE_DEPTH {
            return Ok(false);
        }
        if let Object::Ref(r) = node
            && !seen.insert(*r)
        {
            return Ok(false);
        }
        let Object::Dict(dict) = &*self.resolve(node)? else { return Ok(false) };
        if dict.iter().any(|(key, _)| !matches!(key.as_slice(), b"Kids" | b"Nums" | b"Limits")) {
            return Ok(false);
        }
        let start = out.len();
        match (dict.get(b"Kids"), dict.get(b"Nums")) {
            (Some(_), Some(_)) => return Ok(false),
            (Some(kids), None) => {
                let Object::Array(kids) = &*self.resolve(kids)? else { return Ok(false) };
                for kid in kids {
                    if !self.number_node(kid, depth.saturating_add(1), false, seen, out)? {
                        return Ok(false);
                    }
                }
            }
            (None, Some(nums)) => {
                let Object::Array(items) = &*self.resolve(nums)? else { return Ok(false) };
                if items.len() % 2 != 0 {
                    return Ok(false);
                }
                for pair in items.chunks(2) {
                    let (Some(Object::Int(key)), Some(value)) = (pair.first(), pair.get(1)) else { return Ok(false) };
                    let Ok(key) = u64::try_from(*key) else { return Ok(false) };
                    if out.len() >= MAX_NAME_TREE_ENTRIES {
                        return Err(Fail::TooComplex);
                    }
                    out.push((key, value.clone()));
                }
            }
            (None, None) if root => {}
            (None, None) => return Ok(false),
        }
        if let Some(limits) = dict.get(b"Limits") {
            let Object::Array(limits) = &*self.resolve(limits)? else { return Ok(false) };
            let held = out.get(start..).unwrap_or_default();
            let (Some(first), Some(last)) = (held.first(), held.last()) else { return Ok(false) };
            let bounds: Vec<Option<u64>> =
                limits.iter().map(|limit| if let Object::Int(limit) = limit { u64::try_from(*limit).ok() } else { None }).collect();
            if bounds.as_slice() != [Some(first.0), Some(last.0)] {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// A label dictionary as the label of its range's first page (`number` its `/St`); `None`
    /// where readers could form it differently (see [`Reader::page_labels`]).
    fn label_spec(&mut self, value: &Object) -> Result<Option<PageLabel>, Fail> {
        let Object::Dict(dict) = &*self.resolve(value)? else { return Ok(None) };
        let mut label = PageLabel { style: 0, prefix: None, number: 1 };
        for (key, value) in dict.iter() {
            let value = self.resolve(value)?;
            match (key.as_slice(), &*value) {
                (b"Type", Object::Name(name)) if name.as_slice() == b"PageLabel" => {}
                (b"S", Object::Name(name)) => match name.as_slice() {
                    [style @ (b'D' | b'R' | b'r' | b'A' | b'a')] => label.style = *style,
                    _ => return Ok(None),
                },
                (b"P", Object::String(prefix)) => label.prefix = (!prefix.bytes.is_empty()).then(|| sha256(&[b"prefix", &prefix.bytes])),
                (b"St", Object::Int(first)) if *first >= 1 => label.number = u64::try_from(*first).unwrap_or(1),
                _ => return Ok(None),
            }
        }
        Ok(Some(label))
    }

    /// A named destination's value: an explicit destination, or a dictionary whose `/D` is one
    /// (a dictionary with other entries is compared whole).
    fn named_value(&mut self, value: &Object, pages: &HashMap<ObjRef, usize>) -> Result<Destination, Fail> {
        match &*self.resolve(value)? {
            Object::Dict(dict) if dict.iter().all(|(key, _)| key.as_slice() == b"D") => match dict.get(b"D") {
                Some(dest) => self.destination(dest, pages),
                None => Ok(Destination::Other(self.digest(value)?)),
            },
            Object::Array(_) => self.destination(value, pages),
            _ => Ok(Destination::Other(self.digest(value)?)),
        }
    }

    /// An explicit destination by its page's index and its view; anything else by its digest.
    /// A reference that names no page of this file (a missing object among them) goes nowhere:
    /// `Unpaged`; a page that is `null` too: `NullPage` (R6b-37). A page given by its number is
    /// that page of this file (`Index`), or nowhere when no page has that number (R6b-43); a
    /// number that is not whole is refused, since PDFium takes its whole part and pdf.js none.
    fn destination(&mut self, dest: &Object, pages: &HashMap<ObjRef, usize>) -> Result<Destination, Fail> {
        let resolved = self.resolve(dest)?;
        if let Object::Array(items) = &*resolved
            && let Some(page @ (Object::Ref(_) | Object::Null | Object::Int(_) | Object::Real(_))) = items.first()
        {
            let view = self.digest(&Object::Array(items.iter().skip(1).cloned().collect()))?;
            let numbered = |index: Option<usize>| match index {
                Some(index) if index < pages.len() => Destination::Index(index, view),
                _ => Destination::Unpaged(view),
            };
            return Ok(match page {
                Object::Ref(page) => match pages.get(page) {
                    Some(index) => Destination::Page(*index, view),
                    None => Destination::Unpaged(view),
                },
                Object::Int(number) => numbered(usize::try_from(*number).ok()),
                Object::Real(number) if number.is_finite() && number.fract() == 0.0 => {
                    numbered((*number >= 0.0 && *number < pages.len() as f64).then_some(*number as usize))
                }
                Object::Real(_) => {
                    return Err(read_error("a destination gives its page as a number that is not whole, which readers read differently"));
                }
                _ => Destination::NullPage(view),
            });
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

/// The trailer dictionary of the cross-reference section at `offset` (a stream's dictionary).
pub(crate) fn trailer_at(data: &[u8], offset: u64, is_stream: bool) -> Result<Dict, Fail> {
    let unreadable = || read_error(format!("the cross-reference section at {offset} cannot be read"));
    let at = usize::try_from(offset).map_err(|_| unreadable())?;
    if is_stream {
        let (_, obj) = parse_indirect(data, at, &|_| None).map_err(|_| unreadable())?;
        obj.as_dict().cloned().ok_or_else(unreadable)
    } else {
        let rest = data.get(at..).ok_or_else(unreadable)?;
        let found = rest.windows(7).position(|w| w == b"trailer").ok_or_else(unreadable)?;
        let mut lx = Lexer::new(data, at.saturating_add(found).saturating_add(7));
        lx.object().ok().and_then(|o| o.as_dict().cloned()).ok_or_else(unreadable)
    }
}

/// The trailer of the cross-reference section at `offset`.
fn section(data: &[u8], offset: u64, is_stream: bool) -> Result<Section, Fail> {
    let trailer = trailer_at(data, offset, is_stream)?;
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
    /// See `Reader::names_apart`.
    names_apart: bool,
    ocg_index: Arc<HashMap<ObjRef, usize>>,
    checked: HashMap<u32, u16>,
    /// See `Reader::facts`.
    facts: HashSet<u32>,
    object_streams: HashMap<u32, Arc<Vec<u32>>>,
    digests: HashMap<ObjRef, Digest>,
    images: HashMap<ObjRef, Arc<ImageInfo>>,
    /// Content streams' decoded data as `pdfcraft_cos` decodes it, by digest.
    decoded: HashMap<ObjRef, Digest>,
    /// The same as hayro decoded them (see `Reader::hayro_decoded`).
    hayro_decoded: Arc<HashMap<ObjRef, Digest>>,
    /// Streams whose `/Length` is not their data's (see `Reader::misdeclared`).
    misdeclared: BTreeSet<ObjRef>,
    /// See [`Tolerated`].
    tolerated: Arc<Tolerated>,
    /// Streams whose data hayro read as the same bytes.
    agreed: BTreeSet<ObjRef>,
    /// The document's default layer configuration (`None`: no `/OCProperties`).
    layer_config: Option<LayerConfig>,
    /// Each optional-content mark evaluated (see [`Known::layer`]).
    marks: HashMap<LayerMark, Option<MarkFact>>,
    /// See `Reader::numbered`.
    numbered: bool,
}

/// The document's default optional-content configuration (`/OCProperties /D`), as far as it
/// can be evaluated: which groups exist, and which show.
pub(crate) struct LayerConfig {
    /// `/OCGs`: every group, by its place in the array (its first, if listed twice). The place,
    /// not the object, names a group across the two files: a rewrite renumbers objects, and the
    /// configuration (compared whole) is the same in both wherever a page is compared in views.
    groups: HashMap<ObjRef, usize>,
    /// The groups in that order (each once).
    order: Vec<ObjRef>,
    /// `/BaseState /OFF`: groups show only when listed in `/ON`.
    base_off: bool,
    on: BTreeSet<ObjRef>,
    off: BTreeSet<ObjRef>,
    /// Why the configuration cannot be evaluated (no `/D`, `/BaseState /Unchanged`, a group
    /// both on and off, automatic states `/AS`, another intent than `/View`, entries that are
    /// not groups): readers apply these each their own way.
    problem: Option<String>,
}

impl LayerConfig {
    /// Whether the group at `index` in `/OCGs` shows by default: by `/D` alone, as PDFium
    /// evaluates a group without `/Usage` (`/BaseState`, then `/ON`, then `/OFF`; a group both
    /// on and off, `/AS` and `/BaseState /Unchanged` are refused: see `problem`).
    fn shows(&self, index: usize) -> bool {
        let Some(group) = self.order.get(index) else { return false };
        if self.base_off { self.on.contains(group) } else { !self.off.contains(group) }
    }
}

/// When content inside a group or membership dictionary shows, as a function of which groups
/// are on (R6b-36): each group by its place in `/OCProperties /OCGs`. A membership dictionary's
/// policy over its groups, or its visibility expression, written as one. Its depth is bounded
/// by [`MAX_VISIBILITY_DEPTH`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Visibility {
    Group(usize),
    Not(Box<Visibility>),
    All(Vec<Visibility>),
    Any(Vec<Visibility>),
}

impl Visibility {
    /// Whether it shows when exactly the groups `on` answers true for are on.
    pub fn shows(&self, on: &dyn Fn(usize) -> bool) -> bool {
        match self {
            Visibility::Group(group) => on(*group),
            Visibility::Not(inner) => !inner.shows(on),
            Visibility::All(items) => items.iter().all(|item| item.shows(on)),
            Visibility::Any(items) => items.iter().any(|item| item.shows(on)),
        }
    }

    /// The groups it depends on, added to `out`.
    pub fn groups(&self, out: &mut BTreeSet<usize>) {
        match self {
            Visibility::Group(group) => {
                out.insert(*group);
            }
            Visibility::Not(inner) => inner.groups(out),
            Visibility::All(items) | Visibility::Any(items) => items.iter().for_each(|item| item.groups(out)),
        }
    }
}

/// The optional content a page's content is inside (R6b-36).
#[derive(Clone, Debug, Default)]
pub(crate) struct PageLayers {
    /// Each group and membership dictionary the content is inside, as hayro names it, and when
    /// it shows.
    pub marks: Vec<(LayerObject, Arc<Visibility>)>,
    /// A group the content is inside (directly, or through a membership dictionary) has a
    /// `/Usage` entry, which readers apply differently (PDFium applies `/Usage /View /ViewState`
    /// before `/D`; others ignore it).
    pub usage: bool,
}

impl PageLayers {
    /// The groups the content's visibility depends on, added to `out`.
    pub fn groups(&self, out: &mut BTreeSet<usize>) {
        for (_, visibility) in &self.marks {
            visibility.groups(out);
        }
    }

    /// The groups and membership dictionaries that do not show when exactly the groups `on`
    /// answers true for are on, as hayro names them (for a render of that view).
    pub fn hidden(&self, on: &dyn Fn(usize) -> bool) -> Vec<LayerObject> {
        self.marks.iter().filter(|(_, visibility)| !visibility.shows(on)).map(|(object, _)| *object).collect()
    }
}

/// One optional-content mark evaluated: the group's or membership dictionary's digest, when it
/// shows, whether it shows by default, and whether a group it depends on has `/Usage`.
#[derive(Clone, Debug)]
struct MarkFact {
    digest: Digest,
    visibility: Arc<Visibility>,
    visible: bool,
    usage: bool,
}

/// The membership of content in optional content (R6b-29): the groups and membership
/// dictionaries it is inside, outermost first, each by its digest (a rewrite renumbers them), and
/// whether it shows in the document's default configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct LayerFact {
    pub digest: Digest,
    pub visible: bool,
}

/// A group or membership dictionary as hayro names it: number, generation.
pub(crate) type LayerObject = (i32, i32);

/// The deepest visibility expression (`/VE`) evaluated.
const MAX_VISIBILITY_DEPTH: usize = 16;

fn unevaluable(why: &str) -> Fail {
    read_error(format!("optional content that cannot be evaluated against the document's /OCProperties: {why}"))
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

    /// The optional-content membership `marks` make (outermost first): `None` when none of them
    /// is optional content; fails on any mark that cannot be evaluated against the document's
    /// `/OCProperties` (see `Reader::evaluate_mark`). Content shows by default when every group
    /// or membership dictionary it is inside shows.
    pub fn layer(&mut self, doc: &Document, budget: &mut Budget, marks: &[LayerMark]) -> Result<Option<LayerFact>, Fail> {
        let mut parts: Vec<u8> = Vec::new();
        let mut visible = true;
        let mut any = false;
        for mark in marks {
            let Some(fact) = self.mark(doc, budget, mark)? else { continue };
            any = true;
            visible &= fact.visible;
            parts.extend_from_slice(&fact.digest);
        }
        Ok(any.then(|| LayerFact { digest: sha256(&[b"layers", &parts, &[u8::from(visible)]]), visible }))
    }

    /// Of `marks` (every mark met on a page), each group and membership dictionary as hayro
    /// names it, with when it shows, and whether any depends on a group with `/Usage` (see
    /// [`PageLayers`]; empty: no optional content).
    pub fn page_layers(&mut self, doc: &Document, budget: &mut Budget, marks: &[LayerMark]) -> Result<PageLayers, Fail> {
        let mut layers = PageLayers::default();
        let mut seen = BTreeSet::new();
        for mark in marks {
            let Some(fact) = self.mark(doc, budget, mark)? else { continue };
            layers.usage |= fact.usage;
            // An evaluated mark always names its object.
            let Some(object) = mark.object else { continue };
            if seen.insert(object) {
                layers.marks.push((object, fact.visibility));
            }
        }
        Ok(layers)
    }

    fn mark(&mut self, doc: &Document, budget: &mut Budget, mark: &LayerMark) -> Result<Option<MarkFact>, Fail> {
        if let Some(found) = self.marks.get(mark) {
            return Ok(found.clone());
        }
        budget.tick()?;
        let config = self.layer_config.take();
        let mut reader = self.reader(doc, budget);
        let found = reader.evaluate_mark(mark, config.as_ref());
        self.keep(reader);
        self.layer_config = config;
        let found = found?;
        self.marks.insert(mark.clone(), found.clone());
        Ok(found)
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

    /// Whether object `num` was read for a fact (not only because hayro read it to open the file
    /// or draw a page).
    pub fn was_read(&self, num: u32) -> bool {
        self.facts.contains(&num)
    }

    /// Whether an object read for a fact holds a destination that gives its page by number,
    /// outside the destinations resolved on their own (see [`Structure::numbered_destinations`]).
    pub fn numbered_destinations(&self) -> bool {
        self.numbered
    }

    /// The first stream whose `/Length` is not its data's length that hayro did not read as the
    /// same data (`None`: there is none): what its data is depends on the reader.
    pub fn unconfirmed_length(&self) -> Option<ObjRef> {
        self.misdeclared.difference(&self.agreed).next().copied()
    }

    /// The streams read whose `/Length` is not their data's, each with the offset it was read
    /// from (see [`Structure::misdeclared`]).
    pub fn misdeclared_at(&self, doc: &Document) -> BTreeSet<(ObjRef, u64)> {
        self.misdeclared
            .iter()
            .filter_map(|r| match doc.xref_entry(r.num) {
                Some(XrefEntry::InFile { offset, generation }) if generation == r.generation => Some((*r, offset)),
                _ => None,
            })
            .collect()
    }

    /// Check that `pdfcraft_cos` decodes every content stream hayro decoded to interpret it (see
    /// `Reader::hayro_decoded`) to the same data. Most were checked as they were keyed; this
    /// checks the rest.
    pub fn agree_decoded(&mut self, doc: &Document, budget: &mut Budget) -> Result<(), Fail> {
        let hayro_decoded = self.hayro_decoded.clone();
        let mut reader = self.reader(doc, budget);
        reader.agreeing = true;
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

/// An object's canonical form as `pdfcraft_cos` parsed it, written as hayro's parse is written
/// (`pdfcraft_render::content::object_shape`), so that the two can be compared.
fn shape(object: &Object, depth: usize, out: &mut Vec<u8>) {
    use pdfcraft_render::content::{SHAPE_DEPTH, SHAPE_LEFT_OUT, ShapeItem, object_shape};
    if depth > SHAPE_DEPTH {
        out.push(b'!');
        return;
    }
    let dict = |dict: &Dict, left_out: &[&[u8]], out: &mut Vec<u8>| {
        let mut entries: Vec<(&Vec<u8>, &Object)> = dict.iter().filter(|(key, _)| !left_out.contains(&key.as_slice())).collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        out.push(b'<');
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for (key, value) in entries {
            out.extend_from_slice(&(key.len() as u64).to_le_bytes());
            out.extend_from_slice(key);
            shape(value, depth.saturating_add(1), out);
        }
    };
    match object {
        Object::Null => object_shape(ShapeItem::Null, out),
        Object::Bool(value) => object_shape(ShapeItem::Bool(*value), out),
        Object::Int(value) => object_shape(ShapeItem::Number(*value as f64), out),
        Object::Real(value) => object_shape(ShapeItem::Number(*value), out),
        Object::String(string) => object_shape(ShapeItem::String(&string.bytes), out),
        Object::Name(name) => object_shape(ShapeItem::Name(name), out),
        Object::Ref(r) => object_shape(ShapeItem::Reference(r.num, r.generation), out),
        Object::Array(items) => {
            out.push(b'[');
            out.extend_from_slice(&(items.len() as u64).to_le_bytes());
            for item in items {
                shape(item, depth.saturating_add(1), out);
            }
        }
        Object::Dict(found) => dict(found, &[], out),
        Object::Stream(stream) => {
            out.push(b'S');
            dict(&stream.dict, &SHAPE_LEFT_OUT, out);
        }
    }
}

impl Known {
    /// Check that each object read here parses to the same value in hayro: `shapes` are hayro's
    /// (`ContentDocument::object_shapes`) of `read`, in order. Where each object is and its
    /// stream data are checked by [`Known::agree`]; this checks what is in it, so that no entry
    /// read from a dictionary or array (a page tree's `/Kids` or inherited attributes, an
    /// outline's `/First` and `/Next`, a name tree's `/Names`, an annotation's `/Dest` or `/A`,
    /// a catalog entry) is one the readers parse differently. An object hayro does not read is
    /// checked by the caller.
    pub fn agree_shapes(&self, doc: &Document, budget: &mut Budget, read: &[(u32, u16)], shapes: &[Option<Vec<u8>>]) -> Result<(), Fail> {
        if read.len() != shapes.len() {
            return Err(read_error("readers disagree: hayro parsed another set of objects"));
        }
        for ((num, generation), theirs) in read.iter().zip(shapes) {
            budget.tick()?;
            let Some(theirs) = theirs else { continue };
            let object = doc.try_get(*num).map_err(|error| read_error(format!("object {num} {generation} R cannot be read: {error}")))?;
            let mut ours = Vec::with_capacity(theirs.len());
            shape(&object, 0, &mut ours);
            budget.hashed(ours.len().saturating_add(theirs.len()))?;
            if ours != *theirs {
                return Err(read_error(format!("readers disagree: hayro parses object {num} {generation} R otherwise")));
            }
        }
        Ok(())
    }

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
        reader.agreeing = true;
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
            facts: std::mem::take(&mut self.facts),
            agreeing: false,
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
            tolerated: self.tolerated.clone(),
            numbered: self.numbered,
            label_values: (Vec::new(), Vec::new()),
            content_rule: ContentRule::None,
            content_base: ContentBase::default(),
            shows_text: false,
            field_widgets: BTreeMap::new(),
            names_apart: self.names_apart,
            ocg_index: self.ocg_index.clone(),
        }
    }

    fn keep(&mut self, reader: Reader<'_, '_>) {
        self.checked = reader.checked;
        self.facts = reader.facts;
        self.object_streams = reader.object_streams;
        self.digests = reader.digests;
        self.images = reader.images;
        self.decoded = reader.decoded;
        self.misdeclared = reader.misdeclared;
        self.numbered = reader.numbered;
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
        Ok(ImageUse { digest, matrix: draw.matrix, tiled: draw.tiled, image, layer: None })
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
    tolerated: Tolerated,
    content_rule: ContentRule,
) -> Result<(Structure, Known), Fail> {
    let tolerated = Arc::new(tolerated);
    let data = doc.bytes().as_slice();
    let names_apart = !matches!(content_rule, ContentRule::None);
    let mut reader = Reader {
        names_apart,
        ocg_index: Arc::new(HashMap::new()),
        doc,
        data,
        budget,
        checked: HashMap::new(),
        facts: HashSet::new(),
        agreeing: false,
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
        tolerated: tolerated.clone(),
        numbered: false,
        label_values: (Vec::new(), Vec::new()),
        content_rule,
        content_base: ContentBase::default(),
        shows_text: false,
        field_widgets: BTreeMap::new(),
    };
    let Some(root) = doc.root() else { return Err(read_error("the trailer's /Root is not a reference")) };
    let catalog = reader.get(root)?.as_dict().cloned().ok_or_else(|| read_error("the catalog is not a dictionary"))?;
    if names_apart && let Some(properties) = reader.dict_of(catalog.get(b"OCProperties"), "/OCProperties")? {
        let mut places = HashMap::new();
        for group in reader.array_of(properties.get(b"OCGs"), "/OCGs")? {
            if let Object::Ref(r) = group {
                let next = places.len();
                places.entry(r).or_insert(next);
            }
        }
        reader.ocg_index = Arc::new(places);
    }
    // The page tree first: the page bound is checked before any per-page work.
    let (nodes, page_tree_entries) = reader.page_tree(&catalog)?;
    let mut widgets = BTreeSet::new();
    let ((fields, field_order), form, form_without_xfa, xfa) = match reader.dict_of(catalog.get(b"AcroForm"), "/AcroForm")? {
        Some(form) => (
            reader.fields(&form, &mut widgets)?,
            Some(reader.digest_without(&form, &[b"Fields"])?),
            Some(reader.digest_without(&form, &[b"Fields", b"XFA"])?),
            form.contains(b"XFA"),
        ),
        None => ((BTreeMap::new(), Vec::new()), None, None, false),
    };
    let layers = catalog.get(b"OCProperties").map(|layers| reader.digest(layers)).transpose()?;
    let mut layer_names = Vec::new();
    if names_apart && let Some(properties) = reader.dict_of(catalog.get(b"OCProperties"), "/OCProperties")? {
        for group in reader.array_of(properties.get(b"OCGs"), "/OCGs")? {
            reader.budget.tick()?;
            let name = match reader.dict_of(Some(&group), "an optional-content group")? {
                Some(group) => match group.get(b"Name") {
                    Some(name) => {
                        let redacted = matches!(&*reader.resolve(name)?, Object::String(text) if text.to_text() == "[Redacted]");
                        Some((reader.digest(name)?, redacted))
                    }
                    None => None,
                },
                None => None,
            };
            layer_names.push(name);
        }
    }
    let (structure_tree, structure_links) = reader.structure_tree(&catalog)?;
    let layer_config = reader.layer_config(&catalog)?;
    let page_index: HashMap<ObjRef, usize> = nodes.iter().enumerate().map(|(index, node)| (node.obj, index)).collect();
    let outline = reader.outline(&catalog, &page_index)?;
    let root = reader.dict_of(catalog.get(b"Outlines"), "/Outlines")?.unwrap_or_default();
    let outline_root = reader.digest_without_links(&root, &[b"Type", b"First", b"Last", b"Count"])?;
    let (named_destinations, name_tree_keys) = reader.named_destinations(&catalog, &page_index)?;
    let info = reader.document_information(doc.trailer())?;
    let page_labels_digest = catalog.get(b"PageLabels").map(|labels| reader.digest(labels)).transpose()?;
    let page_labels = reader.page_labels(&catalog, nodes.len(), page_labels_digest)?;
    let (mut labels_past_the_end, mut label_values) = std::mem::take(&mut reader.label_values);
    labels_past_the_end.sort_unstable();
    label_values.sort_unstable();
    let mut entries = BTreeMap::new();
    for (key, value) in catalog.iter() {
        if CATALOG_REPORTED.contains(&key.as_slice()) {
            continue;
        }
        // Follow-up 8 (R6b-51, M4): the opening action given as a destination, whatever view.
        if key.as_slice() == b"OpenAction"
            && let Object::Array(items) = &*reader.resolve(value)?
            && matches!(items.first(), Some(Object::Int(_) | Object::Real(_)))
        {
            reader.numbered = true;
        }
        // `/Names` but for its `/Dests` tree, compared name by name.
        let digest = match (key.as_slice(), &*reader.resolve(value)?) {
            (b"Names", Object::Dict(names)) => reader.digest_without(names, &[b"Dests"])?,
            _ => reader.digest(value)?,
        };
        entries.insert(key.clone(), digest);
    }
    let mut pages = Vec::with_capacity(nodes.len());
    let mut annotation_ids = BTreeSet::new();
    for node in &nodes {
        reader.budget.time()?;
        pages.push(reader.page(node, &widgets, &mut annotation_ids, &page_index)?);
    }
    // Follow-up 8 (P6-33): where each field's widgets are (page and rectangle).
    let mut widget_places: HashMap<ObjRef, WidgetPlace> = HashMap::new();
    for (index, page) in pages.iter().enumerate() {
        for annot in &page.annots {
            if let Some(id) = annot.id {
                widget_places.insert(id, (index, annot.rect));
            }
        }
    }
    let field_places: BTreeMap<String, Vec<WidgetPlace>> = std::mem::take(&mut reader.field_widgets)
        .into_iter()
        .map(|(name, widgets)| (name, widgets.iter().filter_map(|widget| widget_places.get(widget).copied()).collect()))
        .collect();
    let known = Known {
        names_apart,
        ocg_index: reader.ocg_index.clone(),
        checked: reader.checked,
        facts: reader.facts,
        object_streams: reader.object_streams,
        digests: reader.digests,
        images: reader.images,
        decoded: reader.decoded,
        hayro_decoded,
        misdeclared: reader.misdeclared,
        tolerated,
        agreed: BTreeSet::new(),
        layer_config,
        marks: HashMap::new(),
        numbered: reader.numbered,
    };
    Ok((
        Structure {
            pages,
            fields,
            form,
            form_without_xfa,
            xfa,
            layer_names,
            field_order,
            catalog: entries,
            layers,
            outline,
            outline_root,
            named_destinations,
            sections,
            structure_tree,
            structure_links,
            name_tree_keys,
            misdeclared: BTreeSet::new(),
            info,
            numbered_destinations: false,
            page_labels,
            page_labels_digest,
            page_tree_entries,
            labels_past_the_end,
            label_values,
            content_base: std::mem::take(&mut reader.content_base),
            field_places,
        },
        known,
    ))
}

/// What a rewrite took away from a page that no viewer draws on it (R6b-41).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PageRemovals {
    /// Text entries for assistive technology and copying, from its property lists.
    pub text: usize,
    /// Its actions (`/AA`).
    pub actions: bool,
    /// Its thumbnail (`/Thumb`).
    pub thumbnail: bool,
    /// Follow-up 8 (R6b-51, M1): its XMP packet (`/Metadata`).
    pub metadata: bool,
    /// Follow-up 8 (R6b-51, M1): any other entry no viewer draws ([`PAGE_IGNORED`] but `/Type`
    /// and `/Metadata`: an application's data, the tags' link, navigation and presentation).
    pub entries: bool,
}

/// In a rewrite, what was taken away from a page's property-list text and its own entries
/// (see [`PageFacts::text`], [`PageFacts::entries`]); `None` when either changed otherwise
/// than a rewrite may change it.
pub(crate) fn page_removals(base: &PageFacts, edited: &PageFacts) -> Option<PageRemovals> {
    let text = spans_kept(&base.text, &edited.text)?;
    let kept = base.entries.iter().all(|(key, value)| match edited.entries.get(key) {
        Some(now) => now == value,
        None => PAGE_REMOVABLE.contains(&key.as_slice()) || PAGE_IGNORED.contains(&key.as_slice()),
    });
    if !kept || edited.entries.keys().any(|key| !base.entries.contains_key(key)) {
        return None;
    }
    let gone = |key: &[u8]| base.entries.contains_key(key) && !edited.entries.contains_key(key);
    let entries = PAGE_IGNORED.iter().any(|key| !matches!(*key, b"Type" | b"Metadata") && gone(key));
    Some(PageRemovals { text, actions: gone(b"AA"), thumbnail: gone(b"Thumb"), metadata: gone(b"Metadata"), entries })
}

/// The structure tree's marked-content kids by where their content is: the form XObject that
/// holds it (`None`: a page's own content) and the MCID (R6b-47). Built once, for the base.
pub(crate) struct MarkedIndex<'a> {
    links: &'a StructLinks,
    by_mcid: HashMap<(Option<ObjRef>, i64), Vec<u32>>,
}

impl<'a> MarkedIndex<'a> {
    pub fn new(links: &'a StructLinks, budget: &mut Budget) -> Result<Self, Fail> {
        let mut by_mcid: HashMap<(Option<ObjRef>, i64), Vec<u32>> = HashMap::new();
        for (index, kid) in links.kids.iter().enumerate() {
            budget.tick()?;
            by_mcid.entry((kid.form, kid.mcid)).or_default().push(u32::try_from(index).map_err(|_| Fail::TooComplex)?);
        }
        Ok(Self { links, by_mcid })
    }
}

/// `elements` and their ancestors.
fn with_ancestors(mut todo: Vec<u32>, links: &StructLinks, budget: &mut Budget) -> Result<HashSet<u32>, Fail> {
    let mut elements = HashSet::new();
    while let Some(element) = todo.pop() {
        budget.tick()?;
        if element == STRUCT_ROOT || !elements.insert(element) {
            continue;
        }
        if let Some(parent) = links.parents.get(element as usize) {
            todo.push(*parent);
        }
    }
    Ok(elements)
}

/// Follow-up 7 audit (item 10): whether a rewrite kept text standing for an annotation it
/// removed: an element whose object reference names one of `gone` (base annotations), or an
/// ancestor of one, still has `/ActualText`, `/Alt` or `/E` in `edited_tree` (a link's or a
/// widget's description can spell out what the annotation said).
pub(crate) fn object_text_kept(gone: &BTreeSet<ObjRef>, links: &StructLinks, edited_tree: &StructFacts, budget: &mut Budget) -> Result<bool, Fail> {
    if gone.is_empty() {
        return Ok(false);
    }
    let mut todo = Vec::new();
    for (element, target) in &links.objects {
        budget.tick()?;
        if gone.contains(target) {
            todo.push(*element);
        }
    }
    let elements = with_ancestors(todo, links, budget)?;
    Ok(edited_tree.text.iter().any(|(element, key, _)| stands_for_content(*key) && elements.contains(element)))
}

/// Whether a key of [`STRUCT_TEXT_KEYS`] is text standing for the content (`/ActualText`,
/// `/Alt`, `/E`), which may spell out what a redaction removed (R6b-47); `/Lang` and `/T` do
/// not.
fn stands_for_content(key: u8) -> bool {
    key <= 2
}

/// R6b-47: whether a rewrite kept text standing for content it removed. A glyph removed from the
/// base page (`removed`, indices in its glyphs) takes away from every marked-content span it was
/// drawn in (each level of marked content around it, innermost to outermost) and from every
/// structure element that span's MCID belongs to, with their ancestors: each must have lost its
/// `/ActualText`, `/Alt` and `/E` in `edited` (the span's: the edited page's spans must be the
/// base page's with that text taken away, see [`spans_kept`]; the elements': `edited_tree` has
/// none of theirs). Otherwise the redacted words survive for copying and assistive technology,
/// and `true` is returned: the page counts outside the redactions.
///
/// Where the readers do not agree on the page's levels (see [`PageFacts::levels_agree`]) no
/// glyph can be placed in a span: any such text left on the edited page's spans, or on any
/// element (or its ancestors) with marked content on this page, in a form, or naming no page,
/// counts.
pub(crate) fn alternate_text_kept(
    base: &PageFacts,
    edited: &PageFacts,
    removed: &[usize],
    index: &MarkedIndex<'_>,
    edited_tree: &StructFacts,
    budget: &mut Budget,
) -> Result<bool, Fail> {
    if removed.is_empty() {
        return Ok(false);
    }
    if !base.levels_agree {
        let on_spans = edited.text.iter().any(|span| span.text.iter().any(|(key, _)| stands_for_content(*key)));
        let mut todo: Vec<u32> = Vec::new();
        for kid in &index.links.kids {
            budget.tick()?;
            if kid.form.is_some() || kid.page.is_none_or(|page| page == base.object) {
                todo.push(kid.element);
            }
        }
        let elements = with_ancestors(todo, index.links, budget)?;
        let in_tree = edited_tree.text.iter().any(|(element, key, _)| stands_for_content(*key) && elements.contains(element));
        return Ok(on_spans || in_tree);
    }
    // Every level a removed glyph was drawn in, its own and those around it.
    let marked = &base.glyphs.page.marked;
    let mut hit = vec![false; base.levels.len()];
    for &glyph in removed {
        budget.tick()?;
        let Ok(glyph) = u32::try_from(glyph) else { return Err(Fail::TooComplex) };
        let Ok(at) = marked.binary_search_by_key(&glyph, |(drawn, _)| *drawn) else { continue };
        let mut level = marked.get(at).map_or(NO_LEVEL, |(_, level)| *level);
        while let Some(seen) = hit.get_mut(level as usize) {
            if *seen {
                break;
            }
            *seen = true;
            level = base.levels.get(level as usize).map_or(NO_LEVEL, |level| level.parent);
        }
    }
    // Spans: their text standing for the content taken away.
    let mut stripped = base.text.clone();
    let mut elements: HashSet<u32> = HashSet::new();
    let mut todo: Vec<u32> = Vec::new();
    for (level, _) in base.levels.iter().zip(&hit).filter(|(_, hit)| **hit) {
        budget.tick()?;
        if let Some(span) = level.span.and_then(|span| stripped.get_mut(span as usize)) {
            span.text.retain(|(key, _)| !stands_for_content(*key));
        }
        // Elements: those the level's marked content belongs to.
        let Some(mcid) = level.mcid else { continue };
        for &kid in index.by_mcid.get(&(level.form, mcid)).map(Vec::as_slice).unwrap_or_default() {
            budget.tick()?;
            let Some(kid) = index.links.kids.get(kid as usize) else { continue };
            // A page's MCID is the page's: a kid on another page is another's content (a kid
            // naming no page is taken as this page's).
            if level.form.is_none() && kid.page.is_some_and(|page| page != base.object) {
                continue;
            }
            todo.push(kid.element);
        }
    }
    // With their ancestors: an ancestor's text stands for its descendants' content too.
    elements.extend(with_ancestors(todo, index.links, budget)?);
    let element_kept = edited_tree.text.iter().any(|(element, key, _)| stands_for_content(*key) && elements.contains(element));
    Ok(element_kept || spans_kept(&stripped, &edited.text).is_none())
}
