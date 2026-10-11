//! Embed build: redaction and marked-content spans (R6b-47). A span (`/Tag << … >> BDC … EMC`)
//! can carry text standing for its content, `/ActualText`, `/Alt` and `/E`, which copying and
//! assistive technology read instead of the glyphs; once a redaction removed glyphs inside it,
//! that text would still spell out the removed words. So every span whose content the
//! redaction changed (the span itself or a span inside it) loses those entries.
//!
//! A span's content is compared operator by operator, before and after, with the spans of the
//! scope paired in their order. A list written in the content loses the text there; a list
//! named in the resources loses it where it is. Where marked content is unbalanced, every span
//! of the scope loses it. Where the redaction changed the spans themselves (fewer, more,
//! other tags or nesting), every span loses its text: what each stood for can no longer be
//! told.

use std::hash::{DefaultHasher, Hasher};

use pdfcraft_content::{Op, write_op};
use pdfcraft_cos::{CosError, Dict, Document, Object};

/// The entries that stand for a span's content.
const STANDS_FOR_CONTENT: [&[u8]; 3] = [b"ActualText", b"Alt", b"E"];

/// One level of marked content (`BDC`, `BMC`) in a scope's operators.
struct Level {
    tag: Vec<u8>,
    parent: Option<usize>,
    /// Where its `BDC` is: (piece, operator index); `None` for a `BMC`.
    at: Option<(usize, usize)>,
    /// What it holds, operator by operator (nested levels' included, not its own `BDC`/`EMC`).
    content: DefaultHasher,
}

/// The levels of `pieces` (one content stream in pieces), in the order they open, and whether
/// marked content is unbalanced (an `EMC` closing nothing, a level left open).
fn levels(pieces: &[&[Op]]) -> (Vec<Level>, bool) {
    let mut levels: Vec<Level> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut unbalanced = false;
    let mut bytes = Vec::new();
    for (piece, ops) in pieces.iter().enumerate() {
        for (index, op) in ops.iter().enumerate() {
            match op.op.as_slice() {
                b"BDC" | b"BMC" => {
                    let tag = op.operands.first().and_then(Object::as_name).map(<[u8]>::to_vec).unwrap_or_default();
                    let at = op.is("BDC").then_some((piece, index));
                    levels.push(Level { tag, parent: open.last().copied(), at, content: DefaultHasher::new() });
                    open.push(levels.len().saturating_sub(1));
                }
                b"EMC" => {
                    unbalanced |= open.pop().is_none();
                }
                _ => {
                    bytes.clear();
                    write_op(op, &mut bytes);
                    for level in &open {
                        if let Some(level) = levels.get_mut(*level) {
                            level.content.write(&bytes);
                            level.content.write_u8(b'\n');
                        }
                    }
                }
            }
        }
    }
    let unbalanced = unbalanced || !open.is_empty();
    (levels, unbalanced)
}

/// Take the text standing for their content off the spans of `rewritten` (the redaction's
/// output, per piece, with whether the piece changed) whose content differs from the same span
/// in `original`: a list written in the content loses it there (its piece marked changed); a
/// list named in `properties` (the scope's `/Properties`) is returned by name, for
/// [`strip_named`]. Returns how many spans lost text, and the names.
pub(crate) fn strip_changed(doc: &Document, original: &[Vec<Op>], rewritten: &mut [(Vec<Op>, bool)], properties: &Dict) -> (usize, Vec<Vec<u8>>) {
    let before: Vec<&[Op]> = original.iter().map(Vec::as_slice).collect();
    let after: Vec<&[Op]> = rewritten.iter().map(|(ops, _)| ops.as_slice()).collect();
    let ((before, unbalanced), (after, _)) = (levels(&before), levels(&after));
    // Unbalanced, which glyphs a span holds is each reader's guess: every span loses its text.
    let paired =
        !unbalanced && before.len() == after.len() && before.iter().zip(&after).all(|(old, new)| old.tag == new.tag && old.parent == new.parent);
    let changed: Vec<(usize, usize)> = after
        .iter()
        .enumerate()
        .filter(|(index, level)| !paired || before.get(*index).is_none_or(|old| old.content.finish() != level.content.finish()))
        .filter_map(|(_, level)| level.at)
        .collect();
    let mut stripped = 0;
    let mut names: Vec<Vec<u8>> = Vec::new();
    for (piece, index) in changed {
        let Some((ops, piece_changed)) = rewritten.get_mut(piece) else { continue };
        let Some(op) = ops.get_mut(index) else { continue };
        match op.operands.get_mut(1) {
            Some(Object::Dict(list)) if stands_for_content(list) => {
                strip(list);
                *piece_changed = true;
                stripped += 1;
            }
            Some(Object::Name(name)) => {
                let named = properties.get(name).map(|list| doc.resolve(list));
                if named.as_deref().and_then(Object::as_dict).is_some_and(stands_for_content) && !names.contains(name) {
                    names.push(name.clone());
                    stripped += 1;
                }
            }
            _ => {}
        }
    }
    (stripped, names)
}

fn stands_for_content(list: &Dict) -> bool {
    STANDS_FOR_CONTENT.iter().any(|key| list.contains(key))
}

fn strip(list: &mut Dict) {
    for key in STANDS_FOR_CONTENT {
        list.remove(key);
    }
}

/// Take the text standing for content off the property lists `names` of `resources`'
/// `/Properties`, where they are: an object of its own (shared with whatever else names it, which
/// then loses the text too), or written in `/Properties`. Returns the resources to write instead
/// when `/Properties` is written in them and changed.
pub(crate) fn strip_named(doc: &mut Document, resources: &Dict, names: &[Vec<u8>]) -> Result<Option<Dict>, CosError> {
    let (mut properties, own) = match resources.get(b"Properties") {
        Some(Object::Ref(r)) => match doc.get(*r).as_dict() {
            Some(properties) => (properties.clone(), Some(*r)),
            None => return Ok(None),
        },
        Some(Object::Dict(properties)) => (properties.clone(), None),
        _ => return Ok(None),
    };
    let mut written_here = false;
    for name in names {
        match properties.get_mut(name) {
            Some(Object::Ref(list)) => {
                let list = *list;
                doc.update_dict(list, strip)?;
            }
            Some(Object::Dict(list)) => {
                strip(list);
                written_here = true;
            }
            _ => {}
        }
    }
    if !written_here {
        return Ok(None);
    }
    match own {
        Some(r) => {
            doc.update_dict(r, |old| *old = properties)?;
            Ok(None)
        }
        None => {
            let mut resources = resources.clone();
            resources.set(b"Properties".to_vec(), Object::Dict(properties));
            Ok(Some(resources))
        }
    }
}
