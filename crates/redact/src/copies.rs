//! Embed build (R6b-50, R6b-52, R6b-54 N3, N4, N5, N7, N8, R6b-56): copies of the removed words
//! elsewhere in the document, cleared when a host applies marks.
//!
//! **Taken whole, without matching (R6b-56).** Whenever any redaction is applied, every
//! document information value but `/Producer` and `/CreationDate` goes, every XMP packet
//! (`/Metadata`, wherever it is) goes, and the form's XFA (`/XFA`, `/NeedsRendering`, which keeps
//! the fields' values apart from them) goes.
//!
//! **Matched (R6b-52, R6b-56).** Bookmark titles, annotations' `/Contents` (and its rich-text
//! form `/RC`) and `/T` outside the marks, optional-content groups' names, page-label prefixes
//! and destination names. One normalisation ([`normalise`]) reads the removed text and every
//! candidate alike: decoded by the string's own encoding ([`decode_text`]), then NFKC, then
//! format characters taken out, then case and diacritics folded. A copy holds a removed word
//! when it holds one of the removed tokens (three letters or digits or more, a short function
//! word excepted), as a word of its own, a part of one where letters meet digits, or inside a
//! word (as follow-up 8 found removed runs anywhere), five characters or more across words; or
//! a removed phrase (two words or more) whole. A candidate that cannot be decoded refuses the redaction
//! ([`Refusal::CopyNotClearable`]); so do removed glyphs without readable text where any of these
//! places holds text ([`Refusal::UnreadableMarkedText`]), and a bookmark's or the opening
//! action's URI or JavaScript holding a removed word (it cannot be cleared without changing what
//! the action does).
//!
//! **What clearing does.** Bookmark titles read "[Redacted]"; annotations' text entries holding
//! one go; groups' names read "[Redacted]"; prefixes go; named destinations are renamed to opaque
//! names, matched by their bytes (a name or a string, literal or hex alike), with every
//! destination naming them (in links, bookmarks, the opening action, additional actions and
//! chained actions), and the named destinations' tree written again in order.

use std::collections::{HashMap, HashSet};

use pdfcraft_cos::{Dict, Document, ObjRef, Object, PdfString};
use unicode_normalization::UnicodeNormalization;

use crate::{RUN_END, RedactError, Refusal, Report, interp};

/// What replaces a cleared title or name.
pub(crate) const REDACTED: &str = "[Redacted]";

/// The document information entries a redaction keeps (R6b-56).
const INFO_KEPT: [&[u8]; 2] = [b"Producer", b"CreationDate"];

/// Function words of three letters (English and Indonesian) never matched on their own: they
/// are in too many unrelated titles. (Shorter words are never matched on their own anyway.)
const STOP_WORDS: [&str; 46] = [
    "the", "and", "for", "but", "nor", "yet", "not", "are", "was", "its", "our", "you", "she", "his", "her", "him", "all", "any", "can", "may",
    "per", "via", "who", "how", "why", "has", "had", "did", "too", "off", "out", "own", "few", "one", "two", "dan", "ini", "itu", "ada", "pun",
    "lah", "kah", "tak", "dgn", "utk", "tsb",
];

/// Function words that are also given names (Dan, Ada, May): a function word only written in
/// lower case; capitalised, a name, matched like any token.
const NAME_LIKE: [&str; 3] = ["dan", "ada", "may"];

/// Characters a viewer does not show, which must not split a word: the soft hyphen, zero-width
/// spaces and joiners, directional marks, the word joiner and its kin, the byte-order mark.
fn invisible(c: char) -> bool {
    matches!(c, '\u{ad}' | '\u{34f}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}')
}

/// R6b-56: a PDF text string decoded by its own encoding (UTF-16BE, UTF-16LE or UTF-8 after their marks,
/// else PDFDocEncoding), its language marks (U+001B, a code, U+001B, which viewers do not show)
/// taken out. `None` when it cannot be: an odd UTF-16 length or an unpaired surrogate, bad
/// UTF-8, or a byte PDFDocEncoding gives no character (0x7F, 0x9F, 0xAD).
pub(crate) fn decode_text(bytes: &[u8]) -> Option<String> {
    let text = if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        if rest.len() % 2 != 0 {
            return None;
        }
        let units = rest.as_chunks::<2>().0.iter().map(|pair| u16::from_be_bytes(*pair));
        char::decode_utf16(units).collect::<Result<String, _>>().ok()?
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        // Audit (follow-up 9): UTF-16 little-endian with its mark, which pdf.js shows as text.
        if rest.len() % 2 != 0 {
            return None;
        }
        let units = rest.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair));
        char::decode_utf16(units).collect::<Result<String, _>>().ok()?
    } else if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8(rest.to_vec()).ok()?
    } else {
        if bytes.iter().any(|byte| matches!(byte, 0x7F | 0x9F | 0xAD)) {
            return None;
        }
        PdfString::literal(bytes.to_vec()).to_text()
    };
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for c in text.chars() {
        if c == '\u{1b}' {
            inside = !inside;
        } else if !inside {
            out.push(c);
        }
    }
    if inside {
        return None;
    }
    Some(out)
}

/// A name's bytes as text (destination names): UTF-8 when they are, else as a text string.
fn decode_name(bytes: &[u8]) -> Option<String> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text.to_string()),
        Err(_) => decode_text(bytes),
    }
}

/// Case and diacritics folded (after NFKC): decomposed, marks and what is not shown taken out,
/// lower case, ß as ss.
fn fold_case(text: &str) -> String {
    text.nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c) && !invisible(*c))
        .flat_map(char::to_lowercase)
        .flat_map(|c| if c == 'ß' { vec!['s', 's'] } else { vec![c] })
        .collect()
}

/// NFKC, format characters out: before folding.
fn prepared(text: &str) -> String {
    text.nfkc().filter(|c| !invisible(*c)).collect()
}

/// R6b-56: the one normalisation of removed text and candidates alike (on decoded text): NFKC,
/// then format characters out, then case and diacritics folded.
pub(crate) fn normalise(text: &str) -> String {
    fold_case(&prepared(text))
}

/// The words of normalised text: its runs of letters and digits.
fn words(folded: &str) -> Vec<&str> {
    folded.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).collect()
}

/// A word's pieces where letters meet digits ("room101": "room", "101").
fn pieces(word: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut last: Option<bool> = None;
    for (at, c) in word.char_indices() {
        let digit = c.is_numeric();
        if last.is_some_and(|was| was != digit) {
            out.push(&word[start..at]);
            start = at;
        }
        last = Some(digit);
    }
    out.push(&word[start..]);
    out
}

/// What the redaction removed, for matching copies.
#[derive(Debug, Default)]
pub(crate) struct Removed {
    /// Normalised tokens of three characters or more, but the stop words.
    tokens: Vec<String>,
    /// Normalised phrases of two words or more, their words joined by one space.
    phrases: Vec<String>,
    /// A removed glyph had no readable text.
    pub unreadable: bool,
}

impl Removed {
    /// From the removed text (runs apart by [`RUN_END`], words by spaces).
    pub fn from_text(text: &str) -> Self {
        let mut removed = Removed { unreadable: text.contains('\u{fffd}'), ..Removed::default() };
        let add = |removed: &mut Removed, token: &str, capitalised: bool| {
            let stop = STOP_WORDS.contains(&token) && !(capitalised && NAME_LIKE.contains(&token));
            if token.chars().count() >= 3 && !stop && !removed.tokens.iter().any(|t| t == token) {
                removed.tokens.push(token.to_string());
            }
        };
        for run in text.split(RUN_END) {
            let run = prepared(run);
            let mut run_pieces: Vec<String> = Vec::new();
            for raw in run.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()) {
                let capitalised = raw.chars().next().is_some_and(char::is_uppercase);
                let folded = fold_case(raw);
                for word in words(&folded) {
                    add(&mut removed, word, capitalised);
                    let split = pieces(word);
                    if split.len() > 1 {
                        for piece in &split {
                            add(&mut removed, piece, capitalised);
                        }
                    }
                    run_pieces.extend(split.into_iter().map(str::to_string));
                }
            }
            if run_pieces.len() >= 2 {
                let phrase = run_pieces.join(" ");
                if !removed.phrases.contains(&phrase) {
                    removed.phrases.push(phrase);
                }
            }
        }
        removed
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty() && self.phrases.is_empty()
    }

    /// Whether decoded text `copy` holds a removed word (see the module documentation).
    pub fn held_in(&self, copy: &str) -> bool {
        let folded = normalise(copy);
        let copy_words = words(&folded);
        let copy_pieces: Vec<&str> = copy_words.iter().flat_map(|word| pieces(word)).collect();
        let joined = format!(" {} ", copy_pieces.join(" "));
        let compact: String = copy_words.concat();
        self.tokens.iter().any(|token| {
            let length = token.chars().count();
            copy_words.contains(&token.as_str())
                || copy_pieces.contains(&token.as_str())
                // Inside a word ("BudiCorp", "Daniel": follow-up 8 matched every removed run of
                // three characters or more anywhere, and nothing it cleared may be kept now);
                // five or more, across words ("SECRET WORD").
                || copy_words.iter().any(|word| word.contains(token.as_str()))
                || (length >= 5 && compact.contains(token.as_str()))
        }) || self.phrases.iter().any(|phrase| joined.contains(&format!(" {phrase} ")))
    }
}

/// A candidate's text (R6b-56): a string decoded by its own encoding, a name's bytes as text;
/// `None` for anything else. A string or name that cannot be decoded refuses.
fn copy_text(doc: &Document, o: &Object) -> Result<Option<String>, RedactError> {
    let undecodable = RedactError::Refused(Refusal::CopyNotClearable);
    match &*doc.resolve(o) {
        Object::String(s) => decode_text(&s.bytes).map(Some).ok_or(undecodable),
        Object::Name(n) => decode_name(n).map(Some).ok_or(undecodable),
        _ => Ok(None),
    }
}

/// Whether candidate `o` (see [`copy_text`]) holds a removed word.
fn holds(doc: &Document, o: &Object, removed: &Removed) -> Result<bool, RedactError> {
    Ok(copy_text(doc, o)?.is_some_and(|text| removed.held_in(&text)))
}

/// Whether an action (a bookmark's, the opening one), or any action it chains, holds a removed
/// word anywhere (a URI, a file name, JavaScript as a string or a stream).
fn action_holds(doc: &Document, action: &Object, removed: &Removed, work: &interp::Work, depth: usize) -> Result<bool, RedactError> {
    if depth > 32 {
        return Err(RedactError::TooComplex);
    }
    work.charge(4);
    if work.spent() {
        return Err(RedactError::TooComplex);
    }
    // A page or a structure element is reached from an action by reference, not text in it.
    if let Object::Ref(r) = action
        && doc.get(*r).as_dict().is_some_and(|d| matches!(d.name(b"Type"), Some(b"Page" | b"Pages" | b"StructElem")))
    {
        return Ok(false);
    }
    Ok(match &*doc.resolve(action) {
        Object::String(_) | Object::Name(_) => holds(doc, action, removed)?,
        Object::Stream(stream) => {
            let data = stream.decoded().map_err(|_| RedactError::Refused(Refusal::CopyNotClearable))?;
            work.charge(data.len() / 64);
            removed.held_in(&decode_text(&data).ok_or(RedactError::Refused(Refusal::CopyNotClearable))?)
        }
        Object::Array(items) => {
            let mut held = false;
            for item in items {
                held |= action_holds(doc, item, removed, work, depth + 1)?;
            }
            held
        }
        Object::Dict(dict) => {
            let mut held = false;
            for (key, value) in dict.iter() {
                // Where a destination goes is followed apart (renamed with its name).
                if matches!(key.as_slice(), b"D" | b"Dest" | b"Type" | b"S") {
                    continue;
                }
                held |= action_holds(doc, value, removed, work, depth + 1)?;
            }
            held
        }
        _ => false,
    })
}

/// Every annotation listed on a page, by object.
fn page_annotations(doc: &Document, work: &interp::Work) -> Vec<ObjRef> {
    let mut out = Vec::new();
    for page in pdfcraft_model::pages(doc) {
        for annot in crate::annots_of(doc, &page.dict) {
            work.charge(1);
            out.extend(annot.as_ref());
        }
    }
    out
}

/// Clear the copies of what was removed (see the module documentation), counting each in
/// `report`.
pub(crate) fn clear(doc: &mut Document, removed: &Removed, report: &mut Report, work: &interp::Work) -> Result<(), RedactError> {
    let spent = |work: &interp::Work| if work.spent() { Err(RedactError::TooComplex) } else { Ok(()) };
    let Some(root) = doc.root() else { return Ok(()) };
    let mut catalog = doc.get(root).as_dict().cloned().unwrap_or_default();
    // R6b-56: the document information, but its producer and creation date, whole.
    if let Some(info) = doc.trailer().get(b"Info").and_then(Object::as_ref)
        && let Some(dict) = doc.get(info).as_dict().cloned()
    {
        let doomed: Vec<Vec<u8>> = dict.iter().map(|(key, _)| key.clone()).filter(|key| !INFO_KEPT.contains(&key.as_slice())).collect();
        if !doomed.is_empty() {
            doc.update_dict(info, |d| {
                for key in &doomed {
                    d.remove(key);
                }
            })?;
            report.information_cleared += doomed.len();
        }
    }
    // R6b-56: every XMP packet, wherever it is.
    for num in doc.object_numbers() {
        work.charge(1);
        spent(work)?;
        let r = ObjRef::new(num, 0);
        let Ok(object) = doc.try_get(num) else { continue };
        let has = match &*object {
            Object::Dict(d) => d.contains(b"Metadata"),
            Object::Stream(s) => s.dict.contains(b"Metadata"),
            _ => false,
        };
        if has {
            doc.update_dict(r, |d| {
                d.remove(b"Metadata");
            })?;
            report.metadata_removed += 1;
        }
    }
    catalog = doc.get(root).as_dict().cloned().unwrap_or(catalog);
    // The form's XFA.
    let mut catalog_changed = false;
    if catalog.remove(b"NeedsRendering").is_some() {
        catalog_changed = true;
        report.xfa_removed = 1;
    }
    if let Some(form_entry) = catalog.get(b"AcroForm").cloned()
        && let Some(mut form) = doc.resolve(&form_entry).as_dict().cloned()
        && form.remove(b"XFA").is_some()
    {
        report.xfa_removed = 1;
        match form_entry {
            Object::Ref(r) => doc.set(r, Object::Dict(form)),
            _ => {
                catalog.set(b"AcroForm".to_vec(), Object::Dict(form));
                catalog_changed = true;
            }
        }
    }
    if catalog_changed {
        doc.set(root, Object::Dict(catalog.clone()));
    }
    let annotations = page_annotations(doc, work);
    let groups: Vec<ObjRef> = catalog
        .get(b"OCProperties")
        .and_then(|p| doc.resolve(p).as_dict().cloned())
        .and_then(|p| p.get(b"OCGs").and_then(|g| doc.resolve(g).as_array().cloned()))
        .map(|items| items.iter().filter_map(Object::as_ref).collect())
        .unwrap_or_default();
    // Removed glyphs without readable text cannot be looked for where copies are matched.
    if removed.unreadable {
        let bookmarks = catalog.get(b"Outlines").and_then(|o| doc.resolve(o).as_dict().and_then(|d| d.get(b"First").cloned())).is_some();
        let texts =
            annotations.iter().any(|r| doc.get(*r).as_dict().is_some_and(|d| [&b"Contents"[..], b"RC", b"T"].iter().any(|key| d.contains(key))));
        let names = !groups.is_empty() || catalog.contains(b"PageLabels") || catalog.contains(b"Dests") || catalog.contains(b"Names");
        if bookmarks || texts || names {
            return Err(RedactError::Refused(Refusal::UnreadableMarkedText));
        }
    }
    if removed.is_empty() {
        return Ok(());
    }
    // The opening action holding a removed word cannot be cleared.
    if let Some(action) = catalog.get(b"OpenAction").filter(|action| !matches!(&*doc.resolve(action), Object::Array(_)))
        && action_holds(doc, action, removed, work, 0)?
    {
        return Err(RedactError::Refused(Refusal::CopyNotClearable));
    }
    // Bookmarks: depth first, bounded; a walk past its bound, or an entry that is no
    // dictionary, refuses (never skipped).
    let mut todo: Vec<Object> =
        catalog.get(b"Outlines").and_then(|o| doc.resolve(o).as_dict().and_then(|d| d.get(b"First").cloned())).into_iter().collect();
    let mut seen: HashSet<ObjRef> = HashSet::new();
    while let Some(entry) = todo.pop() {
        work.charge(8);
        spent(work)?;
        let Some(r) = entry.as_ref() else { return Err(RedactError::Refused(Refusal::CopyNotClearable)) };
        if !seen.insert(r) {
            continue;
        }
        if seen.len() > 100_000 {
            return Err(RedactError::TooComplex);
        }
        let Some(item) = doc.get(r).as_dict().cloned() else { return Err(RedactError::Refused(Refusal::CopyNotClearable)) };
        if let Some(action) = item.get(b"A")
            && action_holds(doc, action, removed, work, 0)?
        {
            return Err(RedactError::Refused(Refusal::CopyNotClearable));
        }
        if let Some(title) = item.get(b"Title")
            && holds(doc, title, removed)?
        {
            doc.update_dict(r, |d| d.set(b"Title".to_vec(), Object::String(PdfString::text(REDACTED))))?;
            report.bookmarks_cleared += 1;
        }
        todo.extend(item.get(b"Next").cloned());
        todo.extend(item.get(b"First").cloned());
    }
    // Annotations' text, outside the marks (those under them are gone), whatever entries they
    // have otherwise.
    for r in annotations {
        work.charge(4);
        spent(work)?;
        let Some(dict) = doc.get(r).as_dict().cloned() else { continue };
        if dict.name(b"Subtype") == Some(b"Widget") {
            continue;
        }
        let mut doomed: Vec<&[u8]> = Vec::new();
        for key in [&b"Contents"[..], b"RC", b"T"] {
            if let Some(value) = dict.get(key)
                && holds(doc, value, removed)?
            {
                doomed.push(key);
            }
        }
        if !doomed.is_empty() {
            doc.update_dict(r, |d| {
                for key in &doomed {
                    d.remove(key);
                }
            })?;
            report.annotation_texts_cleared += 1;
        }
    }
    // Optional-content groups' names.
    for r in groups {
        work.charge(4);
        let Some(dict) = doc.get(r).as_dict().cloned() else { continue };
        if let Some(name) = dict.get(b"Name")
            && holds(doc, name, removed)?
        {
            doc.update_dict(r, |d| d.set(b"Name".to_vec(), Object::String(PdfString::text(REDACTED))))?;
            report.layer_names_cleared += 1;
        }
    }
    // Page labels' prefixes.
    if let Some(labels) = catalog.get(b"PageLabels").cloned() {
        clear_label_prefixes(doc, &labels, removed, report, work, 0)?;
    }
    rename_destinations(doc, removed, report, work)
}

/// The prefixes holding a removed word in the page-label number tree under `node`, removed.
fn clear_label_prefixes(
    doc: &mut Document,
    node: &Object,
    removed: &Removed,
    report: &mut Report,
    work: &interp::Work,
    depth: usize,
) -> Result<(), RedactError> {
    // Audit (follow-up 9): as deep as the inspector reads a number tree; deeper is refused,
    // never skipped.
    if depth > 64 {
        return Err(RedactError::TooComplex);
    }
    work.charge(4);
    let Some(dict) = doc.resolve(node).as_dict().cloned() else { return Ok(()) };
    if let Some(nums) = dict.get(b"Nums").and_then(|n| doc.resolve(n).as_array().cloned()) {
        let mut nums = nums;
        let mut changed = false;
        for pair in nums.chunks_mut(2) {
            let Some(label) = pair.get_mut(1) else { continue };
            let target = label.as_ref();
            let Some(mut entry) = doc.resolve(label).as_dict().cloned() else { continue };
            if let Some(prefix) = entry.get(b"P")
                && holds(doc, prefix, removed)?
            {
                entry.remove(b"P");
                report.label_prefixes_cleared += 1;
                match target {
                    Some(r) => doc.set(r, Object::Dict(entry)),
                    None => {
                        *label = Object::Dict(entry);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            let mut dict = dict.clone();
            dict.set(b"Nums".to_vec(), Object::Array(nums));
            match node {
                Object::Ref(r) => doc.set(*r, Object::Dict(dict)),
                _ => {
                    // The tree's root written in the catalog: written there again.
                    if let Some(root) = doc.root() {
                        doc.update_dict(root, |c| c.set(b"PageLabels".to_vec(), Object::Dict(dict)))?;
                    }
                }
            }
        }
    }
    if let Some(kids) = dict.get(b"Kids").and_then(|k| doc.resolve(k).as_array().cloned()) {
        for kid in kids {
            clear_label_prefixes(doc, &kid, removed, report, work, depth + 1)?;
        }
    }
    Ok(())
}

/// A destination name's bytes, whether written as a name or a string (literal or hex).
fn name_bytes(o: &Object) -> Option<&[u8]> {
    match o {
        Object::Name(n) => Some(n),
        Object::String(s) => Some(&s.bytes),
        _ => None,
    }
}

/// The named destinations whose name holds a removed word, renamed (by their bytes), with the
/// tree written again in order and every destination naming them following them.
fn rename_destinations(doc: &mut Document, removed: &Removed, report: &mut Report, work: &interp::Work) -> Result<(), RedactError> {
    let Some(root) = doc.root() else { return Ok(()) };
    let mut catalog = doc.get(root).as_dict().cloned().unwrap_or_default();
    // Every name already in use, so a fresh one is no existing name.
    let mut existing: HashSet<Vec<u8>> = HashSet::new();
    let dests_entry = catalog.get(b"Dests").cloned();
    let mut dests = dests_entry.as_ref().and_then(|d| doc.resolve(d).as_dict().cloned());
    if let Some(dests) = &dests {
        existing.extend(dests.iter().map(|(key, _)| key.clone()));
    }
    let names_entry = catalog.get(b"Names").cloned();
    let mut names = names_entry.as_ref().and_then(|n| doc.resolve(n).as_dict().cloned());
    let mut tree: Vec<(Vec<u8>, Object)> = Vec::new();
    if let Some(top) = names.as_ref().and_then(|n| n.get(b"Dests").cloned()) {
        collect_tree(doc, &top, &mut tree, &mut HashSet::new(), work, 0)?;
        existing.extend(tree.iter().map(|(key, _)| key.clone()));
    }
    let mut renames: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut next = 1usize;
    let mut fresh = |existing: &mut HashSet<Vec<u8>>| loop {
        let name = format!("PCRedactedDest{next}").into_bytes();
        next += 1;
        if existing.insert(name.clone()) {
            return name;
        }
    };
    if let Some(dict) = dests.as_mut() {
        let mut doomed: Vec<Vec<u8>> = Vec::new();
        for (key, _) in dict.iter() {
            if removed.held_in(&decode_name(key).ok_or(RedactError::Refused(Refusal::CopyNotClearable))?) {
                doomed.push(key.clone());
            }
        }
        for old in doomed {
            let new = renames.get(&old).cloned().unwrap_or_else(|| fresh(&mut existing));
            if let Some(value) = dict.remove(&old) {
                dict.set(new.clone(), value);
            }
            renames.insert(old, new);
        }
    }
    let mut tree_changed = false;
    for (key, _) in &mut tree {
        if removed.held_in(&decode_text(key).ok_or(RedactError::Refused(Refusal::CopyNotClearable))?) {
            let new = renames.get(key).cloned().unwrap_or_else(|| fresh(&mut existing));
            renames.insert(key.clone(), new.clone());
            *key = new;
            tree_changed = true;
        }
    }
    report.destinations_renamed += renames.len();
    if renames.is_empty() {
        return Ok(());
    }
    if let (Some(dict), Some(entry)) = (dests, dests_entry) {
        match entry {
            Object::Ref(r) => doc.set(r, Object::Dict(dict)),
            _ => {
                catalog.set(b"Dests".to_vec(), Object::Dict(dict));
                doc.set(root, Object::Dict(catalog.clone()));
            }
        }
    }
    if tree_changed && let Some(mut names_dict) = names.take() {
        // Written again as one leaf, in order: no node's limits can be wrong.
        tree.sort_by(|a, b| a.0.cmp(&b.0));
        let mut leaf = Dict::new();
        leaf.set(
            b"Names".to_vec(),
            Object::Array(tree.into_iter().flat_map(|(key, value)| [Object::String(PdfString::literal(key)), value]).collect()),
        );
        let leaf = doc.add(Object::Dict(leaf));
        names_dict.set(b"Dests".to_vec(), Object::Ref(leaf));
        match names_entry {
            Some(Object::Ref(r)) => doc.set(r, Object::Dict(names_dict)),
            _ => {
                catalog.set(b"Names".to_vec(), Object::Dict(names_dict));
                doc.set(root, Object::Dict(catalog));
            }
        }
    }
    // Every destination naming a renamed one, wherever it is written: any /Dest or /D whose
    // value is a name or string with those bytes (links, bookmarks, the opening action, actions
    // chained by /Next, additional actions), the same kind of object as it was.
    for num in doc.object_numbers() {
        work.charge(4);
        if work.spent() {
            return Err(RedactError::TooComplex);
        }
        let r = ObjRef::new(num, 0);
        let Ok(object) = doc.try_get(num) else { continue };
        let mut value = (*object).clone();
        if follow(&mut value, &renames, 0) {
            doc.set(r, value);
        }
    }
    Ok(())
}

/// Every `/Dest` and `/D` in `value` (dictionaries and arrays written in it) naming a renamed
/// destination, renamed. Whether any was.
fn follow(value: &mut Object, renames: &HashMap<Vec<u8>, Vec<u8>>, depth: usize) -> bool {
    // Written deeper than any destination is (the inspector compares every destination it reads
    // with the base's, so one left naming a renamed name is refused there).
    if depth > 64 {
        return false;
    }
    let mut changed = false;
    match value {
        Object::Dict(dict) => changed |= follow_dict(dict, renames, depth),
        Object::Stream(stream) => changed |= follow_dict(&mut stream.dict, renames, depth),
        Object::Array(items) => {
            for item in items {
                changed |= follow(item, renames, depth + 1);
            }
        }
        _ => {}
    }
    changed
}

fn follow_dict(dict: &mut Dict, renames: &HashMap<Vec<u8>, Vec<u8>>, depth: usize) -> bool {
    let mut changed = false;
    let keys: Vec<Vec<u8>> = dict.iter().map(|(key, _)| key.clone()).collect();
    for key in keys {
        let Some(value) = dict.get(&key).cloned() else { continue };
        if matches!(key.as_slice(), b"Dest" | b"D")
            && let Some(new) = name_bytes(&value).and_then(|bytes| renames.get(bytes))
        {
            let renamed = match value {
                Object::Name(_) => Object::Name(new.clone()),
                _ => Object::String(PdfString::literal(new.clone())),
            };
            dict.set(key, renamed);
            changed = true;
            continue;
        }
        let mut inner = value;
        if follow(&mut inner, renames, depth + 1) {
            dict.set(key, inner);
            changed = true;
        }
    }
    changed
}

/// Every (key, value) of the name tree under `node`, in tree order.
fn collect_tree(
    doc: &Document,
    node: &Object,
    out: &mut Vec<(Vec<u8>, Object)>,
    seen: &mut HashSet<ObjRef>,
    work: &interp::Work,
    depth: usize,
) -> Result<(), RedactError> {
    if depth > 64 || out.len() > 100_000 {
        return Err(RedactError::TooComplex);
    }
    if let Some(r) = node.as_ref()
        && !seen.insert(r)
    {
        return Ok(());
    }
    work.charge(4);
    let Some(dict) = doc.resolve(node).as_dict().cloned() else { return Ok(()) };
    if let Some(items) = dict.get(b"Names").and_then(|n| doc.resolve(n).as_array().cloned()) {
        for pair in items.chunks(2) {
            if let (Some(key), Some(value)) = (pair.first(), pair.get(1))
                && let Some(key) = doc.resolve(key).as_string()
            {
                out.push((key.bytes.clone(), value.clone()));
            }
        }
    }
    if let Some(kids) = dict.get(b"Kids").and_then(|k| doc.resolve(k).as_array().cloned()) {
        for kid in kids {
            collect_tree(doc, &kid, out, seen, work, depth + 1)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (removed text, a copy, whether the copy holds a removed word): R6b-52 pinned.
    const CASES: &[(&str, &str, bool)] = &[
        // Where in the copy.
        ("Andi", "Andi", true),
        ("Andi", "Andi and the others", true),
        ("Andi", "Agreement with Andi", true),
        ("Andi", "(Andi)", true),
        ("Andi", "Andi, Wijaya.", true),
        ("Andi", "Andi2024", true),
        ("Andi", "2024Andi", true),
        ("Andi", "BudiAndi", true),
        ("Budi", "BudiCorp", true),
        ("SECRETWORD", "XSECRETWORDX", true),
        ("Room101", "Room 101", true),
        ("Room 101", "Room101", true),
        // Characters viewers do not show, or show alike.
        ("Andi", "An\u{ad}di", true),
        ("Andi", "An\u{200b}di", true),
        ("Andi", "An\u{2060}di", true),
        ("Andi", "\u{feff}Andi", true),
        ("An\u{ad}di", "Andi", true),
        ("Andi Wijaya", "Andi\u{a0}Wijaya", true),
        ("Andi Wijaya", "Andi\t\nWijaya", true),
        ("Andi Wijaya", "Andi  -  Wijaya", true),
        ("Andi", "\u{ff21}\u{ff4e}\u{ff44}\u{ff49}", true),
        ("Profit", "Pro\u{fb01}t", true),
        ("Jos\u{e9}", "JOSE", true),
        ("Jose", "Jos\u{e9}", true),
        ("Jos\u{65}\u{301}", "Jos\u{e9}", true),
        ("\u{3231}Andi", "Andi", true),
        ("Andi\u{b2}", "Andi2", true),
        ("Pro\u{fb01}t", "Profit", true),
        // Names that are also function words, as names.
        ("Dan", "Meeting with Dan", true),
        ("Ada", "Ada Lovelace notes", true),
        ("May", "Letter to May", true),
        ("Ani", "Ani and Budi", true),
        ("Ida", "Ibu Ida", true),
        // Phrases whole.
        ("PT Budi SA", "PT Budi SA", true),
        ("A.B. Holdings", "A B Holdings", true),
        // Not copies: function words alone, two letters, another word.
        ("the", "Other terms", false),
        ("the", "The terms", false),
        ("dan", "Syarat dan Ketentuan", false),
        ("AB", "AB Holdings", false),
        ("Andi", "Budi", false),
        // Letter-spaced, it is the word still.
        ("Wijaya", "Wija ya", true),
    ];

    /// (text, as normalised): decoded text, NFKC, format characters out, folded (R6b-56).
    const NORMALISED: &[(&str, &str)] = &[
        ("Andi", "andi"),
        ("ANDI", "andi"),
        ("Jos\u{e9}", "jose"),
        ("Jos\u{65}\u{301}", "jose"),
        ("\u{ff21}\u{ff4e}\u{ff44}\u{ff49}", "andi"),
        ("Pro\u{fb01}t", "profit"),
        ("An\u{ad}di", "andi"),
        ("An\u{200b}di", "andi"),
        ("\u{3231}Andi", "(株)andi"),
        ("Andi\u{b2}", "andi2"),
        ("Andi\u{a0}Wijaya", "andi wijaya"),
        ("\u{1e9e}", "ss"),
    ];

    #[test]
    fn one_normalisation_is_pinned() {
        for (text, normal) in NORMALISED {
            assert_eq!(normalise(text), *normal, "{text:?}");
        }
    }

    /// Follow-up 8's matcher (`92d7a82`, `removed_words` and `holds_word`), as it was: each
    /// run of removed text (three characters or more) and each word of five or more, as a
    /// substring of the lower-cased copy.
    fn follow_up_8_holds(removed: &str, copy: &str) -> bool {
        let mut words: Vec<String> = Vec::new();
        let mut add = |word: &str, least: usize| {
            let word = word.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
            if word.chars().filter(|c| c.is_alphanumeric()).count() >= least && !words.contains(&word) {
                words.push(word);
            }
        };
        for run in removed.split(RUN_END) {
            add(run, 3);
            for word in run.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '\'') {
                add(word, 5);
            }
        }
        let copy = copy.to_lowercase();
        words.iter().any(|word| copy.contains(word.as_str()))
    }

    /// Removed words R6b-52 deliberately no longer matches on their own (function words).
    fn deliberately_left(removed: &str) -> bool {
        let folded = normalise(removed);
        let words = words(&folded);
        words.len() == 1 && STOP_WORDS.contains(&words[0])
    }

    #[test]
    fn nothing_follow_up_8_cleared_is_kept_now() {
        // Audit (follow-up 9, coverage): every copy follow-up 8's matcher cleared is cleared
        // still, but the function words R6b-52 leaves on purpose.
        let extra: &[(&str, &str, bool)] = &[
            ("Ani", "Daniel", true),
            ("pen", "pending", true),
            ("X-1", "Model X-1", true),
            ("SECRET one", "the secret one clause", true),
            ("Wijaya", "wijaya-corp", true),
            ("O'Neil", "o'neil", true),
            ("Rp 50.000", "Rp 50.000", true),
        ];
        for (removed, copy, _) in CASES.iter().chain(extra) {
            if follow_up_8_holds(removed, copy) && !deliberately_left(removed) {
                assert!(Removed::from_text(removed).held_in(copy), "follow-up 8 cleared {copy:?} for {removed:?}; it is kept now");
            }
        }
        for (removed, copy, held) in extra {
            assert_eq!(Removed::from_text(removed).held_in(copy), *held, "{removed:?} in {copy:?}");
        }
    }

    #[test]
    fn the_matcher_is_pinned() {
        for (removed, copy, held) in CASES {
            assert_eq!(Removed::from_text(removed).held_in(copy), *held, "{removed:?} in {copy:?}");
        }
    }
}
