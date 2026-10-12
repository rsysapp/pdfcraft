//! Embed build (R6b-50, R6b-52, R6b-54 N3, N4, N5, N7, N8): copies of the removed words
//! elsewhere in the document, cleared when a host applies marks.
//!
//! **What counts as a copy (R6b-52).** The removed text is read as an extractor reads it (words
//! apart where their glyphs are apart, phrases apart where a kept glyph lies between), folded on
//! both sides (NFKC, diacritics taken off, lower case). A copy holds a removed word when it holds
//! one of the removed tokens (three letters or digits or more, a short function word excepted),
//! as a word of its own or, five characters or more, anywhere; or when it holds a removed phrase
//! (two words or more) whole. Glyphs removed without readable text (a font giving no Unicode)
//! cannot be matched: where the document holds copies they could be in (bookmarks, document
//! information values, XMP), the redaction is refused ([`Refusal::UnreadableMarkedText`]).
//!
//! **What is cleared.** Bookmark titles read "[Redacted]"; document information values go; the
//! catalog's XMP packet goes, and every other XMP packet holding a removed word; annotations'
//! `/Contents`, `/RC` and `/T` holding one go; optional-content groups' names holding one read
//! "[Redacted]"; page-label prefixes holding one go; any applied redaction removes the form's
//! XFA (`/XFA`, `/NeedsRendering`), which keeps the fields' values apart from them; named
//! destinations whose name holds one are renamed to opaque names, matched by their bytes (a name
//! or a string, literal or hex alike), with every destination naming them (in links, bookmarks,
//! the opening action, additional actions and chained actions), and the named destinations'
//! tree written again in order with its limits.

use std::collections::{HashMap, HashSet};

use pdfcraft_cos::{Dict, Document, ObjRef, Object, PdfString};
use unicode_normalization::UnicodeNormalization;

use crate::{RUN_END, RedactError, Refusal, Report, interp};

/// What replaces a cleared title or name.
pub(crate) const REDACTED: &str = "[Redacted]";

/// Function words of three letters (English and Indonesian) never matched on their own: they
/// are in too many unrelated titles. (Shorter words are never matched on their own anyway.)
const STOP_WORDS: [&str; 46] = [
    "the", "and", "for", "but", "nor", "yet", "not", "are", "was", "its", "our", "you", "she", "his", "her", "him", "all", "any", "can", "may",
    "per", "via", "who", "how", "why", "has", "had", "did", "too", "off", "out", "own", "few", "one", "two", "dan", "ini", "itu", "ada", "pun",
    "lah", "kah", "tak", "dgn", "utk", "tsb",
];

/// `text` folded for matching: NFKC, diacritics taken off, lower case.
pub(crate) fn fold(text: &str) -> String {
    let compatible: String = text.nfkc().collect();
    compatible.nfd().filter(|c| !unicode_normalization::char::is_combining_mark(*c)).flat_map(char::to_lowercase).collect()
}

/// The words of folded text: its runs of letters and digits.
fn words(folded: &str) -> Vec<&str> {
    folded.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).collect()
}

/// What the redaction removed, for matching copies.
#[derive(Debug, Default)]
pub(crate) struct Removed {
    /// Folded tokens of three characters or more, but the stop words.
    tokens: Vec<String>,
    /// Folded phrases of two words or more, their words joined by one space.
    phrases: Vec<String>,
    /// A removed glyph had no readable text.
    pub unreadable: bool,
}

impl Removed {
    /// From the removed text (runs apart by [`RUN_END`], words by spaces).
    pub fn from_text(text: &str) -> Self {
        let mut removed = Removed { unreadable: text.contains('\u{fffd}'), ..Removed::default() };
        for run in text.split(RUN_END) {
            let folded = fold(run);
            let run_words = words(&folded);
            for word in &run_words {
                if word.chars().count() >= 3 && !STOP_WORDS.contains(word) && !removed.tokens.iter().any(|t| t == word) {
                    removed.tokens.push((*word).to_string());
                }
            }
            if run_words.len() >= 2 {
                let phrase = run_words.join(" ");
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

    /// Whether `copy` holds a removed word (see the module documentation).
    pub fn held_in(&self, copy: &str) -> bool {
        let folded = fold(copy);
        let copy_words = words(&folded);
        let joined = format!(" {} ", copy_words.join(" "));
        let compact: String = copy_words.concat();
        self.tokens.iter().any(|token| copy_words.contains(&token.as_str()) || (token.chars().count() >= 5 && compact.contains(token.as_str())))
            || self.phrases.iter().any(|phrase| joined.contains(&format!(" {phrase} ")))
    }

    /// Whether bytes `copy` (a name, or a packet's data) hold one, read every way it could be
    /// meant: as a PDF text string (UTF-16BE or UTF-8 with its mark, else PDFDocEncoding), as
    /// UTF-8, as UTF-16 little-endian with its mark; XML character references and entities read
    /// (an XMP packet's `&#83;`).
    fn held_in_bytes(&self, copy: &[u8]) -> bool {
        let mut readings = vec![PdfString::literal(copy.to_vec()).to_text(), String::from_utf8_lossy(copy).into_owned()];
        if let Some(rest) = copy.strip_prefix(&[0xFF, 0xFE]) {
            let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
            readings.push(String::from_utf16_lossy(&units));
        }
        let unescaped: Vec<String> = readings.iter().map(|text| xml_unescaped(text)).collect();
        readings.extend(unescaped);
        readings.iter().any(|text| self.held_in(&without_language_marks(text)))
    }

    /// Whether a text string `copy` holds one. Audit (follow-up 9): one that does not decode
    /// cleanly (a character no encoding gives) counts as holding one, so it is cleared rather
    /// than kept unread.
    fn held_in_text(&self, copy: &str) -> bool {
        copy.contains('\u{fffd}') || self.held_in(&without_language_marks(copy))
    }
}

/// A text string without its language marks (ISO 32000-2 §7.9.2.2: U+001B, a language code,
/// U+001B), which viewers do not show: a mark inside a word must not split it.
fn without_language_marks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for c in text.chars() {
        if c == '\u{1b}' {
            inside = !inside;
        } else if !inside {
            out.push(c);
        }
    }
    out
}

/// XML character references and the five entities read (`&#83;`, `&#x53;`, `&amp;`, …).
fn xml_unescaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let entity_end = tail.find(';').filter(|end| *end <= 12);
        let decoded = entity_end.and_then(|end| {
            let entity = &tail[1..end];
            match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|digits| digits.parse::<u32>().ok()))
                    .and_then(char::from_u32),
            }
        });
        match (decoded, entity_end) {
            (Some(c), Some(end)) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            _ => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A copy's text: a string's (as a PDF text string), or a name's bytes read as one.
fn text_of(doc: &Document, o: &Object) -> Option<String> {
    match &*doc.resolve(o) {
        Object::String(s) => Some(s.to_text()),
        Object::Name(n) => Some(PdfString::literal(n.clone()).to_text()),
        _ => None,
    }
}

/// Audit (follow-up 9): whether an action (a bookmark's, the opening one), or any action it
/// chains, holds a removed word anywhere (a URI, a file name, JavaScript as a string or a
/// stream): such a copy cannot be cleared without changing what the action does.
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
        Object::String(s) => removed.held_in_text(&s.to_text()) || removed.held_in_bytes(&s.bytes),
        Object::Name(n) => removed.held_in_bytes(n),
        Object::Stream(stream) => {
            let data = stream.decoded().map_err(|_| RedactError::Refused(Refusal::CopyNotClearable))?;
            work.charge(data.len() / 64);
            removed.held_in_bytes(&data)
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

/// Clear the copies of what was removed (see the module documentation), counting each in
/// `report`.
pub(crate) fn clear(doc: &mut Document, removed: &Removed, report: &mut Report, work: &interp::Work) -> Result<(), RedactError> {
    let spent = |work: &interp::Work| if work.spent() { Err(RedactError::TooComplex) } else { Ok(()) };
    let Some(root) = doc.root() else { return Ok(()) };
    let mut catalog = doc.get(root).as_dict().cloned().unwrap_or_default();
    let info = doc.trailer().get(b"Info").and_then(Object::as_ref);
    // Unreadable removed text cannot be looked for where copies could be.
    if removed.unreadable {
        let bookmarks = catalog.get(b"Outlines").and_then(|o| doc.resolve(o).as_dict().and_then(|d| d.get(b"First").cloned())).is_some();
        let information =
            info.and_then(|r| doc.get(r).as_dict().cloned()).is_some_and(|d| d.iter().any(|(_, v)| doc.resolve(v).as_string().is_some()));
        if bookmarks || information || catalog.contains(b"Metadata") {
            return Err(RedactError::Refused(Refusal::UnreadableMarkedText));
        }
    }
    // The catalog's XMP packet, always; any applied redaction, the form's XFA.
    let mut catalog_changed = false;
    if catalog.remove(b"Metadata").is_some() {
        report.metadata_removed += 1;
        catalog_changed = true;
    }
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
    if removed.is_empty() {
        return Ok(());
    }
    // Audit (follow-up 9): the opening action holding a removed word cannot be cleared.
    if let Some(action) = catalog.get(b"OpenAction").filter(|action| !matches!(&*doc.resolve(action), Object::Array(_)))
        && action_holds(doc, action, removed, work, 0)?
    {
        return Err(RedactError::Refused(Refusal::CopyNotClearable));
    }
    // Bookmarks: depth first, bounded.
    let mut todo: Vec<Object> =
        catalog.get(b"Outlines").and_then(|o| doc.resolve(o).as_dict().and_then(|d| d.get(b"First").cloned())).into_iter().collect();
    let mut seen: HashSet<ObjRef> = HashSet::new();
    while let Some(entry) = todo.pop() {
        work.charge(8);
        spent(work)?;
        // Audit (follow-up 9): a walk past its bound, or an entry that is no dictionary, is never
        // skipped: the redaction is refused.
        let Some(r) = entry.as_ref() else { return Err(RedactError::Refused(Refusal::CopyNotClearable)) };
        if !seen.insert(r) {
            continue;
        }
        if seen.len() > 100_000 {
            return Err(RedactError::TooComplex);
        }
        let Some(item) = doc.get(r).as_dict().cloned() else { return Err(RedactError::Refused(Refusal::CopyNotClearable)) };
        // Audit (follow-up 9): a bookmark's action holding a removed word cannot be cleared.
        if let Some(action) = item.get(b"A")
            && action_holds(doc, action, removed, work, 0)?
        {
            return Err(RedactError::Refused(Refusal::CopyNotClearable));
        }
        if item.get(b"Title").and_then(|t| text_of(doc, t)).is_some_and(|title| removed.held_in_text(&title)) {
            doc.update_dict(r, |d| d.set(b"Title".to_vec(), Object::String(PdfString::text(REDACTED))))?;
            report.bookmarks_cleared += 1;
        }
        todo.extend(item.get(b"Next").cloned());
        todo.extend(item.get(b"First").cloned());
    }
    // The document information.
    if let Some(info) = info
        && let Some(dict) = doc.get(info).as_dict().cloned()
    {
        let doomed: Vec<Vec<u8>> = dict
            .iter()
            .filter(|(_, value)| text_of(doc, value).is_some_and(|text| removed.held_in_text(&text)))
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
    // Every object: XMP packets, annotations' text, layers' names holding a removed word.
    for num in doc.object_numbers() {
        work.charge(4);
        spent(work)?;
        let r = ObjRef::new(num, 0);
        let Ok(object) = doc.try_get(num) else { continue };
        let dict = match &*object {
            Object::Dict(d) => d.clone(),
            Object::Stream(s) => s.dict.clone(),
            _ => continue,
        };
        // XMP: the packet's own text.
        if let Some(Object::Ref(metadata)) = dict.get(b"Metadata")
            && let Object::Stream(packet) = &*doc.get(*metadata)
        {
            // Audit (follow-up 9): a packet that does not decode is removed, never kept unread.
            let data = packet.decoded_strict_counted(64 << 20).ok().map(|(data, _)| data);
            work.charge(data.as_ref().map_or(0, Vec::len) / 64);
            if data.is_none_or(|data| removed.held_in_bytes(&data)) {
                doc.update_dict(r, |d| {
                    d.remove(b"Metadata");
                })?;
                report.metadata_removed += 1;
            }
        }
        // An annotation's text (its contents, rich text, author).
        if dict.name(b"Type") == Some(b"Annot") || (dict.contains(b"Subtype") && dict.contains(b"Rect") && dict.contains(b"Contents")) {
            let doomed: Vec<&[u8]> = [&b"Contents"[..], b"RC", b"T"]
                .into_iter()
                .filter(|key| dict.get(key).and_then(|v| text_of(doc, v)).is_some_and(|text| removed.held_in_text(&text)))
                .collect();
            if !doomed.is_empty() && dict.name(b"Subtype") != Some(b"Widget") {
                doc.update_dict(r, |d| {
                    for key in &doomed {
                        d.remove(key);
                    }
                })?;
                report.annotation_texts_cleared += 1;
            }
        }
        // An optional-content group's name.
        if dict.name(b"Type") == Some(b"OCG") && dict.get(b"Name").and_then(|v| text_of(doc, v)).is_some_and(|name| removed.held_in_text(&name)) {
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
            if entry.get(b"P").and_then(|p| text_of(doc, p)).is_some_and(|prefix| removed.held_in_text(&prefix)) {
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
        let doomed: Vec<Vec<u8>> = dict.iter().map(|(key, _)| key.clone()).filter(|key| removed.held_in_bytes(key)).collect();
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
        if removed.held_in_bytes(key) {
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
