//! Splitting the text of a page into sentence segments: pure functions (R-10, §4.16).
//!
//! A segment is the unit translated ahead of reading (TR-15): a range of the page's characters
//! and the text sent to the model. Segments do not overlap and come in reading order; the
//! characters between them (the spaces between sentences) belong to none.
//!
//! The text of the page is flattened first ([`Flat`]): the lines of a block are joined with a
//! space, a word broken by a hyphen at a line end is joined without it (the rule of search,
//! `search/normalize.rs`), and blocks are kept apart. Sentence borders are then found inside
//! each block; the end of a block always ends a segment. [`super::prompt::sentence_around`]
//! uses the same borders, so the sentence a word popup sends is the segment that holds the word.
//!
//! [`skip_reason`] says which segments are not worth translating (TR-18). When in doubt a
//! segment is translated: a needless translation costs less than a blank.

use std::ops::Range;

use crate::text::PageText;

/// The version of the splitting rules. Stored translations made under other rules are dropped
/// (`pretranslate.rs`), so this goes up whenever [`segments`] would split a page differently.
pub const SEGMENT_RULES_VERSION: u32 = 1;

/// A sentence longer than this many characters is split at a comma or a space.
pub const MAX_SEGMENT_CHARS: usize = 300;

/// A long sentence is cut at a comma only when the piece before it is at least this long;
/// otherwise at the last space.
const MIN_COMMA_PIECE: usize = 100;

/// Why a segment is not translated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SkipReason {
    /// Three characters or fewer.
    TooShort,
    /// No Latin letter: numbers and symbols only.
    NoLatin,
    /// More than half of the letters are Hangul: already Korean.
    Korean,
    /// A line of a table of contents: a run of leader dots and a page number.
    Leaders,
    /// Looks like source code (braces, semicolons, `->`, `::` ... make up much of it).
    Code,
    /// One word that looks like an identifier (`SWS_EM_01004`, `ara::exec::ExecutionClient`,
    /// `getValue`).
    Identifier,
}

/// One sentence (or piece of one) of a page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Characters of the page, from the first to one past the last character of the segment.
    pub chars: Range<u32>,
    /// The text sent to the model: lines joined with a space, hyphenated words joined, white
    /// space collapsed.
    pub text: String,
    /// `Some` when the segment is not worth translating.
    pub skip: Option<SkipReason>,
}

/// The sentence segments of `page`, in reading order, not overlapping.
pub fn segments(page: &PageText) -> Vec<Segment> {
    let flat = Flat::new(page);
    let mut out: Vec<Segment> = Vec::new();
    let mut last_end = 0u32;
    for span in flat.spans() {
        let Some(chars) = flat.page_range(span.clone()) else {
            continue;
        };
        // Lines in an unusual order could make ranges overlap; keep them apart.
        let chars = chars.start.max(last_end)..chars.end;
        if chars.is_empty() {
            continue;
        }
        let text = flat.text(span);
        if text.is_empty() {
            continue;
        }
        last_end = chars.end;
        let skip = skip_reason(&text);
        out.push(Segment { chars, text, skip });
    }
    out
}

/// The indices of the segments that share a character with `chars`. A range that starts or ends
/// in the space between two segments takes only the segments it touches; a range that touches
/// none gives an empty range (at the place where it would be).
pub fn covering(segments: &[Segment], chars: Range<u32>) -> Range<usize> {
    let first = segments.partition_point(|s| s.chars.end <= chars.start);
    if chars.is_empty() {
        return first..first;
    }
    let end = segments.partition_point(|s| s.chars.start < chars.end);
    first..end.max(first)
}

/// The hash of a segment's text that the store keeps (XXH3-64).
pub fn text_hash(text: &str) -> u64 {
    xxhash_rust::xxh3::xxh3_64(text.as_bytes())
}

/// The text of the segments around `chars`: those that share a character with it, or else the
/// first one after it (the last one of the page when there is none after). Empty for a page
/// without text.
pub fn sentence_text(page: &PageText, chars: Range<u32>) -> String {
    let segs = segments(page);
    let mut range = covering(&segs, chars.start..chars.end.max(chars.start + 1));
    if range.is_empty() {
        let i = range.start.min(segs.len().saturating_sub(1));
        range = i..(i + 1).min(segs.len());
    }
    segs[range]
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------------------------
// Skipping
// ---------------------------------------------------------------------------------------------

/// Whether `c` is a letter of the Latin script (ASCII, Latin-1 and Latin Extended).
fn is_latin_letter(c: char) -> bool {
    c.is_ascii_alphabetic() || (c.is_alphabetic() && ('\u{00C0}'..='\u{024F}').contains(&c))
}

fn is_hangul(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}')
}

/// Why `text` (a segment's text) is not worth translating, or `None` to translate it.
pub fn skip_reason(text: &str) -> Option<SkipReason> {
    let text = text.trim();
    if text.chars().count() <= 3 {
        return Some(SkipReason::TooShort);
    }
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    if !text.chars().any(is_latin_letter) {
        return Some(SkipReason::NoLatin);
    }
    let hangul = text.chars().filter(|c| is_hangul(*c)).count();
    if hangul * 2 > letters {
        return Some(SkipReason::Korean);
    }
    if has_leaders(text) {
        return Some(SkipReason::Leaders);
    }
    if looks_like_code(text) {
        return Some(SkipReason::Code);
    }
    if !text.contains(char::is_whitespace) && looks_like_identifier(text) {
        return Some(SkipReason::Identifier);
    }
    None
}

/// A run of at least four leader dots (spaces may stand between them) followed by a page
/// number: `Introduction . . . . 12`, `Scope ........ iv`.
fn has_leaders(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !is_leader(chars[i]) {
            i += 1;
            continue;
        }
        let mut dots = 0;
        let mut j = i;
        while j < chars.len() && (is_leader(chars[j]) || chars[j] == ' ') {
            if is_leader(chars[j]) {
                dots += if chars[j] == '\u{2026}' { 3 } else { 1 };
            }
            j += 1;
        }
        if dots >= 4 {
            let number: String = chars[j..]
                .iter()
                .take_while(|c| !c.is_whitespace())
                .collect();
            let is_number = !number.is_empty()
                && (number.chars().all(|c| c.is_ascii_digit())
                    || number
                        .chars()
                        .all(|c| matches!(c.to_ascii_lowercase(), 'i' | 'v' | 'x' | 'l' | 'c')));
            if is_number {
                return true;
            }
        }
        i = j.max(i + 1);
    }
    false
}

fn is_leader(c: char) -> bool {
    matches!(c, '.' | '\u{2026}' | '\u{00B7}' | '\u{2024}')
}

/// Braces, semicolons, brackets, operators, `->` and `::` make up 15% or more of the
/// non-space characters, and one of the strong signs (`;`, a brace, `->`, `::`, `()`) is there.
fn looks_like_code(text: &str) -> bool {
    let chars: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.is_empty() {
        return false;
    }
    let mut score = 0usize;
    let mut k = 0;
    while k < chars.len() {
        let pair = (chars[k], chars.get(k + 1).copied().unwrap_or(' '));
        match pair {
            ('-', '>') | (':', ':') | ('=', '=') | ('&', '&') | ('|', '|') => {
                score += 1;
                k += 2;
                continue;
            }
            (c, _) if "{}();=<>[]".contains(c) => score += 1,
            _ => {}
        }
        k += 1;
    }
    let strong = text.contains(';')
        || text.contains('{')
        || text.contains('}')
        || text.contains("->")
        || text.contains("::")
        || text.contains("()");
    strong && score * 100 >= chars.len() * 15
}

/// One word with an underscore, `::`, `->`, `()`, a slash or a dot between letters, a capital
/// after a small letter (`getValue`), or letters and digits mixed (`IPv6Addr`, `E2E04`).
fn looks_like_identifier(word: &str) -> bool {
    // Punctuation around a word (`(SWS_EM_01004)`, `getValue.`) does not count.
    let w = word.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
    if w.is_empty() {
        return false;
    }
    if w.contains('_') || w.contains("::") || w.contains("->") || w.contains("()") {
        return true;
    }
    let chars: Vec<char> = w.chars().collect();
    let inner = |i: usize| i > 0 && i + 1 < chars.len();
    for (i, c) in chars.iter().enumerate() {
        if matches!(c, '/' | '.')
            && inner(i)
            && chars[i - 1].is_alphanumeric()
            && chars[i + 1].is_alphanumeric()
        {
            return true;
        }
        if c.is_uppercase() && i > 0 && chars[i - 1].is_lowercase() {
            return true;
        }
    }
    let has_digit = chars.iter().any(char::is_ascii_digit);
    let has_letter = chars.iter().any(|c| c.is_alphabetic());
    has_digit && has_letter
}

// ---------------------------------------------------------------------------------------------
// The flattened page
// ---------------------------------------------------------------------------------------------

/// Marks a block border in [`Flat`].
const BREAK: char = '\n';

/// The text of a page as one character sequence: lines joined, blocks separated by [`BREAK`],
/// with the page character behind every position.
pub(crate) struct Flat {
    pub(crate) chars: Vec<char>,
    /// `origin[k]`: the page character at `chars[k]` (`None`: a line-join space or a border).
    origin: Vec<Option<u32>>,
}

impl Flat {
    pub(crate) fn new(page: &PageText) -> Flat {
        let n = page.chars.len();
        let mut line_block: Vec<Option<usize>> = vec![None; page.lines.len()];
        for (bi, b) in page.blocks.iter().enumerate() {
            let first = (b.lines.start as usize).min(line_block.len());
            let last = (b.lines.end as usize).min(line_block.len());
            if let Some(slots) = line_block.get_mut(first..last) {
                slots.fill(Some(bi));
            }
        }
        let mut chars: Vec<char> = Vec::with_capacity(n + page.lines.len());
        let mut origin: Vec<Option<u32>> = Vec::with_capacity(n + page.lines.len());
        let mut prev_block: Option<Option<usize>> = None;
        for (li, line) in page.lines.iter().enumerate() {
            let end = (line.chars.end as usize).min(n);
            let start = (line.chars.start as usize).min(end);
            if start == end {
                continue;
            }
            let block = line_block.get(li).copied().flatten();
            if let Some(pb) = prev_block {
                let same = block.is_some() && pb == block;
                let first = page.chars[start].ch;
                if !same {
                    chars.push(BREAK);
                    origin.push(None);
                } else if hyphen_break(&chars, first) {
                    chars.pop();
                    origin.pop();
                } else {
                    chars.push(' ');
                    origin.push(None);
                }
            }
            for (i, c) in (start..end).zip(&page.chars[start..end]) {
                chars.push(c.ch);
                // Page character indices fit in u32 (they come from `Range<u32>`).
                origin.push(u32::try_from(i).ok());
            }
            prev_block = Some(block);
        }
        Flat { chars, origin }
    }

    /// The page characters of a span: from the first to one past the last page character in it.
    fn page_range(&self, span: Range<usize>) -> Option<Range<u32>> {
        let slice = self.origin.get(span)?;
        let first = slice.iter().flatten().next()?;
        let last = slice.iter().rev().flatten().next()?;
        Some(*first..last + 1)
    }

    /// The text of a span with white space collapsed and soft hyphens dropped.
    fn text(&self, span: Range<usize>) -> String {
        let raw: String = self.chars[span]
            .iter()
            .filter(|c| **c != '\u{00AD}')
            .collect();
        raw.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Whether `chars[k]` is a space put in where two lines were joined.
    fn is_line_join(&self, k: usize) -> bool {
        self.chars.get(k) == Some(&' ') && self.origin.get(k).is_some_and(Option::is_none)
    }

    /// The sentence spans of the whole page, trimmed of white space, in order.
    fn spans(&self) -> Vec<Range<usize>> {
        let s = &self.chars;
        let mut out = Vec::new();
        let mut block_start = 0;
        for k in 0..=s.len() {
            if k == s.len() || s[k] == BREAK {
                self.block_spans(block_start..k, &mut out);
                block_start = k + 1;
            }
        }
        out
    }

    /// The sentence spans of one block. A piece without a letter or a digit (the `}` after
    /// `return 0;`) goes with the sentence before it.
    fn block_spans(&self, block: Range<usize>, out: &mut Vec<Range<usize>>) {
        let mut sentences: Vec<Range<usize>> = Vec::new();
        let mut push = |span: Range<usize>| {
            let span = self.trim(span);
            if span.is_empty() {
                return;
            }
            let wordless = !self.chars[span.clone()].iter().any(|c| c.is_alphanumeric());
            match sentences.last_mut() {
                Some(last) if wordless => last.end = span.end,
                _ => sentences.push(span),
            }
        };
        let mut start = block.start;
        let mut k = block.start;
        while k < block.end {
            if let Some(end) = self.sentence_end(k, start, block.end) {
                push(start..end);
                start = end;
                k = end;
            } else {
                k += 1;
            }
        }
        push(start..block.end);
        for span in sentences {
            self.push_split(span, out);
        }
    }

    /// Trims `span` and pushes it, cut into pieces of at most [`MAX_SEGMENT_CHARS`].
    fn push_split(&self, span: Range<usize>, out: &mut Vec<Range<usize>>) {
        let s = &self.chars;
        let mut span = self.trim(span);
        while span.len() > MAX_SEGMENT_CHARS {
            let window = span.start..span.start + MAX_SEGMENT_CHARS;
            // After a comma that is followed by a space, if the piece is long enough.
            let comma = window
                .clone()
                .rev()
                .find(|&i| {
                    s[i] == ','
                        && s.get(i + 1).is_some_and(|c| c.is_whitespace())
                        && i + 1 - span.start >= MIN_COMMA_PIECE
                })
                .map(|i| i + 1);
            let space = || {
                window
                    .clone()
                    .rev()
                    .find(|&i| i > span.start && s[i].is_whitespace())
            };
            let cut = comma.or_else(space).unwrap_or(window.end);
            let piece = self.trim(span.start..cut);
            if !piece.is_empty() {
                out.push(piece);
            }
            span = self.trim(cut..span.end);
        }
        if !span.is_empty() {
            out.push(span);
        }
    }

    fn trim(&self, span: Range<usize>) -> Range<usize> {
        let s = &self.chars;
        let mut a = span.start;
        let mut b = span.end;
        while a < b && s[a].is_whitespace() {
            a += 1;
        }
        while b > a && s[b - 1].is_whitespace() {
            b -= 1;
        }
        a..b
    }

    /// If a sentence that started at `start` ends at `k`, one past its last character (closing
    /// brackets and quotes after the mark belong to it).
    ///
    /// A sentence ends after `!`, `?` or `;` followed by white space; after `:` followed by a
    /// line end; after `.` followed by white space and an uppercase letter, a digit or an
    /// opening bracket (so `e.g. this` and `3.2` do not end one) — but not after an ellipsis,
    /// a dot standing alone (leader dots), or a known abbreviation (`e.g.`, `Fig.`, `No.` ...).
    /// A sentence of one word does not end (`1. Introduction`, `a. Scope`, `Note:`).
    fn sentence_end(&self, k: usize, start: usize, block_end: usize) -> Option<usize> {
        let s = &self.chars;
        let c = s[k];
        if !matches!(c, '.' | '!' | '?' | ';' | ':') {
            return None;
        }
        let mut j = k + 1;
        while j < block_end && is_closer(s[j]) {
            j += 1;
        }
        // What follows: the end of the block, or white space.
        let at_end = j >= block_end;
        if !at_end && !s[j].is_whitespace() {
            return None;
        }
        let next = (j..block_end).find(|&i| !s[i].is_whitespace());
        match c {
            ':' => {
                let line_end =
                    at_end || (j..next.unwrap_or(block_end)).any(|i| self.is_line_join(i));
                if !line_end {
                    return None;
                }
            }
            '.' => {
                if k > 0 && s[k - 1] == '.' || s.get(k + 1) == Some(&'.') {
                    return None;
                }
                let token_start = (start..k)
                    .rev()
                    .find(|&i| s[i].is_whitespace() || s[i] == '(')
                    .map_or(start, |i| i + 1);
                let token: String = s[token_start..=k].iter().collect();
                if token == "." || is_abbreviation(&token) {
                    return None;
                }
                if let Some(n) = next {
                    let n = s[n];
                    if !(n.is_uppercase() || n.is_ascii_digit() || matches!(n, '(' | '[')) {
                        return None;
                    }
                }
            }
            _ => {}
        }
        // A one-word sentence does not end.
        let body = self.trim(start..j);
        if !s[body].iter().any(|c| c.is_whitespace()) {
            return None;
        }
        Some(j)
    }
}

fn is_closer(c: char) -> bool {
    matches!(c, ')' | ']' | '"' | '\'' | '\u{201D}' | '\u{2019}')
}

/// Abbreviations after which a dot does not end a sentence.
fn is_abbreviation(token: &str) -> bool {
    const ANY_CASE: [&str; 6] = ["e.g.", "i.e.", "cf.", "vs.", "approx.", "resp."];
    const EXACT: [&str; 30] = [
        "Fig.", "Figs.", "No.", "Nos.", "Eq.", "Eqs.", "Ref.", "Refs.", "Sec.", "Sect.", "Ch.",
        "Vol.", "Tab.", "pp.", "p.", "Dr.", "Mr.", "Mrs.", "Ms.", "Prof.", "Jr.", "St.", "al.",
        "Inc.", "Ltd.", "Corp.", "Dept.", "Rev.", "Ver.", "Art.",
    ];
    let lower = token.to_lowercase();
    ANY_CASE.contains(&lower.as_str()) || EXACT.contains(&token)
}

/// Whether the text so far ends in "letter-" and the next line starts with a lowercase letter.
fn hyphen_break(before: &[char], next: char) -> bool {
    let n = before.len();
    n >= 2
        && matches!(before[n - 1], '-' | '\u{2010}' | '\u{00AD}')
        && before[n - 2].is_alphabetic()
        && next.is_lowercase()
}
