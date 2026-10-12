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
//! candidate alike: decoded by the string's own encoding ([`read_text`]), then NFKC, then
//! format characters taken out, then case and diacritics folded. A copy holds a removed word
//! when it holds one of the removed tokens (three letters or digits or more, a short function
//! word excepted) as a word of its own or a part of one where letters meet digits, or, five
//! characters or more, inside a word or across words (R6b-59: a shorter one only at token
//! boundaries); or a removed phrase (two words or more), inside words too. A candidate not read alike by every reader
//! (R6b-57: ambiguous, invalid, decodings that disagree, an encoding not read, an escape that is
//! no language mark, a character the matcher does not fold) counts as holding one: it is
//! cleared, and counted (`copies_cleared_unread`); clearing too much is accepted, keeping a copy
//! is not. Refused are only structural failures (a bookmark that is no dictionary, an outline
//! that loops, a bound reached), removed glyphs without readable text where any of these places
//! holds text ([`Refusal::UnreadableMarkedText`]), and a bookmark's or the opening action's URI
//! or JavaScript holding a removed word (or not read alike): it cannot be cleared without
//! changing what the action does.
//!
//! **What clearing does.** Bookmark titles read "[Redacted]"; annotations' text entries holding
//! one go; form fields' names (`/T`) holding one are renamed `Field<n>` (R6b-58), and an
//! action naming a field by such a string refuses; groups' names read "[Redacted]"; prefixes go;
//! named destinations are renamed to opaque names, matched by their bytes (a name or a string, literal or hex alike), with every
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

/// Characters a viewer does not show, which the matcher folds out (they must not split a word):
/// the soft hyphen, zero-width spaces and joiners, directional marks, the word joiner and its
/// kin, the byte-order mark.
fn invisible(c: char) -> bool {
    matches!(c, '\u{ad}' | '\u{34f}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}')
}

/// The language escapes of a text string (U+001B, a code, U+001B) taken out, as pdf.js takes
/// them (`\x1b[^\x1b]*(?:\x1b|$)`: one left open runs to the end).
fn without_escapes(text: &str) -> String {
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

/// Whether every escape of a text string is a language mark the standard defines (U+001B, a
/// two-letter language code, an optional two-letter country code, U+001B).
fn escapes_known(text: &str) -> bool {
    let mut parts = text.split('\u{1b}');
    parts.next();
    let codes: Vec<&str> = parts.collect();
    if !codes.len().is_multiple_of(2) {
        return false;
    }
    codes.chunks(2).all(|pair| {
        let code = pair[0];
        let letters = |s: &str, lower: bool| s.len() == 2 && s.chars().all(|c| if lower { c.is_ascii_lowercase() } else { c.is_ascii_uppercase() });
        (code.len() == 2 && letters(code, true)) || (code.len() == 4 && letters(&code[..2], true) && letters(&code[2..], false))
    })
}

/// A text string as pdf.js reads it (`stringToPDFString`, escapes kept): after a UTF-16BE,
/// UTF-16LE or UTF-8 mark, decoded strictly, falling back to its PDFDocEncoding table when that
/// fails; its table leaves bytes it does not translate (0x9F) as they are.
fn as_pdf_js_reads(bytes: &[u8]) -> String {
    let utf16 = |rest: &[u8], le: bool| {
        rest.len()
            .is_multiple_of(2)
            .then(|| {
                let units = rest.as_chunks::<2>().0.iter().map(|p| if le { u16::from_le_bytes(*p) } else { u16::from_be_bytes(*p) });
                char::decode_utf16(units).collect::<Result<String, _>>().ok()
            })
            .flatten()
    };
    let marked: Option<Option<String>> = if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        Some(utf16(rest, false))
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        Some(utf16(rest, true))
    } else {
        bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).map(|rest| String::from_utf8(rest.to_vec()).ok())
    };
    if let Some(Some(text)) = marked {
        return text;
    }
    bytes
        .iter()
        .map(|&byte| match PdfString::literal(vec![byte]).to_text().chars().next() {
            Some('\u{fffd}') | None => char::from(byte),
            Some(c) => c,
        })
        .collect()
}

/// A text string as the standard reads it (ISO 32000-2 §7.9.2.2; PDFium and this file's own
/// reader, escapes kept): UTF-16BE or UTF-8 after its mark, else PDFDocEncoding, bad sequences
/// replaced.
fn as_the_standard_reads(bytes: &[u8]) -> String {
    PdfString::literal(bytes.to_vec()).to_text()
}

/// Whether a character is one the matcher cannot stand for: a replacement character, a control
/// character but white space, a private-use or non-character, or a format character it does not
/// fold out (see [`invisible`]).
fn unfolded(c: char) -> bool {
    let code = u32::from(c);
    let format = matches!(code, 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2 | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F);
    c == '\u{fffd}'
        || (c.is_control() && !matches!(c, '\t' | '\n' | '\r' | '\u{c}' | '\u{1b}'))
        || matches!(code, 0xE000..=0xF8FF | 0xF0000..=0x10FFFF | 0xFDD0..=0xFDEF)
        || code & 0xFFFE == 0xFFFE
        || (format && !invisible(c))
}

/// How a candidate reads (R6b-57).
pub(crate) enum Reading {
    /// Read alike by every reader: its text, escapes out.
    Read(String),
    /// Not read alike (ambiguous, invalid, decodings that disagree, an encoding not read, a
    /// character the matcher cannot stand for): taken as holding a removed word.
    Unread,
}

/// R6b-57: a PDF text string as every reader reads it: pdf.js's rules and the standard's must
/// give the same text; a mark's decoding must be valid; bytes past 0x7F without a mark must not
/// also be UTF-8 (which a reader could take them for); every escape must be a language mark; and
/// every character one the matcher folds or stands for.
pub(crate) fn read_text(bytes: &[u8]) -> Reading {
    let (pdf_js, standard) = (as_pdf_js_reads(bytes), as_the_standard_reads(bytes));
    let marked = bytes.starts_with(&[0xFE, 0xFF]) || bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
    let utf8_too = !marked && !bytes.is_ascii() && std::str::from_utf8(bytes).is_ok();
    if pdf_js != standard || utf8_too || !escapes_known(&standard) || standard.chars().any(unfolded) {
        return Reading::Unread;
    }
    Reading::Read(without_escapes(&standard))
}

/// A name's bytes as text (destination names): printable ASCII is read as it is; anything
/// else is unread (R6b-57: readers take a name's other bytes each their own way).
fn read_name(bytes: &[u8]) -> Reading {
    if bytes.iter().all(|byte| (0x21..0x7F).contains(byte)) { Reading::Read(String::from_utf8_lossy(bytes).into_owned()) } else { Reading::Unread }
}

/// Case and diacritics folded (after NFKC and the format strip): case folded, then decomposed
/// and the marks taken out.
fn fold_case(text: &str) -> String {
    // Case first (full folding: ß and ẞ as ss, a final sigma as σ, İ as i and its dot), then the
    // diacritics (the decomposed marks, a dot case folding gave among them), then anything the
    // folding left that is not shown.
    let cased: String = text
        .chars()
        .flat_map(char::to_lowercase)
        .flat_map(|c| match c {
            'ß' => vec!['s', 's'],
            'ς' => vec!['σ'],
            other => vec![other],
        })
        .collect();
    cased.nfd().filter(|c| !unicode_normalization::char::is_combining_mark(*c) && !invisible(*c)).collect()
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
            // Split where the candidate side splits once folded: at what is neither a letter or
            // digit nor a mark or a character folded out.
            let joins = |c: char| c.is_alphanumeric() || unicode_normalization::char::is_combining_mark(c) || invisible(c);
            for raw in run.split(|c: char| !joins(c)).filter(|word| !word.is_empty()) {
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
                // R6b-59: five characters or more, inside a word ("XSECRETWORDX") and across
                // words ("SECRET WORD"); shorter, only as a word or a piece of its own ("Ani"
                // clears "ANI," but not "Daniel").
                || (length >= 5 && compact.contains(token.as_str()))
        }) || self.phrases.iter().any(|phrase| joined.contains(phrase.as_str()))
    }
}

/// R6b-57: whether candidate `o` (a string, or a name's bytes) holds a removed word: read alike
/// by every reader and holding one, or not read alike at all (counted in
/// `copies_cleared_unread`: cleared as if it held one). Anything else is no candidate.
fn holds(doc: &Document, o: &Object, removed: &Removed, report: &mut Report) -> bool {
    let reading = match &*doc.resolve(o) {
        Object::String(s) => read_text(&s.bytes),
        Object::Name(n) => read_name(n),
        _ => return false,
    };
    match reading {
        Reading::Read(text) => removed.held_in(&text),
        Reading::Unread => {
            report.copies_cleared_unread += 1;
            true
        }
    }
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
        // Held, or not read alike (R6b-57): an action cannot be cleared, so either refuses.
        Object::String(_) | Object::Name(_) => holds(doc, action, removed, &mut Report::default()),
        Object::Stream(stream) => {
            let data = stream.decoded().map_err(|_| RedactError::Refused(Refusal::CopyNotClearable))?;
            work.charge(data.len() / 64);
            match read_text(&data) {
                Reading::Read(text) => removed.held_in(&text),
                Reading::Unread => true,
            }
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
        // Final audit: an object that cannot be read could hold a packet: refused, not passed.
        let Ok(object) = doc.try_get(num) else { return Err(RedactError::Refused(Refusal::CopyNotClearable)) };
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
    // R6b-58: form fields' names; every object the field tree reaches is a field or its widget.
    let in_form = rename_fields(doc, &catalog, removed, report, work)?;
    let annotations = page_annotations(doc, work);
    // Final audit: a group written in place (no object of its own) cannot be renamed alone:
    // refused, never passed over.
    let listed = catalog
        .get(b"OCProperties")
        .and_then(|p| doc.resolve(p).as_dict().cloned())
        .and_then(|p| p.get(b"OCGs").and_then(|g| doc.resolve(g).as_array().cloned()))
        .unwrap_or_default();
    let groups: Vec<ObjRef> = listed.iter().filter_map(Object::as_ref).collect();
    if groups.len() != listed.len() {
        return Err(RedactError::Refused(Refusal::CopyNotClearable));
    }
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
    // R6b-57: a candidate not read alike is cleared whatever was removed (an image alone too), so
    // every candidate is read even when no text was removed.
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
        // R6b-57: an outline that loops is a structural failure: refused.
        if !seen.insert(r) {
            return Err(RedactError::Refused(Refusal::CopyNotClearable));
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
            && holds(doc, title, removed, report)
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
        // Not a dictionary: no annotation a viewer shows, and no text.
        let Some(dict) = doc.get(r).as_dict().cloned() else { continue };
        let mut doomed: Vec<&[u8]> = Vec::new();
        // A form field's widget: its /T is the field's name, renamed above (R6b-58). A widget
        // the form's tree does not reach is no field: its texts are any annotation's.
        let field_widget = dict.name(b"Subtype") == Some(b"Widget") && in_form.contains(&r);
        let keys: &[&[u8]] = if field_widget { &[b"Contents", b"RC"] } else { &[b"Contents", b"RC", b"T"] };
        for key in keys.iter().copied() {
            if let Some(value) = dict.get(key)
                && holds(doc, value, removed, report)
            {
                doomed.push(key);
            }
        }
        // Final audit: a form field's widget cannot lose its contents without changing the form:
        // refused, never kept.
        if !doomed.is_empty() && field_widget {
            return Err(RedactError::Refused(Refusal::CopyNotClearable));
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
            && holds(doc, name, removed, report)
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

/// One object of the form's field tree (R6b-58): a field, or a widget kid (no `/T`, no `/Kids`).
struct FormNode {
    obj: ObjRef,
    /// A field (not a widget kid).
    field: bool,
    /// The field type, inherited (`/FT`).
    kind: Option<Vec<u8>>,
    /// A value, inherited (`/V`, not null).
    valued: bool,
}

/// The form's field tree from `/AcroForm /Fields`, every object once, depth first, as the
/// inspector reads it. A tree that loops, runs past its bound or holds an entry that is no
/// dictionary reached by reference is refused: what it holds cannot be told.
fn form_nodes(doc: &Document, catalog: &Dict, work: &interp::Work) -> Result<Vec<FormNode>, RedactError> {
    let refuse = || RedactError::Refused(Refusal::CopyNotClearable);
    let mut out = Vec::new();
    let Some(form) = catalog.get(b"AcroForm").and_then(|f| doc.resolve(f).as_dict().cloned()) else { return Ok(out) };
    let Some(top) = form.get(b"Fields").map(|f| Object::clone(&doc.resolve(f))) else { return Ok(out) };
    let Object::Array(top) = top else { return Err(refuse()) };
    let mut seen: HashSet<ObjRef> = HashSet::new();
    // Each entry with what it inherits, its depth, and whether it is read as a field.
    let mut stack: Vec<(Object, FormNode, usize)> =
        top.into_iter().rev().map(|entry| (entry, FormNode { obj: ObjRef::new(0, 0), field: true, kind: None, valued: false }, 0)).collect();
    while let Some((entry, FormNode { field, kind, valued, .. }, depth)) = stack.pop() {
        work.charge(2);
        if work.spent() {
            return Err(RedactError::TooComplex);
        }
        let Object::Ref(r) = entry else { return Err(refuse()) };
        if depth > 64 || out.len() > 100_000 || !seen.insert(r) {
            return Err(refuse());
        }
        let Some(dict) = doc.get(r).as_dict().cloned() else { return Err(refuse()) };
        let kind = dict.get(b"FT").and_then(|t| doc.resolve(t).as_name().map(<[u8]>::to_vec)).or(kind);
        let valued = match dict.get(b"V") {
            Some(value) => !matches!(&*doc.resolve(value), Object::Null),
            None => valued,
        };
        let kids = match dict.get(b"Kids").map(|k| Object::clone(&doc.resolve(k))) {
            Some(Object::Array(kids)) => kids,
            Some(_) => return Err(refuse()),
            None => Vec::new(),
        };
        for kid in kids.into_iter().rev() {
            let Some(kid_ref) = kid.as_ref() else { return Err(refuse()) };
            let Some(kid_dict) = doc.get(kid_ref).as_dict().cloned() else { return Err(refuse()) };
            let kid_field = kid_dict.contains(b"T") || kid_dict.contains(b"Kids");
            stack.push((kid, FormNode { obj: kid_ref, field: kid_field, kind: kind.clone(), valued }, depth + 1));
        }
        out.push(FormNode { obj: r, field, kind, valued });
    }
    Ok(out)
}

/// R6b-58: whether the document is signed: a signature field (or widget) with a value, or a
/// DocMDP or usage-rights signature (`/Perms`). Rewriting it breaks every signature, and the
/// signer's data cannot be cleared without destroying it.
pub(crate) fn signed(doc: &Document, work: &interp::Work) -> Result<bool, RedactError> {
    let Some(root) = doc.root() else { return Ok(false) };
    let catalog = doc.get(root).as_dict().cloned().unwrap_or_default();
    if let Some(perms) = catalog.get(b"Perms").map(|p| Object::clone(&doc.resolve(p))) {
        let Object::Dict(perms) = perms else { return Ok(true) };
        if [&b"DocMDP"[..], b"UR", b"UR3"].iter().any(|key| perms.contains(key)) {
            return Ok(true);
        }
    }
    let signature = |kind: Option<&[u8]>, valued: bool| kind == Some(b"Sig") && valued;
    if form_nodes(doc, &catalog, work)?.iter().any(|node| signature(node.kind.as_deref(), node.valued)) {
        return Ok(true);
    }
    // A signature widget the form's tree does not reach still carries the signer's data.
    for r in page_annotations(doc, work) {
        let Some(dict) = doc.get(r).as_dict().cloned() else { continue };
        let kind = dict.get(b"FT").and_then(|t| doc.resolve(t).as_name().map(<[u8]>::to_vec));
        let valued = dict.get(b"V").is_some_and(|value| !matches!(&*doc.resolve(value), Object::Null));
        if signature(kind.as_deref(), valued) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// R6b-58: the form fields whose name (`/T`) holds a removed word, or is not read alike, renamed
/// `Field<n>` (a name no field of the form has), each counted, as destinations are. An action
/// naming a field by a string holding one is refused: it cannot follow without changing what
/// it does. Returns every object the form's tree reaches.
fn rename_fields(
    doc: &mut Document,
    catalog: &Dict,
    removed: &Removed,
    report: &mut Report,
    work: &interp::Work,
) -> Result<HashSet<ObjRef>, RedactError> {
    let nodes = form_nodes(doc, catalog, work)?;
    let reached: HashSet<ObjRef> = nodes.iter().map(|node| node.obj).collect();
    let mut taken: HashSet<Vec<u8>> = HashSet::new();
    let mut doomed: Vec<ObjRef> = Vec::new();
    for node in nodes.iter().filter(|node| node.field) {
        let Some(dict) = doc.get(node.obj).as_dict().cloned() else { continue };
        let Some(name) = dict.get(b"T").map(|t| Object::clone(&doc.resolve(t))) else { continue };
        let reading = match &name {
            Object::String(text) => {
                taken.insert(text.bytes.clone());
                read_text(&text.bytes)
            }
            _ => Reading::Unread,
        };
        if name_held(reading, removed, report) {
            doomed.push(node.obj);
        }
    }
    let mut next = 1usize;
    for r in doomed {
        let name = loop {
            let name = format!("Field{next}");
            next += 1;
            if taken.insert(name.clone().into_bytes()) {
                break name;
            }
        };
        doc.update_dict(r, |d| d.set(b"T".to_vec(), Object::String(PdfString::text(&name))))?;
        report.field_names_renamed += 1;
    }
    // Actions that name fields by their names: submitting, resetting, hiding.
    for num in doc.object_numbers() {
        work.charge(1);
        if work.spent() {
            return Err(RedactError::TooComplex);
        }
        let Ok(object) = doc.try_get(num) else { return Err(RedactError::Refused(Refusal::CopyNotClearable)) };
        let dict = match &*object {
            Object::Dict(d) => d.clone(),
            Object::Stream(s) => s.dict.clone(),
            _ => continue,
        };
        let key: &[u8] = match dict.name(b"S") {
            Some(b"SubmitForm" | b"ResetForm") => b"Fields",
            Some(b"Hide") => b"T",
            _ => continue,
        };
        let Some(value) = dict.get(key).map(|v| Object::clone(&doc.resolve(v))) else { continue };
        let strings: Vec<Object> = match value {
            Object::Array(items) => items.iter().map(|item| Object::clone(&doc.resolve(item))).collect(),
            other => vec![other],
        };
        for string in strings {
            if let Object::String(text) = string
                && name_held(read_text(&text.bytes), removed, report)
            {
                return Err(RedactError::Refused(Refusal::CopyNotClearable));
            }
        }
    }
    Ok(reached)
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
                && holds(doc, prefix, removed, report)
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

/// Whether a destination name, as read, holds a removed word (R6b-57: one not read alike is
/// renamed, counted).
fn name_held(reading: Reading, removed: &Removed, report: &mut Report) -> bool {
    match reading {
        Reading::Read(text) => removed.held_in(&text),
        Reading::Unread => {
            report.copies_cleared_unread += 1;
            true
        }
    }
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
            if name_held(read_name(key), removed, report) {
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
    // Final audit: a destination named nowhere (in no /Dests, no tree) whose name holds a removed
    // word is renamed too (it stays named nowhere), never kept.
    for num in doc.object_numbers() {
        work.charge(2);
        if work.spent() {
            return Err(RedactError::TooComplex);
        }
        let Ok(object) = doc.try_get(num) else { return Err(RedactError::Refused(Refusal::CopyNotClearable)) };
        let mut names_used: Vec<Object> = Vec::new();
        destination_names(&object, &mut names_used, 0)?;
        for name in names_used {
            let Some(bytes) = name_bytes(&name).map(<[u8]>::to_vec) else { continue };
            if existing.contains(&bytes) || renames.contains_key(&bytes) {
                continue;
            }
            let reading = match &name {
                Object::Name(n) => read_name(n),
                _ => read_text(&bytes),
            };
            if name_held(reading, removed, report) {
                let new = fresh(&mut existing);
                renames.insert(bytes, new);
            }
        }
    }
    let mut tree_changed = false;
    for (key, _) in &mut tree {
        if name_held(read_text(key), removed, report) {
            let new = renames.get(key).cloned().unwrap_or_else(|| fresh(&mut existing));
            renames.insert(key.clone(), new.clone());
            *key = new;
            tree_changed = true;
        }
    }
    report.destinations_renamed += renames.len();
    if renames.is_empty() {
        // Nothing renamed, nothing to follow.
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
        let Ok(object) = doc.try_get(num) else { return Err(RedactError::Refused(Refusal::CopyNotClearable)) };
        let mut value = (*object).clone();
        if follow(&mut value, &renames, 0) {
            doc.set(r, value);
        }
    }
    Ok(())
}

/// Every name or string a `/Dest` or `/D` in `value` (dictionaries and arrays written in it)
/// holds, into `out`. Deeper than 64 levels refuses (never passed over).
fn destination_names(value: &Object, out: &mut Vec<Object>, depth: usize) -> Result<(), RedactError> {
    if depth > 64 {
        return Err(RedactError::TooComplex);
    }
    let dict = match value {
        Object::Dict(dict) => dict,
        Object::Stream(stream) => &stream.dict,
        Object::Array(items) => {
            for item in items {
                destination_names(item, out, depth + 1)?;
            }
            return Ok(());
        }
        _ => return Ok(()),
    };
    for (key, value) in dict.iter() {
        if matches!(key.as_slice(), b"Dest" | b"D") && matches!(value, Object::Name(_) | Object::String(_)) {
            out.push(value.clone());
        } else {
            destination_names(value, out, depth + 1)?;
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
    // Final audit: a tree that loops is refused (the inspector refuses it too), never passed.
    if let Some(r) = node.as_ref()
        && !seen.insert(r)
    {
        return Err(RedactError::Refused(Refusal::CopyNotClearable));
    }
    work.charge(4);
    let Some(dict) = doc.resolve(node).as_dict().cloned() else { return Ok(()) };
    if let Some(items) = dict.get(b"Names").and_then(|n| doc.resolve(n).as_array().cloned()) {
        for pair in items.chunks(2) {
            let (Some(key), Some(value)) = (pair.first(), pair.get(1)) else { return Err(RedactError::Refused(Refusal::CopyNotClearable)) };
            let Some(key) = doc.resolve(key).as_string().map(|key| key.bytes.clone()) else {
                return Err(RedactError::Refused(Refusal::CopyNotClearable));
            };
            out.push((key, value.clone()));
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
        // R6b-59: shorter than five, only at a boundary; five or more, and phrases, anywhere.
        ("Andi", "BudiAndi", false),
        ("Budi", "BudiCorp", false),
        ("Ani", "Daniel", false),
        ("Ani", "ANI,", true),
        ("Ani", "ani-corp", true),
        ("Wijaya", "PTWijayaCorp", true),
        ("Andi Wijaya", "xandi wijayax", true),
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
        ("\u{fb01}nal", "FINAL", true),
        ("Stra\u{df}e", "STRASSE", true),
        ("STRA\u{1e9e}E", "strasse", true),
        ("\u{130}stanbul", "istanbul", true),
        ("\u{39f}\u{394}\u{39f}\u{3a3}", "\u{3bf}\u{3b4}\u{3bf}\u{3c2}", true),
        ("\u{3bf}\u{3b4}\u{3bf}\u{3c2}", "\u{39f}\u{394}\u{39f}\u{3a3}", true),
        ("Ma\u{331}ria", "Maria", true),
        ("Maria", "Ma\u{331}ria", true),
        ("\u{301}Andi", "Andi", true),
        ("Andi", "\u{ff21}\u{ff2e}\u{ff24}\u{ff29}", true),
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
        // The order pinned (R6b-56): NFKC before folding, the format strip, case then diacritics.
        ("\u{fb01}nal", "final"),
        ("STRA\u{1e9e}E", "strasse"),
        ("Stra\u{df}e", "strasse"),
        ("\u{130}stanbul", "istanbul"),
        ("\u{39f}\u{394}\u{39f}\u{3a3}", "\u{3bf}\u{3b4}\u{3bf}\u{3c3}"),
        ("\u{3bf}\u{3b4}\u{3bf}\u{3c2}", "\u{3bf}\u{3b4}\u{3bf}\u{3c3}"),
        ("\u{ff33}\u{ff25}\u{ff23}\u{ff32}\u{ff25}\u{ff34}", "secret"),
        ("\u{301}Andi", "andi"),
        ("Andi\u{301}", "andi"),
        ("Ma\u{331}ria", "maria"),
        ("A\u{ad}n\u{200b}d\u{2060}i", "andi"),
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

    /// Copies R6b-52 and R6b-59 deliberately no longer match: a function word on its own, and a
    /// word shorter than five characters met only inside a longer one ("Ani" in "Daniel").
    fn deliberately_left(removed: &str, copy: &str) -> bool {
        let folded = normalise(removed);
        let removed_words = words(&folded);
        let copy_folded = normalise(copy);
        let copy_words = words(&copy_folded);
        let copy_pieces: Vec<&str> = copy_words.iter().flat_map(|word| pieces(word)).collect();
        let function_word = removed_words.len() == 1 && STOP_WORDS.contains(&removed_words[0]);
        let inside = removed_words.iter().all(|word| word.chars().count() < 5 && !copy_words.contains(word) && !copy_pieces.contains(word));
        function_word || inside
    }

    #[test]
    fn nothing_follow_up_8_cleared_is_kept_now() {
        // Audit (follow-up 9, coverage): every copy follow-up 8's matcher cleared is cleared
        // still, but the function words R6b-52 and the short words inside others R6b-59 leave
        // on purpose.
        let extra: &[(&str, &str, bool)] = &[
            ("Ani", "Daniel", false),
            ("pen", "pending", false),
            ("Ani", "ANI,", true),
            ("X-1", "Model X-1", true),
            ("SECRET one", "the secret one clause", true),
            ("Wijaya", "wijaya-corp", true),
            ("O'Neil", "o'neil", true),
            ("Rp 50.000", "Rp 50.000", true),
        ];
        for (removed, copy, _) in CASES.iter().chain(extra) {
            if follow_up_8_holds(removed, copy) && !deliberately_left(removed, copy) {
                assert!(Removed::from_text(removed).held_in(copy), "follow-up 8 cleared {copy:?} for {removed:?}; it is kept now");
            }
        }
        for (removed, copy, held) in extra {
            assert_eq!(Removed::from_text(removed).held_in(copy), *held, "{removed:?} in {copy:?}");
        }
    }

    /// A small deterministic generator (xorshift), so the property tests repeat exactly.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len())]
        }
    }

    /// How a candidate reads, as `holds` takes it: read alike and holding a removed word, or not
    /// read alike (cleared either way).
    fn cleared(bytes: &[u8], removed: &Removed) -> bool {
        match read_text(bytes) {
            Reading::Read(text) => removed.held_in(&text),
            Reading::Unread => true,
        }
    }

    fn encode(text: &str, how: usize) -> Option<Vec<u8>> {
        match how {
            0 => Some([0xFE, 0xFF].into_iter().chain(text.encode_utf16().flat_map(u16::to_be_bytes)).collect()),
            1 => Some([0xEF, 0xBB, 0xBF].into_iter().chain(text.bytes()).collect()),
            // PDFDocEncoding, where every character is one of its printable Latin-1 ones.
            _ => text
                .chars()
                .map(|c| u8::try_from(u32::from(c)).ok().filter(|b| (0x20..0x7F).contains(b) || (0xA1..=0xFF).contains(b) && *b != 0xAD))
                .collect(),
        }
    }

    #[test]
    fn a_copy_holding_the_token_in_any_form_is_cleared() {
        // Final audit (property): for random tokens, whatever form the copy writes them in (case,
        // full-width, decomposed, a ligature, a final sigma, an invisible character inside) and
        // whatever encoding (UTF-16BE, UTF-8, PDFDocEncoding), with any separators around, the
        // copy is cleared.
        const LETTERS: [char; 34] = [
            'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'k', 'l', 'm', 'n', 'o', 'p', 'r', 's', 't', 'u', 'w', 'y', 'z', '\u{e9}', '\u{fc}',
            '\u{f1}', '\u{e7}', '\u{3b1}', '\u{3b2}', '\u{3b3}', '\u{3b4}', '\u{3bb}', '\u{3bc}', '\u{3c0}', '\u{3c3}',
        ];
        const SEPARATORS: [&str; 7] = [" ", "\t", ", ", " - ", "\u{a0}", "/", "\n"];
        let mut random = Random(0x9E37_79B9_7F4A_7C15);
        for _ in 0..4_000 {
            let length = 4 + random.below(6);
            let token: String = (0..length).map(|_| *random.pick(&LETTERS)).collect();
            let removed_form = if random.below(2) == 0 { token.clone() } else { token.to_uppercase() };
            let removed = Removed::from_text(&removed_form);
            let mut form = match random.below(6) {
                0 => token.clone(),
                1 => token.to_uppercase(),
                2 => token
                    .chars()
                    .map(|c| if c.is_ascii_lowercase() { char::from_u32(u32::from(c) - 0x61 + 0xFF41).unwrap_or(c) } else { c })
                    .collect(),
                3 => token.nfd().collect(),
                4 => token.replace("fi", "\u{fb01}"),
                _ => token.strip_suffix('\u{3c3}').map(|rest| format!("{rest}\u{3c2}")).unwrap_or_else(|| token.clone()),
            };
            if random.below(3) == 0 {
                let at = form.char_indices().map(|(at, _)| at).nth(1 + random.below(length - 1)).unwrap_or(form.len());
                form.insert(at, *random.pick(&['\u{ad}', '\u{200b}', '\u{2060}', '\u{feff}']));
            }
            let copy = format!("Clause{}{form}{}annex", random.pick(&SEPARATORS), random.pick(&SEPARATORS));
            for how in 0..3 {
                let Some(bytes) = encode(&copy, how) else { continue };
                assert!(cleared(&bytes, &removed), "{removed_form:?} in {copy:?} (encoding {how})");
            }
        }
    }

    #[test]
    fn a_string_the_readers_read_otherwise_or_not_at_all_is_cleared() {
        // Final audit (property): for random bytes (with or without a mark), a string pdf.js's
        // rules and the standard's read otherwise, or that either cannot read cleanly, is
        // cleared, whatever was removed.
        let nothing = Removed::from_text("");
        let mut random = Random(0xD1B5_4A32_D192_ED03);
        for _ in 0..20_000 {
            let mut bytes: Vec<u8> = match random.below(4) {
                0 => vec![0xFE, 0xFF],
                1 => vec![0xFF, 0xFE],
                2 => vec![0xEF, 0xBB, 0xBF],
                _ => Vec::new(),
            };
            let length = random.below(12);
            bytes.extend((0..length).map(|_| if random.below(3) == 0 { random.next() as u8 } else { b'a' + random.below(26) as u8 }));
            let (pdf_js, standard) = (as_pdf_js_reads(&bytes), as_the_standard_reads(&bytes));
            if pdf_js != standard
                || standard.contains('\u{fffd}')
                || standard.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r' | '\u{c}' | '\u{1b}'))
            {
                assert!(cleared(&bytes, &nothing), "{bytes:02X?}: {pdf_js:?} / {standard:?}");
            }
        }
    }

    #[test]
    fn the_matcher_is_pinned() {
        for (removed, copy, held) in CASES {
            assert_eq!(Removed::from_text(removed).held_in(copy), *held, "{removed:?} in {copy:?}");
        }
    }
}
