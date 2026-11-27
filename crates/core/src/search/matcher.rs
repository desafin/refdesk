//! Finding matches in a normalized page and mapping them back to the page's characters.

use std::ops::Range;

use regex::{Regex, RegexBuilder};

use super::normalize::{Normalized, normalize_page, normalize_query};
use super::{MatchPart, SearchError, SearchMatch, SearchQuery};
use crate::text::PageText;

/// How many characters of context are kept on each side of a match.
const CONTEXT_CHARS: usize = 40;

enum Pattern {
    /// The normalized needle.
    Plain(String),
    Regex(Regex),
}

/// A compiled query. Cheap to share between threads.
pub(super) struct Matcher {
    pattern: Pattern,
    whole_word: bool,
    /// Whether the page text is lowercased before matching. Regular expressions do their own
    /// case handling, so the text is left alone for them.
    fold: bool,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The byte offset after the character that starts at `i`.
fn next_boundary(text: &str, i: usize) -> usize {
    i + text
        .get(i..)
        .and_then(|s| s.chars().next())
        .map_or(1, char::len_utf8)
}

impl Matcher {
    /// Compiles `query`.
    pub(super) fn new(query: &SearchQuery) -> Result<Matcher, SearchError> {
        if query.regex {
            if query.text.is_empty() {
                return Err(SearchError::Empty);
            }
            let re = RegexBuilder::new(&query.text)
                .case_insensitive(!query.case_sensitive)
                .build()
                .map_err(|e| SearchError::InvalidRegex(e.to_string()))?;
            Ok(Matcher {
                pattern: Pattern::Regex(re),
                whole_word: query.whole_word,
                fold: false,
            })
        } else {
            let fold = !query.case_sensitive;
            let needle = normalize_query(&query.text, fold);
            if needle.is_empty() {
                return Err(SearchError::Empty);
            }
            Ok(Matcher {
                pattern: Pattern::Plain(needle),
                whole_word: query.whole_word,
                fold,
            })
        }
    }

    /// Byte ranges of the matches that start in the page's own part of `norm`, in order and
    /// without overlap.
    fn scan(&self, norm: &Normalized) -> Vec<Range<usize>> {
        let text = norm.text.as_str();
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos <= text.len() {
            let found = match &self.pattern {
                Pattern::Plain(needle) => text
                    .get(pos..)
                    .and_then(|rest| rest.find(needle.as_str()))
                    .map(|i| pos + i..pos + i + needle.len()),
                Pattern::Regex(re) => re.find_at(text, pos).map(|m| m.range()),
            };
            let Some(r) = found else {
                break;
            };
            if norm.char_of_byte(r.start) >= norm.own {
                break;
            }
            if r.is_empty() {
                pos = next_boundary(text, r.end);
                continue;
            }
            if self.whole_word && !at_word_boundaries(text, &r) {
                pos = next_boundary(text, r.start);
                continue;
            }
            pos = r.end;
            out.push(r);
        }
        out
    }

    /// All matches that start on `page`. `next` is the following page of the document, if any;
    /// it lets a phrase run over the page break.
    pub(super) fn find_in_page(
        &self,
        page_index: u32,
        page: &PageText,
        next: Option<&PageText>,
    ) -> Vec<SearchMatch> {
        if page.chars.is_empty() {
            return Vec::new();
        }
        let norm = normalize_page(page, self.fold, next);
        let found = self.scan(&norm);
        if found.is_empty() {
            return Vec::new();
        }
        // The context is shown to the reader, so it keeps the letters as they are written
        // (`SWS_EM_01004`, not `sws_em_01004`). Lowercasing lengthens a few letters (`ß`, `İ`);
        // on a page that has one the two texts do not line up and the folded one is shown.
        let written = self
            .fold
            .then(|| normalize_page(page, false, next))
            .filter(|w| w.chars.len() == norm.chars.len() && w.own == norm.own);
        let shown = written.as_ref().map_or(&norm.chars, |w| &w.chars);
        found
            .into_iter()
            .filter_map(|r| build_match(page_index, page, next, &norm, shown, &r))
            .collect()
    }
}

/// A match must not run into a word: if it starts with a word character the one before must not
/// be one, and likewise at the end.
fn at_word_boundaries(text: &str, r: &Range<usize>) -> bool {
    let first = text.get(r.start..).and_then(|s| s.chars().next());
    let prev = text.get(..r.start).and_then(|s| s.chars().next_back());
    let last = text.get(..r.end).and_then(|s| s.chars().next_back());
    let next = text.get(r.end..).and_then(|s| s.chars().next());
    let start_ok = !(first.is_some_and(is_word) && prev.is_some_and(is_word));
    let end_ok = !(last.is_some_and(is_word) && next.is_some_and(is_word));
    start_ok && end_ok
}

fn build_match(
    page_index: u32,
    page: &PageText,
    next: Option<&PageText>,
    norm: &Normalized,
    shown: &[char],
    bytes: &Range<usize>,
) -> Option<SearchMatch> {
    let a = norm.char_of_byte(bytes.start);
    let b = norm.char_of_byte(bytes.end);
    if a >= b || a >= norm.own {
        return None;
    }
    let own_end = b.min(norm.own);
    let start = norm.origin(a)?;
    let end = norm.origin(own_end - 1)? + 1;
    let chars = start..end;
    let rects = page.rects(chars.clone());

    let cont = if b > norm.own {
        match (next, norm.origin(norm.own), norm.origin(b - 1)) {
            (Some(next), Some(s), Some(e)) => Some(MatchPart {
                page: page_index + 1,
                chars: s..e + 1,
                rects: next.rects(s..e + 1),
            }),
            _ => None,
        }
    } else {
        None
    };

    let (before, matched, after) = context(shown, a, b);
    Some(SearchMatch {
        page: page_index,
        chars,
        rects,
        cont,
        before,
        matched,
        after,
    })
}

/// The text before, inside and after `a..b`, about [`CONTEXT_CHARS`] on each side, cut where a
/// white space is (so that a word or identifier is never cut in the middle).
fn context(chars: &[char], a: usize, b: usize) -> (String, String, String) {
    let is_cut = |i: usize| {
        i == 0
            || i >= chars.len()
            || chars.get(i - 1).is_some_and(|c| c.is_whitespace())
            || chars.get(i).is_some_and(|c| c.is_whitespace())
    };

    let mut from = a.saturating_sub(CONTEXT_CHARS);
    while from < a && !is_cut(from) {
        from += 1;
    }
    if from == a && a > 0 && !is_cut(a) {
        from = a.saturating_sub(CONTEXT_CHARS);
    }

    let mut to = (b + CONTEXT_CHARS).min(chars.len());
    while to > b && !is_cut(to) {
        to -= 1;
    }
    if to == b && b < chars.len() && !is_cut(b) {
        to = (b + CONTEXT_CHARS).min(chars.len());
    }

    // The outer ends do not need white space.
    while from < a && chars.get(from).is_some_and(|c| c.is_whitespace()) {
        from += 1;
    }
    while to > b && chars.get(to - 1).is_some_and(|c| c.is_whitespace()) {
        to -= 1;
    }

    let collect = |r: Range<usize>| -> String { chars.get(r).unwrap_or_default().iter().collect() };
    (collect(from..a), collect(a..b), collect(b..to))
}

#[cfg(test)]
mod tests {
    use super::super::normalize::tests::page;
    use super::*;

    fn q(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.to_string(),
            ..SearchQuery::default()
        }
    }

    fn find(query: &SearchQuery, lines: &[&str]) -> Vec<SearchMatch> {
        let m = Matcher::new(query).unwrap();
        m.find_in_page(0, &page(lines), None)
    }

    fn found(query: &SearchQuery, lines: &[&str]) -> Vec<String> {
        find(query, lines).into_iter().map(|m| m.matched).collect()
    }

    #[test]
    fn hyphenated_word_is_found_across_two_lines() {
        let ms = find(&q("specification"), &["the speci-", "fication says"]);
        assert_eq!(ms.len(), 1);
        let m = &ms[0];
        assert_eq!(m.matched, "specification");
        // "speci-" is original 4..10, "fication" 10..18: the range covers both lines.
        assert_eq!(m.chars, 4..18);
        assert_eq!(m.rects.len(), 2);
        assert_eq!(m.rects[0], crate::geom::Rect::new(40.0, 0.0, 100.0, 12.0));
        assert_eq!(m.rects[1], crate::geom::Rect::new(0.0, 20.0, 80.0, 32.0));
        assert!(m.cont.is_none());
        assert_eq!(m.before, "the ");
        assert_eq!(m.after, " says");
    }

    #[test]
    fn the_context_keeps_the_letters_as_they_are_written() {
        let ms = find(&q("sws_em"), &["See SWS_EM_01004 in the", "PDF Spec"]);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].before, "See ");
        assert_eq!(ms[0].matched, "SWS_EM");
        assert_eq!(ms[0].after, "_01004 in the PDF Spec");
        // The match itself does not depend on it.
        assert_eq!(ms[0].chars, 4..10);
    }

    #[test]
    fn a_page_with_a_letter_that_grows_when_lowercased_shows_the_folded_text() {
        // "ß" becomes "ss": the written text is one character shorter than the searched one,
        // so the context comes from the searched (folded) text and still lines up.
        let ms = find(&q("abc"), &["Straße ABC Def"]);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].before, "strasse ");
        assert_eq!(ms[0].matched, "abc");
        assert_eq!(ms[0].after, " def");
        assert_eq!(ms[0].chars, 7..10);
    }

    #[test]
    fn line_break_is_a_space() {
        let ms = find(&q("state machine"), &["the state", "machine is"]);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].chars, 4..16);
        assert_eq!(ms[0].rects.len(), 2);
        assert_eq!(ms[0].matched, "state machine");
    }

    #[test]
    fn ligature_is_found_and_the_range_is_the_ligature() {
        let ms = find(&q("file"), &["a \u{FB01}le b"]);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].chars, 2..5); // ﬁ, l, e
        // Searching the middle of a ligature still covers the whole glyph.
        let ms = find(&q("il"), &["a \u{FB01}le"]);
        assert_eq!(ms[0].chars, 2..4);
        // The query may use the ligature too.
        assert_eq!(found(&q("\u{FB01}le"), &["a file"]).len(), 1);
    }

    #[test]
    fn case_options() {
        assert_eq!(found(&q("state"), &["State STATE state"]).len(), 3);
        let cs = SearchQuery {
            case_sensitive: true,
            ..q("State")
        };
        let ms = find(&cs, &["State STATE state"]);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].chars, 0..5);
    }

    #[test]
    fn sharp_s_and_dotted_i_map_back_correctly() {
        // "straße" lowercases to "strasse": the match of "se" starts inside the ß.
        let ms = find(&q("sse"), &["Stra\u{00DF}e"]);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].chars, 4..6); // ß, e
        let ms = find(&q("strasse"), &["x Stra\u{00DF}e"]);
        assert_eq!(ms[0].chars, 2..8);
        // A match that ends in the middle of the ß still covers it.
        let ms = find(&q("stras"), &["Stra\u{00DF}e"]);
        assert_eq!(ms[0].chars, 0..5);
        // 'İ' becomes two characters; the text after it must still line up.
        let ms = find(&q("x"), &["\u{0130}\u{0130}x"]);
        assert_eq!(ms[0].chars, 2..3);
        // Case sensitive: ß is not "ss".
        let cs = SearchQuery {
            case_sensitive: true,
            ..q("strasse")
        };
        assert!(find(&cs, &["stra\u{00DF}e"]).is_empty());
    }

    #[test]
    fn whole_word() {
        let ww = |t: &str| SearchQuery {
            whole_word: true,
            ..q(t)
        };
        assert_eq!(found(&ww("page"), &["pages"]).len(), 0);
        assert_eq!(found(&ww("page"), &["page."]).len(), 1);
        assert_eq!(
            found(&ww("page"), &["a page, pages, (page) page_x xpage"]).len(),
            2
        );
        assert_eq!(found(&ww("SWS_EM"), &["SWS_EM_01004"]).len(), 0);
        assert_eq!(found(&ww("SWS_EM_01004"), &["see SWS_EM_01004."]).len(), 1);
        // A later whole-word occurrence is found after a rejected one.
        assert_eq!(found(&ww("page"), &["pages page"]).len(), 1);
        // Non-word edges do not need a boundary.
        assert_eq!(found(&ww("(a)"), &["x(a)y"]).len(), 1);
    }

    fn re(t: &str) -> SearchQuery {
        SearchQuery {
            regex: true,
            ..q(t)
        }
    }

    #[test]
    fn regex_matches_and_errors() {
        // Text is not lowercased for a regex; the regex handles case itself.
        let ms = found(
            &re(r"\bREQ_\d+"),
            &["see REQ_12 and req_7 but XREQ_3, REQ_"],
        );
        assert_eq!(ms, ["REQ_12", "req_7"]);
        let cs = SearchQuery {
            case_sensitive: true,
            ..re(r"\bREQ_\d+")
        };
        assert_eq!(found(&cs, &["REQ_12 req_7"]), ["REQ_12"]);
        // Upper-case classes keep working because the text keeps its case.
        assert_eq!(found(&cs.clone(), &["x"]).len(), 0);
        let upper = SearchQuery {
            case_sensitive: true,
            ..re(r"\p{Lu}+")
        };
        assert_eq!(found(&upper, &["abCDe"]), ["CD"]);
        // A pattern may span a line break, which is one space.
        assert_eq!(
            found(&re(r"state\s+machine"), &["state", "machine"]).len(),
            1
        );

        assert!(matches!(
            Matcher::new(&re("(unclosed")),
            Err(SearchError::InvalidRegex(m)) if !m.is_empty()
        ));
        assert!(matches!(Matcher::new(&re("")), Err(SearchError::Empty)));
        assert!(matches!(Matcher::new(&q("   ")), Err(SearchError::Empty)));
        assert!(matches!(Matcher::new(&q("")), Err(SearchError::Empty)));
    }

    #[test]
    fn regex_skips_empty_matches() {
        // `x*` matches the empty string everywhere; only the real run is reported.
        assert_eq!(found(&re("x*"), &["ab xx cd x"]), ["xx", "x"]);
        assert!(found(&re("z*"), &["abc"]).is_empty());
        // Multi-byte text must not break the stepping.
        assert_eq!(found(&re("x*"), &["\u{00E4}x\u{00E4}"]), ["x"]);
    }

    #[test]
    fn regex_whole_word() {
        let r = SearchQuery {
            whole_word: true,
            ..re(r"REQ_\d")
        };
        assert_eq!(found(&r, &["REQ_1 REQ_12 REQ_3"]), ["REQ_1", "REQ_3"]);
    }

    #[test]
    fn white_space_in_the_text_matches_a_single_space() {
        assert_eq!(found(&q("a b"), &["a \t  b a   b"]).len(), 2);
        assert_eq!(found(&q("  a   b  "), &["a b"]).len(), 1);
    }

    #[test]
    fn overlapping_candidates_are_not_reported() {
        let ms = find(&q("aa"), &["aaaa"]);
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].chars, 0..2);
        assert_eq!(ms[1].chars, 2..4);
        assert_eq!(find(&q("aa"), &["aaa"]).len(), 1);
    }

    #[test]
    fn no_match_in_empty_page() {
        let m = Matcher::new(&q("a")).unwrap();
        assert!(m.find_in_page(0, &PageText::default(), None).is_empty());
    }

    #[test]
    fn context_is_cut_at_word_boundaries() {
        let words: Vec<String> = (0..30).map(|i| format!("word{i:02}")).collect();
        let line = words.join(" ");
        let ms = find(&q("word15"), &[line.as_str()]);
        let m = &ms[0];
        assert_eq!(m.matched, "word15");
        assert!(m.before.chars().count() <= 40 && m.before.chars().count() >= 30);
        assert!(m.before.starts_with("word"));
        assert!(m.before.ends_with(' '));
        assert!(m.after.chars().count() <= 40 && m.after.chars().count() >= 30);
        assert!(m.after.starts_with(' '));
        assert!(m.after.trim_end().ends_with(|c: char| c.is_ascii_digit()));
        assert!(m.after.trim().split(' ').all(|w| w.len() == 6));
        assert!(m.before.trim().split(' ').all(|w| w.len() == 6));
    }

    #[test]
    fn context_of_a_very_long_word_is_cut_hard() {
        let long = "x".repeat(200);
        let line = format!("{long}needle{long}");
        let ms = find(&q("needle"), &[line.as_str()]);
        assert_eq!(ms[0].before.chars().count(), 40);
        assert_eq!(ms[0].after.chars().count(), 40);
    }

    #[test]
    fn match_at_the_start_and_end_of_a_page() {
        let ms = find(&q("ab"), &["ab cd ab"]);
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].before, "");
        assert_eq!(ms[0].after, " cd ab");
        assert_eq!(ms[1].before, "ab cd ");
        assert_eq!(ms[1].after, "");
        assert_eq!(ms[1].chars, 6..8);
    }

    #[test]
    fn phrase_over_a_page_break_belongs_to_the_first_page() {
        let a = page(&["see the state", "machine"]);
        let b = page(&["diagram of it", "more"]);
        let m = Matcher::new(&q("machine diagram")).unwrap();
        let ms = m.find_in_page(3, &a, Some(&b));
        assert_eq!(ms.len(), 1);
        let hit = &ms[0];
        assert_eq!(hit.page, 3);
        assert_eq!(hit.chars, 13..20); // "machine"
        assert_eq!(hit.rects.len(), 1);
        let cont = hit.cont.as_ref().unwrap();
        assert_eq!(cont.page, 4);
        assert_eq!(cont.chars, 0..7); // "diagram"
        assert_eq!(cont.rects.len(), 1);
        assert_eq!(hit.matched, "machine diagram");
        assert_eq!(hit.after, " of it more");
        // The next page alone has no match: it is reported once, by the first page.
        assert!(m.find_in_page(4, &b, None).is_empty());
    }

    #[test]
    fn hyphenated_word_over_a_page_break() {
        let a = page(&["the speci-"]);
        let b = page(&["fication"]);
        let m = Matcher::new(&q("specification")).unwrap();
        let ms = m.find_in_page(0, &a, Some(&b));
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].chars, 4..9); // "speci" (the hyphen is not part of the page's range)
        let cont = ms[0].cont.as_ref().unwrap();
        assert_eq!(cont.chars, 0..8);
        // Without a next page there is nothing to find.
        assert!(m.find_in_page(0, &a, None).is_empty());
    }

    #[test]
    fn match_inside_the_continuation_is_not_reported() {
        let a = page(&["tail"]);
        let b = page(&["head words"]);
        let m = Matcher::new(&q("head")).unwrap();
        assert!(m.find_in_page(0, &a, Some(&b)).is_empty());
        assert_eq!(m.find_in_page(1, &b, None).len(), 1);
    }
}
