//! Embed build (follow-up 7 audit, item 4; follow-up 8, R6b-51 I1): a rewrite, and each section
//! an appended update adds, holds no bytes but those its cross-reference data lists. An object
//! definition the cross-reference data does not list (a second definition, one marked free, one
//! written after the last `startxref`), a comment, a token after an object's value, an entry of
//! a dictionary a reader skips as junk or overwrites with a second one of the same key, are
//! invisible to every viewer and every fact, yet any tool reading the file finds them: they could
//! carry what a redaction took away. So every byte must belong to: the header (the `%PDF-x.y`
//! line, nothing else on it, and at most one short binary marker, `%` and high bytes), a listed
//! object (`N G obj`, one value written strictly, a stream's data as long as its listed
//! object's, `endobj`), a cross-reference section (a table and its trailer, or a cross-reference
//! stream object), `startxref` with that section's offset and `%%EOF`, or whitespace between
//! them. A value is written strictly: a dictionary holds names, each followed by one value and
//! none twice; a hex string holds hex digits; anything a reader would skip is refused. An
//! appended update is checked from the end of the base: the base's own bytes are its own.

use super::{Budget, Fail};

/// Where a listed object starts, and its stream's data length (`None`: not a stream).
pub(crate) struct ListedObject {
    pub offset: usize,
    pub stream_length: Option<usize>,
}

/// A cross-reference section: where it starts, and whether it is a stream (a listed object).
pub(crate) struct SectionAt {
    pub offset: usize,
    pub is_stream: bool,
}

/// The longest binary-marker comment after the `%PDF-` line.
const MAX_MARKER: usize = 32;
/// The deepest nesting of arrays and dictionaries in one value.
const MAX_DEPTH: usize = 256;

fn refused(what: impl Into<String>) -> Fail {
    Fail::Read(format!("the file holds bytes no cross-reference data lists: {}", what.into()))
}

fn whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0')
}

fn delimiter(byte: u8) -> bool {
    matches!(byte, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

/// Past whitespace from `pos`.
fn past_whitespace(data: &[u8], pos: usize) -> usize {
    data.get(pos..).map_or(pos, |rest| pos.saturating_add(rest.iter().take_while(|byte| whitespace(**byte)).count()))
}

/// Skip whitespace (a comment is refused).
fn skip_whitespace(data: &[u8], pos: usize) -> Result<usize, Fail> {
    let pos = past_whitespace(data, pos);
    if data.get(pos) == Some(&b'%') {
        return Err(refused(format!("a comment at byte {pos}")));
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
fn skip_literal(data: &[u8], pos: usize, budget: &mut Budget) -> Result<usize, Fail> {
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
        if at.is_multiple_of(4_096) {
            budget.tick()?;
        }
    }
    Err(refused(format!("a string at byte {pos} that does not end")))
}

/// A name's bytes as readers take them (`#xx` read as the byte), for telling keys apart.
fn decoded_name(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut at = 0;
    while let Some(&byte) = raw.get(at) {
        let hex = |offset: usize| raw.get(at.saturating_add(offset)).and_then(|digit| char::from(*digit).to_digit(16));
        match (byte, hex(1), hex(2)) {
            (b'#', Some(high), Some(low)) => {
                out.push(u8::try_from(high * 16 + low).unwrap_or(0));
                at = at.saturating_add(3);
            }
            _ => {
                out.push(byte);
                at = at.saturating_add(1);
            }
        }
    }
    out
}

/// Whether `word` is a number as PDF writes one.
fn number(word: &[u8]) -> bool {
    let digits = word.strip_prefix(b"+").or_else(|| word.strip_prefix(b"-")).unwrap_or(word);
    let dots = digits.iter().filter(|byte| **byte == b'.').count();
    !digits.is_empty() && dots <= 1 && digits.iter().any(u8::is_ascii_digit) && digits.iter().all(|byte| byte.is_ascii_digit() || *byte == b'.')
}

/// The keys of a dictionary, as readers take them, in order.
pub(crate) type Keys = Vec<Vec<u8>>;

/// One value written strictly from `pos` (after whitespace): its end, and for a dictionary its
/// keys.
pub(crate) fn value(data: &[u8], pos: usize, depth: usize, budget: &mut Budget) -> Result<(usize, Option<Keys>), Fail> {
    budget.tick()?;
    if depth > MAX_DEPTH {
        return Err(Fail::TooComplex);
    }
    let pos = skip_whitespace(data, pos)?;
    let Some(&byte) = data.get(pos) else { return Err(refused(format!("a value missing at byte {pos}"))) };
    match byte {
        b'<' if data.get(pos.saturating_add(1)) == Some(&b'<') => {
            let mut at = pos.saturating_add(2);
            let mut keys: Keys = Vec::new();
            // Looked up by hash: a dictionary of many keys costs no more than its length.
            let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
            loop {
                budget.tick()?;
                at = skip_whitespace(data, at)?;
                if data.get(at..at.saturating_add(2)) == Some(b">>") {
                    return Ok((at.saturating_add(2), Some(keys)));
                }
                if data.get(at) != Some(&b'/') {
                    return Err(refused(format!("a dictionary entry that is not a name at byte {at}")));
                }
                let raw = token(data, at.saturating_add(1));
                let key = decoded_name(raw);
                if !seen.insert(key.clone()) {
                    return Err(refused(format!("a dictionary naming a key twice at byte {at}")));
                }
                keys.push(key);
                at = value(data, at.saturating_add(1).saturating_add(raw.len()), depth.saturating_add(1), budget)?.0;
            }
        }
        b'[' => {
            let mut at = pos.saturating_add(1);
            loop {
                budget.tick()?;
                at = skip_whitespace(data, at)?;
                if data.get(at) == Some(&b']') {
                    return Ok((at.saturating_add(1), None));
                }
                at = value(data, at, depth.saturating_add(1), budget)?.0;
            }
        }
        b'(' => Ok((skip_literal(data, pos, budget)?, None)),
        b'<' => {
            let close = data.get(pos..).and_then(|rest| rest.iter().position(|byte| *byte == b'>'));
            let Some(close) = close else { return Err(refused(format!("a string at byte {pos} that does not end"))) };
            let inside = data.get(pos.saturating_add(1)..pos.saturating_add(close)).unwrap_or_default();
            if !inside.iter().all(|byte| byte.is_ascii_hexdigit() || whitespace(*byte)) {
                return Err(refused(format!("a hex string holding other bytes at byte {pos}")));
            }
            Ok((pos.saturating_add(close).saturating_add(1), None))
        }
        b'/' => Ok((pos.saturating_add(1).saturating_add(token(data, pos.saturating_add(1)).len()), None)),
        _ => {
            let word = token(data, pos);
            let end = pos.saturating_add(word.len());
            match word {
                b"true" | b"false" | b"null" => Ok((end, None)),
                _ if number(word) => {
                    // A reference: two unsigned integers and `R`.
                    if word.iter().all(u8::is_ascii_digit) {
                        let second_at = past_whitespace(data, end);
                        let second = token(data, second_at);
                        if second_at > end && !second.is_empty() && second.iter().all(u8::is_ascii_digit) {
                            let r_at = past_whitespace(data, second_at.saturating_add(second.len()));
                            if token(data, r_at) == b"R" {
                                return Ok((r_at.saturating_add(1), None));
                            }
                        }
                    }
                    Ok((end, None))
                }
                _ => Err(refused(format!("a stray token at byte {pos}"))),
            }
        }
    }
}

/// Follow-up 8 (R6b-51, I3): the number and generation of the object whose header (`N G obj`,
/// after whitespace) starts at `offset`, read without loading anything: `None` when none does.
pub(crate) fn object_header(data: &[u8], offset: usize) -> Option<(u32, u16)> {
    let mut pos = offset;
    let mut parts = [0u64; 2];
    for (index, expected) in [None, None, Some(&b"obj"[..])].into_iter().enumerate() {
        pos = past_whitespace(data, pos);
        let word = token(data, pos);
        match expected {
            None => {
                if word.is_empty() || word.len() > 10 || !word.iter().all(u8::is_ascii_digit) {
                    return None;
                }
                *parts.get_mut(index)? = std::str::from_utf8(word).ok()?.parse().ok()?;
            }
            Some(keyword) if word == keyword => {}
            Some(_) => return None,
        }
        pos = pos.saturating_add(word.len());
    }
    Some((u32::try_from(parts[0]).ok()?, u16::try_from(parts[1]).ok()?))
}

/// The end (past `endobj`) of the object whose header starts at `listed.offset`, and its keys
/// when it is a dictionary or stream.
fn object_end(data: &[u8], listed: &ListedObject, budget: &mut Budget) -> Result<(usize, Option<Keys>), Fail> {
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
    let (end, keys) = value(data, pos, 0, budget)?;
    pos = skip_whitespace(data, end)?;
    if token(data, pos) == b"stream" {
        let (Some(length), Some(_)) = (listed.stream_length, &keys) else {
            return Err(refused(format!("stream data in a non-stream object at byte {start}")));
        };
        pos = pos.saturating_add(b"stream".len());
        pos = match (data.get(pos), data.get(pos.saturating_add(1))) {
            (Some(b'\r'), Some(b'\n')) => pos.saturating_add(2),
            (Some(b'\n' | b'\r'), _) => pos.saturating_add(1),
            _ => return Err(refused(format!("no end of line after `stream` at byte {pos}"))),
        };
        pos = pos.checked_add(length).filter(|end| *end <= data.len()).ok_or_else(|| refused(format!("stream data past the file at byte {pos}")))?;
        pos = skip_whitespace(data, pos)?;
        if token(data, pos) != b"endstream" {
            return Err(refused(format!("bytes between a stream's data and `endstream` at byte {pos}")));
        }
        pos = skip_whitespace(data, pos.saturating_add(b"endstream".len()))?;
    } else if listed.stream_length.is_some() {
        return Err(refused(format!("a stream object without its data at byte {start}")));
    }
    if token(data, pos) != b"endobj" {
        return Err(refused(format!("a token after an object's value at byte {pos}")));
    }
    Ok((pos.saturating_add(b"endobj".len()), keys))
}

/// The end of a cross-reference table at `start` (`xref`, its rows and `trailer` with its
/// dictionary), and the trailer's keys.
fn table_end(data: &[u8], start: usize, budget: &mut Budget) -> Result<(usize, Keys), Fail> {
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
        let row = word.iter().all(u8::is_ascii_digit) || word == b"f" || word == b"n";
        if word.is_empty() || !row {
            return Err(refused(format!("a stray byte in the cross-reference table at byte {pos}")));
        }
        pos = pos.saturating_add(word.len());
    }
    let (end, keys) = value(data, pos, 0, budget)?;
    let Some(keys) = keys else { return Err(refused(format!("no trailer dictionary at byte {pos}"))) };
    Ok((end, keys))
}

/// Check that every byte of `data` from `from` (0: a rewrite, whose header is checked too; the
/// base's length: an appended update) is accounted for (see the module documentation).
/// `objects`: every object its cross-reference data lists from `from` on; `sections`: the
/// cross-reference sections from `from` on, each followed by `startxref`, its offset and
/// `%%EOF`. Returns each section's trailer keys (a table's trailer, a stream's dictionary), in
/// `sections`' order.
pub(crate) fn check(data: &[u8], from: usize, objects: &[ListedObject], sections: &[SectionAt], budget: &mut Budget) -> Result<Vec<Keys>, Fail> {
    // Every byte is looked at about once more.
    budget.hashed(data.len().saturating_sub(from))?;
    let mut covered: Vec<(usize, usize)> = Vec::with_capacity(objects.len().saturating_add(sections.len().saturating_mul(2)).saturating_add(1));
    if from == 0 {
        // `%PDF-x.y` alone on its line.
        let line = data.iter().position(|byte| matches!(byte, b'\n' | b'\r')).unwrap_or(data.len());
        let version = data.get(..line).and_then(|header| header.strip_prefix(b"%PDF-")).unwrap_or_default();
        if !matches!(version, [major, b'.', minor] if major.is_ascii_digit() && minor.is_ascii_digit()) {
            return Err(refused("a header other than `%PDF-x.y` alone on its line"));
        }
        let mut header_end = line;
        let after = data.get(line..).map_or(line, |rest| line.saturating_add(rest.iter().take_while(|byte| matches!(byte, b'\n' | b'\r')).count()));
        if data.get(after) == Some(&b'%') {
            let marker = data.get(after..).map_or(0, |rest| rest.iter().position(|byte| matches!(byte, b'\n' | b'\r')).unwrap_or(rest.len()));
            let high = data.get(after.saturating_add(1)..after.saturating_add(marker)).unwrap_or_default();
            if marker > MAX_MARKER || high.len() < 4 || high.iter().any(|byte| *byte < 0x80) {
                return Err(refused("a comment after the header other than a binary marker"));
            }
            header_end = after.saturating_add(marker);
        }
        covered.push((0, header_end));
    }
    // Each listed object's end and keys, by where it starts (a cross-reference stream's are
    // looked up there).
    let mut spans: std::collections::HashMap<usize, (usize, Option<Keys>)> = std::collections::HashMap::with_capacity(objects.len());
    for listed in objects {
        budget.tick()?;
        if listed.offset < from {
            return Err(refused(format!("an object listed at {} inside the base", listed.offset)));
        }
        let (end, keys) = object_end(data, listed, budget)?;
        covered.push((listed.offset, end));
        if listed.stream_length.is_some() {
            spans.insert(listed.offset, (end, keys));
        }
    }
    let mut trailers = Vec::with_capacity(sections.len());
    for section in sections {
        let (tail_start, keys) = if section.is_stream {
            let (end, keys) = spans.get(&section.offset).cloned().ok_or_else(|| refused("a cross-reference stream is not listed"))?;
            (end, keys.unwrap_or_default())
        } else {
            let (end, keys) = table_end(data, section.offset, budget)?;
            covered.push((section.offset, end));
            (end, keys)
        };
        trailers.push(keys);
        // `startxref`, this section's offset and `%%EOF`.
        let mut pos = skip_whitespace(data, tail_start)?;
        if token(data, pos) != b"startxref" {
            return Err(refused(format!("no `startxref` right after a cross-reference section, at byte {pos}")));
        }
        pos = skip_whitespace(data, pos.saturating_add(9))?;
        let offset = token(data, pos);
        if std::str::from_utf8(offset).ok().and_then(|digits| digits.parse::<usize>().ok()) != Some(section.offset) {
            return Err(refused("an offset after `startxref` other than its section's"));
        }
        pos = past_whitespace(data, pos.saturating_add(offset.len()));
        if data.get(pos..pos.saturating_add(5)) != Some(b"%%EOF") {
            return Err(refused("no `%%EOF` after `startxref`"));
        }
        covered.push((tail_start, pos.saturating_add(5)));
    }
    // Everything between the spans is whitespace, no two overlap, and nothing follows the last.
    covered.sort_unstable();
    let mut reached = from;
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
    if data.get(reached..).is_some_and(|rest| rest.iter().any(|byte| !whitespace(*byte))) {
        return Err(refused(format!("bytes after the last `%%EOF`, at byte {reached}")));
    }
    Ok(trailers)
}

/// Follow-up 8 (R6b-51, I1): an object stream's decoded data holds its header (`count` pairs
/// of numbers, nothing else) and then exactly the objects it lists, each one value written
/// strictly at its offset, with nothing but whitespace between them or after the last. Returns
/// the object numbers in their order.
pub(crate) fn object_stream(data: &[u8], count: usize, first: usize, budget: &mut Budget) -> Result<Vec<u32>, Fail> {
    let refuse = |what: &str| refused(format!("an object stream {what}"));
    let mut pos = 0usize;
    let mut pairs: Vec<(u32, usize)> = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        budget.tick()?;
        let mut pair = [0usize; 2];
        for slot in &mut pair {
            pos = skip_whitespace(data, pos)?;
            let word = token(data, pos);
            let digits = std::str::from_utf8(word).ok().filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()));
            *slot = digits.and_then(|digits| digits.parse().ok()).ok_or_else(|| refuse("header that is not pairs of numbers"))?;
            pos = pos.saturating_add(word.len());
        }
        pairs.push((u32::try_from(pair[0]).map_err(|_| refuse("numbering an object past any number"))?, pair[1]));
    }
    if data.get(pos..first).is_none_or(|gap| gap.iter().any(|byte| !whitespace(*byte))) {
        return Err(refuse("header followed by bytes before its first object"));
    }
    let mut reached = first;
    for (_, offset) in &pairs {
        let start = first.checked_add(*offset).ok_or_else(|| refuse("object past its data"))?;
        if start < reached || data.get(reached..start).is_none_or(|gap| gap.iter().any(|byte| !whitespace(*byte))) {
            return Err(refuse("holding bytes between its objects"));
        }
        reached = value(data, start, 0, budget)?.0;
    }
    if data.get(reached..).is_some_and(|rest| rest.iter().any(|byte| !whitespace(*byte))) {
        return Err(refuse("holding bytes after its last object"));
    }
    Ok(pairs.into_iter().map(|(num, _)| num).collect())
}
