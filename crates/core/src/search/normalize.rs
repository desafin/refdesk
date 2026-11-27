//! Turns a page's text into a search string and keeps the way back to the original characters.
//!
//! Rules, applied per original character in reading order:
//!
//! * NFKC (a ligature such as `ﬁ` becomes `fi`; every produced character points to the original
//!   one).
//! * A soft hyphen is dropped. A line that ends in a letter and a hyphen (`-`, U+2010, U+00AD),
//!   followed by a line that starts with a lowercase letter, is joined to it without the hyphen
//!   and without a space. Any other line end becomes one space.
//! * A run of white space becomes one space.
//! * With `fold`, letters are lowercased (`ß` becomes `ss`, `İ` becomes `i` + a combining dot).
//!
//! The text of the next page may be appended (up to [`CONTINUATION_CHARS`] characters, joined by
//! the line-end rule) so that a phrase that runs over a page break can be found.
//! The design follows `references/engines/pdfjs/web/pdf_find_controller.js:95-366`.

use unicode_normalization::UnicodeNormalization;

use crate::text::PageText;

/// How many normalized characters of the next page are appended.
pub(super) const CONTINUATION_CHARS: usize = 200;

const SOFT_HYPHEN: char = '\u{00AD}';

/// Hyphen characters that may be removed when a word is broken at a line end.
fn is_hyphen(c: char) -> bool {
    matches!(c, '-' | '\u{2010}' | SOFT_HYPHEN)
}

/// The normalized text of a page (plus a prefix of the next page) and its map back.
pub(super) struct Normalized {
    /// The search string.
    pub(super) text: String,
    /// The characters of `text`.
    pub(super) chars: Vec<char>,
    /// Byte offset of every character of `text`, plus a final `text.len()`.
    byte_starts: Vec<usize>,
    /// For every character of `text`, the index of the original character it came from. Up to
    /// `own` these are indices on this page, after that on the next page.
    map: Vec<u32>,
    /// The first `own` characters belong to this page.
    pub(super) own: usize,
}

impl Normalized {
    /// Character index of a byte offset of `text` (which must be at a character boundary).
    pub(super) fn char_of_byte(&self, byte: usize) -> usize {
        match self.byte_starts.binary_search(&byte) {
            Ok(i) | Err(i) => i,
        }
    }

    /// The original character index that normalized character `i` came from.
    pub(super) fn origin(&self, i: usize) -> Option<u32> {
        self.map.get(i).copied()
    }
}

/// Accumulates normalized characters and their origins.
struct Builder {
    chars: Vec<char>,
    map: Vec<u32>,
    fold: bool,
}

impl Builder {
    fn new(fold: bool) -> Builder {
        Builder {
            chars: Vec::new(),
            map: Vec::new(),
            fold,
        }
    }

    fn push_raw(&mut self, c: char, origin: u32) {
        self.chars.push(c);
        self.map.push(origin);
    }

    fn push_space(&mut self, origin: u32) {
        if self.chars.last() != Some(&' ') {
            self.push_raw(' ', origin);
        }
    }

    /// Normalizes one original character.
    fn push_original(&mut self, ch: char, origin: u32) {
        if ch == SOFT_HYPHEN {
            return;
        }
        if ch.is_ascii() {
            self.push_normalized(ch, origin);
        } else {
            for c in std::iter::once(ch).nfkc() {
                self.push_normalized(c, origin);
            }
        }
    }

    fn push_normalized(&mut self, c: char, origin: u32) {
        if c.is_whitespace() {
            self.push_space(origin);
        } else if !self.fold {
            self.push_raw(c, origin);
        } else if c == 'ß' {
            self.push_raw('s', origin);
            self.push_raw('s', origin);
        } else {
            for l in c.to_lowercase() {
                self.push_raw(l, origin);
            }
        }
    }

    /// Feeds the characters of `page`, stopping when `limit` normalized characters exist.
    /// `next` is the page that follows, used to decide how the last line ends.
    fn feed(&mut self, page: &PageText, next: Option<&PageText>, limit: usize) {
        let n = page.chars.len();
        let mut line_end = vec![false; n];
        for l in &page.lines {
            let end = (l.chars.end as usize).min(n);
            if end > 0 && end > l.chars.start as usize {
                line_end[end - 1] = true;
            }
        }
        let next_first = next.and_then(|p| p.chars.first()).map(|c| c.ch);

        let mut line_start = 0usize;
        for (i, tc) in page.chars.iter().enumerate() {
            if self.chars.len() >= limit {
                return;
            }
            let ch = tc.ch;
            let origin = i as u32;
            // The end of the page is a line end when something follows it.
            let following = if i + 1 < n {
                Some(page.chars[i + 1].ch)
            } else {
                next_first
            };
            let ends_line = (line_end[i] && i + 1 < n) || (i + 1 == n && following.is_some());
            if ends_line {
                let joins = is_hyphen(ch)
                    && i > line_start
                    && page.chars[i - 1].ch.is_alphabetic()
                    && following.is_some_and(char::is_lowercase);
                if !joins {
                    self.push_original(ch, origin);
                    self.push_space(origin);
                }
                line_start = i + 1;
            } else {
                self.push_original(ch, origin);
                if line_end[i] {
                    line_start = i + 1;
                }
            }
        }
    }
}

fn finish(b: Builder, own: usize) -> Normalized {
    let text: String = b.chars.iter().collect();
    let mut byte_starts = Vec::with_capacity(b.chars.len() + 1);
    let mut at = 0usize;
    for c in &b.chars {
        byte_starts.push(at);
        at += c.len_utf8();
    }
    byte_starts.push(at);
    Normalized {
        text,
        chars: b.chars,
        byte_starts,
        map: b.map,
        own,
    }
}

/// Normalizes `page`. When `next` is given and `page` has text, the start of `next` is appended
/// after the page's own characters.
pub(super) fn normalize_page(page: &PageText, fold: bool, next: Option<&PageText>) -> Normalized {
    let mut b = Builder::new(fold);
    let next = next.filter(|_| !page.chars.is_empty());
    b.feed(page, next, usize::MAX);
    let own = b.chars.len();
    if let Some(next) = next {
        b.feed(next, None, own + CONTINUATION_CHARS);
    }
    finish(b, own)
}

/// Normalizes a search text the same way (without line-end rules) and trims it.
pub(super) fn normalize_query(text: &str, fold: bool) -> String {
    let mut b = Builder::new(fold);
    for ch in text.chars() {
        b.push_original(ch, 0);
    }
    let s: String = b.chars.iter().collect();
    s.trim().to_string()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::geom::{Point, Quad, Rect};
    use crate::text::{TextChar, TextLine};

    /// A page whose lines are the given strings. Every character is 10 pt wide; line `l` is at
    /// y = 20 * l.
    pub(in crate::search) fn page(lines: &[&str]) -> PageText {
        let mut p = PageText::default();
        for (l, line) in lines.iter().enumerate() {
            let start = p.chars.len() as u32;
            let y0 = 20.0 * l as f32;
            for (k, ch) in line.chars().enumerate() {
                let x0 = 10.0 * k as f32;
                let r = Rect::new(x0, y0, x0 + 10.0, y0 + 12.0);
                p.chars.push(TextChar {
                    ch,
                    quad: Quad {
                        ul: Point::new(r.x0, r.y0),
                        ur: Point::new(r.x1, r.y0),
                        ll: Point::new(r.x0, r.y1),
                        lr: Point::new(r.x1, r.y1),
                    },
                    size: 12.0,
                });
            }
            p.lines.push(TextLine {
                chars: start..p.chars.len() as u32,
                bounds: Rect::new(0.0, y0, 10.0 * line.chars().count() as f32, y0 + 12.0),
            });
        }
        p
    }

    fn norm(lines: &[&str], fold: bool) -> Normalized {
        normalize_page(&page(lines), fold, None)
    }

    #[test]
    fn hyphen_at_line_end_joins_the_word() {
        let n = norm(&["a speci-", "fication b"], true);
        assert_eq!(n.text, "a specification b");
        // 'speci' is on line 0, 'fication' on line 1 (original index 8 = 'f').
        assert_eq!(n.origin(2), Some(2));
        assert_eq!(n.origin(6), Some(6));
        assert_eq!(n.origin(7), Some(8));
    }

    #[test]
    fn hyphen_stays_before_an_uppercase_or_digit() {
        assert_eq!(norm(&["pre-", "Fix"], true).text, "pre- fix");
        assert_eq!(norm(&["x-", "1"], true).text, "x- 1");
        // No letter before the hyphen.
        assert_eq!(norm(&["a -", "b"], true).text, "a - b");
    }

    #[test]
    fn other_hyphens_and_soft_hyphens() {
        assert_eq!(norm(&["a\u{2010}", "b"], true).text, "ab");
        assert_eq!(norm(&["a\u{00AD}", "b"], true).text, "ab");
        // A soft hyphen inside a line is invisible.
        assert_eq!(norm(&["spe\u{00AD}cial"], true).text, "special");
        // A hyphen inside a line is kept.
        assert_eq!(norm(&["well-known"], true).text, "well-known");
    }

    #[test]
    fn line_ends_become_one_space_that_points_at_the_last_char() {
        let n = norm(&["state", "machine"], true);
        assert_eq!(n.text, "state machine");
        assert_eq!(n.origin(5), Some(4));
        assert_eq!(n.origin(6), Some(5));
        // A line that already ends in a space does not get a second one.
        assert_eq!(norm(&["state ", "machine"], true).text, "state machine");
    }

    #[test]
    fn white_space_runs_collapse() {
        let n = norm(&["a \t  b\u{00A0}c"], true);
        assert_eq!(n.text, "a b c");
        assert_eq!(n.origin(1), Some(1));
        assert_eq!(n.origin(2), Some(5));
    }

    #[test]
    fn ligature_points_at_the_original_char() {
        let n = norm(&["a\u{FB01}le"], true);
        assert_eq!(n.text, "afile");
        assert_eq!(n.origin(1), Some(1));
        assert_eq!(n.origin(2), Some(1));
        assert_eq!(n.origin(3), Some(2));
        assert_eq!(n.origin(4), Some(3));
    }

    #[test]
    fn case_folding_keeps_the_map_aligned() {
        let n = norm(&["Stra\u{00DF}e X"], true);
        assert_eq!(n.text, "strasse x");
        // The two s of "ss" both come from the sharp s at index 4.
        assert_eq!(n.origin(4), Some(4));
        assert_eq!(n.origin(5), Some(4));
        assert_eq!(n.origin(6), Some(5));
        // 'İ' lowercases to two characters.
        let n = norm(&["\u{0130}x"], true);
        assert_eq!(n.chars.len(), 3);
        assert_eq!(n.origin(0), Some(0));
        assert_eq!(n.origin(1), Some(0));
        assert_eq!(n.origin(2), Some(1));
        // Without folding nothing changes.
        assert_eq!(norm(&["Stra\u{00DF}e"], false).text, "Stra\u{00DF}e");
    }

    #[test]
    fn byte_offsets_map_to_char_indices() {
        let n = norm(&["\u{00E4}b\u{00E4}"], false);
        assert_eq!(n.char_of_byte(0), 0);
        assert_eq!(n.char_of_byte(2), 1);
        assert_eq!(n.char_of_byte(3), 2);
        assert_eq!(n.char_of_byte(5), 3);
    }

    #[test]
    fn next_page_is_appended_by_the_line_end_rule() {
        let a = page(&["see the speci-"]);
        let b = page(&["fication of it"]);
        let n = normalize_page(&a, true, Some(&b));
        assert_eq!(n.text, "see the specification of it");
        assert_eq!(n.own, "see the speci".chars().count());
        // First appended char points into the next page.
        assert_eq!(n.origin(n.own), Some(0));

        let a = page(&["state"]);
        let b = page(&["machine"]);
        let n = normalize_page(&a, true, Some(&b));
        assert_eq!(n.text, "state machine");
        assert_eq!(n.own, 6); // "state " (the space belongs to this page)
    }

    #[test]
    fn continuation_is_limited_and_skipped_for_empty_pages() {
        let long = "x ".repeat(300);
        let n = normalize_page(&page(&["a"]), true, Some(&page(&[long.as_str()])));
        assert_eq!(n.chars.len(), n.own + CONTINUATION_CHARS);
        let n = normalize_page(&PageText::default(), true, Some(&page(&["abc"])));
        assert_eq!(n.text, "");
        // Nothing follows an empty next page: no trailing space.
        let n = normalize_page(&page(&["abc"]), true, Some(&PageText::default()));
        assert_eq!(n.text, "abc");
    }

    #[test]
    fn query_is_normalized_and_trimmed() {
        assert_eq!(normalize_query("  Hello \t World ", true), "hello world");
        assert_eq!(normalize_query("\u{FB01}le", true), "file");
        assert_eq!(normalize_query("Hello", false), "Hello");
        assert_eq!(normalize_query("  \t ", true), "");
        assert_eq!(normalize_query("Stra\u{00DF}e", true), "strasse");
    }
}
