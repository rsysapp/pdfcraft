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

    /// Whether bytes `copy` (as text: UTF-16 with its mark, else Latin-1 and UTF-8 both) hold one.
    fn held_in_bytes(&self, copy: &[u8]) -> bool {
        self.held_in(&PdfString::literal(copy.to_vec()).to_text()) || self.held_in(&String::from_utf8_lossy(copy))
    }
}

fn text_of(doc: &Document, o: &Object) -> Option<String> {
    doc.resolve(o).as_string().map(PdfString::to_text)
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
    // Bookmarks: depth first, bounded.
    let mut todo: Vec<Object> =
        catalog.get(b"Outlines").and_then(|o| doc.resolve(o).as_dict().and_then(|d| d.get(b"First").cloned())).into_iter().collect();
    let mut seen: HashSet<ObjRef> = HashSet::new();
    while let Some(entry) = todo.pop() {
        work.charge(8);
        spent(work)?;
        let Some(r) = entry.as_ref() else { continue };
        if seen.len() > 100_000 || !seen.insert(r) {
            continue;
        }
        let Some(item) = doc.get(r).as_dict().cloned() else { continue };
        if item.get(b"Title").and_then(|t| text_of(doc, t)).is_some_and(|title| removed.held_in(&title)) {
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
        let doomed: Vec<Vec<u8>> =
            dict.iter().filter(|(_, value)| text_of(doc, value).is_some_and(|text| removed.held_in(&text))).map(|(key, _)| key.clone()).collect();
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
            let data = packet.decoded().unwrap_or_default();
            work.charge(data.len() / 64);
            if removed.held_in_bytes(&data) {
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
                .filter(|key| dict.get(key).and_then(|v| text_of(doc, v)).is_some_and(|text| removed.held_in(&text)))
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
        if dict.name(b"Type") == Some(b"OCG") && dict.get(b"Name").and_then(|v| text_of(doc, v)).is_some_and(|name| removed.held_in(&name)) {
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
    if depth > 32 {
        return Ok(());
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
            if entry.get(b"P").and_then(|p| text_of(doc, p)).is_some_and(|prefix| removed.held_in(&prefix)) {
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
    if depth > 32 {
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
    if depth > 32 || out.len() > 100_000 {
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
