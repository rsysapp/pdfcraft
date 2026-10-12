//! Redaction (architecture §11.2, execution plan M8.5–M8.7). Layer L4.
//!
//! Content is marked for redaction with Redact annotations (§12.5.6.23); `pdfcraft-annot`
//! creates them. [`apply`] then removes everything under the marks, for good:
//! - text glyphs (the rest of each line keeps its position), inline images, and paths that the
//!   marks cover; images and vectors partly under a mark lose the covered part;
//! - content inside form XObjects (rewritten as new objects, so pages sharing them keep theirs);
//! - comments, links and form fields whose rectangle overlaps a mark;
//! - the marks themselves, replaced by boxes in their fill colour (with their overlay text)
//!   drawn into the page.
//!
//! A verification pass then re-reads every redacted page; if any glyph or inline image is still
//! under a region the whole operation fails (callers keep the previous document: fail-closed).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use pdfcraft_cos::{Dict, Document, ObjRef, Object, Stream};

mod image;
mod interp;
pub mod patterns;
pub mod sanitize;
// Embed build: spans lose the text standing for what a redaction removed (R6b-47).
mod spans;
mod tags;
#[cfg(test)]
mod tests;

use interp::{Mode, Scope, process};

pub type Rgb = [f64; 3];

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RedactError {
    #[error("there are no redaction marks to apply")]
    NothingToApply,
    #[error("page {0} has content that can't be read, so it can't be redacted safely")]
    Unreadable(usize),
    #[error("redaction could not be verified: {0} item(s) were still found under the marks, so nothing was changed")]
    Residue(usize),
    #[error(transparent)]
    Edit(#[from] pdfcraft_edit::EditError),
    #[error(transparent)]
    Form(#[from] pdfcraft_forms::FormError),
    #[error(transparent)]
    Cos(#[from] pdfcraft_cos::CosError),
}

/// A redaction mark on a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    /// 0-based page index.
    pub page: usize,
    pub obj: ObjRef,
    /// The marked areas in user space (one per quadrilateral).
    pub rects: Vec<[f64; 4]>,
    /// The box colour once applied (`/IC`; none = no box).
    pub fill: Option<Rgb>,
    pub overlay: String,
    /// How the overlay text is drawn (`/DA`, `/Q`, `/Repeat`).
    pub look: pdfcraft_annot::OverlayLook,
}

/// The overlay look of a Redact annotation: font, size and colour from `/DA`, alignment from
/// `/Q`, repetition from `/Repeat`.
fn overlay_look(doc: &Document, d: &Dict) -> pdfcraft_annot::OverlayLook {
    let mut look = pdfcraft_annot::OverlayLook::default();
    if let Some(da) = d.get(b"DA").and_then(|o| doc.resolve(o).as_string().map(|s| String::from_utf8_lossy(&s.bytes).into_owned())) {
        let t: Vec<&str> = da.split_whitespace().collect();
        for (i, w) in t.iter().enumerate() {
            match *w {
                "rg" if i >= 3 => {
                    if let (Ok(r), Ok(g), Ok(b)) = (t[i - 3].parse::<f64>(), t[i - 2].parse::<f64>(), t[i - 1].parse::<f64>()) {
                        look.color = [r, g, b];
                    }
                }
                "g" if i >= 1 => {
                    if let Ok(g) = t[i - 1].parse::<f64>() {
                        look.color = [g; 3];
                    }
                }
                "Tf" if i >= 2 => {
                    look.font = pdfcraft_annot::OverlayFont::from_resource(t[i - 2].trim_start_matches('/'));
                    look.size = t[i - 1].parse::<f64>().unwrap_or(0.0).max(0.0);
                }
                _ => {}
            }
        }
    }
    look.align = d.int(b"Q").unwrap_or(1).clamp(0, 2) as u8;
    look.repeat = matches!(d.get(b"Repeat"), Some(Object::Bool(true)));
    look
}

/// What [`apply`] removed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub marks: usize,
    pub pages: usize,
    pub glyphs: usize,
    pub images_removed: usize,
    pub images_cleared: usize,
    pub paths_removed: usize,
    pub paths_clipped: usize,
    pub forms_rewritten: usize,
    pub forms_removed: usize,
    pub annotations: usize,
    pub fields: usize,
    /// Structure elements that lost alternate/actual text or emptied marked content, and (Embed
    /// build) marked-content spans that lost theirs.
    pub tags: usize,
    /// Embed build (R6b-50): redacted pages whose thumbnail (`/Thumb`, a picture of the page as
    /// it was) was removed.
    pub thumbnails: usize,
    /// Embed build (R6b-50): resource entries the redacted content no longer uses, removed (a
    /// font used only by removed text, a pattern or shading only painted under a mark, …).
    pub resources_removed: usize,
    /// Embed build (R6b-50): document-level copies of the removed words. Bookmark titles
    /// holding one, now "[Redacted]"; document information entries holding one, removed; the
    /// document's XMP packet removed (0 or 1); named destinations whose name holds one,
    /// renamed (with what goes to them).
    pub bookmarks_cleared: usize,
    pub information_cleared: usize,
    pub metadata_removed: usize,
    pub destinations_renamed: usize,
}

/// Embed build (P6-33, R6b-50): how marks are applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Every marked area filled solid black, whatever the mark asks (its fill colour, overlay
    /// text, repetition and overlay form are ignored, R6b-11): what a host applying marks for a
    /// save requires.
    pub black: bool,
    /// Clear the copies of the removed words elsewhere in the document (see
    /// [`Report::bookmarks_cleared`]): bookmark titles, document information, the XMP packet,
    /// named destinations.
    pub document_copies: bool,
}

/// Embed build (R6b-50): what ends a run of removed glyphs in the removed text.
pub(crate) const RUN_END: char = '\u{1}';

fn rect_of(doc: &Document, o: Option<&Object>) -> Option<[f64; 4]> {
    let v: Vec<f64> = doc.resolve(o?).as_array()?.iter().filter_map(|x| doc.resolve(x).as_f64()).collect();
    (v.len() == 4 && v.iter().all(|x| x.is_finite())).then(|| [v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
}

fn color(doc: &Document, d: &Dict, key: &[u8]) -> Option<Rgb> {
    let v: Vec<f64> = doc.resolve(d.get(key)?).as_array()?.iter().filter_map(Object::as_f64).collect();
    match v.len() {
        1 => Some([v[0]; 3]),
        3 => Some([v[0], v[1], v[2]]),
        4 => Some([(1.0 - v[0]) * (1.0 - v[3]), (1.0 - v[1]) * (1.0 - v[3]), (1.0 - v[2]) * (1.0 - v[3])]),
        _ => None,
    }
}

pub(crate) fn annots_of(doc: &Document, page: &Dict) -> Vec<Object> {
    page.get(b"Annots").map(|a| doc.resolve(a)).and_then(|a| a.as_array().cloned()).unwrap_or_default()
}

/// Every redaction mark in the document, page by page.
pub fn marks(doc: &Document) -> Vec<Mark> {
    let mut out = Vec::new();
    for (pi, p) in pdfcraft_model::pages(doc).iter().enumerate() {
        for a in annots_of(doc, &p.dict) {
            let Some(r) = a.as_ref() else { continue };
            let obj = doc.get(r);
            let Some(d) = obj.as_dict() else { continue };
            if d.name(b"Subtype") != Some(b"Redact") {
                continue;
            }
            let quads: Vec<f64> = d
                .get(b"QuadPoints")
                .and_then(|q| doc.resolve(q).as_array().map(|a| a.iter().filter_map(Object::as_f64).collect()))
                .unwrap_or_default();
            let mut rects: Vec<[f64; 4]> = quads
                .as_chunks::<8>()
                .0
                .iter()
                .map(|q| {
                    let xs = [q[0], q[2], q[4], q[6]];
                    let ys = [q[1], q[3], q[5], q[7]];
                    [
                        xs.iter().copied().fold(f64::MAX, f64::min),
                        ys.iter().copied().fold(f64::MAX, f64::min),
                        xs.iter().copied().fold(f64::MIN, f64::max),
                        ys.iter().copied().fold(f64::MIN, f64::max),
                    ]
                })
                .filter(|r| r.iter().all(|v| v.is_finite()))
                .collect();
            if rects.is_empty()
                && let Some(r) = rect_of(doc, d.get(b"Rect"))
            {
                rects.push(r);
            }
            if rects.is_empty() {
                continue;
            }
            let overlay = d.get(b"OverlayText").and_then(|t| doc.resolve(t).as_string().map(|s| s.to_text())).unwrap_or_default();
            out.push(Mark { page: pi, obj: r, rects, fill: color(doc, d, b"IC"), overlay, look: overlay_look(doc, d) });
        }
    }
    out
}

/// A page's content streams: (the references or inline objects as listed, their decoded data).
pub(crate) fn page_streams(doc: &Document, page: &Dict, index: usize) -> Result<(Vec<Object>, Vec<Vec<u8>>), RedactError> {
    let list: Vec<Object> = match page.get(b"Contents") {
        None => Vec::new(),
        Some(c) => match &*doc.resolve(c) {
            Object::Array(a) => a.clone(),
            _ => vec![c.clone()],
        },
    };
    let mut data = Vec::new();
    for o in &list {
        match &*doc.resolve(o) {
            Object::Stream(s) => data.push(s.decoded().map_err(|_| RedactError::Unreadable(index + 1))?),
            Object::Null => data.push(Vec::new()),
            _ => return Err(RedactError::Unreadable(index + 1)),
        }
    }
    Ok((list, data))
}

/// The boxes and overlay text drawn for the applied marks.
fn overlay_content(marks: &[&Mark]) -> Vec<u8> {
    let n = |v: f64| {
        let s = format!("{:.3}", v);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    let mut c: Vec<u8> = Vec::new();
    for m in marks {
        let Some(fill) = m.fill else { continue };
        // Embed build: written as the content serializer writes operators, so the server's
        // self-check finds it canonical.
        let mut box_ops =
            vec![pdfcraft_content::Op::new("q", vec![]), pdfcraft_content::Op::new("rg", fill.iter().map(|v| pdfcraft_content::num(*v)).collect())];
        for r in &m.rects {
            box_ops.push(pdfcraft_content::Op::new("re", [r[0], r[1], r[2] - r[0], r[3] - r[1]].into_iter().map(pdfcraft_content::num).collect()));
            box_ops.push(pdfcraft_content::Op::new("f", vec![]));
        }
        box_ops.push(pdfcraft_content::Op::new("Q", vec![]));
        c.extend_from_slice(&pdfcraft_content::serialize_ops(&box_ops));
        if m.overlay.is_empty() {
            continue;
        }
        // The overlay text: its font, colour and alignment; auto-sized to fit unless a size is
        // set; repeated to fill the area when asked.
        let look = &m.look;
        let (res, width): (&str, fn(&str, f64) -> f64) = match look.font {
            pdfcraft_annot::OverlayFont::Helvetica => ("PCHelv", pdfcraft_fonts::helvetica_width),
            // Approximations of the standard metrics (no font program is bundled).
            pdfcraft_annot::OverlayFont::Times => ("PCTimes", |s, size| pdfcraft_fonts::helvetica_width(s, size) * 0.9),
            pdfcraft_annot::OverlayFont::Courier => ("PCCour", |s, size| s.chars().count() as f64 * size * 0.6),
        };
        let [cr, cg, cb] = look.color.map(|v| v.clamp(0.0, 1.0));
        for r in &m.rects {
            let (w, h) = (r[2] - r[0], r[3] - r[1]);
            let mut size = if look.size > 0.0 { look.size } else { (h * 0.7).min(12.0) };
            let tw = width(&m.overlay, size);
            if look.size <= 0.0 && tw > w - 2.0 && tw > 0.0 {
                size *= (w - 2.0).max(0.0) / tw;
            }
            if size < 2.0 {
                continue;
            }
            // One line, or as many repeated lines as fit (each line the text repeated across).
            let lines: Vec<String> = if look.repeat {
                let unit = width(&format!("{} ", m.overlay), size).max(0.01);
                let per_line = ((w - 2.0) / unit).floor().max(1.0) as usize;
                let count = ((h / (size * 1.2)).floor() as usize).max(1);
                vec![vec![m.overlay.as_str(); per_line].join(" "); count]
            } else {
                vec![m.overlay.clone()]
            };
            let block = lines.len() as f64 * size * 1.2;
            let mut y = r[1] + (h + block) / 2.0 - size * 0.95;
            c.extend(
                format!("q {} {} {} {} re W n BT {} {} {} rg /{res} {} Tf ", n(r[0]), n(r[1]), n(w), n(h), n(cr), n(cg), n(cb), n(size)).bytes(),
            );
            for line in &lines {
                let lw = width(line, size);
                let x = match look.align {
                    0 => r[0] + 1.0,
                    2 => r[2] - 1.0 - lw,
                    _ => r[0] + (w - lw) / 2.0,
                };
                c.extend(format!("1 0 0 1 {} {} Tm ", n(x), n(y)).bytes());
                c.extend_from_slice(&pdfcraft_fonts::literal(&pdfcraft_fonts::win_ansi(line)));
                c.extend_from_slice(b" Tj ");
                y -= size * 1.2;
            }
            c.extend_from_slice(b"ET Q\n");
        }
    }
    c
}

/// Apply every redaction mark (or only those on `pages`, 0-based). Irreversible for the saved
/// file; callers keep the previous document for undo.
pub fn apply(doc: &mut Document, pages: Option<&[usize]>) -> Result<Report, RedactError> {
    apply_with(doc, pages, Options::default())
}

/// Embed build: [`apply`], with `options`.
pub fn apply_with(doc: &mut Document, pages: Option<&[usize]>, options: Options) -> Result<Report, RedactError> {
    let mut all_marks = marks(doc);
    if options.black {
        for mark in &mut all_marks {
            mark.fill = Some([0.0, 0.0, 0.0]);
            mark.overlay.clear();
        }
    }
    let chosen: Vec<&Mark> = all_marks.iter().filter(|m| pages.is_none_or(|p| p.contains(&m.page))).collect();
    if chosen.is_empty() {
        return Err(RedactError::NothingToApply);
    }
    let mut report = Report { marks: chosen.len(), ..Report::default() };
    let mut by_page: Vec<usize> = chosen.iter().map(|m| m.page).collect();
    by_page.sort_unstable();
    by_page.dedup();
    report.pages = by_page.len();
    let mut doomed_fields: Vec<String> = Vec::new();
    let widget_owner: Vec<(ObjRef, String)> =
        pdfcraft_forms::fields(doc).into_iter().flat_map(|f| f.widgets.iter().map(|w| (w.obj, f.name.clone())).collect::<Vec<_>>()).collect();
    // Embed build (R6b-50): the removed glyphs' text, and every annotation removed.
    let mut removed_text = String::new();
    let mut all_removed: Vec<ObjRef> = Vec::new();

    for &pi in &by_page {
        let page = pdfcraft_model::pages(doc).swap_remove(pi);
        let page_marks: Vec<&Mark> = chosen.iter().copied().filter(|m| m.page == pi).collect();
        let rects: Vec<[f64; 4]> = page_marks.iter().flat_map(|m| m.rects.iter().copied()).collect();

        // 1. Content.
        let (list, data) = page_streams(doc, &page.dict, pi)?;
        let resources = page.dict.get(b"Resources").and_then(|r| doc.resolve(r).as_dict().cloned()).unwrap_or_default();
        let out = {
            let mut scope = Scope::new(&rects, Mode::Apply, &mut report);
            let out = process(doc, &mut scope, &data, &resources, pdfcraft_content::Matrix::IDENTITY);
            removed_text.push_str(&scope.removed_text);
            out
        };
        // Embed build (R6b-50): a pattern cell or soft mask the redactor could not redact.
        if out.residue > 0 {
            return Err(RedactError::Residue(out.residue));
        }
        // Embed build: named property lists whose spans' content changed lose the text standing
        // for it, where they are (R6b-47).
        let resources = match crate::spans::strip_named(doc, &resources, &out.properties)? {
            Some(rewritten) => rewritten,
            None => resources,
        };
        let mut new_list = list.clone();
        let mut changed = false;
        for (i, new) in out.streams.into_iter().enumerate() {
            let Some(bytes) = new else { continue };
            let mut dict = match &*doc.resolve(&list[i]) {
                Object::Stream(s) => s.dict.clone(),
                _ => Dict::new(),
            };
            dict.remove(b"Length");
            // Edit ▸ Add content keeps an added item's source text in `/PCAdded`; once its glyphs
            // are redacted, keeping it would leave the redacted text in the file. The item stays
            // as plain drawn content but is no longer editable.
            dict.remove(b"PCAdded");
            // A new object: the original may be shared with other pages.
            new_list[i] = Object::Ref(doc.add(Object::Stream(Stream::flate(dict, &bytes))));
            changed = true;
        }
        if changed || !out.xobjects.is_empty() || !out.patterns.is_empty() || !out.states.is_empty() {
            let mut res = resources;
            // Embed build (R6b-50): redacted pattern cells and soft masks under their names.
            interp::set_entries(
                doc,
                &mut res,
                &interp::Output { patterns: out.patterns.clone(), states: out.states.clone(), ..interp::Output::default() },
            );
            if !out.xobjects.is_empty() || !out.dropped.is_empty() {
                let mut xo = res.get(b"XObject").and_then(|x| doc.resolve(x).as_dict().cloned()).unwrap_or_default();
                for (n, r) in &out.xobjects {
                    xo.set(n.clone(), Object::Ref(*r));
                }
                // Embed build: a name the redacted content no longer draws goes, or the
                // unredacted form or image it names would stay in the file.
                let drawn = {
                    let ops: Vec<pdfcraft_content::Op> =
                        out.drawn.iter().map(|name| pdfcraft_content::Op::new("Do", vec![Object::Name(name.clone())])).collect();
                    interp::names_drawn(doc, &ops.iter().collect::<Vec<_>>(), &xo)
                };
                interp::forget_dropped(&mut xo, &out.dropped, &drawn);
                res.set(b"XObject".to_vec(), Object::Dict(xo));
            }
            // Embed build (R6b-50): what the redacted content no longer uses goes (a font used
            // only by the removed text: its subset and map would spell it out).
            let new_streams: Vec<Vec<u8>> = list
                .iter()
                .zip(&new_list)
                .zip(&data)
                .map(|((old, new), data)| {
                    if old == new {
                        data.clone()
                    } else {
                        match &*doc.resolve(new) {
                            Object::Stream(stream) => stream.decoded().unwrap_or_default(),
                            _ => Vec::new(),
                        }
                    }
                })
                .collect();
            report.resources_removed += interp::forget_unused(doc, &mut res, &data.join(&b'\n'), &new_streams.join(&b'\n'));
            doc.update_dict(page.obj, |d| {
                d.set(b"Contents".to_vec(), Object::Array(new_list));
                d.set(b"Resources".to_vec(), Object::Dict(res));
            })?;
            // Tags must not keep what the page no longer shows.
            let after = doc.get(page.obj).as_dict().cloned().unwrap_or_default();
            let (_, new_data) = page_streams(doc, &after, pi)?;
            // Embed build: with the MCIDs of the property lists named in the resources (R6b-47).
            let named = tags::named_mcids(doc, &after.get(b"Resources").and_then(|r| doc.resolve(r).as_dict().cloned()).unwrap_or_default());
            let (changed_ids, empty_ids) = tags::touched(&data.join(&b'\n'), &new_data.join(&b'\n'), &named);
            report.tags += tags::clean(doc, page.obj, &changed_ids, &empty_ids)?;
            // Embed build: and its references into the forms the redaction rewrote (R6b-47).
            report.tags += tags::clean_forms(doc, page.obj, &out.forms)?;
        }

        // Embed build (R6b-50): the page's thumbnail shows it as it was; its private data and
        // XMP may hold what was removed.
        let had_thumbnail = doc.get(page.obj).as_dict().is_some_and(|d| d.contains(b"Thumb"));
        doc.update_dict(page.obj, |d| {
            for key in [&b"Thumb"[..], b"PieceInfo", b"LastModified", b"Metadata"] {
                d.remove(key);
            }
        })?;
        report.thumbnails += usize::from(had_thumbnail);

        // 2. Annotations: the marks, and whatever lies under them (with their pop-ups).
        let annots = annots_of(doc, &doc.get(page.obj).as_dict().cloned().unwrap_or_default());
        let mut removed: Vec<ObjRef> = Vec::new();
        for a in &annots {
            let Some(r) = a.as_ref() else { continue };
            let obj = doc.get(r);
            let Some(d) = obj.as_dict() else { continue };
            let subtype = d.name(b"Subtype").unwrap_or(b"");
            if subtype == b"Redact" {
                if page_marks.iter().any(|m| m.obj == r) {
                    removed.push(r);
                }
                continue;
            }
            if subtype == b"Popup" {
                continue;
            }
            if rect_of(doc, d.get(b"Rect")).is_some_and(|b| rects.iter().any(|x| pdfcraft_content::overlaps(*x, b, 0.0))) {
                removed.push(r);
                if subtype == b"Widget" {
                    if let Some((_, name)) = widget_owner.iter().find(|(w, _)| *w == r)
                        && !doomed_fields.contains(name)
                    {
                        doomed_fields.push(name.clone());
                    }
                } else {
                    report.annotations += 1;
                }
            }
        }
        let kept: Vec<Object> = annots
            .into_iter()
            .filter(|a| {
                let Some(r) = a.as_ref() else { return true };
                if removed.contains(&r) {
                    return false;
                }
                // Pop-ups of removed annotations go too.
                let obj = doc.get(r);
                !obj.as_dict().and_then(|d| d.get(b"Parent")).and_then(Object::as_ref).is_some_and(|p| removed.contains(&p))
            })
            .collect();
        doc.update_dict(page.obj, |d| {
            if kept.is_empty() {
                d.remove(b"Annots");
            } else {
                d.set(b"Annots".to_vec(), Object::Array(kept));
            }
        })?;
        // Embed build: the tags naming a removed annotation lose its text, and the reference
        // (follow-up 7 audit, item 10).
        report.tags += tags::clean_objects(doc, &removed)?;
        all_removed.extend(removed);
    }

    // Embed build (R6b-50): replies to a removed annotation (`/IRT`, any page), replies to
    // them, and their pop-ups go too: each would keep it reachable.
    let replies = remove_threads(doc, &mut all_removed)?;
    report.annotations += replies.len();
    report.tags += tags::clean_objects(doc, &replies)?;

    // 3. Form fields with a widget under a mark (all their widgets go).
    for name in &doomed_fields {
        if pdfcraft_forms::fields(doc).iter().any(|f| &f.name == name) {
            pdfcraft_forms::delete_field(doc, name)?;
            report.fields += 1;
        }
    }

    // 4. The boxes.
    for &pi in &by_page {
        let page_marks: Vec<&Mark> = chosen.iter().copied().filter(|m| m.page == pi).collect();
        let content = overlay_content(&page_marks);
        if !content.is_empty() {
            pdfcraft_edit::stamp(doc, pi, "Redaction", content)?;
        }
    }

    // 5. Verify: nothing readable may remain under a region.
    let mut residue = 0;
    for &pi in &by_page {
        let page = pdfcraft_model::pages(doc).swap_remove(pi);
        let rects: Vec<[f64; 4]> = chosen.iter().filter(|m| m.page == pi).flat_map(|m| m.rects.iter().copied()).collect();
        let (list, data) = page_streams(doc, &page.dict, pi)?;
        // The overlay stream (last) draws text of its own: the overlay label is allowed.
        let original: Vec<Vec<u8>> = list
            .iter()
            .zip(data)
            .filter(|(o, _)| doc.resolve(o).as_dict().and_then(|d| d.name(b"PCMark")) != Some(b"Redaction"))
            .map(|(_, d)| d)
            .collect();
        let resources = page.dict.get(b"Resources").and_then(|r| doc.resolve(r).as_dict().cloned()).unwrap_or_default();
        let mut scratch = Report::default();
        let mut scope = Scope::new(&rects, Mode::Verify, &mut scratch);
        residue += process(doc, &mut scope, &original, &resources, pdfcraft_content::Matrix::IDENTITY).residue;
    }
    if residue > 0 {
        return Err(RedactError::Residue(residue));
    }
    // Embed build (R6b-50): copies of the removed words elsewhere in the document.
    if options.document_copies {
        clear_copies(doc, &removed_words(&removed_text), &mut report)?;
    }
    // The previous revision still holds the removed content: the next save must rewrite.
    doc.require_full_save();
    Ok(report)
}

/// Remove redaction marks without applying them (`None` = all).
pub fn clear_marks(doc: &mut Document, pages: Option<&[usize]>) -> Result<usize, RedactError> {
    let all = marks(doc);
    let doomed: Vec<ObjRef> = all.iter().filter(|m| pages.is_none_or(|p| p.contains(&m.page))).map(|m| m.obj).collect();
    if doomed.is_empty() {
        return Err(RedactError::NothingToApply);
    }
    for p in pdfcraft_model::pages(doc) {
        let annots = annots_of(doc, &p.dict);
        let kept: Vec<Object> = annots
            .iter()
            .filter(|a| {
                let Some(r) = a.as_ref() else { return true };
                let parent = doc.get(r).as_dict().and_then(|d| d.get(b"Parent")).and_then(Object::as_ref);
                !doomed.contains(&r) && !parent.is_some_and(|p| doomed.contains(&p))
            })
            .cloned()
            .collect();
        if kept.len() != annots.len() {
            doc.update_dict(p.obj, |d| d.set(b"Annots".to_vec(), Object::Array(kept)))?;
        }
    }
    Ok(doomed.len())
}

/// Embed build (R6b-50): remove every annotation whose `/IRT` chain reaches one in `removed`
/// (on any page), and the pop-ups of those removed; `removed` grows with them. Returns those
/// removed here.
fn remove_threads(doc: &mut Document, removed: &mut Vec<ObjRef>) -> Result<Vec<ObjRef>, RedactError> {
    let mut gone: Vec<ObjRef> = Vec::new();
    let pages = pdfcraft_model::pages(doc);
    // Until nothing more goes (each pass removes at least one, or stops).
    loop {
        let mut more = false;
        for page in &pages {
            let current = doc.get(page.obj).as_dict().cloned().unwrap_or_default();
            let annots = annots_of(doc, &current);
            let doomed: Vec<ObjRef> = annots
                .iter()
                .filter_map(Object::as_ref)
                .filter(|r| !removed.contains(r))
                .filter(|r| {
                    let obj = doc.get(*r);
                    let dict = obj.as_dict();
                    let reply = dict.and_then(|d| d.get(b"IRT")).and_then(Object::as_ref).is_some_and(|to| removed.contains(&to));
                    let popup = dict.is_some_and(|d| d.name(b"Subtype") == Some(b"Popup"))
                        && dict.and_then(|d| d.get(b"Parent")).and_then(Object::as_ref).is_some_and(|to| removed.contains(&to));
                    reply || popup
                })
                .collect();
            if doomed.is_empty() {
                continue;
            }
            more = true;
            let kept: Vec<Object> = annots.into_iter().filter(|a| !a.as_ref().is_some_and(|r| doomed.contains(&r))).collect();
            doc.update_dict(page.obj, |d| {
                if kept.is_empty() {
                    d.remove(b"Annots");
                } else {
                    d.set(b"Annots".to_vec(), Object::Array(kept));
                }
            })?;
            for r in doomed {
                // A pop-up is part of its markup, not counted on its own.
                if doc.get(r).as_dict().is_some_and(|d| d.name(b"Subtype") != Some(b"Popup")) {
                    gone.push(r);
                }
                removed.push(r);
            }
        }
        if !more {
            return Ok(gone);
        }
    }
}

/// Embed build (R6b-50): what a copy of the removed text elsewhere could hold: each run of
/// removed glyphs whole (three characters or more), and each word of it of five characters or
/// more (shorter words are in too many unrelated titles: "Page" when "Page 4" is redacted),
/// compared without case.
pub(crate) fn removed_words(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut add = |word: &str, least: usize| {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if word.chars().filter(|c| c.is_alphanumeric()).count() >= least && !words.contains(&word) {
            words.push(word);
        }
    };
    for run in text.split(RUN_END) {
        add(run, 3);
        for word in run.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '\'') {
            add(word, 5);
        }
    }
    words
}

fn holds_word(text: &str, words: &[String]) -> bool {
    let text = text.to_lowercase();
    words.iter().any(|word| text.contains(word.as_str()))
}

/// Embed build (R6b-50): clear the copies of the removed `words` elsewhere in the document:
/// bookmark titles holding one read "[Redacted]", document information entries holding one go,
/// the document's XMP packet goes, and named destinations whose name holds one are renamed
/// (with every destination naming them).
fn clear_copies(doc: &mut Document, words: &[String], report: &mut Report) -> Result<(), RedactError> {
    let Some(root) = doc.root() else { return Ok(()) };
    let mut catalog = doc.get(root).as_dict().cloned().unwrap_or_default();
    if catalog.remove(b"Metadata").is_some() {
        report.metadata_removed = 1;
        doc.set(root, Object::Dict(catalog.clone()));
    }
    if words.is_empty() {
        return Ok(());
    }
    // Bookmarks: depth first, bounded.
    let mut todo: Vec<Object> =
        catalog.get(b"Outlines").and_then(|o| doc.resolve(o).as_dict().and_then(|d| d.get(b"First").cloned())).into_iter().collect();
    let mut seen: Vec<ObjRef> = Vec::new();
    while let Some(entry) = todo.pop() {
        let Some(r) = entry.as_ref() else { continue };
        if seen.contains(&r) || seen.len() > 100_000 {
            continue;
        }
        seen.push(r);
        let Some(item) = doc.get(r).as_dict().cloned() else { continue };
        if item.get(b"Title").and_then(|t| doc.resolve(t).as_string().map(|s| s.to_text())).is_some_and(|title| holds_word(&title, words)) {
            doc.update_dict(r, |d| d.set(b"Title".to_vec(), Object::String(pdfcraft_cos::PdfString::text("[Redacted]"))))?;
            report.bookmarks_cleared += 1;
        }
        todo.extend(item.get(b"Next").cloned());
        todo.extend(item.get(b"First").cloned());
    }
    // The document information.
    if let Some(info) = doc.trailer().get(b"Info").and_then(Object::as_ref)
        && let Some(dict) = doc.get(info).as_dict().cloned()
    {
        let doomed: Vec<Vec<u8>> = dict
            .iter()
            .filter(|(_, value)| doc.resolve(value).as_string().is_some_and(|s| holds_word(&s.to_text(), words)))
            .map(|(key, _)| key.clone())
            .collect();
        if !doomed.is_empty() {
            doc.update_dict(info, |d| {
                for key in &doomed {
                    d.remove(key);
                }
            })?;
            report.information_cleared += doomed.len();
        }
    }
    // Named destinations in the catalog's /Dests (by name) and the /Names /Dests tree (by
    // string): renamed to opaque names, and what names them follows.
    let mut renames: Vec<(Object, Object)> = Vec::new();
    if let Some(dests_ref) = catalog.get(b"Dests").cloned()
        && let Some(mut dests) = doc.resolve(&dests_ref).as_dict().cloned()
    {
        let names: Vec<Vec<u8>> = dests.iter().map(|(key, _)| key.clone()).filter(|key| holds_word(&String::from_utf8_lossy(key), words)).collect();
        for (index, name) in names.into_iter().enumerate() {
            let fresh = format!("PCRedactedDest{}", report.destinations_renamed + index + 1).into_bytes();
            if let Some(value) = dests.remove(&name) {
                dests.set(fresh.clone(), value);
                renames.push((Object::Name(name), Object::Name(fresh)));
            }
        }
        report.destinations_renamed += renames.len();
        match dests_ref {
            Object::Ref(r) => doc.set(r, Object::Dict(dests)),
            _ => {
                catalog.set(b"Dests".to_vec(), Object::Dict(dests));
                doc.set(root, Object::Dict(catalog.clone()));
            }
        }
    }
    if let Some(tree) = catalog.get(b"Names").and_then(|n| doc.resolve(n).as_dict().cloned()).and_then(|n| n.get(b"Dests").and_then(Object::as_ref)) {
        rename_in_tree(doc, tree, words, &mut renames, report, 0)?;
    }
    if renames.is_empty() {
        return Ok(());
    }
    // Every destination naming a renamed one: links' and bookmarks' /Dest, go-to actions' /D.
    let numbers: Vec<u32> = doc.object_numbers();
    for num in numbers {
        let r = ObjRef::new(num, 0);
        let Some(dict) = doc.get(r).as_dict().cloned() else { continue };
        let mut changed = dict.clone();
        let mut touched = false;
        for key in [&b"Dest"[..], b"D"] {
            if let Some(value) = dict.get(key)
                && let Some((_, fresh)) = renames.iter().find(|(old, _)| old == value)
            {
                changed.set(key.to_vec(), fresh.clone());
                touched = true;
            }
        }
        if let Some(Object::Dict(action)) = dict.get(b"A")
            && let Some(value) = action.get(b"D")
            && let Some((_, fresh)) = renames.iter().find(|(old, _)| old == value)
        {
            let mut action = action.clone();
            action.set(b"D".to_vec(), fresh.clone());
            changed.set(b"A".to_vec(), Object::Dict(action));
            touched = true;
        }
        if touched {
            doc.set(r, Object::Dict(changed));
        }
    }
    Ok(())
}

/// The `/Names /Dests` tree's leaves under `node`: a name holding a removed word renamed.
fn rename_in_tree(
    doc: &mut Document,
    node: ObjRef,
    words: &[String],
    renames: &mut Vec<(Object, Object)>,
    report: &mut Report,
    depth: usize,
) -> Result<(), RedactError> {
    if depth > 32 {
        return Ok(());
    }
    let Some(mut dict) = doc.get(node).as_dict().cloned() else { return Ok(()) };
    if let Some(Object::Array(mut pairs)) = dict.get(b"Names").map(|n| (*doc.resolve(n)).clone()) {
        let mut touched = false;
        for pair in pairs.chunks_mut(2) {
            if let Some(Object::String(name)) = pair.first()
                && holds_word(&name.to_text(), words)
            {
                report.destinations_renamed += 1;
                let fresh = Object::String(pdfcraft_cos::PdfString::literal(format!("PCRedactedDest{}", report.destinations_renamed).into_bytes()));
                renames.push((pair[0].clone(), fresh.clone()));
                pair[0] = fresh;
                touched = true;
            }
        }
        if touched {
            // Keys must stay in order for a viewer's lookup: sorted again, with /Limits.
            let mut sorted: Vec<(Object, Object)> = pairs.chunks(2).filter_map(|p| Some((p.first()?.clone(), p.get(1)?.clone()))).collect();
            sorted.sort_by(|a, b| a.0.as_string().map(|s| s.bytes.clone()).cmp(&b.0.as_string().map(|s| s.bytes.clone())));
            if let (Some(first), Some(last)) = (sorted.first().map(|p| p.0.clone()), sorted.last().map(|p| p.0.clone()))
                && dict.contains(b"Limits")
            {
                dict.set(b"Limits".to_vec(), Object::Array(vec![first, last]));
            }
            dict.set(b"Names".to_vec(), Object::Array(sorted.into_iter().flat_map(|(k, v)| [k, v]).collect()));
            doc.set(node, Object::Dict(dict.clone()));
        }
    }
    let kids: Vec<ObjRef> =
        dict.get(b"Kids").and_then(|k| doc.resolve(k).as_array().map(|a| a.iter().filter_map(Object::as_ref).collect())).unwrap_or_default();
    for kid in kids {
        rename_in_tree(doc, kid, words, renames, report, depth + 1)?;
    }
    Ok(())
}
