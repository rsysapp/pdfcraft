//! Follow-up 8 (R6b-51; the redaction re-review's C1): a content stream a rewrite brings (one
//! whose decoded data is no content stream of the base's) is written as the redactor writes:
//! drawing operators each with the operands its kind takes, in number and type, and nothing
//! else. A comment, an operator no viewer knows (or one inside `BX`/`EX`), an operand left over
//! or of another type, a `TJ` array holding anything but strings and numbers: each draws
//! nothing, yet carries what it holds to anyone reading the file, so each is refused. Marked
//! content (`BMC`, `BDC`, `MP`, `DP`) may only be what the base's content had: its tag, and
//! its property list (named, or written in place, but for its text for assistive technology,
//! which may only be taken away and is compared span by span elsewhere).

use pdfcraft_cos::{Dict, Lexer, Object};

use super::structure::{ContentBase, Digest, sha256};
use super::{Budget, Fail};

/// The operands an operator takes.
#[derive(Clone, Copy)]
enum Shape {
    /// This many numbers.
    Numbers(usize),
    /// One name.
    Name,
    /// A name and a number (`Tf`).
    NameNumber,
    /// One string (`Tj`, `'`).
    Text,
    /// Two numbers and a string (`"`).
    SpacedText,
    /// An array of strings and numbers (`TJ`).
    Shown,
    /// An array of numbers and a number (`d`).
    Dash,
    /// One to four numbers (`sc`, `SC`).
    Colour,
    /// Numbers then, or only, a name (`scn`, `SCN`).
    PatternColour,
    /// A tag (`BMC`, `MP`).
    Tag,
    /// A tag and a property list, named or written in place (`BDC`, `DP`).
    TagList,
}

fn shape(operator: &[u8]) -> Option<Shape> {
    Some(match operator {
        b"b" | b"B" | b"b*" | b"B*" | b"f" | b"F" | b"f*" | b"n" | b"s" | b"S" | b"h" | b"W" | b"W*" | b"q" | b"Q" | b"BT" | b"ET" | b"EMC"
        | b"T*" => Shape::Numbers(0),
        b"w" | b"J" | b"j" | b"M" | b"i" | b"Tc" | b"Tw" | b"Tz" | b"TL" | b"Tr" | b"Ts" | b"g" | b"G" => Shape::Numbers(1),
        b"m" | b"l" | b"Td" | b"TD" | b"d0" => Shape::Numbers(2),
        b"rg" | b"RG" => Shape::Numbers(3),
        b"re" | b"v" | b"y" | b"k" | b"K" => Shape::Numbers(4),
        b"c" | b"cm" | b"Tm" | b"d1" => Shape::Numbers(6),
        b"ri" | b"gs" | b"Do" | b"sh" | b"cs" | b"CS" => Shape::Name,
        b"Tf" => Shape::NameNumber,
        b"Tj" | b"'" => Shape::Text,
        b"\"" => Shape::SpacedText,
        b"TJ" => Shape::Shown,
        b"d" => Shape::Dash,
        b"sc" | b"SC" => Shape::Colour,
        b"scn" | b"SCN" => Shape::PatternColour,
        b"BMC" | b"MP" => Shape::Tag,
        b"BDC" | b"DP" => Shape::TagList,
        _ => return None,
    })
}

fn number(object: &Object) -> bool {
    matches!(object, Object::Int(_) | Object::Real(_))
}

fn fits(shape: Shape, operands: &[Object]) -> bool {
    match (shape, operands) {
        (Shape::Numbers(count), _) => operands.len() == count && operands.iter().all(number),
        (Shape::Name | Shape::Tag, [Object::Name(_)]) => true,
        (Shape::NameNumber, [Object::Name(_), size]) => number(size),
        (Shape::Text, [Object::String(_)]) => true,
        (Shape::SpacedText, [word, character, Object::String(_)]) => number(word) && number(character),
        (Shape::Shown, [Object::Array(items)]) => items.iter().all(|item| number(item) || matches!(item, Object::String(_))),
        (Shape::Dash, [Object::Array(items), phase]) => number(phase) && items.iter().all(number),
        (Shape::Colour, _) => (1..=4).contains(&operands.len()) && operands.iter().all(number),
        (Shape::PatternColour, [rest @ .., Object::Name(_)]) => rest.len() <= 4 && rest.iter().all(number),
        (Shape::PatternColour, _) => (1..=4).contains(&operands.len()) && operands.iter().all(number),
        (Shape::TagList, [Object::Name(_), Object::Name(_) | Object::Dict(_)]) => true,
        _ => false,
    }
}

/// The entries of a property list that stand for content (taken away by a redaction, compared
/// span by span elsewhere).
const TEXT_KEYS: [&[u8]; 5] = [b"ActualText", b"Alt", b"E", b"Lang", b"T"];

/// A marked-content operator by what it is: its keyword, its tag and its property list (a
/// name, or a dictionary without its text), as a digest.
fn marked(operator: &[u8], operands: &[Object]) -> Digest {
    let mut encoded = Vec::new();
    for operand in operands {
        let operand = match operand {
            Object::Dict(list) => Object::Dict(
                list.iter().filter(|(key, _)| !TEXT_KEYS.contains(&key.as_slice())).map(|(k, v)| (k.clone(), v.clone())).collect::<Dict>(),
            ),
            other => other.clone(),
        };
        pdfcraft_cos::serialize(&operand, &mut encoded);
        encoded.push(b' ');
    }
    sha256(&[b"marked", operator, &encoded])
}

fn refused(what: impl Into<String>) -> Fail {
    Fail::Read(format!("a content stream the rewrite brings holds {}, which draws nothing but carries data", what.into()))
}

/// An operator with its operands, as a digest.
fn whole(operator: &[u8], operands: &[Object]) -> Digest {
    let mut encoded = Vec::new();
    for operand in operands {
        pdfcraft_cos::serialize(operand, &mut encoded);
        encoded.push(b' ');
    }
    sha256(&[b"operator", operator, &encoded])
}

/// What a base's content stream holds that a rewrite may bring again: its marked-content
/// operators (see [`marked`]), its inline images' data, and every operator, whole (with its
/// operands). Read leniently: the base is read as it is.
pub(crate) fn gather(data: &[u8], base: &mut ContentBase, budget: &mut Budget) -> Result<(), Fail> {
    walk(data, budget, false, &mut |operator, operands| {
        if matches!(operator, b"BMC" | b"BDC" | b"MP" | b"DP") {
            base.marked.insert(marked(operator, operands));
        }
        if operator == b"BI" {
            if let Some(Object::String(image)) = operands.get(1) {
                base.inline.insert(sha256(&[b"inline", &image.bytes]));
            }
            return Ok(());
        }
        base.operators.insert(whole(operator, operands));
        Ok(())
    })
}

/// What a content stream a rewrite brings holds (see [`check`]).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Brought {
    /// It shows text (`Tj`, `TJ`, `'`, `"`): where no glyph is extracted (a tiling pattern's
    /// cell, a soft mask's group), nothing would compare it.
    pub shows_text: bool,
}

/// An inline image's dictionary entry as the redactor keeps one: a standard key holding a
/// name, a number, a boolean, an array of them, or decoding parameters of numbers. (Its data is
/// compared whole with the base's inline images.)
fn plain_image_entry(key: &[u8], value: &Object) -> bool {
    const KEYS: [&[u8]; 22] = [
        b"W",
        b"Width",
        b"H",
        b"Height",
        b"BPC",
        b"BitsPerComponent",
        b"CS",
        b"ColorSpace",
        b"D",
        b"Decode",
        b"DP",
        b"DecodeParms",
        b"F",
        b"Filter",
        b"I",
        b"Interpolate",
        b"IM",
        b"ImageMask",
        b"Intent",
        b"L",
        b"Length",
        b"SMaskInData",
    ];
    let plain = |value: &Object| matches!(value, Object::Int(_) | Object::Real(_) | Object::Name(_) | Object::Bool(_) | Object::Null);
    KEYS.contains(&key)
        && match value {
            Object::Array(items) => {
                items.iter().all(|item| plain(item) || matches!(item, Object::Dict(parameters) if parameters.iter().all(|(_, v)| plain(v))))
            }
            Object::Dict(parameters) => parameters.iter().all(|(_, v)| plain(v)),
            other => plain(other),
        }
}

/// Check a content stream a rewrite brings (see the module documentation) against what the
/// base's content streams hold (see [`gather`]): an operator written otherwise than the
/// redactor writes may only be one the base's content had, operands and all.
pub(crate) fn check(data: &[u8], base: &ContentBase, budget: &mut Budget) -> Result<Brought, Fail> {
    // The editor's own wrapper around a page's original content (`pdfcraft_edit`), whose
    // comment names the editor and nothing else.
    if matches!(data, b"q %PdfCraft\n" | b"Q %PdfCraft\n") {
        return Ok(Brought::default());
    }
    let mut brought = Brought::default();
    // Where in the redactor's clip (see [`CLIP`]) or box (`re`, then `f`) the operators are.
    let mut clip: Option<usize> = None;
    let mut boxed = false;
    walk(data, budget, true, &mut |operator, operands| {
        // Inside the redactor's clip, or after its box's `re`: exactly what comes next there.
        if let Some(at) = clip {
            if CLIP.get(at) != Some(&operator) || !shape(operator).is_some_and(|shape| fits(shape, operands)) || !plain_numbers(operands) {
                return Err(refused("path operators the redactor does not write"));
            }
            clip = (at + 1 < CLIP.len()).then_some(at + 1);
            return Ok(());
        }
        if boxed {
            boxed = false;
            if operator != b"f" || !operands.is_empty() {
                return Err(refused("a box the redactor does not write"));
            }
            return Ok(());
        }
        if operator == b"BI" {
            // An inline image: one of the base's, its dictionary holding nothing but how to
            // read it.
            let (Some(Object::Dict(dict)), Some(Object::String(image))) = (operands.first(), operands.get(1)) else {
                return Err(refused("an inline image that cannot be read"));
            };
            if let Some((key, _)) = dict.iter().find(|(key, value)| !plain_image_entry(key, value)) {
                return Err(refused(format!("an inline image's /{}", String::from_utf8_lossy(key))));
            }
            if !base.inline.contains(&sha256(&[b"inline", &image.bytes])) {
                return Err(refused("an inline image the base's content did not have"));
            }
            return Ok(());
        }
        brought.shows_text |= matches!(operator, b"Tj" | b"TJ" | b"'" | b"\"");
        // Marked content: as the base had it (its text may only be taken away, compared span by
        // span elsewhere).
        if matches!(operator, b"BMC" | b"BDC" | b"MP" | b"DP") {
            if !shape(operator).is_some_and(|shape| fits(shape, operands)) || !base.marked.contains(&marked(operator, operands)) {
                return Err(refused(format!("marked content (`{}`) the base's content did not have", String::from_utf8_lossy(operator))));
            }
            return Ok(());
        }
        // An operator the base's content had, operands and all.
        if base.operators.contains(&whole(operator, operands)) {
            return Ok(());
        }
        // Else one the redactor writes (follow-up 8 audit, operators): its own kinds, exactly
        // their operands, numbers as it writes them (to four places), a `TJ` never holding two
        // numbers in a row (it adds up displacements), `Do` naming its own copies. Nothing
        // else can carry data in numbers or names.
        if !REDACTOR_WRITES.contains(&operator) {
            return Err(match shape(operator) {
                None => refused(format!("the operator `{}`", String::from_utf8_lossy(operator))),
                Some(_) => refused(format!(
                    "`{}`, which the redactor does not write, with operands the base's content did not have",
                    String::from_utf8_lossy(operator)
                )),
            });
        }
        if !shape(operator).is_some_and(|shape| fits(shape, operands)) {
            return Err(refused(format!("operands `{}` does not take", String::from_utf8_lossy(operator))));
        }
        let numbers_plain = plain_numbers(operands);
        // A path the redactor writes: its clip (a quadrilateral around, one inside, even-odd,
        // per mark), or a box; never a path point of its own elsewhere.
        match operator {
            b"m" => {
                if !numbers_plain || !shape(operator).is_some_and(|shape| fits(shape, operands)) {
                    return Err(refused("path operators the redactor does not write"));
                }
                clip = Some(1);
                return Ok(());
            }
            b"l" | b"h" | b"W*" => return Err(refused("path operators the redactor does not write")),
            b"re" => boxed = true,
            _ => {}
        }
        let displacements_apart = match operands.first() {
            Some(Object::Array(items)) if operator == b"TJ" => !items.windows(2).any(|pair| number(&pair[0]) && number(&pair[1])),
            _ => true,
        };
        let own_copy = operator != b"Do"
            || operands.first().and_then(Object::as_name).is_some_and(|name| {
                name.strip_prefix(b"PCRedacted")
                    .is_some_and(|digits| !digits.is_empty() && digits.len() <= 9 && digits.iter().all(u8::is_ascii_digit))
            });
        if !(numbers_plain && displacements_apart && own_copy) {
            return Err(refused(format!("operands of `{}` the redactor does not write", String::from_utf8_lossy(operator))));
        }
        Ok(())
    })?;
    if clip.is_some() || boxed {
        return Err(refused("a path the redactor does not finish"));
    }
    Ok(brought)
}

/// Follow-up 9 (R6b-55, P6-33): whether `data` only constructs and paints paths, sets colours,
/// line styles and graphics states (`m l c v y h re`, `S s f F f* B B* b b* n`, `g G rg RG k K
/// cs CS sc SC scn SCN`, `w J j M d`, `q Q cm gs`), each with the operands its kind takes, and
/// holds nothing else (no comment, text, image, form, shading or marked content): a redaction
/// mark's plain appearance, by the host's rule.
pub(crate) fn plain_drawing(data: &[u8], budget: &mut Budget) -> Result<bool, Fail> {
    const PLAIN: [&[u8]; 38] = [
        b"m", b"l", b"c", b"v", b"y", b"h", b"re", b"S", b"s", b"f", b"F", b"f*", b"B", b"B*", b"b", b"b*", b"n", b"g", b"G", b"rg", b"RG", b"k",
        b"K", b"cs", b"CS", b"sc", b"SC", b"scn", b"SCN", b"w", b"J", b"j", b"M", b"d", b"q", b"Q", b"cm", b"gs",
    ];
    let result = walk(data, budget, true, &mut |operator, operands| {
        if PLAIN.contains(&operator) && shape(operator).is_some_and(|shape| fits(shape, operands)) {
            Ok(())
        } else {
            Err(refused("an operator a plain appearance does not hold"))
        }
    });
    match result {
        Ok(()) => Ok(true),
        Err(Fail::Read(_)) => Ok(false),
        Err(other) => Err(other),
    }
}

/// Follow-up 8 audit (operators): a content stream as the content serializer writes it
/// (`pdfcraft_content::write_op`), from its operators (a comment, or bytes no operator reads,
/// left out): what `apply_marks` writes for every content stream it brings, and what its
/// self-check finds them to be, byte for byte.
pub(crate) fn canonical(data: &[u8], budget: &mut Budget) -> Result<Vec<u8>, Fail> {
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    walk(data, budget, false, &mut |operator, operands| {
        write_canonical(operator, operands, &mut out);
        Ok(())
    })?;
    Ok(out)
}

/// One operator as the content serializer writes it: each operand and a space, the keyword, a
/// line end; an inline image as `BI`, its entries, ` ID `, its data, a line end and `EI`.
fn write_canonical(operator: &[u8], operands: &[Object], out: &mut Vec<u8>) {
    if operator == b"BI" {
        if let (Some(Object::Dict(dict)), Some(Object::String(image))) = (operands.first(), operands.get(1)) {
            out.extend_from_slice(b"BI");
            for (key, value) in dict.iter() {
                out.push(b' ');
                pdfcraft_cos::serialize(&Object::Name(key.clone()), out);
                out.push(b' ');
                pdfcraft_cos::serialize(value, out);
            }
            out.extend_from_slice(b" ID ");
            out.extend_from_slice(&image.bytes);
            out.extend_from_slice(b"\nEI\n");
        }
        return;
    }
    for operand in operands {
        pdfcraft_cos::serialize(operand, out);
        out.push(b' ');
    }
    out.extend_from_slice(operator);
    out.push(b'\n');
}

/// The clip the redactor writes around each mark (`pdfcraft_redact`'s `clip_out`).
const CLIP: [&[u8]; 12] = [b"m", b"l", b"l", b"l", b"h", b"m", b"l", b"l", b"l", b"h", b"W*", b"n"];

/// Whether every number among `operands` (and in their arrays) is written as the redactor
/// writes numbers: to four places.
fn plain_numbers(operands: &[Object]) -> bool {
    let four_places = |value: &Object| match value {
        Object::Real(real) => {
            let scaled = real * 10_000.0;
            real.is_finite() && (scaled - scaled.round()).abs() < 1e-6
        }
        _ => true,
    };
    operands.iter().all(|operand| match operand {
        Object::Array(items) => items.iter().all(four_places),
        other => four_places(other),
    })
}

/// The operators the redactor writes of its own (`pdfcraft_redact`): the state around what it
/// clips (`q`, `Q`), the clip that leaves the marks out (`m`, `l`, `h`, `W*`, `n`), the boxes
/// (`g`, `rg`, `re`, `f`), what it keeps of a line of text (`TJ`, and `Tw`, `Tc`, `T*` for `'`
/// and `"`, `Tf` naming a font the content still shows text with in place of one that shows
/// nothing any more), and its copies of forms and images (`Do`).
const REDACTOR_WRITES: [&[u8]; 17] = [b"q", b"Q", b"m", b"l", b"h", b"W*", b"n", b"g", b"rg", b"re", b"f", b"TJ", b"Tw", b"Tc", b"T*", b"Tf", b"Do"];

/// What [`walk`] calls with each operator and its operands.
type Each<'a> = dyn FnMut(&[u8], &[Object]) -> Result<(), Fail> + 'a;

/// Read a content stream operator by operator, calling `each` with each operator and its
/// operands. `strict`: a comment or operands no operator takes is refused (else skipped).
/// Inline images (`BI` … `ID` data `EI`) are passed over whole.
fn walk(data: &[u8], budget: &mut Budget, strict: bool, each: &mut Each<'_>) -> Result<(), Fail> {
    let mut lexer = Lexer::new(data, 0);
    let mut operands: Vec<Object> = Vec::new();
    loop {
        budget.tick()?;
        // Whitespace, and a comment, which a strict reading refuses.
        let mut pos = lexer.pos;
        while let Some(&byte) = data.get(pos) {
            match byte {
                b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0' => pos += 1,
                b'%' if strict => return Err(refused(format!("a comment at byte {pos}"))),
                b'%' => pos += data.get(pos..).map_or(0, |rest| rest.iter().position(|byte| matches!(byte, b'\n' | b'\r')).unwrap_or(rest.len())),
                _ => break,
            }
        }
        lexer.pos = pos;
        let Some(&byte) = data.get(pos) else { break };
        if matches!(byte, b'/' | b'(' | b'<' | b'[' | b'+' | b'-' | b'.') || byte.is_ascii_digit() {
            let operand = match lexer.object() {
                Ok(operand) if lexer.pos > pos => operand,
                _ if strict => return Err(refused(format!("an operand that cannot be read at byte {pos}"))),
                // Read leniently: past the byte.
                _ => {
                    lexer.pos = pos.saturating_add(1);
                    operands.clear();
                    continue;
                }
            };
            if matches!(operand, Object::Ref(_)) {
                return Err(refused("a reference"));
            }
            operands.push(operand);
            continue;
        }
        let keyword = lexer.token();
        if keyword.is_empty() {
            if !strict {
                lexer.pos = pos.saturating_add(1);
                operands.clear();
                continue;
            }
            return Err(refused(format!("a stray byte at {pos}")));
        }
        let keyword = keyword.to_vec();
        if matches!(keyword.as_slice(), b"true" | b"false" | b"null") {
            operands.push(Object::Null);
            continue;
        }
        if keyword == b"BI" {
            if strict && !operands.is_empty() {
                return Err(refused("operands before an inline image"));
            }
            match inline_image(data, lexer.pos) {
                Some((end, dict, image)) => {
                    lexer.pos = end;
                    let image = data.get(image).unwrap_or_default().to_vec();
                    each(b"BI", &[Object::Dict(dict), Object::String(pdfcraft_cos::PdfString { bytes: image, hex: false })])?;
                }
                None if strict => return Err(refused("an inline image that does not end")),
                None => break,
            }
            operands.clear();
            continue;
        }
        each(&keyword, &operands)?;
        operands.clear();
    }
    if strict && !operands.is_empty() {
        return Err(refused("operands after the last operator"));
    }
    Ok(())
}

/// Where an inline image's data ends (after `EI`), from just after `BI`: its dictionary read
/// up to `ID`; where its size, colour space and depth give its data's length (no filter), that
/// many bytes after the one whitespace byte, then whitespace and `EI`; otherwise as the
/// structure scan finds it (whitespace, `EI`, then whitespace, a delimiter or the end).
fn inline_image(data: &[u8], from: usize) -> Option<(usize, Dict, std::ops::Range<usize>)> {
    let mut lexer = Lexer::new(data, from);
    let mut dict = Dict::new();
    loop {
        lexer.skip_ws();
        if lexer.eat_keyword(b"ID") {
            break;
        }
        let Ok(Object::Name(key)) = lexer.object() else { return None };
        let value = lexer.object().ok()?;
        dict.set(key, value);
    }
    let begin = lexer.pos.checked_add(1)?;
    let int = |a: &[u8], b: &[u8]| dict.get(a).or_else(|| dict.get(b)).and_then(Object::as_int).and_then(|v| usize::try_from(v).ok());
    let components = match dict.get(b"CS").or_else(|| dict.get(b"ColorSpace")).and_then(Object::as_name) {
        Some(b"G" | b"DeviceGray") => Some(1),
        Some(b"RGB" | b"DeviceRGB") => Some(3),
        Some(b"CMYK" | b"DeviceCMYK") => Some(4),
        _ => None,
    };
    let unfiltered = dict.get(b"F").is_none() && dict.get(b"Filter").is_none();
    if let (true, Some(width), Some(height), Some(depth), Some(components)) =
        (unfiltered, int(b"W", b"Width"), int(b"H", b"Height"), int(b"BPC", b"BitsPerComponent"), components)
    {
        let row = width.checked_mul(components)?.checked_mul(depth)?.div_ceil(8);
        let end = begin.checked_add(row.checked_mul(height)?)?;
        let rest = data.get(end..)?;
        let ei = rest.iter().position(|byte| !byte.is_ascii_whitespace())?;
        if rest.get(ei..ei.checked_add(2)?) == Some(b"EI") {
            return Some((end.checked_add(ei)?.checked_add(2)?, dict, begin..end));
        }
    }
    let mut at = begin;
    loop {
        let (&e, &i) = (data.get(at)?, data.get(at.checked_add(1)?)?);
        let before = at == begin || data.get(at.wrapping_sub(1)).is_some_and(|byte| byte.is_ascii_whitespace());
        let after = data.get(at.checked_add(2)?).is_none_or(|byte| byte.is_ascii_whitespace() || b"()<>[]{}/%".contains(byte));
        if e == b'E' && i == b'I' && before && after {
            return Some((at.checked_add(2)?, dict, begin..at));
        }
        at = at.checked_add(1)?;
    }
}
