//! Embed build (follow-up 7 audit, item 4): a rewrite holds no bytes but those its
//! cross-reference data lists. An object definition the cross-reference data does not list (a
//! second definition, one marked free, one written after the last `startxref`), or a comment,
//! is invisible to every viewer and every fact, yet any tool reading the file finds it: it could
//! carry what the rewrite took away. So every byte of a rewrite must belong to: the header (the
//! `%PDF-` line and at most one short binary-marker comment), a listed object (`N G obj …
//! endobj`, a stream's data as long as its listed object's), its one cross-reference section
//! (a table and its trailer, or a cross-reference stream object), `startxref`, its offset and
//! `%%EOF`, or whitespace between them. A comment anywhere else, outside strings and stream
//! data, is refused too.

use super::{Budget, Fail};

/// Where a listed object starts, and its stream's data length (`None`: not a stream).
pub(crate) struct ListedObject {
    pub offset: usize,
    pub stream_length: Option<usize>,
}

/// The longest binary-marker comment after the `%PDF-` line.
const MAX_MARKER: usize = 32;

fn refused(what: impl Into<String>) -> Fail {
    Fail::Read(format!("the rewrite holds bytes no cross-reference data lists: {}", what.into()))
}

fn whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0')
}

fn delimiter(byte: u8) -> bool {
    matches!(byte, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

/// Skip whitespace (a comment is refused).
fn skip_whitespace(data: &[u8], mut pos: usize) -> Result<usize, Fail> {
    while let Some(&byte) = data.get(pos) {
        if byte == b'%' {
            return Err(refused(format!("a comment at byte {pos}")));
        }
        if !whitespace(byte) {
            break;
        }
        pos = pos.saturating_add(1);
    }
    Ok(pos)
}

/// The regular characters from `pos` (a keyword, number or a name's rest).
fn token(data: &[u8], pos: usize) -> &[u8] {
    let end =
        data.get(pos..).map_or(pos, |rest| pos.saturating_add(rest.iter().take_while(|byte| !whitespace(**byte) && !delimiter(**byte)).count()));
    data.get(pos..end).unwrap_or_default()
}

/// Past a literal string opening at `pos` (`(`), nested parentheses and escapes included.
fn skip_literal(data: &[u8], pos: usize) -> Result<usize, Fail> {
    let mut depth = 0usize;
    let mut at = pos;
    while let Some(&byte) = data.get(at) {
        match byte {
            b'\\' => at = at.saturating_add(1),
            b'(' => depth = depth.saturating_add(1),
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(at.saturating_add(1));
                }
            }
            _ => {}
        }
        at = at.saturating_add(1);
    }
    Err(refused(format!("a string at byte {pos} that does not end")))
}

/// Follow-up 8 (R6b-51, I3): the number of the object whose header (`N G obj`, after
/// whitespace) starts at `offset`, read without loading anything: `None` when none does.
pub(crate) fn object_header(data: &[u8], offset: usize) -> Option<u32> {
    let mut pos = offset;
    let mut number = None;
    for expected in [None, None, Some(&b"obj"[..])] {
        pos = data.get(pos..).map_or(pos, |rest| pos.saturating_add(rest.iter().take_while(|byte| whitespace(**byte)).count()));
        let word = token(data, pos);
        let fits = match expected {
            None => !word.is_empty() && word.len() <= 10 && word.iter().all(u8::is_ascii_digit),
            Some(keyword) => word == keyword,
        };
        if !fits {
            return None;
        }
        if number.is_none() {
            number = std::str::from_utf8(word).ok().and_then(|digits| digits.parse::<u32>().ok());
            number?;
        }
        pos = pos.saturating_add(word.len());
    }
    number
}

/// The end (past `endobj`) of the object whose header starts at `listed.offset`.
fn object_end(data: &[u8], listed: &ListedObject, budget: &mut Budget) -> Result<usize, Fail> {
    let start = listed.offset;
    let mut pos = start;
    for expected in [None, None, Some(&b"obj"[..])] {
        pos = skip_whitespace(data, pos)?;
        let word = token(data, pos);
        let fits = match expected {
            None => !word.is_empty() && word.iter().all(u8::is_ascii_digit),
            Some(keyword) => word == keyword,
        };
        if !fits {
            return Err(refused(format!("no object header at byte {start}")));
        }
        pos = pos.saturating_add(word.len());
    }
    loop {
        budget.tick()?;
        pos = skip_whitespace(data, pos)?;
        let Some(&byte) = data.get(pos) else { return Err(refused(format!("the object at byte {start} does not end"))) };
        match byte {
            b'(' => pos = skip_literal(data, pos)?,
            b'<' if data.get(pos.saturating_add(1)) == Some(&b'<') => pos = pos.saturating_add(2),
            b'>' if data.get(pos.saturating_add(1)) == Some(&b'>') => pos = pos.saturating_add(2),
            b'<' => {
                let close = data.get(pos..).and_then(|rest| rest.iter().position(|byte| *byte == b'>'));
                let Some(close) = close else { return Err(refused(format!("a string at byte {pos} that does not end"))) };
                pos = pos.saturating_add(close).saturating_add(1);
            }
            b'[' | b']' | b'{' | b'}' => pos = pos.saturating_add(1),
            b'/' => pos = pos.saturating_add(1).saturating_add(token(data, pos.saturating_add(1)).len()),
            b')' | b'>' => return Err(refused(format!("a stray delimiter at byte {pos}"))),
            _ => {
                let word = token(data, pos);
                if word.is_empty() {
                    return Err(refused(format!("a stray byte at {pos}")));
                }
                pos = pos.saturating_add(word.len());
                match word {
                    b"endobj" => return Ok(pos),
                    b"stream" => {
                        let Some(length) = listed.stream_length else {
                            return Err(refused(format!("stream data in a non-stream object at byte {start}")));
                        };
                        pos = match (data.get(pos), data.get(pos.saturating_add(1))) {
                            (Some(b'\r'), Some(b'\n')) => pos.saturating_add(2),
                            (Some(b'\n' | b'\r'), _) => pos.saturating_add(1),
                            _ => return Err(refused(format!("no end of line after `stream` at byte {pos}"))),
                        };
                        pos = pos
                            .checked_add(length)
                            .filter(|end| *end <= data.len())
                            .ok_or_else(|| refused(format!("stream data past the file at byte {pos}")))?;
                        pos = skip_whitespace(data, pos)?;
                        if token(data, pos) != b"endstream" {
                            return Err(refused(format!("bytes between a stream's data and `endstream` at byte {pos}")));
                        }
                        pos = pos.saturating_add(b"endstream".len());
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The end of a cross-reference table at `start` (`xref`, its rows and `trailer` with its
/// dictionary).
fn table_end(data: &[u8], start: usize, budget: &mut Budget) -> Result<usize, Fail> {
    let mut pos = start;
    if token(data, pos) != b"xref" {
        return Err(refused(format!("no cross-reference table at byte {start}")));
    }
    pos = pos.saturating_add(4);
    loop {
        budget.tick()?;
        pos = skip_whitespace(data, pos)?;
        let word = token(data, pos);
        if word == b"trailer" {
            pos = pos.saturating_add(word.len());
            break;
        }
        if word.is_empty() || !word.iter().all(|byte| byte.is_ascii_digit() || matches!(byte, b'f' | b'n')) {
            return Err(refused(format!("a stray byte in the cross-reference table at byte {pos}")));
        }
        pos = pos.saturating_add(word.len());
    }
    // The trailer dictionary: scanned as an object's value, up to its closing `>>`.
    pos = skip_whitespace(data, pos)?;
    if data.get(pos..pos.saturating_add(2)) != Some(b"<<") {
        return Err(refused(format!("no trailer dictionary at byte {pos}")));
    }
    let mut depth = 0usize;
    loop {
        budget.tick()?;
        pos = skip_whitespace(data, pos)?;
        let Some(&byte) = data.get(pos) else { return Err(refused("the trailer does not end")) };
        match byte {
            b'<' if data.get(pos.saturating_add(1)) == Some(&b'<') => {
                depth = depth.saturating_add(1);
                pos = pos.saturating_add(2);
            }
            b'>' if data.get(pos.saturating_add(1)) == Some(&b'>') => {
                depth = depth.saturating_sub(1);
                pos = pos.saturating_add(2);
                if depth == 0 {
                    return Ok(pos);
                }
            }
            b'(' => pos = skip_literal(data, pos)?,
            b'<' => {
                let close = data.get(pos..).and_then(|rest| rest.iter().position(|byte| *byte == b'>'));
                let Some(close) = close else { return Err(refused("a trailer string that does not end")) };
                pos = pos.saturating_add(close).saturating_add(1);
            }
            b'[' | b']' => pos = pos.saturating_add(1),
            b'/' => pos = pos.saturating_add(1).saturating_add(token(data, pos.saturating_add(1)).len()),
            _ => {
                let word = token(data, pos);
                if word.is_empty() {
                    return Err(refused(format!("a stray byte in the trailer at {pos}")));
                }
                pos = pos.saturating_add(word.len());
            }
        }
    }
}

/// Check that every byte of `data` (a rewrite) is accounted for (see the module documentation).
/// `objects`: every object its cross-reference data lists in the file; `section`: its
/// cross-reference section's offset, and whether it is a stream (an object among `objects`).
pub(crate) fn check(data: &[u8], objects: &[ListedObject], section: (usize, bool), budget: &mut Budget) -> Result<(), Fail> {
    // The header.
    let mut covered: Vec<(usize, usize)> = Vec::with_capacity(objects.len().saturating_add(3));
    let first_line = data.iter().position(|byte| matches!(byte, b'\n' | b'\r')).unwrap_or(data.len());
    if !data.starts_with(b"%PDF-") {
        return Err(refused("no `%PDF-` line first"));
    }
    let mut header_end = first_line;
    let after = data
        .get(first_line..)
        .map_or(first_line, |rest| first_line.saturating_add(rest.iter().take_while(|byte| matches!(byte, b'\n' | b'\r')).count()));
    if data.get(after) == Some(&b'%') {
        let marker = data.get(after..).map_or(0, |rest| rest.iter().position(|byte| matches!(byte, b'\n' | b'\r')).unwrap_or(rest.len()));
        if marker > MAX_MARKER {
            return Err(refused("a long comment after the header"));
        }
        header_end = after.saturating_add(marker);
    }
    covered.push((0, header_end));
    for listed in objects {
        budget.tick()?;
        covered.push((listed.offset, object_end(data, listed, budget)?));
    }
    let (section_offset, is_stream) = section;
    let tail_start = if is_stream {
        covered
            .iter()
            .find(|(start, _)| *start == section_offset)
            .map(|(_, end)| *end)
            .ok_or_else(|| refused("the cross-reference stream is not listed"))?
    } else {
        let end = table_end(data, section_offset, budget)?;
        covered.push((section_offset, end));
        end
    };
    // `startxref`, its offset and `%%EOF`, after the section, then nothing but whitespace.
    let mut pos = skip_whitespace(data, tail_start)?;
    if token(data, pos) != b"startxref" {
        return Err(refused(format!("no `startxref` right after the cross-reference section, at byte {pos}")));
    }
    pos = skip_whitespace(data, pos.saturating_add(9))?;
    let offset = token(data, pos);
    if offset.is_empty() || !offset.iter().all(u8::is_ascii_digit) {
        return Err(refused("no offset after `startxref`"));
    }
    pos = pos.saturating_add(offset.len());
    pos = data.get(pos..).map_or(pos, |rest| pos.saturating_add(rest.iter().take_while(|byte| whitespace(**byte)).count()));
    if data.get(pos..pos.saturating_add(5)) != Some(b"%%EOF") {
        return Err(refused("no `%%EOF` after `startxref`"));
    }
    pos = pos.saturating_add(5);
    if data.get(pos..).is_some_and(|rest| rest.iter().any(|byte| !whitespace(*byte))) {
        return Err(refused(format!("bytes after `%%EOF`, at byte {pos}")));
    }
    covered.push((tail_start, pos));
    // Everything between the spans is whitespace, and no two overlap.
    covered.sort_unstable();
    let mut reached = 0usize;
    for (start, end) in covered {
        budget.tick()?;
        if start < reached {
            return Err(refused(format!("two listed parts overlap at byte {start}")));
        }
        let gap = data.get(reached..start).unwrap_or_default();
        if let Some(at) = gap.iter().position(|byte| !whitespace(*byte)) {
            return Err(refused(format!("bytes at {} that no listed part holds", reached.saturating_add(at))));
        }
        reached = end;
    }
    Ok(())
}
