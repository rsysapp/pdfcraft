//! The content interpreter that applies (or verifies) redaction on one content scope: a page's
//! streams or a form XObject's.
//!
//! It tracks just enough graphics state to place every glyph, image and path in user space:
//! the CTM stack, the text state (font, size, spacing, scaling, rise, leading) and the text
//! matrices. In apply mode it rewrites the operators:
//! - text: glyphs whose boxes overlap a region are cut out of `Tj`/`TJ`/`'`/`"`, replaced by a
//!   `TJ` displacement of the same width so the remaining text stays exactly where it was;
//! - images: fully covered → the `Do` is removed; partly covered → the covered pixels are
//!   cleared in a copy of the image (or the image is removed when its codec can't be re-encoded);
//!   inline images under a region are removed;
//! - vectors: fully covered paths are removed; partly covered ones are clipped so nothing shows
//!   inside the regions; shadings are clipped the same way;
//! - form XObjects under a region are rewritten recursively into new objects (copy-on-write, so
//!   other pages using the original are untouched).
//!
//! In verify mode nothing changes; glyphs and inline images that still overlap a region are
//! counted.

use std::collections::HashMap;
use std::rc::Rc;

use pdfcraft_content::{Matrix, Op, contains, num, overlaps, parse, serialize_ops, string};
use pdfcraft_cos::{Dict, Document, ObjRef, Object, Stream};

use crate::{Report, image};
use pdfcraft_fonts::pdf::Metrics;

const MAX_DEPTH: usize = 12;

/// The nesting a redaction follows (forms, tiling patterns, soft masks together).
#[cfg(test)]
pub(crate) const MAX_NESTING: u32 = MAX_DEPTH as u32;

/// Embed build (follow-up 8 audit): the work one redaction may do (see [`Scope::work`]): about
/// a second or two.
pub(crate) const MAX_WORK: u64 = 400_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Apply,
    Verify,
}

#[derive(Clone)]
struct Gs {
    ctm: Matrix,
    font: Rc<Metrics>,
    size: f64,
    char_spacing: f64,
    word_spacing: f64,
    render_mode: i64,
    scale: f64,
    leading: f64,
    rise: f64,
    /// Embed build: the font's glyphs cannot be placed (vertical writing, a CMap not read):
    /// text shown with it near a region fails verification.
    unplaceable: bool,
    /// Embed build (R6b-50): the tiling pattern painting fills and strokes (`scn`/`SCN` with a
    /// name), whose cell is redacted where it shows under a region.
    patterns: [Option<Vec<u8>>; 2],
}

/// What processing a scope produced.
#[derive(Default)]
pub(crate) struct Output {
    /// The rewritten streams (`None` = unchanged).
    pub streams: Vec<Option<Vec<u8>>>,
    /// New XObjects the rewritten content refers to: (resource name, object).
    pub xobjects: Vec<(Vec<u8>, ObjRef)>,
    /// Glyphs / inline images still overlapping a region (verify mode).
    pub residue: usize,
    /// Embed build: property lists named in the scope's `/Properties` whose spans' content
    /// changed: they lose the text standing for it (`spans::strip_named`, R6b-47).
    pub properties: Vec<Vec<u8>>,
    /// Embed build: XObject names whose `Do` the redaction removed or replaced, and every name
    /// the rewritten content still draws: a name no longer drawn leaves the resources, or the
    /// original it names (the unredacted form or image) would stay in the file.
    pub dropped: Vec<Vec<u8>>,
    pub drawn: Vec<Vec<u8>>,
    /// Embed build: forms rewritten, for the structure tree's references into them (`/Stm`).
    pub forms: Vec<FormRewrite>,
    /// Embed build (R6b-50): tiling patterns whose cell was redacted, and graphics states whose
    /// soft mask's group was: (resource name, the redacted copy), to set in the scope's
    /// resources under the same name.
    pub patterns: Vec<(Vec<u8>, ObjRef)>,
    pub states: Vec<(Vec<u8>, Object)>,
}

/// Embed build: a form XObject the redaction rewrote into a new object.
pub(crate) struct FormRewrite {
    pub old: ObjRef,
    pub new: ObjRef,
    /// The MCIDs of its content that changed, and those left empty (see `tags::touched`).
    pub changed: std::collections::HashSet<i64>,
    pub empty: std::collections::HashSet<i64>,
}

/// Embed build: the XObject names `ops` draw, and those the forms they draw that have no
/// resources of their own (and so draw with `xobjects`, the scope's) draw in turn.
pub(crate) fn names_drawn(doc: &Document, ops: &[&Op], xobjects: &Dict) -> std::collections::HashSet<Vec<u8>> {
    let mut drawn: std::collections::HashSet<Vec<u8>> = ops.iter().filter(|op| op.is("Do")).filter_map(|op| op.name(0).map(<[u8]>::to_vec)).collect();
    let mut todo: Vec<(Vec<u8>, usize)> = drawn.iter().map(|name| (name.clone(), 0)).collect();
    let mut seen: std::collections::HashSet<ObjRef> = std::collections::HashSet::new();
    while let Some((name, depth)) = todo.pop() {
        let Some(r) = xobjects.get(&name).and_then(Object::as_ref) else { continue };
        if depth >= MAX_DEPTH || !seen.insert(r) {
            continue;
        }
        let obj = doc.get(r);
        let Object::Stream(form) = &*obj else { continue };
        if form.dict.name(b"Subtype") != Some(b"Form") || form.dict.contains(b"Resources") {
            continue;
        }
        let Ok(data) = form.decoded() else { continue };
        for op in parse(&data).ops.iter().filter(|op| op.is("Do")) {
            if let Some(inner) = op.name(0)
                && drawn.insert(inner.to_vec())
            {
                todo.push((inner.to_vec(), depth + 1));
            }
        }
    }
    drawn
}

/// Embed build (R6b-50): the resource names content names, by category (`Font`, `XObject`,
/// `Pattern`, `Shading`, `ExtGState`, `ColorSpace`, `Properties`), with those the forms it draws
/// that have no resources of their own (and so look up the same) name, recursively.
fn names_used(
    doc: &Document,
    data: &[u8],
    resources: &Dict,
    depth: usize,
    out: &mut std::collections::HashMap<&'static [u8], std::collections::HashSet<Vec<u8>>>,
) {
    let xobjects = res_dict(doc, resources, b"XObject");
    for op in parse(data).ops {
        let (category, operand): (&'static [u8], Option<&Object>) = match op.op.as_slice() {
            b"Tf" => (b"Font", op.operands.first()),
            b"Do" => (b"XObject", op.operands.first()),
            b"scn" | b"SCN" => (b"Pattern", op.operands.last()),
            b"sh" => (b"Shading", op.operands.first()),
            b"gs" => (b"ExtGState", op.operands.first()),
            b"cs" | b"CS" => (b"ColorSpace", op.operands.first()),
            b"BDC" | b"DP" => (b"Properties", op.operands.get(1)),
            _ => continue,
        };
        let Some(name) = operand.and_then(Object::as_name) else { continue };
        if !out.entry(category).or_default().insert(name.to_vec()) || category != b"XObject" || depth >= MAX_DEPTH {
            continue;
        }
        if let Some(r) = xobjects.get(name).and_then(Object::as_ref)
            && let Object::Stream(form) = &*doc.get(r)
            && form.dict.name(b"Subtype") == Some(b"Form")
            && !form.dict.contains(b"Resources")
            && let Ok(inner) = form.decoded()
        {
            names_used(doc, &inner, resources, depth + 1, out);
        }
    }
}

/// Embed build (R6b-50): remove from `resources` every entry the content `before` named and
/// the content `after` (the redacted) no longer names: a font used only by the removed text, a
/// pattern or shading painted only under a mark, … (Entries `before` did not name are left
/// alone: something else may look them up.) How many went.
pub(crate) fn forget_unused(doc: &Document, resources: &mut Dict, before: &[u8], after: &[u8]) -> usize {
    let (mut was, mut is) = (std::collections::HashMap::new(), std::collections::HashMap::new());
    names_used(doc, before, resources, 0, &mut was);
    names_used(doc, after, resources, 0, &mut is);
    let mut gone = 0;
    for (category, names) in was {
        let still = is.get(category);
        let mut dict = res_dict(doc, resources, category);
        let mut changed = false;
        for name in names {
            if !still.is_some_and(|still| still.contains(&name)) && dict.remove(&name).is_some() {
                gone += 1;
                changed = true;
            }
        }
        if changed {
            resources.set(category.to_vec(), Object::Dict(dict));
        }
    }
    gone
}

/// Embed build: `xobjects` without the `dropped` names `drawn` does not hold. Whether any went.
pub(crate) fn forget_dropped(xobjects: &mut Dict, dropped: &[Vec<u8>], drawn: &std::collections::HashSet<Vec<u8>>) -> bool {
    let mut gone = false;
    for name in dropped {
        if !drawn.contains(name) && xobjects.remove(name).is_some() {
            gone = true;
        }
    }
    gone
}

pub(crate) struct Scope<'a> {
    pub rects: &'a [[f64; 4]],
    pub mode: Mode,
    pub report: &'a mut Report,
    /// Sanitize: remove hidden text (invisible render modes, or wholly outside this page box).
    pub hidden_text: Option<[f64; 4]>,
    /// Sanitize: optional content groups (and membership dictionaries) that are off; content
    /// marked with them is removed.
    pub hidden_layers: Vec<ObjRef>,
    /// Content removed from hidden layers (blocks and XObjects).
    pub layer_blocks: usize,
    fonts: HashMap<Vec<u8>, Rc<Metrics>>,
    used_names: Vec<Vec<u8>>,
    depth: usize,
    /// Embed build: the resources of the scopes around this one (a form's page or form),
    /// outermost first: a name missing from a form's own resources is looked up there, as hayro
    /// does.
    parents: Vec<Dict>,
    /// Embed build (R6b-50): the text of the glyphs removed, each run of them followed by a
    /// space (for clearing copies of the removed words elsewhere in the document).
    pub removed_text: String,
    /// Embed build (follow-up 8 audit): the work left for the whole redaction (an operator, a
    /// glyph or pixel tested against each region), shared by every nested scope; at zero the
    /// redaction fails (see [`Scope::exhausted`]).
    pub work: std::rc::Rc<std::cell::Cell<u64>>,
}

impl<'a> Scope<'a> {
    pub fn new(rects: &'a [[f64; 4]], mode: Mode, report: &'a mut Report) -> Self {
        Scope {
            rects,
            mode,
            report,
            hidden_text: None,
            hidden_layers: Vec::new(),
            layer_blocks: 0,
            fonts: HashMap::new(),
            used_names: Vec::new(),
            depth: 0,
            parents: Vec::new(),
            removed_text: String::new(),
            work: std::rc::Rc::new(std::cell::Cell::new(MAX_WORK)),
        }
    }

    /// Embed build (follow-up 8 audit): spend `units` of work.
    fn charge(&self, units: usize) {
        self.work.set(self.work.get().saturating_sub(units as u64));
    }

    /// Embed build (follow-up 8 audit): whether the work bound is spent.
    pub fn exhausted(&self) -> bool {
        self.work.get() == 0
    }

    /// Embed build: resource `name` of `category`: in `own` (the scope's resources, by
    /// category), else in the enclosing scopes', innermost first.
    fn resource(&self, doc: &Document, own: &Dict, category: &[u8], name: &[u8]) -> Option<Object> {
        if let Some(found) = own.get(name) {
            return Some(found.clone());
        }
        self.parents.iter().rev().find_map(|parent| res_dict(doc, parent, category).get(name).cloned())
    }

    /// Sanitizing visits every form XObject (not only those under a region).
    fn everywhere(&self) -> bool {
        self.hidden_text.is_some() || !self.hidden_layers.is_empty()
    }

    /// Is this optional-content reference (an OCG or OCMD) hidden?
    fn layer_hidden(&self, doc: &Document, o: &Object) -> bool {
        if self.hidden_layers.is_empty() {
            return false;
        }
        if let Some(r) = o.as_ref()
            && self.hidden_layers.contains(&r)
        {
            return true;
        }
        // A membership dictionary (default policy AnyOn): hidden when all its groups are.
        let d = doc.resolve(o);
        let Some(d) = d.as_dict() else { return false };
        if d.name(b"Type") != Some(b"OCMD") {
            return false;
        }
        let groups: Vec<ObjRef> = match d.get(b"OCGs").map(|g| (*doc.resolve(g)).clone()) {
            Some(Object::Array(a)) => a.iter().filter_map(Object::as_ref).collect(),
            Some(_) => d.get(b"OCGs").and_then(Object::as_ref).into_iter().collect(),
            None => Vec::new(),
        };
        !groups.is_empty() && groups.iter().all(|g| self.hidden_layers.contains(g))
    }

    fn hits(&self, b: [f64; 4]) -> bool {
        self.rects.iter().any(|r| overlaps(*r, b, 0.0))
    }

    fn covered(&self, b: [f64; 4]) -> bool {
        self.rects.iter().any(|r| contains(*r, b, 0.01))
    }

    /// Does a glyph box count as under a region? Any real overlap does (more than a fifth of
    /// the glyph or a point, whichever is less, in both directions), so neighbours that only
    /// touch an edge survive while partly covered glyphs go. Degenerate boxes (zero-size text)
    /// count when their origin is inside.
    fn glyph_hit(&self, b: [f64; 4], origin: (f64, f64)) -> bool {
        let (gw, gh) = (b[2] - b[0], b[3] - b[1]);
        if gw < 0.01 || gh < 0.01 {
            return self.rects.iter().any(|r| origin.0 >= r[0] && origin.0 <= r[2] && origin.1 >= r[1] && origin.1 <= r[3]);
        }
        // Embed build (follow-up 8 audit): any glyph whose box reaches into a region goes, however
        // little (one only partly covered would keep part of what was marked readable).
        self.charge(self.rects.len());
        self.rects.iter().any(|r| {
            let ox = b[2].min(r[2]) - b[0].max(r[0]);
            let oy = b[3].min(r[3]) - b[1].max(r[1]);
            ox > 0.0 && oy > 0.0
        })
    }
}

/// Embed build: whether the redactor cannot place a font's glyphs: a composite font written
/// vertically (`/WMode 1`, a `-V` CMap) or with a CMap it does not read (any predefined CMap but
/// `Identity-H`, whose codes and widths it would split and measure wrongly, or one that uses
/// another, `/UseCMap`).
fn unplaceable(doc: &Document, font: &Dict) -> bool {
    if font.name(b"Subtype") != Some(b"Type0") {
        return false;
    }
    match font.get(b"Encoding").map(|encoding| doc.resolve(encoding)).as_deref() {
        Some(Object::Name(name)) => name.as_slice() != b"Identity-H",
        Some(Object::Stream(cmap)) => {
            let vertical = cmap.dict.get(b"WMode").and_then(|mode| doc.resolve(mode).as_int()).unwrap_or(0) != 0;
            let data = cmap.decoded().unwrap_or_default();
            vertical
                || cmap.dict.contains(b"UseCMap")
                || data.windows(7).any(|w| w == b"usecmap")
                || data.windows(6).any(|w| w == b"/WMode") && !data.windows(8).any(|w| w == b"/WMode 0")
        }
        _ => true,
    }
}

fn res_dict(doc: &Document, res: &Dict, key: &[u8]) -> Dict {
    res.get(key).map(|o| doc.resolve(o)).and_then(|o| o.as_dict().cloned()).unwrap_or_default()
}

fn fresh_name(scope: &mut Scope<'_>, xobjects: &Dict) -> Vec<u8> {
    let mut i = scope.used_names.len() + 1;
    loop {
        let n = format!("PCRedacted{i}").into_bytes();
        if !xobjects.contains(&n) && !scope.used_names.contains(&n) {
            scope.used_names.push(n.clone());
            return n;
        }
        i += 1;
    }
}

/// The clip that hides everything inside each region: one even-odd clip per region (a big
/// rectangle with the region cut out), in the current user space. Returns `None` when the CTM
/// can't be inverted (nothing is painted then anyway).
fn clip_out(rects: &[[f64; 4]], ctm: &Matrix, around: [f64; 4]) -> Option<Vec<Op>> {
    let inv = ctm.invert()?;
    let mut ops = Vec::new();
    let big = [around[0] - 10.0, around[1] - 10.0, around[2] + 10.0, around[3] + 10.0];
    for r in rects.iter().filter(|r| overlaps(**r, around, 0.0)) {
        let outer = [(big[0], big[1]), (big[2], big[1]), (big[2], big[3]), (big[0], big[3])];
        let inner = [(r[0], r[1]), (r[0], r[3]), (r[2], r[3]), (r[2], r[1])];
        for poly in [outer, inner] {
            for (k, (x, y)) in poly.iter().enumerate() {
                let (u, v) = inv.apply(*x, *y);
                ops.push(Op::new(if k == 0 { "m" } else { "l" }, vec![num(u), num(v)]));
            }
            ops.push(Op::new("h", vec![]));
        }
        ops.push(Op::new("W*", vec![]));
        ops.push(Op::new("n", vec![]));
    }
    Some(ops)
}

/// One glyph of a shown string.
struct Glyph {
    bytes: std::ops::Range<usize>,
    /// Advance in unscaled text space (before `Th`).
    advance: f64,
    hit: bool,
    /// Embed build (R6b-50): its code, for the text removed.
    code: u32,
}

/// Process a scope's content streams (state flows from one stream to the next).
pub(crate) fn process(doc: &mut Document, scope: &mut Scope<'_>, streams: &[Vec<u8>], resources: &Dict, ctm: Matrix) -> Output {
    let mut out = Output::default();
    let fallback = Rc::new(Metrics::fallback());
    let mut gs = Gs {
        ctm,
        font: fallback.clone(),
        size: 0.0,
        char_spacing: 0.0,
        word_spacing: 0.0,
        render_mode: 0,
        scale: 1.0,
        leading: 0.0,
        rise: 0.0,
        unplaceable: false,
        patterns: [None, None],
    };
    let mut stack: Vec<Gs> = Vec::new();
    let (mut tm, mut tlm) = (Matrix::IDENTITY, Matrix::IDENTITY);
    // Embed build: codes of unplaceable text shown since the line was last set (see `show`).
    let mut drift = 0.0;
    let fonts_res = res_dict(doc, resources, b"Font");
    let mut xobjects = res_dict(doc, resources, b"XObject");
    // The current path: its operators, its bounding box in user space, and whether it clips.
    let mut path: Vec<Op> = Vec::new();
    let mut path_box: Option<[f64; 4]> = None;
    let mut clip = false;
    let mut forced_change = false;

    // The streams are one content stream in pieces, which may be split between any two tokens
    // (ISO 32000-2 §7.8.2): an operator's operands can end one piece and the operator start the
    // next. Parse them joined, then give each operator back to the piece its keyword is in.
    let mut joined: Vec<u8> = Vec::new();
    let mut ends: Vec<usize> = Vec::with_capacity(streams.len());
    for data in streams {
        joined.extend_from_slice(data);
        joined.push(b'\n');
        ends.push(joined.len());
    }
    let piece_of = |at: usize| ends.iter().position(|&e| at < e).unwrap_or(ends.len().saturating_sub(1));
    let mut parsed = parse(&joined);
    if !scope.hidden_layers.is_empty() {
        let props = res_dict(doc, resources, b"Properties");
        let (kept, removed) = strip_hidden_layers(doc, scope, parsed.ops, &props);
        parsed.ops = kept;
        if removed > 0 {
            scope.layer_blocks += removed;
            forced_change = true;
        }
    }
    let mut pieces: Vec<Vec<Op>> = streams.iter().map(|_| Vec::new()).collect();
    let mut rewrite: Vec<bool> = vec![forced_change; streams.len()];
    for op in parsed.ops {
        let (first, last) = (piece_of(op.span.start), piece_of(op.span.end.saturating_sub(1)));
        // An operator split across pieces is written whole into its keyword's piece, so every
        // piece it spans is rewritten.
        if first < last {
            for r in rewrite.iter_mut().take(last + 1).skip(first) {
                *r = true;
            }
        }
        if let Some(p) = pieces.get_mut(last) {
            p.push(op);
        }
    }

    // Embed build: the operators as they were, for the spans whose content changes (R6b-47).
    let original: Vec<Vec<Op>> = if scope.mode == Mode::Apply { pieces.clone() } else { Vec::new() };
    let mut rewritten: Vec<(Vec<Op>, bool)> = Vec::with_capacity(pieces.len());
    for (piece, mut changed) in pieces.into_iter().zip(rewrite) {
        let mut ops: Vec<Op> = Vec::with_capacity(piece.len());
        for op in piece {
            scope.charge(1usize.saturating_add(scope.rects.len() / 64));
            if scope.exhausted() {
                out.residue += 1;
                break;
            }
            let o = op.op.as_slice();
            // Path construction.
            if matches!(o, b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re" | b"W" | b"W*") {
                let pts: Vec<(f64, f64)> = match o {
                    b"re" => op.nums::<4>().map(|[x, y, w, h]| vec![(x, y), (x + w, y), (x, y + h), (x + w, y + h)]).unwrap_or_default(),
                    b"h" | b"W" | b"W*" => Vec::new(),
                    _ => op.operands.iter().filter_map(Object::as_f64).collect::<Vec<_>>().as_chunks::<2>().0.iter().map(|p| (p[0], p[1])).collect(),
                };
                for (x, y) in pts {
                    let (u, v) = gs.ctm.apply(x, y);
                    path_box = Some(match path_box {
                        None => [u, v, u, v],
                        Some(b) => [b[0].min(u), b[1].min(v), b[2].max(u), b[3].max(v)],
                    });
                }
                clip |= matches!(o, b"W" | b"W*");
                path.push(op);
                continue;
            }
            if matches!(o, b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"n") {
                let body = std::mem::take(&mut path);
                let bbox = path_box.take();
                let clips = std::mem::replace(&mut clip, false);
                let hit = scope.mode == Mode::Apply && o != b"n" && bbox.is_some_and(|b| scope.hits(b));
                // Embed build (R6b-50): a tiling pattern painted over a region shows its cell
                // there: the cell is redacted too (its data would keep what lies under the box).
                if hit && let Some(b) = bbox {
                    let fills = matches!(o, b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*");
                    let strokes = matches!(o, b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*");
                    for (paints, slot) in [(fills, 0usize), (strokes, 1)] {
                        if paints && let Some(name) = gs.patterns[slot].clone() {
                            redact_pattern(doc, scope, &name, resources, ctm, b, &mut out);
                        }
                    }
                }
                if !hit {
                    ops.extend(body);
                    ops.push(op);
                    continue;
                }
                let b = bbox.unwrap_or_default();
                changed = true;
                if scope.covered(b) {
                    scope.report.paths_removed += 1;
                    if clips {
                        // Keep the clipping, drop the painting.
                        ops.extend(body);
                        ops.push(Op::new("n", vec![]));
                    }
                } else if clips {
                    // A clip-and-paint path can't be wrapped in q/Q without losing its clip:
                    // paint it clipped, then set its clip with a no-op path.
                    scope.report.paths_clipped += 1;
                    if let Some(c) = clip_out(scope.rects, &gs.ctm, b) {
                        let plain: Vec<Op> = body.iter().filter(|p| !p.is("W") && !p.is("W*")).cloned().collect();
                        ops.push(Op::new("q", vec![]));
                        ops.extend(c);
                        ops.extend(plain);
                        ops.push(op);
                        ops.push(Op::new("Q", vec![]));
                    }
                    ops.extend(body);
                    ops.push(Op::new("n", vec![]));
                } else {
                    scope.report.paths_clipped += 1;
                    if let Some(c) = clip_out(scope.rects, &gs.ctm, b) {
                        ops.push(Op::new("q", vec![]));
                        ops.extend(c);
                        ops.extend(body);
                        ops.push(op);
                        ops.push(Op::new("Q", vec![]));
                    }
                }
                continue;
            }
            if !path.is_empty() {
                // A path not ended by a painting operator: pass it through.
                ops.append(&mut path);
                path_box = None;
                clip = false;
            }
            match o {
                b"q" => stack.push(gs.clone()),
                b"Q" => {
                    if let Some(g) = stack.pop() {
                        gs = g;
                    }
                }
                b"cm" => {
                    if let Some(m) = op.nums::<6>() {
                        gs.ctm = Matrix(m).then(&gs.ctm);
                    }
                }
                b"BT" => {
                    tm = Matrix::IDENTITY;
                    tlm = Matrix::IDENTITY;
                    drift = 0.0;
                }
                b"Tf" => {
                    gs.size = op.num(1).unwrap_or(gs.size);
                    if let Some(name) = op.name(0) {
                        // Embed build: through the enclosing scopes' fonts too, as hayro looks
                        // them up; a font whose glyphs cannot be placed is noted.
                        let font = scope.resource(doc, &fonts_res, b"Font", name).and_then(|f| doc.resolve(&f).as_dict().cloned());
                        gs.unplaceable = font.as_ref().is_some_and(|font| unplaceable(doc, font));
                        gs.font = match scope.fonts.get(name) {
                            Some(f) => f.clone(),
                            None => {
                                let m = font.map(|d| Rc::new(Metrics::from_dict(doc, &d))).unwrap_or_else(|| fallback.clone());
                                scope.fonts.insert(name.to_vec(), m.clone());
                                m
                            }
                        };
                    }
                }
                b"Tc" => gs.char_spacing = op.num(0).unwrap_or(0.0),
                b"Tw" => gs.word_spacing = op.num(0).unwrap_or(0.0),
                b"Tz" => gs.scale = op.num(0).unwrap_or(100.0) / 100.0,
                b"TL" => gs.leading = op.num(0).unwrap_or(0.0),
                b"Ts" => gs.rise = op.num(0).unwrap_or(0.0),
                b"Tr" => gs.render_mode = op.num(0).unwrap_or(0.0) as i64,
                b"Td" | b"TD" => {
                    if let Some([x, y]) = op.nums::<2>() {
                        if o == b"TD" {
                            gs.leading = -y;
                        }
                        tlm = Matrix::translate(x, y).then(&tlm);
                        tm = tlm;
                        drift = 0.0;
                    }
                }
                b"Tm" => {
                    if let Some(m) = op.nums::<6>() {
                        tlm = Matrix(m);
                        tm = tlm;
                        drift = 0.0;
                    }
                }
                b"T*" => {
                    tlm = Matrix::translate(0.0, -gs.leading).then(&tlm);
                    tm = tlm;
                    drift = 0.0;
                }
                b"Tj" | b"TJ" | b"'" | b"\"" => {
                    if o == b"\"" {
                        gs.word_spacing = op.num(0).unwrap_or(gs.word_spacing);
                        gs.char_spacing = op.num(1).unwrap_or(gs.char_spacing);
                    }
                    if matches!(o, b"'" | b"\"") {
                        tlm = Matrix::translate(0.0, -gs.leading).then(&tlm);
                        tm = tlm;
                        drift = 0.0;
                    }
                    let items: Vec<Object> = match o {
                        b"TJ" => op.operands.first().and_then(Object::as_array).cloned().unwrap_or_default(),
                        _ => op.operands.last().cloned().into_iter().collect(),
                    };
                    let mut removed_text = String::new();
                    let (new_items, removed) = show(scope, &gs, &mut tm, &items, &mut drift, &mut removed_text);
                    // Embed build: unplaceable glyphs are never removed, only reported (above).
                    let removed = if gs.unplaceable && scope.mode == Mode::Apply { 0 } else { removed };
                    if removed == 0 {
                        ops.push(op);
                        continue;
                    }
                    if scope.mode == Mode::Verify {
                        out.residue += removed;
                        ops.push(op);
                        continue;
                    }
                    changed = true;
                    scope.report.glyphs += removed;
                    scope.removed_text.push_str(&removed_text);
                    scope.removed_text.push(crate::RUN_END);
                    // Embed build (R6b-50): text filled with a tiling pattern shows its cell.
                    if let Some(name) = gs.patterns[0].clone() {
                        let all = scope
                            .rects
                            .iter()
                            .fold([f64::MAX, f64::MAX, f64::MIN, f64::MIN], |u, r| [u[0].min(r[0]), u[1].min(r[1]), u[2].max(r[2]), u[3].max(r[3])]);
                        redact_pattern(doc, scope, &name, resources, ctm, all, &mut out);
                    }
                    match o {
                        b"\"" => {
                            ops.push(Op::new("Tw", vec![num(gs.word_spacing)]));
                            ops.push(Op::new("Tc", vec![num(gs.char_spacing)]));
                            ops.push(Op::new("T*", vec![]));
                        }
                        b"'" => ops.push(Op::new("T*", vec![])),
                        _ => {}
                    }
                    ops.push(Op::new("TJ", vec![Object::Array(new_items)]));
                    continue;
                }
                b"BI" => {
                    let b = gs.ctm.bbox([0.0, 0.0, 1.0, 1.0]);
                    if scope.hits(b) {
                        if scope.mode == Mode::Verify {
                            out.residue += 1;
                        } else {
                            scope.report.images_removed += 1;
                            changed = true;
                            continue;
                        }
                    }
                }
                // Embed build (R6b-50): which tiling pattern paints, and soft masks' groups.
                b"scn" | b"SCN" | b"sc" | b"SC" | b"cs" | b"CS" | b"g" | b"G" | b"rg" | b"RG" | b"k" | b"K" => {
                    let slot = usize::from(o.first().is_some_and(u8::is_ascii_uppercase));
                    gs.patterns[slot] = match o {
                        b"scn" | b"SCN" => op.operands.last().and_then(Object::as_name).map(<[u8]>::to_vec),
                        _ => None,
                    };
                }
                b"gs" if scope.mode == Mode::Apply => {
                    if let Some(name) = op.name(0) {
                        redact_soft_mask(doc, scope, name, resources, &gs, &mut out);
                    }
                }
                b"sh" if scope.mode == Mode::Apply => {
                    // A shading fills the current clip: clip the regions out.
                    if let Some(c) = clip_out(scope.rects, &gs.ctm, [-1e6, -1e6, 1e6, 1e6]) {
                        changed = true;
                        ops.push(Op::new("q", vec![]));
                        ops.extend(c);
                        ops.push(op);
                        ops.push(Op::new("Q", vec![]));
                        continue;
                    }
                }
                b"Do" => {
                    if let Some(new) = xobject(doc, scope, &op, &gs, resources, &mut xobjects, &mut out) {
                        changed = true;
                        if let Some(name) = op.name(0) {
                            out.dropped.push(name.to_vec());
                        }
                        if let Some(n) = new {
                            ops.push(n);
                        }
                        continue;
                    }
                }
                _ => {}
            }
            ops.push(op);
        }
        if !path.is_empty() {
            ops.append(&mut path);
        }
        rewritten.push((ops, changed));
    }
    // Embed build (R6b-50): a font left showing nothing (its every glyph removed) gives way to
    // one the content still shows text with, so it can leave the file: its subset and map
    // would spell the removed words. What is shown stays where it was: a text displacement
    // depends on the size alone.
    if scope.mode == Mode::Apply && rewritten.iter().any(|(_, changed)| *changed) {
        silence_empty_fonts(&mut rewritten);
    }
    // Embed build: a span whose glyphs (or anything else) the redaction changed no longer
    // carries text standing for them: its /ActualText, /Alt and /E would spell out the removed
    // words for copying and assistive technology (R6b-47).
    if scope.mode == Mode::Apply && rewritten.iter().any(|(_, changed)| *changed) {
        let properties = res_dict(doc, resources, b"Properties");
        let (stripped, named) = crate::spans::strip_changed(doc, &original, &mut rewritten, &properties);
        scope.report.tags += stripped;
        out.properties = named;
    }
    out.drawn = rewritten.iter().flat_map(|(ops, _)| ops.iter()).filter(|op| op.is("Do")).filter_map(|op| op.name(0).map(<[u8]>::to_vec)).collect();
    for (ops, changed) in rewritten {
        out.streams.push(changed.then(|| serialize_ops(&ops)));
    }
    out
}

/// Embed build (R6b-50): every `Tf` whose font shows only empty strings (as far as the font it
/// sets is in effect, `q`/`Q` followed) is given the font of another `Tf` that shows text, the
/// same size; the pieces it changes are marked changed.
fn silence_empty_fonts(pieces: &mut [(Vec<Op>, bool)]) {
    let shows = |op: &Op| -> Option<bool> {
        match op.op.as_slice() {
            b"Tj" | b"'" | b"\"" => Some(op.operands.last().and_then(Object::as_string).is_some_and(|s| !s.bytes.is_empty())),
            b"TJ" => Some(
                op.operands
                    .first()
                    .and_then(Object::as_array)
                    .is_some_and(|items| items.iter().any(|item| item.as_string().is_some_and(|s| !s.bytes.is_empty()))),
            ),
            _ => None,
        }
    };
    // Which `Tf` (piece, index) is in effect at each show, and which show text.
    let mut current: Option<(usize, usize)> = None;
    let mut stack: Vec<Option<(usize, usize)>> = Vec::new();
    let mut showing: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    let mut all: Vec<(usize, usize)> = Vec::new();
    for (p, (ops, _)) in pieces.iter().enumerate() {
        for (i, op) in ops.iter().enumerate() {
            match op.op.as_slice() {
                b"q" => stack.push(current),
                b"Q" => current = stack.pop().unwrap_or(current),
                b"Tf" => {
                    current = Some((p, i));
                    all.push((p, i));
                }
                _ => {
                    if shows(op) == Some(true)
                        && let Some(at) = current
                    {
                        showing.insert(at);
                    }
                }
            }
        }
    }
    let donor = all.iter().find(|at| showing.contains(at)).and_then(|(p, i)| pieces.get(*p)?.0.get(*i)?.name(0).map(<[u8]>::to_vec));
    let Some(donor) = donor else { return };
    for (p, i) in all {
        if showing.contains(&(p, i)) {
            continue;
        }
        if let Some((ops, changed)) = pieces.get_mut(p)
            && let Some(op) = ops.get_mut(i)
            && op.name(0).is_some_and(|name| name != donor.as_slice())
            && let Some(first) = op.operands.first_mut()
        {
            *first = Object::Name(donor.clone());
            *changed = true;
        }
    }
}

/// Lay out a shown string (or `TJ` array) glyph by glyph, advancing `tm`. Returns the `TJ`
/// items with the glyphs under a region replaced by displacements, and how many were removed.
/// `drift` (Embed build): how many codes of unplaceable text this line showed so far.
fn show(scope: &Scope<'_>, gs: &Gs, tm: &mut Matrix, items: &[Object], drift: &mut f64, removed_text: &mut String) -> (Vec<Object>, usize) {
    let f = &gs.font;
    let size = gs.size;
    let mut out: Vec<Object> = Vec::new();
    let mut removed = 0;
    let push_num = |out: &mut Vec<Object>, v: f64| {
        if v == 0.0 {
            return;
        }
        if let Some(last) = out.last_mut()
            && let Some(prev) = last.as_f64()
        {
            *last = num(prev + v);
            return;
        }
        out.push(num(v));
    };
    for item in items {
        match item {
            // Embed build: glyphs that cannot be placed are never taken for outside a region:
            // shown anywhere near one (within twice the string's length in ems, every way), they
            // count as left under it, so verification fails and nothing is changed.
            Object::String(s) if gs.unplaceable => {
                // Where earlier strings of the line left off is not known either.
                *drift += s.bytes.len() as f64;
                let reach = (*drift + 1.0) * 2.0;
                let trm = Matrix([size * gs.scale, 0.0, 0.0, size, 0.0, gs.rise]).then(tm).then(&gs.ctm);
                if scope.hits(trm.bbox([-reach, -reach, reach, reach])) && scope.mode == Mode::Verify {
                    removed += s.bytes.len().max(1);
                }
                out.push(item.clone());
            }
            Object::String(s) => {
                let bytes = &s.bytes;
                let mut glyphs = Vec::new();
                let mut pos = 0;
                for (code, len) in f.codes(bytes) {
                    let w0 = f.width(code);
                    let spacing = gs.char_spacing + if f.is_space(code, len) { gs.word_spacing } else { 0.0 };
                    let trm = Matrix([size * gs.scale, 0.0, 0.0, size, 0.0, gs.rise]).then(tm).then(&gs.ctm);
                    let b = trm.bbox([0.0, f.descent, w0, f.ascent]);
                    let origin = trm.apply(0.0, 0.0);
                    let advance = w0 * size + spacing;
                    let hit = match scope.hidden_text {
                        // Invisible (Tr 3) and clip-only (Tr 7) text, or text wholly off the page.
                        Some(page) => {
                            let degenerate = b[2] - b[0] < 0.01 && b[3] - b[1] < 0.01;
                            matches!(gs.render_mode, 3 | 7) || !(overlaps(page, b, 0.0) || degenerate)
                        }
                        None => scope.glyph_hit(b, origin),
                    };
                    glyphs.push(Glyph { bytes: pos..pos + len, advance, hit, code });
                    *tm = Matrix::translate(advance * gs.scale, 0.0).then(tm);
                    pos += len;
                }
                let mut run: Vec<u8> = Vec::new();
                for g in &glyphs {
                    // Embed build (R6b-50): the removed glyphs' text, runs apart.
                    if g.hit {
                        removed_text.push_str(f.text_of(g.code).unwrap_or("\u{fffd}"));
                    } else if !removed_text.ends_with(crate::RUN_END) && !removed_text.is_empty() {
                        removed_text.push(crate::RUN_END);
                    }
                    if g.hit {
                        removed += 1;
                        if !run.is_empty() {
                            out.push(string(std::mem::take(&mut run)));
                        }
                        // A TJ number moves by -n/1000 × size (× Th): the removed advance.
                        if size != 0.0 {
                            push_num(&mut out, -g.advance * 1000.0 / size);
                        }
                    } else {
                        run.extend_from_slice(&bytes[g.bytes.clone()]);
                    }
                }
                if !run.is_empty() {
                    out.push(string(run));
                }
            }
            other => {
                if let Some(v) = other.as_f64() {
                    *tm = Matrix::translate(-v / 1000.0 * size * gs.scale, 0.0).then(tm);
                    push_num(&mut out, v);
                }
            }
        }
    }
    (out, removed)
}

/// `Do`: `None` = leave the operator alone; `Some(None)` = drop it; `Some(Some(op))` = replace it.
fn xobject(
    doc: &mut Document,
    scope: &mut Scope<'_>,
    op: &Op,
    gs: &Gs,
    resources: &Dict,
    xobjects: &mut Dict,
    out: &mut Output,
) -> Option<Option<Op>> {
    let name = op.name(0)?.to_vec();
    // Embed build: through the enclosing scopes' resources too, as hayro looks it up.
    let r = scope.resource(doc, xobjects, b"XObject", &name)?.as_ref()?;
    let obj = doc.get(r);
    let Object::Stream(s) = &*obj else { return None };
    match s.dict.name(b"Subtype") {
        _ if s.dict.get(b"OC").is_some_and(|oc| scope.layer_hidden(doc, oc)) => {
            if scope.mode == Mode::Verify {
                return None;
            }
            scope.layer_blocks += 1;
            Some(None)
        }
        Some(b"Image") => {
            let b = gs.ctm.bbox([0.0, 0.0, 1.0, 1.0]);
            if !scope.hits(b) || scope.mode == Mode::Verify {
                return None;
            }
            if scope.covered(b) {
                scope.report.images_removed += 1;
                return Some(None);
            }
            // Embed build (follow-up 8 audit): each pixel is tested against the regions.
            let pixels = s.dict.int(b"Width").unwrap_or(0).max(0).saturating_mul(s.dict.int(b"Height").unwrap_or(0).max(0));
            scope.charge(usize::try_from(pixels).unwrap_or(usize::MAX).saturating_mul(scope.rects.len().max(1)).saturating_mul(2));
            if scope.exhausted() {
                out.residue += 1;
                return None;
            }
            match image::clear(doc, s, &gs.ctm, scope.rects) {
                Ok(None) => None,
                Ok(Some(mut new)) => {
                    // Embed build (R6b-50): its soft mask (and a stencil mask) cleared under the
                    // regions too, at its own resolution; one that cannot be cleared removes
                    // the image.
                    for key in [&b"SMask"[..], b"Mask"] {
                        let Some(mask_ref) = new.dict.get(key).and_then(Object::as_ref) else { continue };
                        let mask = doc.get(mask_ref);
                        let Object::Stream(mask) = &*mask else { continue };
                        match image::clear(doc, mask, &gs.ctm, scope.rects) {
                            Ok(None) => {}
                            Ok(Some(cleared)) => {
                                let cleared = doc.add(Object::Stream(cleared));
                                new.dict.set(key.to_vec(), Object::Ref(cleared));
                            }
                            Err(()) => {
                                scope.report.images_removed += 1;
                                return Some(None);
                            }
                        }
                    }
                    let nr = doc.add(Object::Stream(new));
                    let n = fresh_name(scope, xobjects);
                    xobjects.set(n.clone(), Object::Ref(nr));
                    out.xobjects.push((n.clone(), nr));
                    scope.report.images_cleared += 1;
                    Some(Some(Op::new("Do", vec![Object::Name(n)])))
                }
                Err(()) => {
                    scope.report.images_removed += 1;
                    Some(None)
                }
            }
        }
        Some(b"Form") => {
            let m = s.dict.get(b"Matrix").and_then(|m| Matrix::from_operands(doc.resolve(m).as_array()?)).unwrap_or_default();
            let fctm = m.then(&gs.ctm);
            let bbox = s.dict.get(b"BBox").and_then(|b| {
                let v: Vec<f64> = doc.resolve(b).as_array()?.iter().filter_map(Object::as_f64).collect();
                (v.len() == 4).then(|| [v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
            });
            if let Some(bb) = bbox
                && !scope.everywhere()
                && !scope.hits(fctm.bbox(bb))
            {
                return None;
            }
            if scope.depth >= MAX_DEPTH {
                // Embed build (follow-up 8 audit): nested past the bound, the redaction fails.
                out.residue += 1;
                return None;
            }
            let data = match s.decoded() {
                Ok(d) => d,
                Err(_) if scope.mode == Mode::Apply => {
                    scope.report.forms_removed += 1;
                    return Some(None);
                }
                Err(_) => return None,
            };
            let own = s.dict.get(b"Resources").and_then(|r| doc.resolve(r).as_dict().cloned());
            let res = own.clone().unwrap_or_else(|| resources.clone());
            let saved_fonts = std::mem::take(&mut scope.fonts);
            scope.depth += 1;
            // Embed build: what the form's own resources lack, it finds in this scope's.
            scope.parents.push(resources.clone());
            let inner = process(doc, scope, std::slice::from_ref(&data), &res, fctm);
            scope.parents.pop();
            scope.depth -= 1;
            // Embed build: the form's named property lists whose spans changed lose their text
            // (R6b-47); written in its resources, those are written again below.
            let mut own_changed = false;
            let mut res = res;
            if !inner.properties.is_empty() && scope.mode == Mode::Apply {
                match crate::spans::strip_named(doc, &res, &inner.properties) {
                    Ok(Some(new)) => {
                        res = new;
                        own_changed = true;
                    }
                    Ok(None) => {}
                    // The text could not be taken away: the form goes, as a form that cannot be
                    // read does.
                    Err(_) => {
                        scope.report.forms_removed += 1;
                        return Some(None);
                    }
                }
            }
            scope.fonts = saved_fonts;
            out.residue += inner.residue;
            if scope.mode == Mode::Verify {
                return None;
            }
            let new_data = inner.streams.first().cloned().flatten();
            if new_data.is_none() && inner.xobjects.is_empty() && inner.patterns.is_empty() && inner.states.is_empty() {
                return None;
            }
            let mut dict = s.dict.clone();
            dict.remove(b"Length");
            // Embed build (R6b-50): the copy keeps nothing of the original but what it draws:
            // not its XMP, an application's private data or its prepress (`/OPI`) data.
            for key in [&b"Metadata"[..], b"PieceInfo", b"LastModified", b"OPI"] {
                dict.remove(key);
            }
            // Embed build: the names the redaction stopped drawing leave the form's resources
            // (with their unredacted originals).
            let mut xo = res_dict(doc, &res, b"XObject");
            for (n, r) in &inner.xobjects {
                xo.set(n.clone(), Object::Ref(*r));
            }
            let drawn = names_drawn(doc, &parse(new_data.as_deref().unwrap_or(&data)).ops.iter().collect::<Vec<_>>(), &xo);
            let forgotten = forget_dropped(&mut xo, &inner.dropped, &drawn);
            if own_changed || forgotten || !inner.xobjects.is_empty() || !inner.patterns.is_empty() || !inner.states.is_empty() {
                let mut res = res.clone();
                res.set(b"XObject".to_vec(), Object::Dict(xo));
                // Embed build (R6b-50): its redacted patterns and soft masks.
                set_entries(doc, &mut res, &Output { patterns: inner.patterns.clone(), states: inner.states.clone(), ..Output::default() });
                dict.set(b"Resources".to_vec(), Object::Dict(res));
            }
            let stream = match &new_data {
                Some(d) => Stream::flate(dict, d),
                None => Stream { dict, raw: s.raw.clone() },
            };
            let nr = doc.add(Object::Stream(stream));
            // Embed build: the structure tree's references into the original (`/Stm`) follow
            // the rewritten form, its changed marked content cleaned (R6b-47).
            let named = crate::tags::named_mcids(doc, &res);
            let (changed, empty) = match &new_data {
                Some(new) => crate::tags::touched(&data, new, &named),
                None => Default::default(),
            };
            out.forms.extend(inner.forms);
            out.forms.push(FormRewrite { old: r, new: nr, changed, empty });
            let n = fresh_name(scope, xobjects);
            xobjects.set(n.clone(), Object::Ref(nr));
            out.xobjects.push((n.clone(), nr));
            scope.report.forms_rewritten += 1;
            Some(Some(Op::new("Do", vec![Object::Name(n)])))
        }
        _ => None,
    }
}

/// The most tiles of a pattern a redaction looks at under one painted area; more is refused
/// (the redaction is not applied).
const MAX_TILES: usize = 4_096;

/// Embed build (R6b-50): redact the cell of the tiling pattern `name` (in `resources`, or the
/// enclosing scopes'), painted over `painted` (user space) while the scope's base matrix is
/// `base`: the cell's content is processed against the regions under the painted area, moved
/// into every tile that reaches it, and the copy is set under the same name. Once per name and
/// scope; too many tiles fails the redaction (residue).
fn redact_pattern(doc: &mut Document, scope: &mut Scope<'_>, name: &[u8], resources: &Dict, base: Matrix, painted: [f64; 4], out: &mut Output) {
    if out.patterns.iter().any(|(done, _)| done.as_slice() == name) {
        return;
    }
    if scope.depth >= MAX_DEPTH {
        out.residue += 1;
        return;
    }
    let Some(r) = scope.resource(doc, &res_dict(doc, resources, b"Pattern"), b"Pattern", name).and_then(|o| o.as_ref()) else { return };
    let obj = doc.get(r);
    let Object::Stream(pattern) = &*obj else { return };
    if pattern.dict.int(b"PatternType") != Some(1) {
        return;
    }
    let matrix = pattern.dict.get(b"Matrix").and_then(|m| Matrix::from_operands(doc.resolve(m).as_array()?)).unwrap_or_default();
    let to_user = matrix.then(&base);
    let cell = pattern.dict.get(b"BBox").and_then(|b| {
        let v: Vec<f64> = doc.resolve(b).as_array()?.iter().filter_map(Object::as_f64).collect();
        (v.len() == 4).then(|| [v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
    });
    let steps =
        (pattern.dict.get(b"XStep").and_then(Object::as_f64).map(f64::abs), pattern.dict.get(b"YStep").and_then(Object::as_f64).map(f64::abs));
    let (Some(cell), (Some(x_step), Some(y_step)), Some(back)) = (cell, steps, to_user.invert()) else {
        out.residue += 1;
        return;
    };
    if x_step <= 0.0 || y_step <= 0.0 || !x_step.is_finite() || !y_step.is_finite() {
        out.residue += 1;
        return;
    }
    // The regions under the painted area, and the tiles that reach it.
    let under: Vec<[f64; 4]> = scope
        .rects
        .iter()
        .filter(|r| overlaps(**r, painted, 0.0))
        .map(|r| [r[0].max(painted[0]), r[1].max(painted[1]), r[2].min(painted[2]), r[3].min(painted[3])])
        .collect();
    let area = back.bbox(painted);
    let columns = ((area[0] - cell[2]) / x_step).floor() as i64..=((area[2] - cell[0]) / x_step).ceil() as i64;
    let rows = ((area[1] - cell[3]) / y_step).floor() as i64..=((area[3] - cell[1]) / y_step).ceil() as i64;
    let count = (columns.end() - columns.start() + 1).max(0).saturating_mul((rows.end() - rows.start() + 1).max(0));
    if count <= 0 || count as usize > MAX_TILES {
        out.residue += 1;
        return;
    }
    // Each tile is the cell moved by a whole number of steps: in user space, a translation.
    let origin = to_user.apply(0.0, 0.0);
    let moved = usize::try_from(count).unwrap_or(usize::MAX).saturating_mul(under.len());
    scope.charge(moved);
    // Held at once: at most a million moved regions.
    if scope.exhausted() || moved > 1_000_000 {
        out.residue += 1;
        return;
    }
    let mut shifted = Vec::new();
    for i in columns {
        for j in rows.clone() {
            let (x, y) = to_user.apply(i as f64 * x_step, j as f64 * y_step);
            let (dx, dy) = (x - origin.0, y - origin.1);
            shifted.extend(under.iter().map(|r| [r[0] - dx, r[1] - dy, r[2] - dx, r[3] - dy]));
        }
    }
    let Ok(data) = pattern.decoded() else {
        out.residue += 1;
        return;
    };
    let own = pattern.dict.get(b"Resources").and_then(|o| doc.resolve(o).as_dict().cloned()).unwrap_or_default();
    let mut report = Report::default();
    let (inner, removed_text) = {
        let mut cell_scope = Scope::new(&shifted, Mode::Apply, &mut report);
        cell_scope.parents = scope.parents.clone();
        cell_scope.parents.push(resources.clone());
        cell_scope.depth = scope.depth + 1;
        cell_scope.work = scope.work.clone();
        let inner = process(doc, &mut cell_scope, std::slice::from_ref(&data), &own, to_user);
        (inner, std::mem::take(&mut cell_scope.removed_text))
    };
    scope.removed_text.push_str(&removed_text);
    merge(scope.report, &report);
    out.residue += inner.residue;
    let mut dict = pattern.dict.clone();
    for key in [&b"Length"[..], b"Filter", b"DecodeParms", b"DL"] {
        dict.remove(key);
    }
    let mut own = own;
    if !inner.xobjects.is_empty() || !inner.patterns.is_empty() || !inner.states.is_empty() {
        set_entries(doc, &mut own, &inner);
        dict.set(b"Resources".to_vec(), Object::Dict(own));
    }
    let new_data = inner.streams.into_iter().next().flatten().unwrap_or(data);
    let nr = doc.add(Object::Stream(Stream::flate(dict, &new_data)));
    out.patterns.push((name.to_vec(), nr));
}

/// Embed build (R6b-50): redact the group of the soft mask the graphics state `name` sets, when
/// the group reaches a region: a copy of the state whose mask's group is processed against the
/// regions, in the space current when the state is set, set under the same name.
fn redact_soft_mask(doc: &mut Document, scope: &mut Scope<'_>, name: &[u8], resources: &Dict, gs: &Gs, out: &mut Output) {
    if out.states.iter().any(|(done, _)| done.as_slice() == name) {
        return;
    }
    if scope.depth >= MAX_DEPTH {
        out.residue += 1;
        return;
    }
    let Some(entry) = scope.resource(doc, &res_dict(doc, resources, b"ExtGState"), b"ExtGState", name) else { return };
    let Some(mut state) = doc.resolve(&entry).as_dict().cloned() else { return };
    let Some(mut mask) = state.get(b"SMask").and_then(|m| doc.resolve(m).as_dict().cloned()) else { return };
    let Some(group_ref) = mask.get(b"G").and_then(Object::as_ref) else { return };
    let obj = doc.get(group_ref);
    let Object::Stream(group) = &*obj else { return };
    let matrix = group.dict.get(b"Matrix").and_then(|m| Matrix::from_operands(doc.resolve(m).as_array()?)).unwrap_or_default();
    let to_user = matrix.then(&gs.ctm);
    let reaches = group.dict.get(b"BBox").and_then(|b| {
        let v: Vec<f64> = doc.resolve(b).as_array()?.iter().filter_map(Object::as_f64).collect();
        (v.len() == 4).then(|| to_user.bbox([v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])]))
    });
    if reaches.is_some_and(|b| !scope.hits(b)) {
        return;
    }
    let Ok(data) = group.decoded() else {
        out.residue += 1;
        return;
    };
    let own = group.dict.get(b"Resources").and_then(|o| doc.resolve(o).as_dict().cloned()).unwrap_or_default();
    let rects = scope.rects.to_vec();
    let mut report = Report::default();
    let (inner, removed_text) = {
        let mut group_scope = Scope::new(&rects, Mode::Apply, &mut report);
        group_scope.parents = scope.parents.clone();
        group_scope.parents.push(resources.clone());
        group_scope.depth = scope.depth + 1;
        group_scope.work = scope.work.clone();
        let inner = process(doc, &mut group_scope, std::slice::from_ref(&data), &own, to_user);
        (inner, std::mem::take(&mut group_scope.removed_text))
    };
    scope.removed_text.push_str(&removed_text);
    merge(scope.report, &report);
    out.residue += inner.residue;
    let Some(new_data) = inner.streams.first().cloned().flatten() else {
        if inner.xobjects.is_empty() && inner.patterns.is_empty() && inner.states.is_empty() {
            return;
        }
        out.residue += 1;
        return;
    };
    let mut dict = group.dict.clone();
    for key in [&b"Length"[..], b"Filter", b"DecodeParms", b"DL", b"Metadata", b"PieceInfo", b"LastModified", b"OPI"] {
        dict.remove(key);
    }
    let mut own = own;
    set_entries(doc, &mut own, &inner);
    dict.set(b"Resources".to_vec(), Object::Dict(own));
    let nr = doc.add(Object::Stream(Stream::flate(dict, &new_data)));
    mask.set(b"G".to_vec(), Object::Ref(nr));
    state.set(b"SMask".to_vec(), Object::Dict(mask));
    out.states.push((name.to_vec(), Object::Dict(state)));
}

/// Add up what a nested scope removed.
fn merge(into: &mut Report, from: &Report) {
    into.glyphs += from.glyphs;
    into.images_removed += from.images_removed;
    into.images_cleared += from.images_cleared;
    into.paths_removed += from.paths_removed;
    into.paths_clipped += from.paths_clipped;
    into.forms_rewritten += from.forms_rewritten;
    into.forms_removed += from.forms_removed;
    into.tags += from.tags;
}

/// Embed build (R6b-50): set in `resources` what a scope's processing made: its new XObjects,
/// and its redacted patterns and graphics states under their names.
pub(crate) fn set_entries(doc: &Document, resources: &mut Dict, out: &Output) {
    for (category, entries) in [
        (&b"XObject"[..], out.xobjects.iter().map(|(n, r)| (n.clone(), Object::Ref(*r))).collect::<Vec<_>>()),
        (b"Pattern", out.patterns.iter().map(|(n, r)| (n.clone(), Object::Ref(*r))).collect()),
        (b"ExtGState", out.states.clone()),
    ] {
        if entries.is_empty() {
            continue;
        }
        let mut dict = res_dict(doc, resources, category);
        for (name, value) in entries {
            dict.set(name, value);
        }
        resources.set(category.to_vec(), Object::Dict(dict));
    }
}

/// Remove marked-content blocks of hidden layers (`/OC /name BDC … EMC`, nested blocks
/// included). Returns the remaining operators and how many blocks went.
fn strip_hidden_layers(doc: &Document, scope: &Scope<'_>, ops: Vec<Op>, props: &Dict) -> (Vec<Op>, usize) {
    let mut out = Vec::with_capacity(ops.len());
    // For each open marked-content block: does it hide its contents?
    let mut stack: Vec<bool> = Vec::new();
    let mut removed = 0;
    for op in ops {
        let hidden_now = stack.last().copied().unwrap_or(false);
        match op.op.as_slice() {
            b"BDC" | b"BMC" => {
                let this = op.is("BDC")
                    && op.name(0) == Some(b"OC")
                    && match op.operands.get(1) {
                        Some(Object::Name(n)) => props.get(n).is_some_and(|o| scope.layer_hidden(doc, o)),
                        Some(o) => scope.layer_hidden(doc, o),
                        None => false,
                    };
                if this && !hidden_now {
                    removed += 1;
                }
                stack.push(hidden_now || this);
                if !(hidden_now || this) {
                    out.push(op);
                }
            }
            b"EMC" => {
                let was = stack.pop().unwrap_or(false);
                if !was {
                    out.push(op);
                }
            }
            _ => {
                if !hidden_now {
                    out.push(op);
                }
            }
        }
    }
    (out, removed)
}
