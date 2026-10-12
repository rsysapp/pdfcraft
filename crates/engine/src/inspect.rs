//! Embed build: compare a saved PDF with the file it was edited from. A host checks every save from
//! the embed build with this. Facts only; what is acceptable is the host's rule.
//!
//! **Every fact is certain, or the call fails.** A host refuses an edited file on any error, so
//! failing closed is always safe; reporting a fact that could not be established (an unreadable
//! page as "no text", an image that could not be hashed as "no image") would let a forged save
//! through. So:
//! - structure (page tree, page dictionaries, annotations, form fields, the cross-reference chain
//!   and the trailer) comes from one parser, `pdfcraft_cos`, the one the writer uses; every object
//!   read must sit where the cross-reference data says, and any repair is an error;
//! - hayro (through `pdfcraft_render::content`) only interprets page content (each glyph, image
//!   and form it draws, and where: one interpretation, the one the render shows) and renders, and
//!   must agree with `pdfcraft_cos` on the page count and each page's annotation count; a glyph
//!   named neither by its text nor by its font and glyph, an image hayro draws that
//!   `pdfcraft_cos` does not read as an image, an interpreter warning or a rebuilt cross-reference
//!   table is an error;
//! - hayro interprets every optional-content layer as visible, whatever the document's
//!   configuration says: which layers show is each viewer's choice (a Layers panel shows hidden
//!   ones), and readers disagree on what marked content hides, so text, images and pixels are
//!   compared in every layer; each glyph, image and form also carries the optional content it is
//!   inside and whether it shows by default, evaluated here against `/OCProperties` (not as
//!   hayro would), and optional content that cannot be evaluated so that every reader agrees
//!   (a group named by another tag, missing from `/OCGs`, automatic states, no `/OCProperties`,
//!   marked content unbalanced around it, a group with `/Usage` on a changed page, …) is an
//!   error; a changed page of a rewrite is compared in every combination of its layers' states
//!   (at most 64);
//! - hayro reads every content stream it interprets to its end: one it stops reading early (an
//!   operator after which it gives up, data it cannot parse) or cannot decode is an error, since
//!   what follows would be checked by nothing; and it draws everything the content draws as
//!   `pdfcraft_cos` reads it (every image, directly, in forms and in tiling patterns' cells, and
//!   every form): anything it leaves undrawn is an error;
//! - the two read one document, checked: every object hayro reads (opening the file and drawing
//!   each page) must be where `pdfcraft_cos`'s cross-reference data puts it and pass the same
//!   checks, every object must parse to the same value in both (each dictionary entry and array
//!   item: no value any fact rests on is read by one parser alone), every stream's data must be
//!   the same bytes, each page must be the same object with the same inherited boxes and
//!   rotation, and
//!   its content as hayro decoded and joined it must be what `pdfcraft_cos` decoded; any
//!   difference is an error; so is a content stream hayro interprets (a form, a pattern's cell, a
//!   Type 3 glyph procedure) that it decoded to other data than `pdfcraft_cos` does; a stream
//!   whose `/Length` is not its data's length is read only in an object of the base revision
//!   (in the base itself; in an appended update, an object the base read, at the offset the
//!   base revision's cross-reference data names for it; never an object a rewrite or an update
//!   writes or points elsewhere, since the editor's writer declares every length exactly) and
//!   only where hayro read the same data;
//! - streams are compared by their decoded data only where hayro decoded them and the readers
//!   agree, everywhere else by their encoded bytes and encoding (see `structure`);
//! - an appended update must chain onto the base's own cross-reference sections (its newest
//!   section's `/Prev` chain reaching the base's newest section at exactly its offset), and every
//!   object it writes must be one this comparison reads (bar its own cross-reference streams;
//!   the document information is read and compared in both kinds of save), so that each is
//!   compared under the appended-update rules; it must
//!   leave every base page's content and resources as they were (edit mode never writes page
//!   content there); a rewrite is accepted only as an applied redaction,
//!   checked visually (every changed pixel lies inside a uniformly filled black rectangle), glyph
//!   by glyph (a glyph is its text and its place; every removed glyph lies under a box, and none
//!   is added), image by image at each image's own resolution (pixels outside the boxes are
//!   the base's, or within requantisation of them for a JPEG), and content stream by content
//!   stream: forms drawn more than once, Type 3 fonts with everything their glyph procedures
//!   draw, and annotation appearances (a form or appearance that changed must lie wholly under
//!   the boxes; a Type 3 font may not change); a form drawn exactly once is part of its page's
//!   glyphs, images and pixels, and judged by them;
//! - a rewrite carries nothing nothing reaches: it has one cross-reference section, and every
//!   object it lists (but for its own cross-reference stream and null objects) is reached from
//!   the trailer's `/Root` or `/Info` through references, an object stream through what it
//!   holds (R6b-48);
//! - every byte a rewrite holds, and every byte an appended update adds, is accounted for
//!   (R6b-51): the `%PDF-x.y` header alone on its line and a binary marker (a rewrite), listed
//!   objects each written strictly (one value, dictionaries of names each once, no stray token,
//!   no comment), object streams holding exactly the objects rows list in them, Flate data ending
//!   where its stream does, the cross-reference sections with only the writer's trailer entries
//!   (or, in an update, the base's) and identifier, `startxref` and `%%EOF`; rows are checked
//!   against the object header they name before anything is loaded;
//! - a rewrite is the server's redaction (R6b-49), so its page content is checked as the
//!   redactor writes it (R6b-51; the redaction re-review's C1 and C2): a content stream it brings
//!   (one that is no content stream of the base's) holds only drawing operators with the
//!   operands their kind takes, no comment, marked content only as the base had it, and names
//!   only resources that exist; a changed page draws only with fonts, patterns, shadings, colour
//!   spaces, graphics states and property lists its base page drew with; otherwise the page
//!   counts outside its redactions;
//! - the page tree reads alike in every viewer (R6b-51): in both files every page's and node's
//!   `/Parent` is the node listing it (PDFium and pdf.js inherit through `/Parent`, hayro and this
//!   reader along the path walked) and the root has none; the nodes' own entries (other than
//!   their kids, count, parent, type and inherited attributes) may not change; and each
//!   marked-content kid of the structure tree stays on its page (`/Pg`), followed through the
//!   pages' pairing;
//! - work is bounded (pages, decoded bytes, hashed bytes, rendered pixels, nesting, a deadline),
//!   and exceeding a bound is [`InspectError::TooComplex`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use pdfcraft_cos::{CosError, Document, ObjRef};
use pdfcraft_render::content::{ContentDocument, ContentError, ContentOpenError, UprightRaster};

mod apply;
mod glyphs;
mod images;
mod jpeg;
mod layout;
mod operators;
mod outline;
mod structure;
mod visual;

pub use apply::{Applied, AppliedFacts, ApplyError, apply_marks};
use images::{Areas, Images};
use structure::{Annot, Digest, Glyphs, PageFacts, Structure};

/// A longer document is refused rather than compared slowly.
pub const MAX_PAGES: usize = 2_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inspection {
    /// The edited file starts with the base's bytes: the edit is an appended update.
    pub base_kept: bool,
    pub pages: PageChanges,
    /// Form fields (by full name) whose value, field dictionary or widgets changed, appeared or
    /// went, plus one when the form's own dictionary (`/AcroForm` without `/Fields`) or the
    /// fields' order (`/Fields`, each field's `/Kids`) changed.
    /// A widget's appearance changing with its value unchanged counts here (it can show other
    /// text than the value), as does `/NeedAppearances` (it lets viewers redraw every field):
    /// both are form changes, not annotations.
    pub form_fields_changed: u32,
    pub annotations: AnnotationChanges,
    pub text: TextChanges,
    pub content: ContentChanges,
    pub outlines: OutlineChanges,
    pub destinations: DestinationChanges,
    /// The page labels, page by page (R6b-46).
    pub labels: PageLabelChanges,
    /// The document information (title, author, …), compared in both kinds of save (R6b-42).
    pub info: InfoChanges,
    /// What a rewrite took away that no viewer draws on a page (R6b-41).
    pub structure_removed: StructureRemovals,
    /// Rewrites only (empty for appended updates): the applied redaction boxes that explain the
    /// changes, as (page number in the edited file, from 1; rectangle `[x0, y0, x1, y1]` in that
    /// page's user space, PDF points). Boxes that overlap (the line boxes of one mark) are one
    /// area, given as the fewest row bands; each is accurate to a pixel of the comparison render
    /// (at most about half a point on a letter or A4 page). Pages counted in
    /// `content.pages_changed_outside_redactions` contribute none. Ordered by page, then top to
    /// bottom. More than [`MAX_REDACTION_AREAS`] is [`InspectError::TooComplex`].
    pub redaction_areas: Vec<(u32, [f32; 4])>,
    /// Follow-up 8 (P6-33): every area a `Redact` mark of the edited file marks, as (page
    /// number in the edited file, from 1; the area `[x0, y0, x1, y1]` in that page's user space:
    /// each quadrilateral's bounding box, or `/Rect`), in page order, then the marks' order: the
    /// complete list (more than [`MAX_MARKED_AREAS`] is [`InspectError::TooComplex`]). A host
    /// applying the marks of an appended update checks every redaction area against these.
    pub marked_areas: Vec<(u32, [f32; 4])>,
    /// Follow-up 8 (P6-33): the edited file's `Redact` marks that carry overlay entries: overlay
    /// text (`/OverlayText`), a form for the overlay (`/RO`), repetition (`/Repeat`), or a fill
    /// colour (`/IC`) other than black. The redactor ignores them all and fills every area
    /// black (R6b-11); a host refuses such marks.
    pub marks_with_overlay: u32,
    /// Follow-up 8 (P6-33), rewrites: form fields gone with every widget wholly under a
    /// redaction area of a page applied (the redactor removes the fields under a mark). Not
    /// counted in `form_fields_changed`.
    pub form_fields_removed_by_redaction: u32,
    /// Follow-up 8 (P6-35), rewrites: one area per quadrilateral (or `/Rect`, without
    /// `/QuadPoints`) of every base `Redact` mark counted in `annotations.redactions_applied`,
    /// as (page number in the edited file, from 1; the area in that page's default user space,
    /// before `/Rotate`, without `/UserUnit`), in the base's page order then the marks' order.
    /// Each was found filled. A host applying marks checks these against the marks it asked for.
    pub applied_areas: Vec<(u32, [f32; 4])>,
    /// Follow-up 8 (P6-35), appended updates: form fields the update changed (filled) whose
    /// every widget lies wholly under one of the edited file's marks on its page.
    pub form_fields_under_marks: u32,
}

/// Areas on pages: (page number, from 1; `[x0, y0, x1, y1]`).
pub type PageAreas = Vec<(u32, [f32; 4])>;

/// The most marked areas an inspection reports.
pub const MAX_MARKED_AREAS: usize = 10_000;

/// The most redaction areas an inspection reports.
pub const MAX_REDACTION_AREAS: usize = 10_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageChanges {
    pub before: u32,
    pub after: u32,
    /// Pages present in both files whose `/Rotate` differs.
    pub rotated: u32,
    pub inserted: u32,
    pub deleted: u32,
    /// Pages present in both files out of their order: the fewest of them that, moved, give the
    /// edited file's order, whether they changed or not (a page paired by its content counts as
    /// any other).
    pub moved: u32,
}

/// Annotations (form-field widgets are form fields; pop-ups without an appearance belong to their
/// markup and are not counted). Added ones are counted by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnnotationChanges {
    /// Added `Stamp` annotations.
    pub stamps: u32,
    /// Added ink, line, square, circle, polygon and polyline annotations.
    pub drawings: u32,
    /// Added notes, text boxes, text markup, carets, file attachments and sounds.
    pub comments: u32,
    /// Added `Redact` annotations (marks not applied). Marks kept from the base are not counted.
    pub redaction_marks: u32,
    /// Added annotations of any other kind (links, widgets outside the form, pop-ups with an
    /// appearance, unknown subtypes).
    pub other: u32,
    /// Base annotations gone. In an appended update, by object identity; in a rewrite (which
    /// renumbers objects), a base annotation whose dictionary is no longer on its page.
    pub removed: u32,
    /// Appended updates: base annotations (by object identity) whose dictionary or appearance
    /// (by its encoded data and encoding) changed, or that moved to another page. (A rewrite
    /// reports these as removed and added.)
    pub changed: u32,
    /// Rewrites: base `Redact` marks gone from a page where their area is now uniformly filled.
    pub redactions_applied: u32,
    /// Follow-up 8 (P6-33), rewrites: base annotations (not `Redact` marks) gone from a page
    /// whose redactions are applied, their `/Rect` wholly inside one of that page's redaction
    /// areas (within a point; the redactor removes what lies under a mark). Not counted in
    /// `removed`; one without a `/Rect`, or reaching outside, is.
    pub removed_by_redaction: u32,
    /// Follow-up 8 (P6-35), appended updates: added stamps, and added drawings (ink, line,
    /// square, circle, polygon, polyline), whose `/Rect` lies wholly under one of the edited
    /// file's marks on its page (within a point).
    pub stamps_under_marks: u32,
    pub drawings_under_marks: u32,
}

/// Among pages present in both files, glyph by glyph: a glyph is its name and its place (its em
/// box, within half a point on every side), and glyphs drawn as spaces count like any other. A
/// glyph's name is its text, or for a glyph without a Unicode value its font (the font
/// dictionary, by its content) and its glyph ID in the font. Glyphs in every optional-content
/// layer count, shown by default or not. Pages where a base glyph has no edited glyph with the
/// same name at the same place (text removed); pages where an edited glyph has no base glyph with
/// the same name at the same place (text added: moved or new text). A glyph's identity includes
/// its optional-content membership: the groups and membership dictionaries it is inside and
/// whether it shows by default, so a glyph moved into or out of a layer is removed and added. A
/// redaction removes text and adds none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextChanges {
    pub pages_with_text_removed: u32,
    pub pages_with_text_added: u32,
}

/// Among pages present in both files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContentChanges {
    /// Pages that lost an image (drawn directly, inside a form XObject, or inline; in any
    /// optional-content layer). Appended updates compare images by a hash of each one's encoded
    /// stream (a cleared image is a new one). Rewrites compare each image placement where hayro's interpreter drew it (also in soft
    /// masks' groups; an image in a tiling pattern's cell by its hash alone): a base image gone,
    /// or replaced at its place by one whose pixels differ under a redaction area (an image
    /// redacted in place).
    pub pages_with_images_removed: u32,
    /// Pages that gained an image. In rewrites, an image redacted in place (the same size and
    /// meaning at the same place, pixels changed only under the redaction areas) is not one.
    pub pages_with_images_added: u32,
    /// Pages that lost text or an image, each page once.
    pub pages_with_content_removed: u32,
    /// Appended updates only (0 for rewrites): pages whose content streams, resources or other
    /// page-dictionary entries (all but `/Rotate` and `/Annots`) differ from the base page's, or
    /// every page present in both when the document's layer configuration changed. A content
    /// stream hayro decoded (a form, a pattern's cell, a Type 3 glyph procedure it drew) is
    /// compared by its decoded data: one written again with another encoding is not a change;
    /// every other stream by its encoded data.
    pub pages_with_content_changed: u32,
    /// Rewrites only (0 for appended updates): pages whose content changed anywhere but inside
    /// the editor's redaction boxes: opaque, uniformly filled rectangles in the redaction fill
    /// colour (black), found in the page rendered upright without annotations. A deletion under a
    /// box of any other colour, or a translucent one, counts here, and so does, visible or not:
    /// - a removed glyph (see [`TextChanges`]) whose em box's centre lies outside every box;
    /// - an image changed at its own resolution outside the boxes: a pixel whose centre lies
    ///   outside every box that differs from the base (losslessly coded images), or differs by
    ///   more than requantising the base with the edited image's quantisation tables explains
    ///   (baseline JPEGs); an image replaced by one of another size, meaning, coding or placement,
    ///   or removed, unless it lay wholly under the boxes. Each image is placed where hayro's
    ///   interpreter drew it, and all of it is compared, whatever a clip hides;
    /// - an image drawn in a tiling pattern's cell added, removed or changed at all;
    /// - a form XObject drawn (directly, in forms or tiling patterns' cells, or as a soft mask's
    ///   group; compared by its dictionary and data, not its resources, and by its decoded data
    ///   where hayro decoded it, so not by its encoding) added, removed or changed, unless its
    ///   `/BBox` lies wholly under the boxes everywhere hayro drew it on the page (never for a
    ///   form drawn in a pattern's cell, or one hayro did not draw). A form drawn exactly once in
    ///   the file (on one page, at one place, directly or in forms) is not compared this way: what
    ///   it draws is part of its page's glyphs, images and pixels, compared as above;
    /// - a Type 3 font in the resources the content is drawn with added, removed or changed: its
    ///   dictionary, its glyph procedures, and every resource they name (forms, images, nested
    ///   Type 3 fonts, …, as they are looked up when the glyphs are drawn), recursively;
    /// - an annotation whose appearance alone changed (by its encoded data: no second reader
    ///   decodes it), unless its `/Rect`, before and after, lies wholly under the boxes;
    /// - every page, when the document's layer configuration (`/OCProperties`) changed: which
    ///   layers a viewer shows by default is not a redaction;
    /// - a page drawn with something hayro may render otherwise than other viewers (a blend mode
    ///   other than `/Normal`, a transfer function, a knockout group, or a key no comparison
    ///   knows in a font, graphics state, form, pattern, shading, property list or resource
    ///   dictionary it draws with), on either side, so that a change hayro's pixels do not show
    ///   may show in another viewer;
    /// - glyphs kept at their places but drawn in another order (the order readers extract
    ///   text in);
    /// - a page entry no fact reports (any but its content, resources, boxes, rotation,
    ///   annotations and what no viewer draws) added, removed or changed, but for its actions
    ///   (`/AA`) and thumbnail (`/Thumb`) removed (reported in [`StructureRemovals`]); text for
    ///   assistive technology and copying in a property list it draws with (`/ActualText`,
    ///   `/Alt`, `/E`, `/Lang`, `/T`) added, changed or moved to another marked-content span,
    ///   the spans taken in drawing order, each with its tag and its list's other entries
    ///   (R6b-41; text taken away is reported in [`StructureRemovals::tagged_text`]);
    /// - a glyph, image or form in other optional content than at its place in the base, or shown
    ///   otherwise by default (R6b-29), whatever the boxes cover;
    /// - where content is in optional content: the page in any combination of the states of the
    ///   groups its content depends on (the default view and every layer off among them; more
    ///   than [`MAX_VIEW_GROUPS`] groups is [`InspectError::TooComplex`]) changed other than
    ///   under the boxes (content moved into or out of a layer, or covered or uncovered by a
    ///   layer's content, which the render of every layer cannot tell; R6b-36). A changed page
    ///   whose content is in a group with `/Usage` fails the inspection: readers apply it
    ///   differently.
    pub pages_changed_outside_redactions: u32,
}

/// The document outline (bookmarks), entry by entry. Entries are read depth first and paired in
/// that order by their depth and title; an entry whose title alone changed pairs with the base
/// entry at its place among the unpaired ones. Two paired entries must agree in everything else
/// (colour, style, any action other than going to a destination in the file), an added entry may
/// have no such action and must go to one of the file's pages, a named destination or nowhere,
/// and the outline's root dictionary may not change, or the inspection fails: no fact here
/// reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutlineChanges {
    /// Paired entries whose title differs.
    pub titles_changed: u32,
    /// Paired entries whose destination changed by following its page: the same page, at the
    /// same view, now at another page number (a page deleted or moved before it renumbers it),
    /// or no destination any more where its page was deleted (an editor drops those). Any other
    /// change of a destination (another page, another view, a new destination, a named or
    /// external one) fails the inspection. A destination that gives its page by number
    /// (`[2 /Fit]`, read as a page index by PDFium and pdf.js) names that page of each file, and
    /// must follow it the same way: its number changes with the pages deleted, moved or
    /// inserted before it (R6b-43).
    pub destinations_changed: u32,
    /// Edited entries paired with no base entry.
    pub added: u32,
    /// Base entries paired with no edited entry.
    pub removed: u32,
}

/// Named destinations and links' destinations that followed their page (R6b-32), as outline
/// destinations do (see [`OutlineChanges::destinations_changed`]): the same page, at the same
/// view, now at another page number (a page deleted or moved before it), or, where its page was
/// deleted, going nowhere (no destination, a `null` page, or a page reference or number that
/// names no page of the file: R6b-37) or, for a named destination, the name gone. A host may
/// accept these as part of a page delete or move. Any other change of a destination fails the
/// inspection: another page, another view, a destination gained, a named destination added,
/// removed (its page still there) or retargeted. A name tree that viewers could resolve
/// differently (keys out of order or named twice, `/Limits` that do not bound a node) fails it
/// too.
///
/// A destination that gives its page by number (R6b-43) follows it as one by reference does,
/// in each file (see [`OutlineChanges::destinations_changed`]). Where such a destination is
/// compared whole instead (the catalog's opening action, an action chained after another, a
/// form field's action, a named destination with entries besides `/D`, …), any page deleted,
/// moved or inserted fails the inspection: it would go to another page, and no fact could say.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DestinationChanges {
    /// Named destinations (the catalog's `/Dests`, the `/Names /Dests` tree) whose destination
    /// followed its page.
    pub named: u32,
    /// Link annotations, on pages present in both files, whose destination (`/Dest`, or a plain
    /// go-to action) followed its page. Their annotation is otherwise unchanged: such a link is
    /// not counted in `annotations.changed`.
    pub links: u32,
    /// Follow-up 8 (R6b-50), rewrites: named destinations whose name held a word a redaction
    /// removed, renamed (`PCRedactedDest` and a number) with every destination naming them;
    /// otherwise unchanged.
    pub renamed: u32,
}

/// The page labels (the catalog's `/PageLabels`: what a viewer shows for a page in place of its
/// number, "iii", "7", "A-2"), page by page (R6b-46). Each page present in both files must show
/// the label it showed: its numbering style, prefix and number. A page deleted takes its label
/// with it; a page inserted may show any label (it is reported inserted). So a label follows its
/// page through deletes, moves and inserts, reported here; any other label change (a page
/// relabelled, a label left behind on the page that took another's place when pages were
/// deleted, moved or inserted, labels added or removed) fails the inspection. A page no range
/// covers (or every page of a file without labels) shows its number: one moved shows its new
/// number, as without labels.
///
/// Labels readers could read differently (a number tree with keys out of order or `/Limits`
/// that do not bound a node, a style no reader shares, a start below 1, an entry no label has,
/// …) are compared whole: unchanged, they are accepted only while no page is deleted, moved or
/// inserted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageLabelChanges {
    /// Pages present in both files, now at another page number, whose label went with them
    /// (the edited file's labels give them the label the base's gave them). A host may accept
    /// these as part of a page delete, move or insert.
    pub followed: u32,
}

/// The document information dictionary (the trailer's `/Info`: title, author, subject,
/// keywords, creation and modification dates, creator, producer, any other key), which viewers
/// show (a document's properties, its title in place of the file name), compared key by key in
/// both kinds of save (R6b-42). **The host refuses any change**: the editor never edits the
/// document information, so a save that changed it was not made by the editor alone.
///
/// The document information must be a reference to a dictionary in both files (PDFium reads
/// nothing else), or be absent: anything else (a stream, a dictionary written in the trailer)
/// fails the inspection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InfoChanges {
    /// The keys whose value was added, removed or changed, in byte order, each written as a PDF
    /// name without its slash (a byte that is not printable ASCII, a space, `#` or a delimiter
    /// written `#xx`, as in a PDF file): `["Title"]`, `["ModDate", "Producer"]`. Every key of the
    /// base's when the edited file has no document information; every key of the edited file's
    /// when the base has none. Empty: unchanged.
    pub changed: Vec<String>,
}

/// What a rewrite (an applied redaction) took away that no viewer draws on a page, each
/// accepted as part of the rewrite and reported so that a host can show it (R6b-41): a
/// redaction cleans the tags of the text it removed, and an editor's sanitising removes the
/// XMP packet, pages' actions and thumbnails. Counts, all 0 for an appended update, which may
/// take none of these away (there, each is a change: the structure tree and the XMP packet fail
/// the inspection, a page's entries count in `content.pages_with_content_changed`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StructureRemovals {
    /// Text for assistive technology and copying taken away, entry by entry (an `/ActualText`,
    /// `/Alt`, `/E`, `/Lang` or `/T`): the structure tree's elements' (R6b-44), and the
    /// marked-content property lists' that pages present in both files draw with (R6b-41).
    /// Such text may only be taken away: text added, changed or moved to another structure
    /// element fails the inspection (an element is its place in the tree's order and its
    /// parent's), and text added, changed or moved to another marked-content span (the spans
    /// taken in drawing order) counts the page in `content.pages_changed_outside_redactions`.
    pub tagged_text: u32,
    /// XMP metadata packets taken away: the document's (the catalog's `/Metadata`), and each
    /// page's present in both files (R6b-51, M1). A form's or image's own XMP packet is not
    /// compared (no viewer shows it).
    pub metadata: u32,
    /// Pages present in both files whose actions (`/AA`: what runs when the page opens or
    /// closes) were taken away.
    pub actions: u32,
    /// Pages present in both files whose thumbnail image (`/Thumb`, which shows the page as it
    /// was) was taken away.
    pub thumbnails: u32,
    /// Pages present in both files that lost another entry no viewer draws (R6b-51, M1): an
    /// application's private data (`/PieceInfo`, `/LastModified`), the tags' link
    /// (`/StructParents`), or navigation and presentation entries (`/ID`, `/PZ`, `/Tabs`, `/B`,
    /// `/Dur`, `/Trans`, `/PresSteps`).
    pub page_entries: u32,
}

/// Why a comparison has no answer. The host refuses the edited file on every one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectError {
    /// The base could not be read (the host's own stored version: an operator problem).
    Base(String),
    /// The edited file could not be read, or a fact about it could not be established.
    Edited(String),
    /// The edited file is encrypted (output encryption is refused by the host).
    Encrypted,
    /// A work bound or the deadline was exceeded.
    TooComplex,
}

/// How long an inspection may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectLimits {
    /// Wall-clock time for the whole comparison, checked between units of work (a page's text,
    /// a page render, a batch of objects). Not measured on wasm32, which has no clock here.
    /// The work bounds hold on every target and do not depend on a clock. They are what bounds
    /// the time on wasm32: each page's text and render, and the comparison as a whole, may do
    /// only a fixed amount of work, at most about half a second a page. Measured natively in a
    /// release build without a clock, the worst whole comparison found (hundreds to 2,000
    /// changed pages that share no glyph or image, paired by their renders, R6b-38) ends in
    /// [`InspectError::TooComplex`] within 45 seconds; wasm32 runs slower than that by its own
    /// factor. Natively this deadline (60 seconds by default) still applies on top.
    pub deadline: Duration,
}

impl Default for InspectLimits {
    fn default() -> Self {
        Self { deadline: Duration::from_secs(60) }
    }
}

/// Compare `edited` with the `base` it was edited from.
pub fn inspect(base: &[u8], edited: &[u8], limits: InspectLimits) -> Result<Inspection, InspectError> {
    inspect_with(base, edited, Limits { deadline: limits.deadline, ..Limits::default() })
}

/// Every bound (the public [`InspectLimits`] sets only the deadline).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    // Embed build: wasm32 has no clock here, so the deadline goes unread there (the wasm32 clippy gate).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub deadline: Duration,
    /// Content-stream bytes decoded (page and form contents, object streams), in both files, plus
    /// the content bytes each page makes the interpreter read (per text extraction and render).
    pub content_bytes: usize,
    /// Stream bytes hashed (images and every other stream reached from a page), in both files.
    pub hashed_bytes: usize,
    /// Pixels rendered for the visual comparison.
    pub rendered_pixels: u64,
    /// Form XObjects inside form XObjects.
    pub form_depth: usize,
    /// Objects read (a cross-reference entry checked and loaded), in both files.
    pub objects: usize,
    /// XObject and inline-image uses walked, in both files.
    pub uses: usize,
    /// Image pixels a page may draw (sum over its uses) for it to be rendered.
    pub page_image_pixels: u64,
    /// Content bytes a page may make the interpreter read (forms counted per use) for it to be
    /// rendered: rendering heavy vector content takes tens of megabytes a page.
    pub page_render_content: usize,
    /// The longer side of a page render, in pixels.
    pub render_side: u32,
    /// The work one interpretation (a page's text, a page's render) may do, in hayro's work units:
    /// one per operator and glyph, and each fill, stroke, image or shading by its device area
    /// (one unit per 256 pixels). Measured: about 0.7 µs a unit at worst (tiny fills), so a page
    /// is bounded to well under a second whatever it draws, with or without a clock.
    pub page_work: u64,
    /// The work all interpretations of both files may do together.
    pub work: u64,
    /// Glyphs kept (text and place, for the glyph pairing), in both files.
    pub glyphs: usize,
    /// Bytes of image samples (or JPEG coefficients) decoded for a rewrite's image comparison,
    /// in all, and for one image.
    pub image_bytes: usize,
    pub image_bytes_each: usize,
    /// Image samples compared one by one.
    pub compared: u64,
    /// The steps pairing changed pages by similarity may take (a feature counted, a shared
    /// feature looked up for a page, a candidate pair weighed, a sixty-fourth of the blocks of
    /// two page renders compared), in all.
    pub pairing: u64,
    /// The working memory pairing changed pages may hold at once (feature tables, the index of
    /// the base pages' features, candidate pairs, page renders reduced to blocks), estimated
    /// from above, in bytes.
    pub pairing_bytes: usize,
    /// Follow-up 8 (R6b-51, I3): the small units of work (an object listed, a token scanned, an
    /// entry compared: every [`Budget::tick`]) both files may take together. This bounds every
    /// loop on every target, wasm32 included, where no clock is read.
    pub steps: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            deadline: InspectLimits::default().deadline,
            content_bytes: 256 << 20,
            hashed_bytes: 512 << 20,
            rendered_pixels: MAX_PAGES as u64 * 1_600 * 1_600,
            form_depth: 16,
            objects: 4_000_000,
            uses: 1_000_000,
            page_image_pixels: 1 << 26,
            page_render_content: 4 << 20,
            render_side: 1_600,
            page_work: 1_000_000,
            work: 64_000_000,
            glyphs: 8_000_000,
            image_bytes: 1 << 30,
            image_bytes_each: 64 << 20,
            compared: 1 << 30,
            pairing: 1 << 25,
            pairing_bytes: 64 << 20,
            steps: 1 << 28,
        }
    }
}

/// Internal failure, before it is attributed to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fail {
    Read(String),
    Encrypted,
    TooComplex,
}

/// Which file a failure belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Base,
    Edited,
}

impl Fail {
    fn of(self, role: Role) -> InspectError {
        match (self, role) {
            (Fail::TooComplex, _) => InspectError::TooComplex,
            (Fail::Encrypted, Role::Edited) => InspectError::Encrypted,
            (Fail::Encrypted, Role::Base) => InspectError::Base("the file is encrypted".into()),
            (Fail::Read(message), Role::Base) => InspectError::Base(message),
            (Fail::Read(message), Role::Edited) => InspectError::Edited(message),
        }
    }
}

/// The work left, shared by both files.
pub(crate) struct Budget {
    #[cfg(not(target_arch = "wasm32"))]
    deadline: Option<std::time::Instant>,
    content_left: usize,
    hashed_left: usize,
    pixels_left: u64,
    objects_left: usize,
    uses_left: usize,
    work_left: u64,
    glyphs_left: usize,
    image_bytes_left: usize,
    compared_left: u64,
    pairing_left: u64,
    /// The pairing's working memory held now (see [`Limits::pairing_bytes`]).
    pairing_held: usize,
    ticks: u32,
    /// Follow-up 8 (R6b-51, I3): see [`Limits::steps`].
    steps_left: u64,
    pub limits: Limits,
}

impl Budget {
    fn new(limits: Limits) -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            deadline: std::time::Instant::now().checked_add(limits.deadline),
            content_left: limits.content_bytes,
            hashed_left: limits.hashed_bytes,
            pixels_left: limits.rendered_pixels,
            objects_left: limits.objects,
            uses_left: limits.uses,
            work_left: limits.work,
            glyphs_left: limits.glyphs,
            image_bytes_left: limits.image_bytes,
            compared_left: limits.compared,
            pairing_left: limits.pairing,
            pairing_held: 0,
            ticks: 0,
            steps_left: limits.steps,
            limits,
        }
    }

    /// Fail when the deadline has passed.
    pub fn time(&mut self) -> Result<(), Fail> {
        #[cfg(not(target_arch = "wasm32"))]
        if self.deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err(Fail::TooComplex);
        }
        Ok(())
    }

    /// A small unit of work: counted against [`Limits::steps`] on every target, and the deadline
    /// is checked every 1,024 of them.
    pub fn tick(&mut self) -> Result<(), Fail> {
        self.steps_left = self.steps_left.checked_sub(1).ok_or(Fail::TooComplex)?;
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks.is_multiple_of(1_024) { self.time() } else { Ok(()) }
    }

    fn take(left: &mut usize, amount: usize) -> Result<(), Fail> {
        *left = left.checked_sub(amount).ok_or(Fail::TooComplex)?;
        Ok(())
    }

    /// Bytes a decoder may still produce.
    pub fn content_room(&self) -> usize {
        self.content_left
    }

    pub fn content(&mut self, bytes: usize) -> Result<(), Fail> {
        Self::take(&mut self.content_left, bytes)
    }

    pub fn hashed(&mut self, bytes: usize) -> Result<(), Fail> {
        Self::take(&mut self.hashed_left, bytes)?;
        self.tick()
    }

    pub fn object(&mut self) -> Result<(), Fail> {
        Self::take(&mut self.objects_left, 1)?;
        self.tick()
    }

    pub fn uses(&mut self) -> Result<(), Fail> {
        Self::take(&mut self.uses_left, 1)?;
        self.tick()
    }

    /// Charge the work `hayro` did since it had done `before`.
    fn work(&mut self, hayro: &ContentDocument, before: u64) -> Result<(), Fail> {
        let done = hayro.work_spent().saturating_sub(before);
        self.work_left = self.work_left.checked_sub(done).ok_or(Fail::TooComplex)?;
        Ok(())
    }

    fn pixels(&mut self, pixels: u64) -> Result<(), Fail> {
        self.pixels_left = self.pixels_left.checked_sub(pixels).ok_or(Fail::TooComplex)?;
        Ok(())
    }

    fn glyphs(&mut self, count: usize) -> Result<(), Fail> {
        Self::take(&mut self.glyphs_left, count)
    }

    /// Bytes one image's decoded samples may still take.
    pub fn image_room(&self) -> usize {
        self.image_bytes_left.min(self.limits.image_bytes_each)
    }

    pub fn image_bytes(&mut self, bytes: usize) -> Result<(), Fail> {
        Self::take(&mut self.image_bytes_left, bytes)?;
        self.time()
    }

    /// A step of pairing changed pages (see [`Limits::pairing`]).
    pub fn pairing(&mut self) -> Result<(), Fail> {
        self.pairing_left = self.pairing_left.checked_sub(1).ok_or(Fail::TooComplex)?;
        self.tick()
    }

    /// The pairing takes `bytes` more working memory (see [`Limits::pairing_bytes`]).
    pub fn pairing_hold(&mut self, bytes: usize) -> Result<(), Fail> {
        self.pairing_held = self.pairing_held.saturating_add(bytes);
        if self.pairing_held > self.limits.pairing_bytes { Err(Fail::TooComplex) } else { Ok(()) }
    }

    /// The pairing gives back `bytes` of working memory.
    pub fn pairing_release(&mut self, bytes: usize) {
        self.pairing_held = self.pairing_held.saturating_sub(bytes);
    }

    /// The pairing is over: it holds nothing.
    fn pairing_done(&mut self) {
        self.pairing_held = 0;
    }

    pub fn compared(&mut self, samples: usize) -> Result<(), Fail> {
        self.compared_left = self.compared_left.checked_sub(samples as u64).ok_or(Fail::TooComplex)?;
        self.tick()
    }
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// What one file says: its structure (from `pdfcraft_cos`) and its pages' text (from hayro).
struct Side {
    structure: Structure,
    hayro: ContentDocument,
    /// The file again, for decoding images a rewrite replaced.
    images: Images,
    /// R6b-48: why a rewrite carries an object nothing reaches (see
    /// `check_rewrite_reachable`), reported once every other fact is established, so that a
    /// file refused for another reason says that reason.
    unreached: Option<String>,
}

/// The whole comparison, with every bound.
pub(crate) fn inspect_with(base: &[u8], edited: &[u8], limits: Limits) -> Result<Inspection, InspectError> {
    // A last resort: nothing below may panic, but a panic must not pass for a result either.
    match catch_unwind(AssertUnwindSafe(|| compare(base, edited, limits))) {
        Ok(result) => result,
        Err(_) => Err(InspectError::Edited("internal error while inspecting the file".into())),
    }
}

fn compare(base: &[u8], edited: &[u8], limits: Limits) -> Result<Inspection, InspectError> {
    let mut budget = Budget::new(limits);
    budget.time().map_err(|fail| fail.of(Role::Base))?;
    let base_kept = edited.starts_with(base);
    // A rewrite's image placements are compared against its redaction boxes.
    let before = read_side(base, None, !base_kept, &mut budget).map_err(|fail| fail.of(Role::Base))?;
    let mut after = read_side(edited, Some((base, &before.structure, base_kept)), !base_kept, &mut budget).map_err(|fail| fail.of(Role::Edited))?;
    // Follow-up 8 (R6b-50): named destinations the redactor renamed are read as the base's.
    let renamed = if base_kept { 0 } else { structure::rename_destinations(&before.structure, &mut after.structure) };
    if let Some(key) = structure::catalog_change(&before.structure, &after.structure, base_kept) {
        return Err(InspectError::Edited(format!("the document's /{key} changed, which this comparison does not report")));
    }
    if before.structure.name_tree_keys != after.structure.name_tree_keys {
        return Err(InspectError::Edited("an entry of the named destinations' tree changed, which this comparison does not report".into()));
    }
    // Follow-up 8 (R6b-51, M5): page tree nodes' own entries, which no viewer draws, and
    // attributes no page takes, are compared nowhere else: each must be one the base's tree
    // holds (taken away is allowed: page operations move inherited attributes to the pages).
    if !sorted_subset(&after.structure.page_tree_entries.untaken, &before.structure.page_tree_entries.all) {
        return Err(InspectError::Edited("an entry of a page tree node changed, which this comparison does not report".into()));
    }
    // The structure tree: an appended update leaves it as it was; a rewrite may only take text
    // and marked content away from it (a redaction cleans the tags of what it removed), element
    // by element (R6b-44), never add, change or move text, nor change anything else in it.
    let (tags_before, tags_after) = (&before.structure.structure_tree, &after.structure.structure_tree);
    let tags_removed = if base_kept { (tags_before == tags_after).then_some(0) } else { structure::tags_kept(tags_before, tags_after) };
    let Some(tags_removed) = tags_removed else {
        return Err(InspectError::Edited("the structure tree's text or tags changed, which this comparison does not report".into()));
    };
    let mut inspection = Inspection { base_kept, ..Inspection::default() };
    inspection.info = InfoChanges { changed: info_changed(&before.structure.info, &after.structure.info) };
    inspection.structure_removed = StructureRemovals {
        tagged_text: count(tags_removed),
        metadata: u32::from(
            !base_kept
                && before.structure.catalog.contains_key(b"Metadata".as_slice())
                && !after.structure.catalog.contains_key(b"Metadata".as_slice()),
        ),
        ..StructureRemovals::default()
    };
    let edited_fail = |fail: Fail| fail.of(Role::Edited);
    // Outline entries pair by title, not by page: where their destinations went says, between
    // changed pages equally alike, which went where.
    let outline_pairs = outline::pair(&before.structure.outline, &after.structure.outline, &mut budget).map_err(edited_fail)?;
    let votes = Votes::new(&before.structure, &after.structure, &outline_pairs);
    // Pages that share no glyph or image are paired by their renders (R6b-38).
    let mut thumbs = |edited: bool, index: usize, budget: &mut Budget| {
        render(if edited { &after } else { &before }, index, budget).map(|raster| visual::thumb(&raster))
    };
    let pairs =
        match_pages(&before.structure.pages, &after.structure.pages, &mut inspection.pages, &votes, &mut thumbs, &mut budget).map_err(edited_fail)?;
    // Follow-up 8 (R6b-51, I2): the structure tree's marked content stays on its page.
    if !structure::marked_pages_followed(&before.structure, &after.structure, &pairs, base_kept, &mut budget).map_err(edited_fail)? {
        return Err(InspectError::Edited(
            "the structure tree's marked content names another page (/Pg) than its page, which this comparison does not report".into(),
        ));
    }
    // Destinations are judged through the pages' pairing: one may follow its page, nothing more.
    let followed: BTreeMap<usize, usize> = pairs.iter().copied().collect();
    // R6b-43: a destination giving its page by number that no destination rule resolves (one
    // compared by its digest) would go to another page, unseen, once a page moved.
    let renumbered =
        pairs.len() != before.structure.pages.len() || pairs.len() != after.structure.pages.len() || pairs.iter().any(|(old, new)| old != new);
    if renumbered && (before.structure.numbered_destinations || after.structure.numbered_destinations) {
        return Err(InspectError::Edited(
            "a destination gives its page by number where this comparison cannot follow it (an action, a named destination with other entries, …), and pages were deleted, moved or inserted".into(),
        ));
    }
    inspection.labels = labels_followed(&before.structure, &after.structure, &pairs, renumbered)?;
    // Follow-up 8 (R6b-51, I1): labels no page shows hold only what the base's labels held.
    if !sorted_subset(&after.structure.labels_past_the_end, &before.structure.label_values) {
        return Err(InspectError::Edited("the page labels name a range past the last page that the base did not have".into()));
    }
    if before.structure.outline_root != after.structure.outline_root {
        return Err(InspectError::Edited("the outline's root dictionary changed, which this comparison does not report".into()));
    }
    inspection.outlines =
        outline::compare(&before.structure.outline, &after.structure.outline, outline_pairs, &followed, &mut budget).map_err(edited_fail)?;
    inspection.destinations.named =
        outline::named(&before.structure.named_destinations, &after.structure.named_destinations, &followed, &mut budget).map_err(edited_fail)?;
    inspection.destinations.renamed = count(renamed);
    // The document's layer configuration decides what every page shows.
    let layers_changed = before.structure.layers != after.structure.layers;
    // Follow-up 8 (P6-33): every area the edited file's marks mark, and those with overlays.
    for (index, page) in after.structure.pages.iter().enumerate() {
        for annot in page.annots.iter().filter(|annot| annot.subtype == b"Redact") {
            inspection.marks_with_overlay = inspection.marks_with_overlay.saturating_add(u32::from(annot.overlay));
            for area in &annot.areas {
                if inspection.marked_areas.len() >= MAX_MARKED_AREAS {
                    return Err(InspectError::TooComplex);
                }
                let page_number = u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX);
                inspection.marked_areas.push((page_number, area.map(|value| value as f32)));
            }
        }
    }
    // The order of each page's annotations (which is on top, the tab order) may not change.
    for &(old, new) in &pairs {
        let (Some(b), Some(e)) = (before.structure.pages.get(old), after.structure.pages.get(new)) else {
            return Err(InspectError::Edited("a page went missing".into()));
        };
        if !annotations_in_order(b, e, base_kept) {
            return Err(InspectError::Edited(format!(
                "the order of page {}'s annotations changed, which this comparison does not report",
                new.saturating_add(1)
            )));
        }
    }
    let mut rasters = Rasters::default();
    let outside_pages = compare_pages(&before, &after, &pairs, base_kept, layers_changed, &mut budget, &mut rasters, &mut inspection)?;
    // Follow-up 8 (P6-33): fields a rewrite took away with every widget under a redaction area
    // of a page applied are reported apart.
    let redacted = if base_kept {
        BTreeSet::new()
    } else {
        fields_under_areas(&before.structure, &after.structure, &pairs, &outside_pages, &inspection.redaction_areas)
    };
    inspection.form_fields_removed_by_redaction = count(redacted.len());
    inspection.form_fields_changed = fields_changed(&before.structure, &after.structure, &redacted);
    // Follow-up 8 (P6-35): fields an update fills wholly under its own marks.
    if base_kept {
        let marked = marks_by_page(&after.structure);
        let filled =
            after.structure.fields.iter().filter(|(name, digest)| before.structure.fields.get(*name) != Some(*digest)).filter(|(name, _)| {
                let places = after.structure.field_places.get(*name).map(Vec::as_slice).unwrap_or_default();
                !places.is_empty()
                    && places.iter().all(|(page, rect)| {
                        rect.is_some_and(|rect| within_area(&rect, u32::try_from(page.saturating_add(1)).unwrap_or(u32::MAX), &marked))
                    })
            });
        inspection.form_fields_under_marks = count(filled.count());
    }
    inspection.redaction_areas.sort_by(|(page_a, a), (page_b, b)| page_a.cmp(page_b).then(b[3].total_cmp(&a[3])).then(a[0].total_cmp(&b[0])));
    (inspection.annotations, inspection.destinations.links, inspection.applied_areas) =
        compare_annotations(&before, &after, &pairs, base_kept, &outside_pages, &inspection.redaction_areas.clone(), &mut budget, &mut rasters)?;
    if let Some(message) = after.unreached {
        return Err(InspectError::Edited(message));
    }
    Ok(inspection)
}

/// Whether every item of `part` (sorted) is in `whole` (sorted), as many times as it is in `part`.
fn sorted_subset<T: Ord>(part: &[T], whole: &[T]) -> bool {
    let mut rest = whole.iter();
    part.iter().all(|item| rest.any(|candidate| candidate == item))
}

/// Whether each page present in both files kept its label (see [`PageLabelChanges`]), and how
/// many of them at another page number. `renumbered`: a page was deleted, moved or inserted.
fn labels_followed(before: &Structure, after: &Structure, pairs: &Pairs, renumbered: bool) -> Result<PageLabelChanges, InspectError> {
    use structure::PageLabels;
    match (&before.page_labels, &after.page_labels) {
        (PageLabels::Read(base), PageLabels::Read(edited)) => {
            let mut followed = 0usize;
            for &(old, new) in pairs {
                let (Some(was), Some(is)) = (base.get(old), edited.get(new)) else {
                    return Err(InspectError::Edited("a page went missing".into()));
                };
                if was != is {
                    return Err(InspectError::Edited(format!(
                        "page {}'s label changed (its page labels did not follow it), which this comparison does not report",
                        new.saturating_add(1)
                    )));
                }
                followed += usize::from(old != new && was.is_some());
            }
            Ok(PageLabelChanges { followed: count(followed) })
        }
        // Compared whole: unchanged, and every page where it was.
        _ if !renumbered && before.page_labels_digest == after.page_labels_digest => Ok(PageLabelChanges::default()),
        _ => {
            Err(InspectError::Edited("the page labels changed, or cannot be compared page by page and pages were deleted, moved or inserted".into()))
        }
    }
}

/// The document information's keys whose value was added, removed or changed (see
/// [`InfoChanges::changed`]).
fn info_changed(before: &BTreeMap<Vec<u8>, structure::Digest>, after: &BTreeMap<Vec<u8>, structure::Digest>) -> Vec<String> {
    let keys: BTreeSet<&Vec<u8>> = before.keys().chain(after.keys()).collect();
    keys.into_iter().filter(|key| before.get(*key) != after.get(*key)).map(|key| name_text(key)).collect()
}

/// A PDF name's bytes as it is written, without its slash: printable ASCII as it is, any other
/// byte (and a space, `#` or a delimiter) as `#xx`, so that two names never read alike.
fn name_text(name: &[u8]) -> String {
    let mut out = String::with_capacity(name.len());
    for &byte in name {
        let plain = byte.is_ascii_graphic() && !matches!(byte, b'#' | b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%');
        if plain {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("#{byte:02X}"));
        }
    }
    out
}

/// Read one file. `against` is the base (bytes, structure, whether `bytes` starts with it) when
/// reading the edited file: its cross-reference chain is checked against the base's. `placements`
/// keeps where each image and form was drawn, and takes out the forms drawn once (a rewrite's
/// page rules judge them).
fn read_side(bytes: &[u8], against: Option<(&[u8], &Structure, bool)>, placements: bool, budget: &mut Budget) -> Result<Side, Fail> {
    let shared = Arc::new(bytes.to_vec());
    let doc = match catch_unwind(AssertUnwindSafe(|| Document::open(shared.clone()))) {
        Ok(Ok(doc)) => doc,
        Ok(Err(CosError::NeedsPassword | CosError::WrongPassword | CosError::Security(_))) => return Err(Fail::Encrypted),
        // A file that names an encryption dictionary anywhere (an earlier revision's, which the
        // newest trailer dropped) is refused as encrypted: that is likely why it cannot be read.
        Ok(Err(_)) | Err(_) if names_encryption(bytes) => return Err(Fail::Encrypted),
        Ok(Err(error)) => return Err(Fail::Read(error.to_string())),
        Err(_) => return Err(Fail::Read("the file could not be parsed".into())),
    };
    if doc.security().is_some() || doc.trailer().contains(b"Encrypt") {
        return Err(Fail::Encrypted);
    }
    if let Some(repair) = doc.repair_log().first() {
        return Err(Fail::Read(format!("the file is damaged: {repair}")));
    }
    // Encryption in any revision, whatever the newest trailer says: the readers apply security
    // handlers, crypt filters (`/StmF`, `/StrF`, `/Identity`), `/EncryptMetadata` and the empty
    // password each their own way, and an update dropping `/Encrypt` leaves the older objects
    // encrypted for one reader and not for the other. No encrypted file is compared.
    let sections = structure::sections(&doc)?;
    if sections.iter().any(|section| section.encrypted) {
        return Err(Fail::Encrypted);
    }
    let hayro = match ContentDocument::open(shared.clone(), budget.limits.page_work) {
        Ok(hayro) => hayro,
        Err(ContentOpenError::Encrypted) => return Err(Fail::Encrypted),
        Err(ContentOpenError::Invalid) => return Err(Fail::Read("the file could not be parsed for its content".into())),
    };
    if hayro.page_count() > MAX_PAGES {
        return Err(Fail::TooComplex);
    }
    // hayro reads every page first: how `pdfcraft_cos`'s structure keys a content stream depends
    // on whether hayro decoded it (see `structure`). What it decoded is kept as digests only.
    let mut contents = Vec::with_capacity(hayro.page_count());
    let mut decoded: HashMap<ObjRef, Digest> = HashMap::new();
    for index in 0..hayro.page_count() {
        budget.time()?;
        let before = hayro.work_spent();
        let content = hayro.content(index);
        budget.work(&hayro, before)?;
        let mut content = content.map_err(content_failure)?;
        for ((num, generation), data) in std::mem::take(&mut content.resolved.content) {
            budget.hashed(data.len())?;
            let r = ObjRef::new(num, generation);
            let digest = structure::sha256(&[&data]);
            if *decoded.entry(r).or_insert(digest) != digest {
                return Err(Fail::Read(format!("readers disagree: hayro decoded content stream {r} two ways")));
            }
        }
        let page_data = content.decoded.take().unwrap_or_default();
        budget.hashed(page_data.len())?;
        budget.glyphs(content.glyphs.len().saturating_add(content.glyphs.chars.len()))?;
        // The levels of marked content around the glyphs (R6b-47), each with its tag.
        let tags = content.glyphs.marked_levels.iter().map(|(tag, _)| tag.len().saturating_add(1)).fold(0usize, usize::saturating_add);
        budget.glyphs(tags.saturating_add(content.glyphs.marked.len()))?;
        contents.push((content, structure::sha256(&[&page_data])));
    }
    // A /Length that disagrees with `endstream` is tolerated only in the base's own objects:
    // those the base read, at the offsets the base revision names for them, in the bytes this
    // file keeps of the base (R6b-30, R6b-35).
    let tolerated = match against {
        None => structure::Tolerated::Base,
        Some((base_bytes, base, true)) => structure::Tolerated::Kept { kept_until: base_bytes.len(), objects: base.misdeclared.clone() },
        Some((_, _, false)) => structure::Tolerated::Kept { kept_until: 0, objects: BTreeSet::new() },
    };
    // Follow-up 8 (R6b-51, C1): a rewrite's base gathers its content streams; the rewrite's
    // own are checked against them.
    let content_rule = match against {
        None if placements => structure::ContentRule::Gather,
        Some((_, base, false)) => structure::ContentRule::Check(Arc::new(base.content_base.clone())),
        _ => structure::ContentRule::None,
    };
    let (structure, mut known) = structure::read(&doc, budget, Arc::new(decoded), sections, tolerated, content_rule)?;
    if let Some((base_bytes, base, base_kept)) = against {
        structure::check_chain(&structure.sections, base_bytes.len(), &base.sections, base_kept)?;
    }
    // What hayro read opening the file (catalog, page tree, pages) is what `pdfcraft_cos` reads.
    known.agree(&doc, budget, hayro.opened())?;
    if hayro.page_count() != structure.pages.len() {
        return Err(Fail::Read(format!("readers disagree on the page count ({} or {})", structure.pages.len(), hayro.page_count())));
    }
    let mut side = Side { structure, hayro, images: Images::new(shared), unreached: None };
    for (index, (page, (content, page_data))) in side.structure.pages.iter_mut().zip(contents).enumerate() {
        budget.time()?;
        let number = index.saturating_add(1);
        if side.hayro.annotation_count(index) != Some(page.annots_in_array) {
            return Err(Fail::Read(format!("readers disagree on the annotations of page {number}")));
        }
        page.size = side.hayro.upright_size(index).ok_or_else(|| Fail::Read(format!("page {number} could not be read")))?;
        // The page's boxes and rotation, inherited through the page tree, are what hayro took:
        // hayro skips a value it cannot read and inherits another (or a default).
        let (media, crop, rotation) = side.hayro.page_geometry(index).ok_or_else(|| Fail::Read(format!("page {number} could not be read")))?;
        // hayro reads each number as a 32-bit float.
        let same = |ours: &[f64; 4], theirs: &[f64; 4]| ours.iter().zip(theirs).all(|(a, b)| f64::from(*a as f32) == *b);
        if !same(&page.boxes.0, &media) || !same(page.boxes.1.as_ref().unwrap_or(&page.boxes.0), &crop) || i64::from(rotation) != page.rotation {
            return Err(Fail::Read(format!("readers disagree on the boxes or rotation page {number} inherits")));
        }
        budget.content(page.interpreted)?;
        // The page hayro drew is this page, its decoded content this content, and every object
        // it read to draw it the object `pdfcraft_cos` reads.
        if content.page != Some((page.object.num, page.object.generation)) {
            return Err(Fail::Read(format!("readers disagree: page {number} is another object to hayro")));
        }
        if page_data != page.content {
            return Err(Fail::Read(format!("readers disagree on the content of page {number}")));
        }
        known.agree(&doc, budget, &content.resolved)?;
        // Every image and form the content draws (as `pdfcraft_cos` reads it) is one hayro drew.
        structure::check_drawn(page, &content, number)?;
        // R6b-47: which glyph is in which level of marked content (and so in which span and
        // structure element) is known only where both readers open the same levels, each with
        // its tag inside the same level, and marked content is balanced.
        page.levels_agree = !page.levels_unbalanced
            && !content.layers_unbalanced
            && page.levels.len() == content.glyphs.marked_levels.len()
            && page
                .levels
                .iter()
                .zip(&content.glyphs.marked_levels)
                .all(|(ours, (tag, parent))| ours.tag.as_deref() == Some(tag.as_slice()) && ours.parent == *parent);
        // A glyph without text is named by its font's content, which a rewrite does not renumber.
        let mut unnamed = HashMap::with_capacity(content.glyphs.unnamed.len());
        for (glyph, (num, generation), id) in &content.glyphs.unnamed {
            let font = known.font_digest(&doc, budget, ObjRef::new(*num, *generation))?;
            unnamed.insert(*glyph, (font, *id));
        }
        // Optional content (R6b-29): every mark the page met is evaluated against the document's
        // configuration (or the page refused), and each glyph, image and form carries the
        // membership it was drawn in.
        let layers = known.page_layers(&doc, budget, &content.layer_marks)?;
        if !layers.marks.is_empty() && content.layers_unbalanced {
            return Err(Fail::Read(format!(
                "page {number}: marked content is not balanced in a content stream, so which content is in which layer is each reader's guess"
            )));
        }
        let memberships = content.layers.iter().map(|marks| known.layer(&doc, budget, marks)).collect::<Result<Vec<_>, _>>()?;
        let membership = |index: Option<u32>| index.and_then(|index| memberships.get(index as usize).copied().flatten());
        page.layers = layers;
        let layers: HashMap<u32, structure::LayerFact> =
            content.glyphs.layers.iter().filter_map(|(glyph, index)| membership(Some(*index)).map(|fact| (*glyph, fact))).collect();
        // Where each image is drawn is where hayro's interpretation (the one the render check
        // sees) drew it; `pdfcraft_cos` says which image it is.
        if placements {
            page.image_uses = known.image_uses(&doc, budget, &content.images)?;
            for (used, draw) in page.image_uses.iter_mut().zip(&content.images) {
                used.layer = membership(draw.layer);
            }
            page.form_layers = content.forms.iter().map(|form| membership(form.layer)).collect();
            page.form_draws = content.forms;
        }
        page.glyphs = Glyphs { page: content.glyphs, unnamed, layers };
        page.fingerprint = structure::fingerprint(page);
    }
    // Every object `pdfcraft_cos` read (pages, resources, annotations, form fields, the outline,
    // the catalog's entries, …) is one hayro reads at the same place, with the same data: no
    // object any fact rests on is identified by one reader alone.
    let read = known.objects_read(&doc);
    for _ in &read {
        budget.object()?;
    }
    // Follow-up 6 audit: an edited file may read no object numbered at or above its trailer's
    // `/Size` (one more than the highest object number, ISO 32000-2 §7.5.5). Both readers here
    // take such an object; a viewer that sizes its table by `/Size` may not, and show another
    // document.
    if against.is_some() {
        let size = doc.trailer().int(b"Size").and_then(|size| u32::try_from(size).ok());
        if let Some((num, generation)) = read.iter().find(|(num, _)| size.is_none_or(|size| *num >= size)) {
            return Err(Fail::Read(format!(
                "object {num} {generation} R is numbered at or above the trailer's /Size, which viewers read differently"
            )));
        }
    }
    let (resolved, unread) = side.hayro.read_objects(&read);
    // An object hayro cannot read is accepted only where `pdfcraft_cos` reads it as null: one
    // `pdfcraft_cos` cannot read now either is refused, not passed over.
    if let Some((num, generation)) =
        unread.into_iter().find(|(num, _)| !doc.try_get(*num).is_ok_and(|object| matches!(*object, pdfcraft_cos::Object::Null)))
    {
        return Err(Fail::Read(format!("readers disagree: hayro cannot read object {num} {generation} R")));
    }
    known.agree(&doc, budget, &resolved)?;
    // And parses to the same value: every dictionary entry and array item any fact rests on.
    known.agree_shapes(&doc, budget, &read, &side.hayro.object_shapes(&read))?;
    // Every content stream hayro decoded is one `pdfcraft_cos` decodes alike, and every stream
    // whose /Length is wrong one hayro read as the same data.
    known.agree_decoded(&doc, budget)?;
    if let Some(stream) = known.unconfirmed_length() {
        return Err(Fail::Read(format!("stream {stream} does not have the length it declares")));
    }
    side.structure.misdeclared = known.misdeclared_at(&doc);
    // Every digest is taken by now: whether any held a destination giving its page by number.
    side.structure.numbered_destinations = known.numbered_destinations();
    // An appended update is compared by what the comparison reads: every object it writes must
    // be one of them (audit of the appended-update path).
    if let Some((base_bytes, _, true)) = against {
        check_update_read(&doc, base_bytes, &side.structure.sections, &known, budget)?;
    }
    // R6b-48: a rewrite carries nothing that is neither drawn nor referenced.
    if let Some((_, _, false)) = against {
        side.unreached = match check_rewrite_reachable(&doc, &side.structure.sections, &known, budget) {
            Ok(()) => None,
            Err(Fail::Read(message)) => Some(message),
            Err(other) => return Err(other),
        };
    }
    if placements {
        structure::retain_shared_forms(&mut side.structure.pages);
    }
    drop(doc);
    if side.hayro.repaired() {
        return Err(Fail::Read("the file's cross-reference data is damaged".into()));
    }
    Ok(side)
}

/// Every object an appended update writes (an in-use cross-reference entry other than the base
/// revision's for that number) must be one this comparison read for a fact, and so compared
/// under the appended-update rules: an object nothing reads could change what a reader shows
/// through a path this comparison does not follow, and no fact would say so. Left out: the
/// cross-reference streams of the update's own sections. (The trailer's `/Info` is read for a
/// fact, [`InfoChanges`], like everything else.) Freed objects are not checked: a reference to
/// one reads null, which the facts compare.
fn check_update_read(
    doc: &Document,
    base: &[u8],
    sections: &[structure::Section],
    known: &structure::Known,
    budget: &mut Budget,
) -> Result<(), Fail> {
    let base_doc = match catch_unwind(AssertUnwindSafe(|| Document::open(Arc::new(base.to_vec())))) {
        Ok(Ok(base_doc)) => base_doc,
        _ => return Err(Fail::Read("the base could not be read again to check the update".into())),
    };
    // Follow-up 8 (R6b-51, I1): every byte the update adds is a listed object, written strictly,
    // or its cross-reference sections.
    account_bytes(doc, base.len(), Some(base_doc.trailer()), budget)?;
    let section_offsets: BTreeSet<u64> = sections.iter().map(|section| section.offset).collect();
    for num in doc.object_numbers() {
        budget.tick()?;
        let entry = doc.xref_entry(num);
        if entry == base_doc.xref_entry(num) || known.was_read(num) {
            continue;
        }
        match entry {
            Some(pdfcraft_cos::XrefEntry::InFile { offset, .. }) if section_offsets.contains(&offset) => {}
            Some(pdfcraft_cos::XrefEntry::InFile { .. } | pdfcraft_cos::XrefEntry::InStream { .. }) => {
                return Err(Fail::Read(format!("the update writes object {num}, which nothing this comparison reports reads")));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Trailer entries a cross-reference section may hold (and those a stream section's dictionary
/// holds beside them).
const TRAILER_KEYS: [&[u8]; 5] = [b"Size", b"Root", b"Info", b"ID", b"Prev"];
const XREF_STREAM_KEYS: [&[u8]; 7] = [b"Type", b"W", b"Index", b"Filter", b"DecodeParms", b"Length", b"DL"];
/// The length of each string of a file identifier the writer makes.
const ID_LENGTH: usize = 16;

/// Follow-up 8 (R6b-51, I1, I3): the bytes of `doc` from `from` (0: a rewrite; the base's
/// length: an appended update, `base_trailer` the base's newest trailer) are accounted for:
/// - every object listed there starts where its row says (checked before it is loaded, each
///   charged), is written strictly, and a Flate stream's data ends where its data ends (bytes
///   after it no decoder reads);
/// - every object stream listed there holds exactly the objects the cross-reference data puts
///   in it, at their indexes, and nothing else; every row naming an object in it says so;
/// - every byte is one of them, a cross-reference section with its `startxref` and `%%EOF`, the
///   header (rewrite) or whitespace (see `layout`);
/// - a section's trailer holds only `/Size`, `/Root`, `/Info`, `/ID`, `/Prev` (and a stream's
///   own entries), or for an update an entry the base's trailer holds with the same value; a
///   file identifier is two strings of the writer's 16 bytes (an update keeps the base's).
fn account_bytes(doc: &Document, from: usize, base_trailer: Option<&pdfcraft_cos::Dict>, budget: &mut Budget) -> Result<(), Fail> {
    let data = doc.bytes().as_slice();
    let refuse = |what: String| Fail::Read(what);
    let mut listed = Vec::new();
    let mut containers: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut stored: Vec<(u32, u32, u32)> = Vec::new();
    for num in doc.object_numbers() {
        budget.tick()?;
        match doc.xref_entry(num) {
            Some(pdfcraft_cos::XrefEntry::InFile { offset, generation }) => {
                let offset = usize::try_from(offset).map_err(|_| refuse(format!("object {num} lies past the file")))?;
                if offset < from {
                    continue;
                }
                // Each listed object is charged, and a row naming another object's place is
                // refused before anything is loaded (`pdfcraft_cos` would search the whole file
                // for the number, once per such row).
                budget.object()?;
                if layout::object_header(data, offset) != Some((num, generation)) {
                    return Err(refuse(format!("the file lists object {num} where no object {num} starts")));
                }
                let found = doc.try_get(num).map_err(|_| refuse(format!("the file holds object {num}, which cannot be read")))?;
                let stream_length = match &*found {
                    pdfcraft_cos::Object::Stream(stream) => {
                        budget.hashed(stream.raw.len())?;
                        let (tail, made) = stream.trailing_data(budget.content_room()).map_err(|error| match structure::decode_error(&error) {
                            Fail::TooComplex => Fail::TooComplex,
                            _ => refuse(format!("stream {num}'s data cannot be read to its end")),
                        })?;
                        budget.content(made)?;
                        if tail.is_some_and(|tail| tail.iter().any(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0'))) {
                            return Err(refuse(format!("stream {num} holds data after its end, which no decoder reads")));
                        }
                        if stream.dict.name(b"Type") == Some(b"ObjStm") {
                            // Its dictionary is read for nothing but how to find what it holds.
                            const OBJECT_STREAM_KEYS: [&[u8]; 7] = [b"Type", b"N", b"First", b"Filter", b"DecodeParms", b"Length", b"DL"];
                            if let Some((key, _)) =
                                stream.dict.iter().find(|(key, value)| !OBJECT_STREAM_KEYS.contains(&key.as_slice()) || !plain_entry(key, value))
                            {
                                return Err(refuse(format!(
                                    "object stream {num}'s /{} is not what an object stream says",
                                    String::from_utf8_lossy(key)
                                )));
                            }
                            let (decoded, made) = stream
                                .decoded_strict_counted(budget.content_room())
                                .map_err(|_| refuse(format!("object stream {num} cannot be decoded")))?;
                            budget.content(made)?;
                            let count = stream.dict.int(b"N").and_then(|count| usize::try_from(count).ok());
                            let first = stream.dict.int(b"First").and_then(|first| usize::try_from(first).ok());
                            let (Some(count), Some(first)) = (count, first) else {
                                return Err(refuse(format!("object stream {num} does not say what it holds")));
                            };
                            containers.insert(num, layout::object_stream(&decoded, count, first, budget)?);
                        }
                        Some(stream.raw.len())
                    }
                    _ => None,
                };
                listed.push(layout::ListedObject { offset, stream_length });
            }
            Some(pdfcraft_cos::XrefEntry::InStream { stream, index }) => stored.push((num, stream, index)),
            _ => {}
        }
    }
    // Every object stream listed here holds exactly the objects rows put in it, at their places.
    let mut placed: HashMap<u32, usize> = HashMap::new();
    for (num, stream, index) in stored {
        budget.tick()?;
        let Some(members) = containers.get(&stream) else {
            // In an object stream of the base's own bytes (an update): the base's.
            match doc.xref_entry(stream) {
                Some(pdfcraft_cos::XrefEntry::InFile { offset, .. }) if usize::try_from(offset).is_ok_and(|offset| offset < from) => continue,
                _ => return Err(refuse(format!("object {num} is listed in object {stream}, which is no object stream the file lists"))),
            }
        };
        if usize::try_from(index).ok().and_then(|index| members.get(index)) != Some(&num) {
            return Err(refuse(format!("object {num} is listed in object stream {stream} at a place that holds another")));
        }
        *placed.entry(stream).or_default() += 1;
    }
    for (stream, members) in &containers {
        if placed.get(stream).copied().unwrap_or(0) != members.len() {
            return Err(refuse(format!("object stream {stream} holds an object no cross-reference row lists in it")));
        }
    }
    let sections: Vec<layout::SectionAt> = doc
        .revisions()
        .iter()
        .filter_map(|revision| Some(layout::SectionAt { offset: usize::try_from(revision.xref_offset).ok()?, is_stream: revision.is_stream }))
        .filter(|section| section.offset >= from)
        .collect();
    let keys = layout::check(data, from, &listed, &sections, budget)?;
    for (section, keys) in sections.iter().zip(keys) {
        let trailer = structure::trailer_at(data, section.offset as u64, section.is_stream)?;
        for key in &keys {
            budget.tick()?;
            let own = TRAILER_KEYS.contains(&key.as_slice()) || (section.is_stream && XREF_STREAM_KEYS.contains(&key.as_slice()));
            let kept = base_trailer.is_some_and(|base| base.get(key).is_some() && base.get(key) == trailer.get(key));
            if !own && !kept {
                return Err(refuse(format!("a trailer holds /{}, which no fact reports", String::from_utf8_lossy(key))));
            }
            // Its value is what the key says, and nothing more (a dictionary or array that no
            // reader reads could hold anything).
            if own && !kept && key.as_slice() != b"ID" && !trailer.get(key).is_some_and(|value| plain_entry(key, value)) {
                return Err(refuse(format!("a trailer's /{} is not what the key says", String::from_utf8_lossy(key))));
            }
        }
        if let Some(id) = trailer.get(b"ID") {
            let fresh = matches!(id, pdfcraft_cos::Object::Array(items) if items.len() == 2
                && items.iter().all(|item| item.as_string().is_some_and(|text| text.bytes.len() == ID_LENGTH)));
            let kept = base_trailer.and_then(|base| base.get(b"ID")).is_some_and(|base| base == id);
            let base_has_none = base_trailer.is_none_or(|base| base.get(b"ID").is_none());
            if !(kept || (fresh && base_has_none)) {
                return Err(refuse("a trailer's /ID is not the writer's (two 16-byte strings, or the base's)".into()));
            }
        }
    }
    Ok(())
}

/// Follow-up 8 audit (C): whether a trailer's, cross-reference stream's or object stream's
/// entry holds what its key says and nothing more: numbers where numbers go, references,
/// filter names, and decoding parameters of numbers.
fn plain_entry(key: &[u8], value: &pdfcraft_cos::Object) -> bool {
    use pdfcraft_cos::Object;
    let int = |value: &Object| matches!(value, Object::Int(_));
    let parameters = |value: &Object| match value {
        Object::Dict(parameters) => parameters
            .iter()
            .all(|(key, value)| matches!(key.as_slice(), b"Predictor" | b"Columns" | b"Colors" | b"BitsPerComponent" | b"EarlyChange") && int(value)),
        Object::Null => true,
        _ => false,
    };
    match key {
        b"Size" | b"Prev" | b"N" | b"First" | b"Length" | b"DL" => int(value),
        b"Root" | b"Info" => matches!(value, Object::Ref(_)),
        b"Type" => matches!(value, Object::Name(name) if matches!(name.as_slice(), b"XRef" | b"ObjStm")),
        b"W" | b"Index" => matches!(value, Object::Array(items) if items.iter().all(int)),
        b"Filter" => match value {
            Object::Name(_) => true,
            Object::Array(names) => names.iter().all(|name| matches!(name, Object::Name(_))),
            _ => false,
        },
        b"DecodeParms" => match value {
            Object::Array(items) => items.iter().all(parameters),
            other => parameters(other),
        },
        _ => false,
    }
}

/// R6b-48: every object a rewrite holds must be reachable from the trailer's `/Root` or `/Info`
/// through references (an object stream through the objects stored in it), but for its own
/// cross-reference streams and objects that are null; and a rewrite has one cross-reference section, so no earlier
/// section's objects are carried, superseded, beside the ones read. Anything else could hold
/// what the rewrite took away (the unredacted original as an object nothing reaches), which no
/// fact would report. Each object visited counts toward the objects bound.
fn check_rewrite_reachable(doc: &Document, sections: &[structure::Section], known: &structure::Known, budget: &mut Budget) -> Result<(), Fail> {
    if sections.len() != 1 {
        return Err(Fail::Read(format!(
            "the rewrite has {} cross-reference sections, so it carries objects of an earlier one nothing reaches",
            sections.len()
        )));
    }
    let section_offsets: BTreeSet<u64> = sections.iter().map(|section| section.offset).collect();
    // Follow-up 7 audit (item 4), follow-up 8 (R6b-51, I1): no bytes but what the
    // cross-reference data lists, each written strictly.
    account_bytes(doc, 0, None, budget)?;
    let mut reached: BTreeSet<u32> = BTreeSet::new();
    let mut todo: Vec<pdfcraft_cos::Object> = [&b"Root"[..], b"Info"].iter().filter_map(|key| doc.trailer().get(key).cloned()).collect();
    while let Some(object) = todo.pop() {
        budget.tick()?;
        match object {
            pdfcraft_cos::Object::Ref(r) => {
                if !reached.insert(r.num) {
                    continue;
                }
                budget.object()?;
                // Follow-up 7 audit (item 5): an object reached that cannot be read is not
                // passed over: what it holds is each reader's guess.
                let found = doc.try_get(r.num).map_err(|_| Fail::Read(format!("the rewrite holds object {}, which cannot be read", r.num)))?;
                // Follow-up 7 audit (item 4): reached is not compared. An object reached that no
                // fact read (only through an entry no comparison reads) may hold nothing but
                // references, or it could carry what the rewrite took away.
                if !known.was_read(r.num) && carries_data(&found, budget)? {
                    return Err(Fail::Read(format!("the rewrite holds object {}, which no comparison reads and which holds text or data", r.num)));
                }
                todo.push((*found).clone());
            }
            pdfcraft_cos::Object::Array(items) => todo.extend(items),
            pdfcraft_cos::Object::Dict(dict) => todo.extend(dict.iter().map(|(_, value)| value.clone())),
            pdfcraft_cos::Object::Stream(stream) => todo.extend(stream.dict.iter().map(|(_, value)| value.clone())),
            _ => {}
        }
    }
    // Object streams holding a reached object are reached through it.
    let mut containers: BTreeSet<u32> = BTreeSet::new();
    for num in doc.object_numbers() {
        budget.tick()?;
        if let Some(pdfcraft_cos::XrefEntry::InStream { stream, .. }) = doc.xref_entry(num)
            && reached.contains(&num)
        {
            containers.insert(stream);
        }
    }
    for num in doc.object_numbers() {
        budget.tick()?;
        if reached.contains(&num) || containers.contains(&num) {
            continue;
        }
        // A null object carries nothing.
        if doc.try_get(num).is_ok_and(|object| matches!(*object, pdfcraft_cos::Object::Null)) {
            continue;
        }
        match doc.xref_entry(num) {
            // The rewrite's own cross-reference stream.
            Some(pdfcraft_cos::XrefEntry::InFile { offset, .. }) if section_offsets.contains(&offset) => {}
            Some(pdfcraft_cos::XrefEntry::InFile { .. } | pdfcraft_cos::XrefEntry::InStream { .. }) => {
                return Err(Fail::Read(format!("the rewrite holds object {num}, which nothing reaches from its catalog or document information")));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Whether `object` (not following references) holds anything but references and nulls in
/// arrays: a string, a stream, a dictionary (its keys are names), a name, a number or a
/// boolean can each spell out text.
fn carries_data(object: &pdfcraft_cos::Object, budget: &mut Budget) -> Result<bool, Fail> {
    let mut todo = vec![object];
    while let Some(object) = todo.pop() {
        budget.tick()?;
        match object {
            pdfcraft_cos::Object::Array(items) => todo.extend(items),
            pdfcraft_cos::Object::Ref(_) | pdfcraft_cos::Object::Null => {}
            _ => return Ok(true),
        }
    }
    Ok(false)
}

/// Whether `bytes` hold the name `/Encrypt` (not `/EncryptMetadata`).
fn names_encryption(bytes: &[u8]) -> bool {
    let name = b"/Encrypt";
    bytes
        .windows(name.len())
        .enumerate()
        .any(|(at, window)| window == name && bytes.get(at.saturating_add(name.len())).is_none_or(|next| !next.is_ascii_alphanumeric()))
}

fn content_failure(error: ContentError) -> Fail {
    match error {
        ContentError::Unreadable(message) => Fail::Read(message),
        ContentError::TooComplex => Fail::TooComplex,
    }
}

/// Pages present in both files: (base page, edited page).
type Pairs = Vec<(usize, usize)>;

/// Longest common subsequence of page fingerprints, as (base page, edited page) pairs.
fn in_order(a: &[Digest], b: &[Digest]) -> Pairs {
    let width = b.len().saturating_add(1);
    let mut table = vec![0u16; a.len().saturating_add(1).saturating_mul(width)];
    let cell = |table: &[u16], i: usize, j: usize| table.get(i.saturating_mul(width).saturating_add(j)).copied().unwrap_or(0);
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            let value = if a.get(i) == b.get(j) {
                cell(&table, i.saturating_add(1), j.saturating_add(1)).saturating_add(1)
            } else {
                cell(&table, i.saturating_add(1), j).max(cell(&table, i, j.saturating_add(1)))
            };
            if let Some(slot) = table.get_mut(i.saturating_mul(width).saturating_add(j)) {
                *slot = value;
            }
        }
    }
    let (mut i, mut j, mut pairs) = (0usize, 0usize, Vec::new());
    while i < a.len() && j < b.len() {
        if a.get(i) == b.get(j) {
            pairs.push((i, j));
            i = i.saturating_add(1);
            j = j.saturating_add(1);
        } else if cell(&table, i.saturating_add(1), j) >= cell(&table, i, j.saturating_add(1)) {
            i = i.saturating_add(1);
        } else {
            j = j.saturating_add(1);
        }
    }
    pairs
}

/// A page's content as a multiset of features, for pairing changed pages by similarity: each
/// glyph by its name and the 4-point cell of its centre, each image by its digest and the 4-point
/// cell of its placement's origin (where its placements are known, else by its digest alone).
/// The cells are coarse on purpose: a rewrite that sets kept text a little off still leaves most
/// glyphs in their cells. Only which pages pair rests on this; the comparison that follows is
/// exact.
/// Bytes a feature table holds per feature counted (a `HashMap<u64, u32>` entry and its control
/// byte, at most half full after a resize), from above. The caller releases
/// [`feature_bytes`] of the table when it drops it.
const FEATURE_BYTES: usize = 48;

fn feature_bytes(page: &PageFacts) -> usize {
    page.glyphs.len().saturating_add(page.image_uses.len().max(page.images.len())).saturating_mul(FEATURE_BYTES)
}

fn features(page: &PageFacts, budget: &mut Budget) -> Result<HashMap<u64, u32>, Fail> {
    use std::hash::{Hash, Hasher};
    budget.pairing_hold(feature_bytes(page))?;
    let cell = |value: f64| (value / 4.0).floor() as i64;
    let mut out: HashMap<u64, u32> = HashMap::new();
    let mut add = |key: u64| {
        let slot = out.entry(key).or_default();
        *slot = slot.saturating_add(1);
    };
    for index in 0..page.glyphs.len() {
        budget.pairing()?;
        let Some((name, rect)) = page.glyphs.glyph(index) else { return Err(Fail::Read("a glyph went missing".into())) };
        let mut hasher = std::hash::DefaultHasher::new();
        b'g'.hash(&mut hasher);
        match name {
            structure::GlyphName::Text(chars) => chars.hash(&mut hasher),
            structure::GlyphName::Font(font, id) => (font, id).hash(&mut hasher),
        }
        cell((f64::from(rect[0]) + f64::from(rect[2])) / 2.0).hash(&mut hasher);
        cell((f64::from(rect[1]) + f64::from(rect[3])) / 2.0).hash(&mut hasher);
        add(hasher.finish());
    }
    if page.image_uses.is_empty() {
        for digest in &page.images {
            budget.pairing()?;
            let mut hasher = std::hash::DefaultHasher::new();
            (b'i', digest).hash(&mut hasher);
            add(hasher.finish());
        }
    } else {
        for used in &page.image_uses {
            budget.pairing()?;
            let mut hasher = std::hash::DefaultHasher::new();
            (b'p', &used.digest, used.tiled).hash(&mut hasher);
            if used.matrix[4].is_finite() && used.matrix[5].is_finite() {
                (cell(used.matrix[4]), cell(used.matrix[5])).hash(&mut hasher);
            }
            add(hasher.finish());
        }
    }
    Ok(out)
}

/// A feature shared by more of the changed base pages than this tells none of them apart (a
/// running header): it does not count toward any page's similarity.
const MAX_SHARED_FEATURE: usize = 16;

/// The most candidate pairs of changed pages weighed: each edited page shares features with at
/// most [`MAX_SHARED_FEATURE`] base pages a feature, so this is reached only by pages built to
/// look alike in groups.
const MAX_CANDIDATE_PAIRS: usize = 1 << 20;

/// The similarity of each changed base page to each changed edited page that shares content
/// with it: (score, base page, edited page, the edited page's features the base page lacks). The
/// score is the features the two share, less those the base page lacks (a redaction removes
/// content and adds none).
fn similarities(
    before: &[PageFacts],
    after: &[PageFacts],
    base_free: &[usize],
    edited_free: &[usize],
    budget: &mut Budget,
) -> Result<Vec<(i64, usize, usize, i64)>, Fail> {
    // The index of the base pages' features: per feature a table entry and a list, per base
    // page sharing it a list item (lists at most half full), held until the pairing ends.
    let mut index: HashMap<u64, Vec<(usize, u32)>> = HashMap::new();
    for &page in base_free {
        let Some(facts) = before.get(page) else { return Err(Fail::Read("a page went missing".into())) };
        let found = features(facts, budget)?;
        for (key, count) in found {
            budget.pairing_hold(INDEX_ITEM_BYTES)?;
            let list = index.entry(key).or_default();
            if list.is_empty() {
                budget.pairing_hold(INDEX_KEY_BYTES)?;
            }
            list.push((page, count));
        }
        budget.pairing_release(feature_bytes(facts));
    }
    let mut out = Vec::new();
    for &other in edited_free {
        let Some(facts) = after.get(other) else { return Err(Fail::Read("a page went missing".into())) };
        let found = features(facts, budget)?;
        let total: i64 = found.values().map(|count| i64::from(*count)).sum();
        let mut shared: BTreeMap<usize, i64> = BTreeMap::new();
        let mut shared_bytes = 0usize;
        for (key, count) in &found {
            let Some(list) = index.get(key) else { continue };
            if list.len() > MAX_SHARED_FEATURE {
                continue;
            }
            for (page, base_count) in list {
                budget.pairing()?;
                let slot = shared.entry(*page).or_insert_with(|| {
                    shared_bytes = shared_bytes.saturating_add(SHARED_BYTES);
                    0
                });
                *slot += i64::from((*count).min(*base_count));
            }
        }
        budget.pairing_hold(shared_bytes)?;
        for (page, common) in shared {
            let lacking = total.saturating_sub(common);
            budget.pairing_hold(CANDIDATE_BYTES)?;
            out.push((common.saturating_sub(lacking), page, other, lacking));
        }
        budget.pairing_release(shared_bytes.saturating_add(feature_bytes(facts)));
    }
    Ok(out)
}

/// Bytes the index of the base pages' features holds per feature (a table entry with its
/// list's header and control byte, the table at most half full) and per item of a list (at
/// most half full), from above.
const INDEX_KEY_BYTES: usize = 80;
const INDEX_ITEM_BYTES: usize = 32;
/// Bytes a base page sharing features with an edited page holds while that page is weighed (a
/// B-tree entry, its nodes at most half full), from above.
const SHARED_BYTES: usize = 48;
/// Bytes a candidate pair holds until the pairing ends: its place in the candidates (at most
/// half full, and as much again to sort them) and in the tables by edited and by base page (at
/// most half full, with their tables' entries), from above.
const CANDIDATE_BYTES: usize = 64 + 32 + 2 * 48 + 2 * 16;

/// What the documents' destinations say about where pages went: an outline entry (paired by its
/// title) or a named destination (by its name) going to base page `b` and now to edited page `e`
/// votes for that pair; one that went to `b` and now goes nowhere says `b` was deleted. Used
/// only to choose between changed pages equally alike (see [`match_pages`]): the pages are then
/// compared as paired, and every destination must have followed that pairing.
#[derive(Default)]
struct Votes {
    pairs: HashMap<(usize, usize), u32>,
    deleted: HashMap<usize, u32>,
}

impl Votes {
    fn new(before: &Structure, after: &Structure, outline_pairs: &[(usize, usize)]) -> Self {
        let mut votes = Self::default();
        for &(old, new) in outline_pairs {
            if let (Some(a), Some(b)) = (before.outline.get(old), after.outline.get(new)) {
                votes.vote(&a.destination, b.destination.clone());
            }
        }
        for (name, destination) in &before.named_destinations {
            votes.vote(destination, after.named_destinations.get(name).cloned().unwrap_or(structure::Destination::None));
        }
        votes
    }

    fn vote(&mut self, before: &structure::Destination, after: structure::Destination) {
        use structure::Destination;
        let (Destination::Page(page, _) | Destination::Index(page, _)) = before else { return };
        let slot = match after {
            Destination::Page(other, _) | Destination::Index(other, _) => self.pairs.entry((*page, other)).or_default(),
            Destination::None | Destination::Unpaged(_) | Destination::NullPage(_) => self.deleted.entry(*page).or_default(),
            _ => return,
        };
        *slot = slot.saturating_add(1);
    }
}

/// Pair the pages: identical pages in order, then identical pages elsewhere (moved), then the
/// changed pages by content similarity (R6b-31: the glyphs and images they share, at their
/// places; see [`similarities`]), the most similar first; position decides only between equally
/// similar pages: a page between the same two pages paired in order first, then the nearest.
/// Before position, between equally similar pages and for pages that share nothing, what the
/// destinations say ([`Votes`]). Pages that share nothing and no destination paired, by their
/// renders (R6b-38, [`pair_by_renders`]); pages that could be a redaction of no other, by
/// position. What is left was inserted or deleted. The pairing's work and working memory are
/// bounded ([`Limits::pairing`], [`Limits::pairing_bytes`]).
fn match_pages(
    before: &[PageFacts],
    after: &[PageFacts],
    changes: &mut PageChanges,
    votes: &Votes,
    thumbs: &mut Thumbs<'_>,
    budget: &mut Budget,
) -> Result<Pairs, Fail> {
    let pairs = match_pages_holding(before, after, changes, votes, thumbs, budget);
    budget.pairing_done();
    pairs
}

/// A page's render reduced to blocks (see [`visual::Thumb`]): of the edited file's page when the
/// flag is set, else of the base's.
type Thumbs<'a> = dyn FnMut(bool, usize, &mut Budget) -> Result<visual::Thumb, Fail> + 'a;

/// Pair changed pages that neither glyphs and images nor destinations paired by their renders
/// (R6b-38): an edited page pairs with the one base page it could be a redaction of, outside the
/// areas its redaction fills cover (see [`visual::unexplained`]). Another base page it could as
/// well be a redaction of, or another edited page that could as well be one of that base page,
/// leaves which page became which to a guess: an error. Pages that could be a redaction of none
/// are left (they are not redactions of each other, whichever pairs).
fn pair_by_renders(base_free: &[usize], edited_free: &[usize], thumbs: &mut Thumbs<'_>, budget: &mut Budget) -> Result<Pairs, Fail> {
    if base_free.is_empty() || edited_free.is_empty() {
        return Ok(Vec::new());
    }
    let mut render = |edited: bool, pages: &[usize], budget: &mut Budget| -> Result<Vec<(usize, visual::Thumb)>, Fail> {
        let mut out = Vec::with_capacity(pages.len());
        for &page in pages {
            let thumb = thumbs(edited, page, budget)?;
            budget.pairing_hold(thumb.bytes())?;
            out.push((page, thumb));
        }
        Ok(out)
    };
    let base = render(false, base_free, budget)?;
    let edited = render(true, edited_free, budget)?;
    let mut plausible: Pairs = Vec::new();
    let (mut by_base, mut by_edited): (HashMap<usize, usize>, HashMap<usize, usize>) = (HashMap::new(), HashMap::new());
    for (other, edited_thumb) in &edited {
        for (page, base_thumb) in &base {
            for _ in 0..=edited_thumb.blocks() / 64 {
                budget.pairing()?;
            }
            if visual::unexplained(base_thumb, edited_thumb) != Some(0) {
                continue;
            }
            budget.pairing_hold(CANDIDATE_BYTES)?;
            if let Some(rival) = by_edited.insert(*other, *page) {
                return Err(Fail::Read(format!(
                    "the pages cannot be told apart: base pages {} and {} could each be what edited page {} shows",
                    rival.saturating_add(1),
                    page.saturating_add(1),
                    other.saturating_add(1)
                )));
            }
            if let Some(rival) = by_base.insert(*page, *other) {
                return Err(Fail::Read(format!(
                    "the pages cannot be told apart: edited pages {} and {} could each be what base page {} became",
                    rival.saturating_add(1),
                    other.saturating_add(1),
                    page.saturating_add(1)
                )));
            }
            plausible.push((*page, *other));
        }
    }
    Ok(plausible)
}

fn match_pages_holding(
    before: &[PageFacts],
    after: &[PageFacts],
    changes: &mut PageChanges,
    votes: &Votes,
    thumbs: &mut Thumbs<'_>,
    budget: &mut Budget,
) -> Result<Pairs, Fail> {
    let base_prints: Vec<Digest> = before.iter().map(|page| page.fingerprint).collect();
    let edited_prints: Vec<Digest> = after.iter().map(|page| page.fingerprint).collect();
    let anchors = in_order(&base_prints, &edited_prints);
    let mut pairs = anchors.clone();
    let used_base: BTreeSet<usize> = pairs.iter().map(|pair| pair.0).collect();
    let used_edited: BTreeSet<usize> = pairs.iter().map(|pair| pair.1).collect();
    let mut base_free: Vec<usize> = (0..before.len()).filter(|page| !used_base.contains(page)).collect();
    let mut edited_free: Vec<usize> = (0..after.len()).filter(|page| !used_edited.contains(page)).collect();
    base_free.retain(|&page| {
        let Some(position) = edited_free.iter().position(|&other| base_prints.get(page) == edited_prints.get(other)) else { return true };
        pairs.push((page, edited_free.remove(position)));
        false
    });
    // Which gap between pages paired in order a page lies in.
    let slot_of_base = |page: usize| anchors.partition_point(|anchor| anchor.0 < page);
    let slot_of_edited = |page: usize| anchors.partition_point(|anchor| anchor.1 < page);
    let said = |page: usize, other: usize| {
        (std::cmp::Reverse(votes.pairs.get(&(page, other)).copied().unwrap_or(0)), votes.deleted.get(&page).copied().unwrap_or(0))
    };
    let apart = |page: usize, other: usize| (said(page, other), slot_of_base(page).abs_diff(slot_of_edited(other)), page.abs_diff(other));
    let mut candidates = similarities(before, after, &base_free, &edited_free, budget)?;
    if candidates.len() > MAX_CANDIDATE_PAIRS {
        return Err(Fail::TooComplex);
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(apart(a.1, a.2).cmp(&apart(b.1, b.2))).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    candidates.dedup();
    let mut base_taken: BTreeSet<usize> = BTreeSet::new();
    let mut edited_taken: BTreeSet<usize> = BTreeSet::new();
    // Each candidate's score, by edited page and by base page, for telling pages apart.
    let mut by_edited: HashMap<usize, Vec<(usize, i64, i64)>> = HashMap::new();
    let mut by_base: HashMap<usize, Vec<(usize, i64, i64)>> = HashMap::new();
    for &(score, page, other, lacking) in &candidates {
        by_edited.entry(other).or_default().push((page, score, lacking));
        by_base.entry(page).or_default().push((other, score, lacking));
    }
    for &(score, page, other, lacking) in &candidates {
        budget.pairing()?;
        if base_taken.contains(&page) || edited_taken.contains(&other) {
            continue;
        }
        // The pairing must be certain: another page still free as alike as this one (within
        // a margin), lacking no more of the other's content (so that one could be a redaction
        // of the other as well), leaves which page became which to a guess, unless the
        // destinations say which (and are then checked against it).
        let margin = (score / 16).max(2);
        let said = votes.pairs.get(&(page, other)).copied().unwrap_or(0) > 0;
        let alike = |rival_score: i64, rival_lacking: i64| rival_score >= score.saturating_sub(margin) && rival_lacking <= lacking;
        let rival_base = by_edited.get(&other).into_iter().flatten().find(|(rival, rival_score, rival_lacking)| {
            *rival != page
                && !base_taken.contains(rival)
                && alike(*rival_score, *rival_lacking)
                && (!said || votes.pairs.contains_key(&(*rival, other)))
        });
        let rival_edited = by_base.get(&page).into_iter().flatten().find(|(rival, rival_score, rival_lacking)| {
            *rival != other
                && !edited_taken.contains(rival)
                && alike(*rival_score, *rival_lacking)
                && (!said || votes.pairs.contains_key(&(page, *rival)))
        });
        if let Some((rival, ..)) = rival_base {
            return Err(Fail::Read(format!(
                "the pages cannot be told apart: base pages {} and {} are as alike to edited page {}",
                page.saturating_add(1),
                rival.saturating_add(1),
                other.saturating_add(1)
            )));
        }
        if let Some((rival, ..)) = rival_edited {
            return Err(Fail::Read(format!(
                "the pages cannot be told apart: edited pages {} and {} are as alike to base page {}",
                other.saturating_add(1),
                rival.saturating_add(1),
                page.saturating_add(1)
            )));
        }
        base_taken.insert(page);
        edited_taken.insert(other);
        pairs.push((page, other));
    }
    base_free.retain(|page| !base_taken.contains(page));
    edited_free.retain(|page| !edited_taken.contains(page));
    // Pages that share nothing with any other: as the destinations went, then by their renders,
    // then (what could be a redaction of nothing) in order within the same gap (pages a
    // destination says were deleted last), then in order.
    let mut positional = Vec::new();
    let mut voted: Vec<(&(usize, usize), &u32)> = votes.pairs.iter().collect();
    voted.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (&(page, other), _) in voted {
        if base_free.contains(&page) && edited_free.contains(&other) {
            positional.push((page, other));
            base_free.retain(|free| *free != page);
            edited_free.retain(|free| *free != other);
        }
    }
    // Then by their renders (R6b-38); what could be a redaction of nothing, by position.
    for (page, other) in pair_by_renders(&base_free, &edited_free, thumbs, budget)? {
        positional.push((page, other));
        base_free.retain(|free| *free != page);
        edited_free.retain(|free| *free != other);
    }
    base_free.sort_by_key(|page| (votes.deleted.get(page).copied().unwrap_or(0), *page));
    for &page in &base_free {
        if let Some(position) = edited_free.iter().position(|&other| slot_of_edited(other) == slot_of_base(page)) {
            positional.push((page, edited_free.remove(position)));
        }
    }
    base_free.retain(|page| !positional.iter().any(|pair| pair.0 == *page));
    base_free.sort_unstable();
    let rest = base_free.len().min(edited_free.len());
    positional.extend(base_free.drain(..rest).zip(edited_free.drain(..rest)));
    pairs.extend(positional.iter().copied());
    *changes = PageChanges {
        before: count(before.len()),
        after: count(after.len()),
        rotated: count(pairs.iter().filter(|(old, new)| before.get(*old).map(|p| p.rotation) != after.get(*new).map(|p| p.rotation)).count()),
        inserted: count(edited_free.len()),
        deleted: count(base_free.len()),
        moved: 0,
    };
    pairs.sort_unstable();
    // Pages out of their order, whatever paired them (the same page elsewhere, or a changed page
    // by similarity): the fewest that, moved, give the edited order.
    let in_place = longest_increasing(&pairs.iter().map(|pair| pair.1).collect::<Vec<_>>());
    changes.moved = count(pairs.len().saturating_sub(in_place));
    Ok(pairs)
}

/// The length of the longest strictly increasing subsequence of `values`.
fn longest_increasing(values: &[usize]) -> usize {
    let mut tails: Vec<usize> = Vec::new();
    for &value in values {
        let at = tails.partition_point(|tail| *tail < value);
        match tails.get_mut(at) {
            Some(slot) => *slot = value,
            None => tails.push(value),
        }
    }
    tails.len()
}

/// Whether an image of the multiset `a` is missing from the multiset `b`.
fn images_missing(a: &[Digest], b: &[Digest]) -> bool {
    let mut counts: BTreeMap<&Digest, i64> = BTreeMap::new();
    for image in a {
        *counts.entry(image).or_default() += 1;
    }
    for image in b {
        *counts.entry(image).or_default() -= 1;
    }
    counts.values().any(|value| *value > 0)
}

/// The renders of pages compared visually.
type RasterPair = (Arc<UprightRaster>, Arc<UprightRaster>);

/// Renders kept for the redaction-mark check, only of base pages that have a `Redact` mark (a
/// page's renders take megabytes; a document's would take gigabytes).
#[derive(Default)]
struct Rasters {
    /// (base page, edited page) → (base render, edited render).
    pairs: HashMap<(usize, usize), RasterPair>,
}

impl Rasters {
    fn get(&mut self, before: &Side, after: &Side, pair: (usize, usize), budget: &mut Budget) -> Result<RasterPair, InspectError> {
        if let Some(found) = self.pairs.get(&pair) {
            return Ok(found.clone());
        }
        let base = Arc::new(render(before, pair.0, budget).map_err(|fail| fail.of(Role::Base))?);
        let edited = Arc::new(render(after, pair.1, budget).map_err(|fail| fail.of(Role::Edited))?);
        let marked = before.structure.pages.get(pair.0).is_some_and(|page| page.annots.iter().any(|annot| annot.subtype == b"Redact"));
        if marked {
            self.pairs.insert(pair, (base.clone(), edited.clone()));
        }
        Ok((base, edited))
    }
}

/// Whether glyphs or forms of the base page are on the edited page in other optional content
/// (R6b-29): glyphs that pair by name and place but not with their membership (`pairing` paired
/// them with it), or more forms drawn in some membership than the base page draws there (a
/// redaction only removes forms). Images are compared with theirs (see `images`).
fn layers_changed_for(base: &PageFacts, edited: &PageFacts, pairing: &glyphs::Pairing, budget: &mut Budget) -> Result<bool, Fail> {
    if !(base.glyphs.layers.is_empty() && edited.glyphs.layers.is_empty()) {
        let loose = glyphs::pair_ignoring_layers(&base.glyphs, &edited.glyphs, budget)?;
        if loose.removed.len() < pairing.removed.len() {
            return Ok(true);
        }
    }
    let tally = |page: &PageFacts| {
        let mut counts: BTreeMap<Option<structure::LayerFact>, usize> = BTreeMap::new();
        for layer in &page.form_layers {
            let count = counts.entry(*layer).or_default();
            *count = count.saturating_add(1);
        }
        counts
    };
    let (in_base, in_edited) = (tally(base), tally(edited));
    Ok(in_edited.iter().any(|(layer, count)| *count > in_base.get(layer).copied().unwrap_or(0)))
}

/// The most optional-content groups a changed page in a rewrite may depend on: it is compared in
/// every combination of their states, 2^6 = 64 views (R6b-36). More is [`InspectError::TooComplex`].
pub const MAX_VIEW_GROUPS: usize = 6;

/// Whether base page `pair.0` and edited page `pair.1` differ other than under redaction boxes
/// in any view of their optional content: each combination of the states of the groups either
/// page's content depends on (the default view and the view with every layer off among them),
/// each view rendered without the content its groups and membership dictionaries hide there.
/// Views that hide nothing on either page are the render of every layer, compared already.
fn layer_views_differ(before: &Side, after: &Side, pair: (usize, usize), budget: &mut Budget) -> Result<bool, InspectError> {
    let (Some(b), Some(e)) = (before.structure.pages.get(pair.0), after.structure.pages.get(pair.1)) else {
        return Err(InspectError::Edited("a page went missing".into()));
    };
    let mut groups = BTreeSet::new();
    b.layers.groups(&mut groups);
    e.layers.groups(&mut groups);
    if groups.len() > MAX_VIEW_GROUPS {
        return Err(InspectError::TooComplex);
    }
    let groups: Vec<usize> = groups.into_iter().collect();
    let mut seen: BTreeSet<(Vec<structure::LayerObject>, Vec<structure::LayerObject>)> = BTreeSet::new();
    for view in 0u32..(1u32 << groups.len()) {
        let on = |group: usize| groups.iter().position(|known| *known == group).is_some_and(|bit| view & (1u32 << bit) != 0);
        let (base_hidden, edited_hidden) = (b.layers.hidden(&on), e.layers.hidden(&on));
        if (base_hidden.is_empty() && edited_hidden.is_empty()) || !seen.insert((base_hidden.clone(), edited_hidden.clone())) {
            continue;
        }
        let base_view = render_hiding(before, pair.0, &base_hidden, budget).map_err(|fail| fail.of(Role::Base))?;
        let edited_view = render_hiding(after, pair.1, &edited_hidden, budget).map_err(|fail| fail.of(Role::Edited))?;
        if visual::redaction_boxes(&base_view, &edited_view).outside {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The page rendered without the content inside `hidden` optional content: one view of its
/// layers (see [`structure::PageLayers::hidden`]).
fn render_hiding(side: &Side, index: usize, hidden: &[(i32, i32)], budget: &mut Budget) -> Result<UprightRaster, Fail> {
    render_with(side, index, hidden, budget)
}

fn render(side: &Side, index: usize, budget: &mut Budget) -> Result<UprightRaster, Fail> {
    render_with(side, index, &[], budget)
}

fn render_with(side: &Side, index: usize, hidden: &[(i32, i32)], budget: &mut Budget) -> Result<UprightRaster, Fail> {
    budget.time()?;
    let page = side.structure.pages.get(index).ok_or_else(|| Fail::Read("a page went missing".into()))?;
    if page.image_pixels > budget.limits.page_image_pixels || page.interpreted > budget.limits.page_render_content {
        return Err(Fail::TooComplex);
    }
    let side_px = u64::from(budget.limits.render_side);
    budget.pixels(side_px.saturating_mul(side_px))?;
    budget.content(page.interpreted)?;
    let before = side.hayro.work_spent();
    let raster = side.hayro.render_upright_hiding(index, budget.limits.render_side, hidden);
    budget.work(&side.hayro, before)?;
    raster.map_err(content_failure)
}

/// Per-page counts, in [`ContentChanges`] and [`TextChanges`] order.
#[derive(Default)]
struct PageCounts {
    text_removed: usize,
    text_added: usize,
    images_removed: usize,
    images_added: usize,
    content_removed: usize,
    content_changed: usize,
    outside_redactions: usize,
}

#[allow(clippy::too_many_arguments)]
fn compare_pages(
    before: &Side,
    after: &Side,
    pairs: &Pairs,
    base_kept: bool,
    layers_changed: bool,
    budget: &mut Budget,
    rasters: &mut Rasters,
    inspection: &mut Inspection,
) -> Result<BTreeSet<usize>, InspectError> {
    let mut counts = PageCounts::default();
    // The pages (edited indices) counted outside their redactions: no mark of theirs is applied.
    let mut outside_pages = BTreeSet::new();
    // The base's structure tree's marked content by where it is, once a rewrite needs it (R6b-47).
    let mut marked_index: Option<structure::MarkedIndex<'_>> = None;
    for &(old, new) in pairs {
        budget.time().map_err(|fail| fail.of(Role::Edited))?;
        let (Some(b), Some(e)) = (before.structure.pages.get(old), after.structure.pages.get(new)) else {
            return Err(InspectError::Edited("a page went missing".into()));
        };
        let drawn_differently = b.content != e.content || b.resources != e.resources || b.other != e.other || layers_changed;
        // R6b-36: readers apply `/Usage` differently (PDFium applies `/Usage /View /ViewState`
        // before `/D`, so its default view is not the one `/D` gives), so no fact about a changed
        // page that uses such a group would hold for every reader.
        if drawn_differently && (b.layers.usage || e.layers.usage) {
            return Err(InspectError::Edited(format!(
                "page {} is changed and uses optional content with /Usage, which readers apply differently",
                new.saturating_add(1)
            )));
        }
        // The same content, resources and page entries draw the same glyphs and images.
        let pairing = if drawn_differently {
            glyphs::pair(&b.glyphs, &e.glyphs, budget).map_err(|fail| fail.of(Role::Edited))?
        } else {
            glyphs::Pairing { removed: Vec::new(), added: Vec::new(), in_order: true }
        };
        let lost_text = !pairing.removed.is_empty();
        let gained_text = !pairing.added.is_empty();
        let mut lost_image = images_missing(&b.images, &e.images);
        let mut gained_image = images_missing(&e.images, &b.images);
        if base_kept {
            counts.content_changed += usize::from(drawn_differently);
        } else {
            // Unchanged content and resources leave only the annotations to compare (nothing on
            // the page explains a change in them).
            let (boxes, mut outside) = if drawn_differently {
                let (base_raster, edited_raster) = rasters.get(before, after, (old, new), budget)?;
                let verdict = visual::redaction_boxes(&base_raster, &edited_raster);
                (verdict.boxes.iter().map(|rect| user_rect(&edited_raster, *rect)).collect(), verdict.outside)
            } else {
                (Vec::new(), false)
            };
            // R6b-29, R6b-36. The page in every combination of its layers: the render of every
            // layer cannot tell content moved into or out of a layer, or covered or uncovered
            // by a layer's content, from what was there. (A changed layer configuration is a
            // change outside the redactions on every page already.)
            if drawn_differently && !outside && !layers_changed {
                outside |= layer_views_differ(before, after, (old, new), budget)?;
            }
            // A glyph, image or form now in other optional content (or shown otherwise by
            // default) than it was: not a redaction, whatever the boxes cover. Nor is text for
            // assistive technology added or changed, or a page entry no fact reports changed.
            if drawn_differently {
                outside |= layers_changed_for(b, e, &pairing, budget).map_err(|fail| fail.of(Role::Edited))?;
                match structure::page_removals(b, e) {
                    Some(removed) => {
                        let taken = &mut inspection.structure_removed;
                        taken.tagged_text = taken.tagged_text.saturating_add(count(removed.text));
                        taken.actions = taken.actions.saturating_add(u32::from(removed.actions));
                        taken.thumbnails = taken.thumbnails.saturating_add(u32::from(removed.thumbnail));
                        taken.metadata = taken.metadata.saturating_add(u32::from(removed.metadata));
                        taken.page_entries = taken.page_entries.saturating_add(u32::from(removed.entries));
                    }
                    None => outside = true,
                }
                // R6b-47: the spans and structure elements whose glyphs were removed may not
                // keep text standing for them (the redacted words would survive for copying and
                // assistive technology).
                if !pairing.removed.is_empty() {
                    let index = match marked_index.take() {
                        Some(index) => index,
                        None => structure::MarkedIndex::new(&before.structure.structure_links, budget).map_err(|fail| fail.of(Role::Base))?,
                    };
                    outside |= structure::alternate_text_kept(b, e, &pairing.removed, &index, &after.structure.structure_tree, budget)
                        .map_err(|fail| fail.of(Role::Edited))?;
                    marked_index = Some(index);
                }
                // Text kept but drawn in another order reads otherwise.
                outside |= !pairing.in_order;
                // Follow-up 7 audit (item 7): a resource entry nothing draws may only be one the
                // base page did not draw either (the unredacted original, kept under its old
                // name or a new one, is not).
                outside |= !e.unused_resources.is_subset(&b.unused_resources);
                // Follow-up 8 (R6b-51, I1): an unused standard font holds no data but its name.
                outside |= !e.unused_standard_fonts.is_subset(&b.offered_resources);
                // Follow-up 8 (R6b-51, C1): content the rewrite brings, written otherwise than
                // the redactor writes (a comment, an unknown operator, a surplus operand, …).
                outside |= e.content_violation.is_some();
                // Follow-up 8 (the redaction re-review's C2): drawn only with what the base
                // page drew with (the redactor makes no font, pattern, shading, colour space or
                // graphics state).
                outside |= !e.drawing_resources.is_subset(&b.drawing_resources);
            }
            let mut areas = Areas::new(&boxes);
            // Every removed glyph by its centre, and every image at its own resolution.
            let glyphs_inside = pairing.removed.iter().all(|&index| {
                b.glyphs
                    .glyph(index)
                    .is_some_and(|(_, rect)| areas.take_point(f64::from(rect[0] + rect[2]) / 2.0, f64::from(rect[1] + rect[3]) / 2.0))
            });
            if drawn_differently {
                let images = images::compare(&b.image_uses, &e.image_uses, (&before.images, &after.images), &mut areas, budget)
                    .map_err(|fail| fail.of(Role::Edited))?;
                lost_image = images.removed;
                gained_image = images.added;
                outside |= images.outside;
            }
            // Content streams: forms drawn more than once where hayro drew them, Type 3 fonts
            // (whose procedures draw unclipped, wherever the glyph is) and annotations'
            // appearances by their rectangle.
            outside |= !glyphs_inside
                || !forms_under(b, e, &mut areas, budget).map_err(|fail| fail.of(Role::Edited))?
                || b.type3 != e.type3
                || !appearances_under(b, e, &mut areas, budget).map_err(|fail| fail.of(Role::Edited))?
                // Which layers show by default changed: not a redaction, whatever the boxes.
                || layers_changed
                // Changed, and drawn with something other viewers may show otherwise than
                // hayro's pixels, on either side (R6b-32).
                || (drawn_differently && (b.render_risk.is_some() || e.render_risk.is_some()))
                // Follow-up 7 audit (item 10): what a box covers is gone, not kept under it.
                || (drawn_differently && kept_under_boxes(b, e, &pairing, &boxes, budget).map_err(|fail| fail.of(Role::Edited))?);
            if outside {
                counts.outside_redactions += 1;
                outside_pages.insert(new);
            } else {
                let page = u32::try_from(new.saturating_add(1)).unwrap_or(u32::MAX);
                // An area that explains no removal is not a redaction.
                for (area, used) in boxes.into_iter().zip(areas.used) {
                    if !used {
                        continue;
                    }
                    if inspection.redaction_areas.len() >= MAX_REDACTION_AREAS {
                        return Err(InspectError::TooComplex);
                    }
                    inspection.redaction_areas.push((page, area));
                }
            }
        }
        counts.text_removed += usize::from(lost_text);
        counts.text_added += usize::from(gained_text);
        counts.images_removed += usize::from(lost_image);
        counts.images_added += usize::from(gained_image);
        counts.content_removed += usize::from(lost_text || lost_image);
    }
    inspection.text = TextChanges { pages_with_text_removed: count(counts.text_removed), pages_with_text_added: count(counts.text_added) };
    inspection.content = ContentChanges {
        pages_with_images_removed: count(counts.images_removed),
        pages_with_images_added: count(counts.images_added),
        pages_with_content_removed: count(counts.content_removed),
        pages_with_content_changed: count(counts.content_changed),
        pages_changed_outside_redactions: count(counts.outside_redactions),
    };
    Ok(outside_pages)
}

/// Whether a pixel of a `width` × `height` image placed by `matrix` has its centre inside one of
/// `boxes`; `None` where the placement is neither upright nor a quarter turn (not told here).
fn pixel_centre_under(matrix: &structure::Matrix, width: usize, height: usize, boxes: &[[f32; 4]]) -> Option<bool> {
    let [a, b, c, d, e, f] = *matrix;
    let tiny = |value: f64| value.abs() < 1e-9;
    // Which image axis each user axis follows: x from u (and y from v), or x from v (y from u).
    let (x_scale, y_scale, x_count, y_count) = if tiny(b) && tiny(c) && !tiny(a) && !tiny(d) {
        (a, d, width, height)
    } else if tiny(a) && tiny(d) && !tiny(b) && !tiny(c) {
        (c, b, height, width)
    } else {
        return None;
    };
    if x_count == 0 || y_count == 0 {
        return Some(false);
    }
    // Some centre (i + 0.5) / count strictly between the two unit-space bounds.
    let centre_between = |p: f64, q: f64, count: usize| {
        let (low, high) = (p.min(q).max(0.0), p.max(q).min(1.0));
        let count = count as f64;
        let first = (low * count - 0.5).floor() + 1.0;
        first < count && (first + 0.5) / count < high && (first + 0.5) / count > low
    };
    Some(boxes.iter().any(|bx| {
        let (x0, x1) = ((f64::from(bx[0]) - e) / x_scale, (f64::from(bx[2]) - e) / x_scale);
        let (y0, y1) = ((f64::from(bx[1]) - f) / y_scale, (f64::from(bx[3]) - f) / y_scale);
        centre_between(x0, x1, x_count) && centre_between(y0, y1, y_count)
    }))
}

/// Follow-up 7 audit (items 10, 11): whether a rewrite added a glyph, or kept, under one of the
/// page's redaction boxes,
/// something the base page had there: a glyph whose em box's centre lies inside a box (paired,
/// not removed), an image drawn at the same place and the same, or an annotation unchanged
/// whose rectangle reaches into a box. A box drawn over content that survives under it
/// (copied, searched, read aloud, or shown as a comment) is no redaction.
fn kept_under_boxes(base: &PageFacts, edited: &PageFacts, pairing: &glyphs::Pairing, boxes: &[[f32; 4]], budget: &mut Budget) -> Result<bool, Fail> {
    // Follow-up 7 audit (item 11): a redaction removes text and adds none; a glyph added (the
    // redacted word written again, invisible or white, anywhere) is no redaction.
    if !pairing.added.is_empty() {
        return Ok(true);
    }
    if boxes.is_empty() {
        return Ok(false);
    }
    let inside = |x: f32, y: f32| boxes.iter().any(|b| x > b[0] && x < b[2] && y > b[1] && y < b[3]);
    // More than a point into a box, both ways.
    let reaches = |rect: [f64; 4]| {
        boxes.iter().any(|b| {
            let width = rect[2].min(f64::from(b[2])) - rect[0].max(f64::from(b[0]));
            let height = rect[3].min(f64::from(b[3])) - rect[1].max(f64::from(b[1]));
            width > 1.0 && height > 1.0
        })
    };
    let removed: BTreeSet<usize> = pairing.removed.iter().copied().collect();
    for index in 0..base.glyphs.len() {
        budget.tick()?;
        if removed.contains(&index) {
            continue;
        }
        if let Some((_, rect)) = base.glyphs.glyph(index)
            && inside((rect[0] + rect[2]) / 2.0, (rect[1] + rect[3]) / 2.0)
        {
            return Ok(true);
        }
    }
    for image in edited.image_uses.iter().filter(|image| !image.tiled) {
        budget.tick()?;
        let kept = base.image_uses.iter().any(|old| !old.tiled && old.digest == image.digest && images::same_placement(&old.matrix, &image.matrix));
        if !kept {
            continue;
        }
        // A pixel whose centre lies under a box (what the redactor clears) kept; where that
        // cannot be told exactly (an inline image, a rotated or skewed placement), any reach.
        let under = match image.image.as_deref() {
            Some(info) => match pixel_centre_under(&image.matrix, info.width as usize, info.height as usize, boxes) {
                Some(under) => under,
                None => reaches(structure::unit_square(&image.matrix)),
            },
            None => reaches(structure::unit_square(&image.matrix)),
        };
        if under {
            return Ok(true);
        }
    }
    for annot in edited.annots.iter().filter(|annot| annot.tracked) {
        budget.tick()?;
        let kept = base.annots.iter().any(|old| old.digest == annot.digest);
        if kept && annot.rect.is_some_and(reaches) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the forms the base page draws and the edited page draws differ only under the areas:
/// each form (by its key, see [`PageFacts::forms`]) the one page draws more often than the other
/// must lie wholly under the areas, by its `/BBox` everywhere hayro drew it on that page. A form
/// hayro did not draw on the page (in a tiling pattern's cell) is never under them. Forms drawn
/// once in their file are not among them (see `structure::retain_shared_forms`).
fn forms_under(base: &PageFacts, edited: &PageFacts, areas: &mut Areas, budget: &mut Budget) -> Result<bool, Fail> {
    let tally = |page: &PageFacts| {
        let mut counts: BTreeMap<Digest, usize> = BTreeMap::new();
        for form in &page.forms {
            let count = counts.entry(form.key).or_default();
            *count = count.saturating_add(1);
        }
        counts
    };
    let (in_base, in_edited) = (tally(base), tally(edited));
    let keys: BTreeSet<&Digest> = in_base.keys().chain(in_edited.keys()).collect();
    for key in keys {
        budget.tick()?;
        let (from_base, from_edited) = (in_base.get(key).copied().unwrap_or(0), in_edited.get(key).copied().unwrap_or(0));
        if from_base == from_edited {
            continue;
        }
        let page = if from_base > from_edited { base } else { edited };
        let objects: BTreeSet<ObjRef> = page.forms.iter().filter(|form| &form.key == key).map(|form| form.obj).collect();
        let mut placed = false;
        for draw in page.form_draws.iter().filter(|draw| objects.contains(&ObjRef::new(draw.num, draw.generation))) {
            if draw.tiled || !areas.take_rect(&draw.rect) {
                return Ok(false);
            }
            placed = true;
        }
        if !placed {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether the annotations of the base page and the edited page that are alike but for their
/// appearance (compared as a stream: see `structure`) lie wholly under the areas, by `/Rect`.
fn appearances_under(base: &PageFacts, edited: &PageFacts, areas: &mut Areas, budget: &mut Budget) -> Result<bool, Fail> {
    let mut by_digest: HashMap<Digest, Vec<&Annot>> = HashMap::new();
    for annot in edited.annots.iter().filter(|annot| annot.tracked) {
        by_digest.entry(annot.digest).or_default().push(annot);
    }
    let mut changed: Vec<&Annot> = Vec::new();
    for annot in base.annots.iter().filter(|annot| annot.tracked) {
        budget.tick()?;
        if by_digest.get_mut(&annot.digest).and_then(Vec::pop).is_none() {
            changed.push(annot);
        }
    }
    // The edited annotations left, by their dictionaries without the appearance.
    let mut by_bare: HashMap<Digest, Vec<&Annot>> = HashMap::new();
    for annot in by_digest.into_values().flatten() {
        by_bare.entry(annot.bare).or_default().push(annot);
    }
    for annot in changed {
        budget.tick()?;
        let Some(other) = by_bare.get_mut(&annot.bare).and_then(Vec::pop) else { continue };
        for rect in [annot.rect, other.rect] {
            if !rect.is_some_and(|rect| areas.take_rect(&rect)) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// How far (PDF points) a removed glyph's centre, or a changed image pixel's, may lie outside a
/// redaction area.
pub(crate) const AREA_TOLERANCE: f32 = 1.0;

/// A box of render pixels in the page's user space (PDF points), widened by its anti-aliased edge.
fn user_rect(raster: &UprightRaster, rect: visual::PixelRect) -> [f32; 4] {
    let scale = if raster.scale > 0.0 { raster.scale } else { 1.0 };
    let x = |px: usize| raster.crop[0] + px as f32 / scale;
    let y = |py: usize| raster.crop[1] + raster.height_pt - py as f32 / scale;
    let (x0, x1) = (rect.x0.saturating_sub(1), rect.x1.saturating_add(1).min(raster.width as usize));
    let (top, bottom) = (rect.y0.saturating_sub(1), rect.y1.saturating_add(1).min(raster.height as usize));
    [x(x0), y(bottom), x(x1), y(top)]
}

/// Whether the annotations both pages have come in the same order in their `/Annots`: each by
/// its object in an appended update (which keeps object numbers), else by its digest; those
/// added or removed are left out.
fn annotations_in_order(base: &PageFacts, edited: &PageFacts, base_kept: bool) -> bool {
    let key = |annot: &Annot| match annot.id {
        Some(id) if base_kept => (Some(id), [0u8; 32]),
        _ => (None, annot.digest),
    };
    let tally = |page: &PageFacts| {
        let mut counts: BTreeMap<(Option<ObjRef>, Digest), usize> = BTreeMap::new();
        for annot in &page.annots {
            let slot = counts.entry(key(annot)).or_default();
            *slot = slot.saturating_add(1);
        }
        counts
    };
    let (in_base, in_edited) = (tally(base), tally(edited));
    // Each side's annotations that the other has too (as many times as both have them).
    let common = |page: &PageFacts, other: &BTreeMap<(Option<ObjRef>, Digest), usize>| {
        let mut left = other.clone();
        page.annots
            .iter()
            .filter_map(|annot| {
                let slot = left.get_mut(&key(annot))?;
                if *slot == 0 {
                    return None;
                }
                *slot -= 1;
                Some(key(annot))
            })
            .collect::<Vec<_>>()
    };
    common(base, &in_edited) == common(edited, &in_base)
}

/// Whether `rect` lies wholly inside one of `areas` on page `page` (from 1), within a point
/// (follow-up 8 audit: what only reaches into an area was not under the mark).
fn within_area(rect: &[f64; 4], page: u32, areas: &[(u32, [f32; 4])]) -> bool {
    const SLACK: f64 = 1.0;
    areas.iter().any(|(on, area)| {
        *on == page
            && rect[0] >= f64::from(area[0]) - SLACK
            && rect[1] >= f64::from(area[1]) - SLACK
            && rect[2] <= f64::from(area[2]) + SLACK
            && rect[3] <= f64::from(area[3]) + SLACK
    })
}

/// Follow-up 8 (P6-35): the edited file's marks' areas, by page (from 1).
fn marks_by_page(after: &Structure) -> Vec<(u32, [f32; 4])> {
    let mut areas = Vec::new();
    for (index, page) in after.pages.iter().enumerate() {
        for annot in page.annots.iter().filter(|annot| annot.subtype == b"Redact") {
            for area in &annot.areas {
                areas.push((u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX), area.map(|value| value as f32)));
            }
        }
    }
    areas
}

/// Follow-up 8 (P6-33): the base's terminal fields the edited file no longer has whose every
/// widget lay on a page present in both files, not counted outside its redactions, wholly
/// inside one of its redaction areas.
fn fields_under_areas(
    before: &Structure,
    after: &Structure,
    pairs: &Pairs,
    outside: &BTreeSet<usize>,
    areas: &[(u32, [f32; 4])],
) -> BTreeSet<String> {
    let followed: HashMap<usize, usize> = pairs.iter().copied().collect();
    before
        .fields
        .keys()
        .filter(|name| !after.fields.contains_key(*name))
        .filter(|name| {
            let places = before.field_places.get(*name).map(Vec::as_slice).unwrap_or_default();
            !places.is_empty()
                && places.iter().all(|(page, rect)| {
                    let Some(new) = followed.get(page).copied() else { return false };
                    !outside.contains(&new)
                        && rect.is_some_and(|rect| within_area(&rect, u32::try_from(new.saturating_add(1)).unwrap_or(u32::MAX), areas))
                })
        })
        .cloned()
        .collect()
}

fn fields_changed(before: &Structure, after: &Structure, redacted: &BTreeSet<String>) -> u32 {
    let names: BTreeSet<&String> = before.fields.keys().chain(after.fields.keys()).filter(|name| !redacted.contains(*name)).collect();
    let fields = names.into_iter().filter(|name| before.fields.get(*name) != after.fields.get(*name)).count();
    // The fields' order (`/Fields`, and each field's `/Kids`): their navigation and calculation
    // order in some viewers; a change counts as the form's own (the fields a redaction took away
    // left out of the base's).
    let base_order: Vec<&String> = before.field_order.iter().filter(|name| !redacted.contains(*name)).collect();
    let form_changed = before.form != after.form || base_order != after.field_order.iter().collect::<Vec<_>>();
    count(fields).saturating_add(u32::from(form_changed))
}

fn add_kind(changes: &mut AnnotationChanges, subtype: &[u8]) {
    let slot = match subtype {
        b"Stamp" => &mut changes.stamps,
        b"Ink" | b"Line" | b"Square" | b"Circle" | b"Polygon" | b"PolyLine" => &mut changes.drawings,
        b"Text" | b"FreeText" | b"Highlight" | b"Underline" | b"StrikeOut" | b"Squiggly" | b"Caret" | b"FileAttachment" | b"Sound" => {
            &mut changes.comments
        }
        b"Redact" => &mut changes.redaction_marks,
        _ => &mut changes.other,
    };
    *slot = slot.saturating_add(1);
}

/// What the annotation comparison needs besides the annotations.
struct AnnotationContext<'s> {
    before: &'s Side,
    after: &'s Side,
    base_kept: bool,
    /// The pages counted outside their redactions (edited indices): no mark of theirs counts
    /// as applied (follow-up 7 audit, item 10).
    outside_pages: &'s BTreeSet<usize>,
    /// Base annotations a rewrite took away from pages present in both files, by object.
    gone: std::cell::RefCell<BTreeSet<ObjRef>>,
    /// The redaction areas (see [`Inspection::redaction_areas`]).
    areas: &'s [(u32, [f32; 4])],
    /// See [`Inspection::applied_areas`].
    applied: std::cell::RefCell<Vec<(u32, [f32; 4])>>,
}

impl AnnotationContext<'_> {
    /// A base annotation gone from the edited file: an applied redaction when it is a `Redact`
    /// mark of a page present in a rewrite and its whole area is now uniformly filled.
    fn gone(
        &self,
        annot: &Annot,
        pair: Option<(usize, usize)>,
        budget: &mut Budget,
        rasters: &mut Rasters,
        changes: &mut AnnotationChanges,
    ) -> Result<(), InspectError> {
        if !self.base_kept
            && pair.is_some()
            && let Some(id) = annot.id
        {
            self.gone.borrow_mut().insert(id);
        }
        if !self.base_kept
            && annot.subtype == b"Redact"
            && !annot.areas.is_empty()
            && let Some(pair) = pair
            && !self.outside_pages.contains(&pair.1)
        {
            let (base_raster, edited_raster) = rasters.get(self.before, self.after, pair, budget)?;
            if annot.areas.iter().all(|area| visual::area_filled(&base_raster, &edited_raster, *area)) {
                changes.redactions_applied = changes.redactions_applied.saturating_add(1);
                let page = u32::try_from(pair.1.saturating_add(1)).unwrap_or(u32::MAX);
                self.applied.borrow_mut().extend(annot.areas.iter().map(|area| (page, area.map(|value| value as f32))));
                return Ok(());
            }
        }
        // Follow-up 8 (P6-33): what the redactor removes under a mark.
        if !self.base_kept
            && annot.subtype != b"Redact"
            && let Some((_, new)) = pair
            && !self.outside_pages.contains(&new)
            && annot.rect.is_some_and(|rect| within_area(&rect, u32::try_from(new.saturating_add(1)).unwrap_or(u32::MAX), self.areas))
        {
            changes.removed_by_redaction = changes.removed_by_redaction.saturating_add(1);
            return Ok(());
        }
        changes.removed = changes.removed.saturating_add(1);
        Ok(())
    }
}

/// The annotations compared, and how many links' destinations followed their page.
#[allow(clippy::too_many_arguments)]
fn compare_annotations(
    before: &Side,
    after: &Side,
    pairs: &Pairs,
    base_kept: bool,
    outside_pages: &BTreeSet<usize>,
    areas: &[(u32, [f32; 4])],
    budget: &mut Budget,
    rasters: &mut Rasters,
) -> Result<(AnnotationChanges, u32, PageAreas), InspectError> {
    let context = AnnotationContext {
        before,
        after,
        base_kept,
        outside_pages,
        gone: std::cell::RefCell::new(BTreeSet::new()),
        areas,
        applied: std::cell::RefCell::new(Vec::new()),
    };
    let mut changes = AnnotationChanges::default();
    let mut links = 0u32;
    let to_edited: HashMap<usize, usize> = pairs.iter().copied().collect();
    let pages: BTreeMap<usize, usize> = pairs.iter().copied().collect();
    // A link's destination may change only by following its page (R6b-32).
    let link_moved = |a: &Annot, b: &Annot| -> Result<bool, InspectError> {
        let none = structure::Destination::None;
        outline::followed(a.link.as_ref().unwrap_or(&none), b.link.as_ref().unwrap_or(&none), &pages)
            .map_err(|_| InspectError::Edited("a link's destination changed other than by following its page".into()))
    };
    // Appended updates keep object numbers: their annotations are matched by identity.
    let mut edited_by_id: BTreeMap<ObjRef, (usize, &Annot)> = BTreeMap::new();
    // Per edited page, the annotations matched by content instead: all of them in a rewrite
    // (which renumbers objects), the direct dictionaries in an appended update.
    let mut by_content: BTreeMap<usize, Vec<&Annot>> = BTreeMap::new();
    for (index, page) in after.structure.pages.iter().enumerate() {
        for annot in page.annots.iter().filter(|annot| annot.tracked) {
            match annot.id {
                Some(id) if base_kept => {
                    edited_by_id.insert(id, (index, annot));
                }
                _ => by_content.entry(index).or_default().push(annot),
            }
        }
    }
    let mut seen: BTreeSet<ObjRef> = BTreeSet::new();
    for (old, page) in before.structure.pages.iter().enumerate() {
        let new = to_edited.get(&old).copied();
        let pair = new.map(|new| (old, new));
        for annot in page.annots.iter().filter(|annot| annot.tracked) {
            budget.tick().map_err(|fail| fail.of(Role::Edited))?;
            if base_kept && let Some(id) = annot.id {
                match edited_by_id.get(&id) {
                    Some((index, other)) => {
                        seen.insert(id);
                        let moved = link_moved(annot, other)?;
                        if other.digest != annot.digest || Some(*index) != new {
                            changes.changed = changes.changed.saturating_add(1);
                        } else {
                            links = links.saturating_add(u32::from(moved));
                        }
                    }
                    None => context.gone(annot, pair, budget, rasters, &mut changes)?,
                }
                continue;
            }
            // The same dictionary, its destination (a link's) followed.
            let mut found = None;
            let mut retargeted = false;
            if let Some(list) = new.and_then(|new| by_content.get_mut(&new)) {
                for (position, other) in list.iter().enumerate() {
                    if other.digest != annot.digest {
                        continue;
                    }
                    match link_moved(annot, other) {
                        Ok(moved) => {
                            found = Some((position, moved));
                            break;
                        }
                        Err(_) => retargeted = true,
                    }
                }
                if let Some((position, moved)) = found {
                    list.remove(position);
                    links = links.saturating_add(u32::from(moved));
                }
            }
            if found.is_none() {
                if retargeted {
                    return Err(InspectError::Edited("a link's destination changed other than by following its page".into()));
                }
                context.gone(annot, pair, budget, rasters, &mut changes)?;
            }
        }
    }
    // Follow-up 8 (P6-35): what an update adds wholly under its own marks.
    let marked = if base_kept { marks_by_page(&after.structure) } else { Vec::new() };
    let under = |changes: &mut AnnotationChanges, index: usize, annot: &Annot| {
        let page = u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX);
        if !base_kept || !annot.rect.is_some_and(|rect| within_area(&rect, page, &marked)) {
            return;
        }
        match annot.subtype.as_slice() {
            b"Stamp" => changes.stamps_under_marks = changes.stamps_under_marks.saturating_add(1),
            b"Ink" | b"Line" | b"Square" | b"Circle" | b"Polygon" | b"PolyLine" => {
                changes.drawings_under_marks = changes.drawings_under_marks.saturating_add(1);
            }
            _ => {}
        }
    };
    for (id, (index, annot)) in &edited_by_id {
        if !seen.contains(id) {
            add_kind(&mut changes, &annot.subtype);
            under(&mut changes, *index, annot);
        }
    }
    for (index, annots) in &by_content {
        for annot in annots {
            add_kind(&mut changes, &annot.subtype);
            under(&mut changes, *index, annot);
        }
    }
    // Follow-up 7 audit (item 10): what stands for an annotation a rewrite took away goes too.
    if !base_kept
        && structure::object_text_kept(&context.gone.borrow(), &before.structure.structure_links, &after.structure.structure_tree, budget)
            .map_err(|fail| fail.of(Role::Edited))?
    {
        return Err(InspectError::Edited(
            "the structure tree keeps text standing for an annotation the rewrite took away, which this comparison does not report".into(),
        ));
    }
    Ok((changes, links, context.applied.into_inner()))
}

#[cfg(test)]
mod agreement_tests;
#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod conformance_tests;
#[cfg(test)]
mod corpus_tests;
#[cfg(test)]
mod destination_tests;
#[cfg(test)]
mod layer_tests;
#[cfg(test)]
mod limit_tests;
#[cfg(test)]
mod placement_tests;
#[cfg(test)]
mod selfcheck_tests;
#[cfg(test)]
mod tests;
