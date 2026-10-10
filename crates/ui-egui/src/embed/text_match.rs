//! Text matching for host jumps and marks (the embed API's rules): whitespace runs are one space,
//! Unicode compatibility forms are folded (NFKC), curly quotes and dashes straightened, case
//! folded. A match may span lines but never pages: each page is searched on its own.

use std::cell::RefCell;
use std::ops::Range;

use pdfcraft_render::PageText;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Longest needle searched, in folded characters; longer ones match nothing (the search is
/// quadratic in the worst case and runs on the frame).
pub const MAX_NEEDLE_CHARS: usize = 10_000;

/// Longest passage taken from the host, in UTF-8 bytes of its raw text, checked before anything is
/// folded: folding is one long, unsplittable pass, so a huge passage that is mostly whitespace (and
/// would fold to almost nothing) must not get that far. Longer ones match nothing.
pub const MAX_RAW_PASSAGE_BYTES: usize = 256 * 1024;

/// Whether the host's raw passage `text` is over [`MAX_RAW_PASSAGE_BYTES`] and so matches nothing.
pub fn exceeds_raw_cap(text: &str) -> bool {
    text.len() > MAX_RAW_PASSAGE_BYTES
}

/// A host passage as matching compares it, or `None` when it can never match: over the raw cap
/// (never folded), blank once folded, or over [`MAX_NEEDLE_CHARS`] folded. Marks take their
/// needles through here; a jump is refused by [`exceeds_raw_cap`] before it waits for any text.
pub fn fold_passage(text: &str) -> Option<Vec<char>> {
    if exceeds_raw_cap(text) {
        return None;
    }
    Some(fold(text)).filter(|needle| !needle.is_empty() && needle.len() <= MAX_NEEDLE_CHARS)
}

#[cfg(test)]
thread_local! {
    /// How many times [`fold`] ran on this thread, so a test can prove a passage was never folded.
    pub(crate) static FOLD_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `text` as matching compares it. It runs the same pipeline as the page side ([`fold_mapped`]), so a
/// needle and the page text can never be folded by different rules.
pub fn fold(text: &str) -> Vec<char> {
    #[cfg(test)]
    FOLD_CALLS.with(|calls| calls.set(calls.get() + 1));
    let chars: Vec<char> = text.chars().collect();
    let mut folded: Vec<char> = fold_mapped(&chars).into_iter().map(|(folded_char, _)| folded_char).collect();
    if folded.last() == Some(&' ') {
        folded.pop();
    }
    folded
}

/// Every match of each needle on `text`, as glyph ranges: `result[k]` belongs to `needles[k]`,
/// in page order, matches not overlapping.
pub fn find_all(text: &PageText, needles: &[Vec<char>]) -> Vec<Vec<Range<usize>>> {
    let counts = RefCell::new(Vec::with_capacity(needles.len()));
    let flat = text.find_with(|chars| {
        let hay = fold_mapped(chars);
        let mut all = Vec::new();
        for needle in needles {
            let found = matches(&hay, needle);
            // The only borrow of `counts` while `find_with` runs, so it can't fail.
            counts.borrow_mut().push(found.len());
            all.extend(found);
        }
        all
    });
    let mut flat = flat.into_iter();
    counts.into_inner().into_iter().map(|n| flat.by_ref().take(n).collect()).collect()
}

/// One character as matching compares it, before whitespace runs are collapsed.
fn fold_simple(c: char, out: &mut Vec<char>) {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => out.push('\''),
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => out.push('"'),
        '\u{2010}'..='\u{2015}' | '\u{2212}' => out.push('-'),
        c if c.is_whitespace() => out.push(' '),
        c => out.extend(c.to_lowercase()),
    }
}

/// The page's characters (as `PageText::find_with` hands them over) folded like [`fold`], each
/// with the index of the character it came from. A character and the combining marks after it
/// are normalised together, so a decomposed accent matches a composed one.
fn fold_mapped(chars: &[char]) -> Vec<(char, usize)> {
    let mut out: Vec<(char, usize)> = Vec::with_capacity(chars.len());
    let mut folded = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + 1..chars.len()).find(|&i| chars.get(i).is_some_and(|c| !is_combining_mark(*c))).unwrap_or(chars.len());
        folded.clear();
        for c in chars.get(start..end).unwrap_or_default().iter().copied().nfkc() {
            fold_simple(c, &mut folded);
        }
        for &c in &folded {
            if c != ' ' || out.last().is_some_and(|l| l.0 != ' ') {
                out.push((c, start));
            }
        }
        start = end;
    }
    out
}

/// Ranges in the page's characters of the non-overlapping matches of `needle` in `hay`.
fn matches(hay: &[(char, usize)], needle: &[char]) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    if needle.is_empty() || needle.len() > MAX_NEEDLE_CHARS {
        return found;
    }
    let mut at = 0;
    while let Some(window) = hay.get(at..at + needle.len()) {
        if window.iter().map(|w| w.0).eq(needle.iter().copied())
            && let (Some(first), Some(last)) = (window.first(), window.last())
        {
            found.push(first.1..last.1 + 1);
            at += needle.len();
        } else {
            at += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use pdfcraft_render::{PageText, TextGlyph};

    use super::*;

    /// A page whose lines are made of words, one glyph per character.
    fn page(lines: &[&str]) -> PageText {
        let mut text = PageText::default();
        for (line, words) in lines.iter().enumerate() {
            for (w, word) in words.split(' ').enumerate() {
                for (k, ch) in word.chars().enumerate() {
                    let x = text.glyphs.len() as f32 * 7.0;
                    let y = line as f32 * 20.0;
                    text.glyphs.push(TextGlyph { text: ch.to_string(), rect: [x, y, x + 6.0, y + 12.0] });
                    text.line_of.push(line as u32);
                    text.space_before.push(w > 0 && k == 0);
                }
            }
        }
        text
    }

    /// A single match; written out so clippy's one-element-range lint doesn't mistake it for a typo.
    fn one_match(start: usize, end: usize) -> Vec<Range<usize>> {
        Vec::from([Range { start, end }])
    }

    fn find(text: &PageText, needle: &str) -> Vec<Range<usize>> {
        find_all(text, &[fold(needle)]).into_iter().flatten().collect()
    }

    #[test]
    fn folding_matches_the_api_rules() {
        let folded: String = fold("  \u{201C}Quoted\u{201D}\tTEXT \u{2014}  \u{FB01}ne \u{2018}x\u{2019} ").into_iter().collect();
        assert_eq!(folded, "\"quoted\" text - fine 'x'");
    }

    #[test]
    fn matches_span_lines_and_report_glyph_ranges() {
        let text = page(&["The quick", "brown fox"]);
        assert_eq!(find(&text, "QUICK   brown"), one_match(3, 13));
        assert_eq!(find(&text, "fox"), one_match(13, 16));
    }

    #[test]
    fn ligatures_quotes_and_combining_marks_fold_like_the_needle() {
        assert_eq!(find(&page(&["\u{FB01}nal clause"]), "final"), one_match(0, 4));
        assert_eq!(find(&page(&["the \u{201C}Goods\u{201D} are"]), "\"goods\""), one_match(3, 10));
        assert_eq!(find(&page(&["e\u{301}tude"]), "\u{E9}tude"), one_match(0, 6));
    }

    #[test]
    fn every_match_is_listed_per_needle_in_page_order() {
        let text = page(&["goods and goods", "more GOODS"]);
        let found = find_all(&text, &[fold("goods"), fold("and"), fold("absent")]);
        assert_eq!(found, vec![vec![0..5, 8..13, 17..22], vec![5..8], vec![]]);
    }

    #[test]
    fn empty_blank_and_oversized_needles_match_nothing() {
        let text = page(&["anything at all"]);
        assert!(find(&text, "").is_empty());
        assert!(find(&text, "   \n\t").is_empty());
        assert!(find(&text, &"a".repeat(MAX_NEEDLE_CHARS + 1)).is_empty());
    }

    #[test]
    fn a_passage_over_the_raw_cap_is_never_folded() {
        let folds_before = FOLD_CALLS.with(std::cell::Cell::get);
        // 300 KiB that would fold to a single character, so only the raw check can refuse it cheaply.
        let mostly_blank = format!("x{}", " ".repeat(300 * 1024));
        assert!(exceeds_raw_cap(&mostly_blank));
        assert_eq!(fold_passage(&mostly_blank), None);
        assert_eq!(FOLD_CALLS.with(std::cell::Cell::get), folds_before, "refused before any folding");
    }

    #[test]
    fn a_passage_at_the_raw_cap_that_folds_short_still_works() {
        let padded = format!("The  Goods{}", " ".repeat(MAX_RAW_PASSAGE_BYTES - "The  Goods".len()));
        assert_eq!(padded.len(), MAX_RAW_PASSAGE_BYTES);
        assert!(!exceeds_raw_cap(&padded));
        assert_eq!(fold_passage(&padded).map(|n| n.into_iter().collect::<String>()), Some("the goods".to_string()));
        assert!(exceeds_raw_cap(&format!("{padded} ")), "one byte over is refused");
        // The folded-length and blank rules are unchanged.
        assert_eq!(fold_passage("   "), None);
        assert_eq!(fold_passage(&"a".repeat(MAX_NEEDLE_CHARS + 1)), None);
        assert_eq!(fold_passage(&"a".repeat(MAX_NEEDLE_CHARS)).map(|n| n.len()), Some(MAX_NEEDLE_CHARS));
    }

    #[test]
    fn fold_is_the_folded_characters_of_the_page_side_pipeline() {
        let sample = "  \u{201C}Quoted\u{201D}\u{00A0}\u{00A0}TEXT \u{2014}   \u{FB01}ne   \u{2018}x\u{2019}";
        let chars: Vec<char> = sample.chars().collect();
        let mut mapped: Vec<char> = fold_mapped(&chars).into_iter().map(|(folded_char, _)| folded_char).collect();
        if mapped.last() == Some(&' ') {
            mapped.pop();
        }
        assert_eq!(fold(sample), mapped);
        assert_eq!(fold(sample).into_iter().collect::<String>(), "\"quoted\" text - fine 'x'");
    }
}
