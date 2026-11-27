//! Subsequence matching with a score, for the command palette and quick open.
//!
//! A query matches a text when its characters occur in the text in order (case ignored). Among
//! the ways to place them the best-scoring one is chosen: consecutive characters, characters that
//! start a word and characters near the front of the text score higher; skipped characters cost
//! a little. Pure functions over `char`s so they are tested without a window.

use std::ops::Range;

/// Score of every matched character.
const MATCH: i32 = 16;
/// Extra score when a character directly follows the previous matched one.
const CONSECUTIVE: i32 = 8;
/// Extra score when a character starts a word (after a space, `.`, `_` …, or a camel-case hump).
const WORD_START: i32 = 10;
/// Extra score for the first characters of the text, falling to zero at this index.
const EARLY: i32 = 6;
/// Cost of each skipped character between two matched ones.
const GAP: i32 = 1;
/// Marks an impossible state in the tables below.
const NONE: i32 = i32::MIN / 2;

/// A successful match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Higher is better. Only comparable between matches of the same query.
    pub score: i32,
    /// The matched characters, as indices into `text.chars()`, increasing.
    pub positions: Vec<usize>,
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn starts_word(chars: &[char], i: usize) -> bool {
    match i.checked_sub(1).map(|p| chars[p]) {
        None => true,
        Some(prev) => !prev.is_alphanumeric() || (prev.is_lowercase() && chars[i].is_uppercase()),
    }
}

/// Matches `query` against `text`. White space in the query is ignored, so `zoom in` finds
/// `ZoomIn`; an empty query matches everything with score 0.
pub fn fuzzy_match(query: &str, text: &str) -> Option<FuzzyMatch> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(fold)
        .collect();
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let chars: Vec<char> = text.chars().collect();
    let folded: Vec<char> = chars.iter().copied().map(fold).collect();
    let (m, n) = (query.len(), chars.len());
    if m > n || !is_subsequence(&query, &folded) {
        return None;
    }

    // `score[j][i]`: the best score with query[j] matched at text[i] (and everything before it
    // matched too); `from[j][i]`: where query[j - 1] sat in that best arrangement.
    let mut score: Vec<Vec<i32>> = Vec::with_capacity(m);
    let mut from: Vec<Vec<usize>> = Vec::with_capacity(m);
    for (j, q) in query.iter().enumerate() {
        let mut row = vec![NONE; n];
        let mut back = vec![usize::MAX; n];
        // The best (score, index) over earlier matches that leave at least one character
        // unmatched before `i`, already charged for the skipped characters.
        let mut far = (NONE, usize::MAX);
        for i in 0..n {
            if j > 0 && i >= 2 {
                let before = &score[j - 1];
                if before[i - 2] > far.0 {
                    far = (before[i - 2], i - 2);
                }
                far.0 -= GAP;
            }
            if folded[i] != *q {
                continue;
            }
            let mut base = MATCH + EARLY.saturating_sub(i as i32).max(0);
            if starts_word(&chars, i) {
                base += WORD_START;
            }
            if j == 0 {
                row[i] = base;
                continue;
            }
            let mut best = (NONE, usize::MAX);
            if far.0 > NONE / 2 {
                best = far;
            }
            if i >= 1 && score[j - 1][i - 1] > NONE / 2 {
                let adjacent = score[j - 1][i - 1] + CONSECUTIVE;
                if adjacent >= best.0 {
                    best = (adjacent, i - 1);
                }
            }
            if best.0 > NONE / 2 {
                row[i] = base + best.0;
                back[i] = best.1;
            }
        }
        score.push(row);
        from.push(back);
    }

    let last = &score[m - 1];
    let mut end = None;
    for (i, s) in last.iter().enumerate() {
        if *s > NONE / 2 && end.is_none_or(|e: usize| *s > last[e]) {
            end = Some(i);
        }
    }
    let mut i = end?;
    let total = last[i];
    let mut positions = vec![i];
    for j in (1..m).rev() {
        i = from[j][i];
        positions.push(i);
    }
    positions.reverse();
    Some(FuzzyMatch {
        score: total,
        positions,
    })
}

fn is_subsequence(query: &[char], folded: &[char]) -> bool {
    let mut rest = folded.iter();
    query.iter().all(|q| rest.any(|c| c == q))
}

/// Splits `text` into byte ranges that alternate between plain and highlighted, where the
/// highlighted ones hold the characters at `positions` (indices into `text.chars()`, as
/// [`FuzzyMatch::positions`]). The ranges cover the whole text; the flag says "highlighted".
pub fn highlight_ranges(text: &str, positions: &[usize]) -> Vec<(Range<usize>, bool)> {
    let mut out: Vec<(Range<usize>, bool)> = Vec::new();
    for (index, (start, c)) in text.char_indices().enumerate() {
        let lit = positions.binary_search(&index).is_ok();
        let end = start + c.len_utf8();
        match out.last_mut() {
            Some((range, state)) if *state == lit => range.end = end,
            _ => out.push((start..end, lit)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, text: &str) -> i32 {
        fuzzy_match(query, text)
            .unwrap_or_else(|| panic!("{query:?} should match {text:?}"))
            .score
    }

    #[test]
    fn a_subsequence_matches_and_a_different_order_does_not() {
        assert!(fuzzy_match("ctb", "Close Tab").is_some());
        assert!(fuzzy_match("cltab", "Close Tab").is_some());
        assert!(fuzzy_match("tc", "Close Tab").is_none());
        assert!(fuzzy_match("xyz", "Close Tab").is_none());
        assert!(fuzzy_match("tabs", "Close Tab").is_none());
        assert!(fuzzy_match("a", "").is_none());
    }

    #[test]
    fn case_is_ignored() {
        assert!(fuzzy_match("CLOSE", "close tab").is_some());
        assert!(fuzzy_match("close", "CLOSE TAB").is_some());
        assert!(fuzzy_match("ÄB", "äb").is_some());
    }

    #[test]
    fn an_empty_query_matches_everything_without_highlights() {
        let m = fuzzy_match("", "Close Tab").expect("match");
        assert_eq!((m.score, m.positions), (0, vec![]));
        assert!(fuzzy_match("  ", "x").is_some());
        assert!(fuzzy_match("", "").is_some());
    }

    #[test]
    fn white_space_in_the_query_is_ignored() {
        assert!(fuzzy_match("zoom in", "ZoomIn").is_some());
        assert!(fuzzy_match(" close  tab ", "Close Tab").is_some());
    }

    #[test]
    fn consecutive_matches_rank_above_scattered_ones() {
        assert!(score("tab", "xtabx") > score("tab", "xtxaxbx"));
        assert!(score("split", "Split Right") > score("split", "Sxpxlxixt"));
    }

    #[test]
    fn word_starts_rank_above_middle_matches() {
        // "ct" at the start of "Close" and "Tab" beats the run in the middle of "Action".
        assert!(score("ct", "Close Tab") > score("ct", "Action"));
        assert!(score("ct", "Close Tab") > score("ct", "Direct"));
        assert!(score("ot", "Open Tab") > score("ot", "Cloth"));
    }

    #[test]
    fn matches_near_the_front_rank_above_later_ones() {
        assert!(score("zoom", "Zoom In") > score("zoom", "Reset Zoom"));
        assert!(score("tab", "tabular") > score("tab", "xxxxxxxtabular"));
    }

    #[test]
    fn the_best_arrangement_is_found_not_the_first() {
        // The first `a` is far from the `b`; the later `a` is adjacent to it.
        let m = fuzzy_match("ab", "xa....xab").expect("match");
        assert_eq!(m.positions, vec![7, 8]);
        let m = fuzzy_match("ct", "Close Tab").expect("match");
        assert_eq!(m.positions, vec![0, 6]);
    }

    #[test]
    fn positions_count_characters_not_bytes() {
        let m = fuzzy_match("분할", "오른쪽으로 분할").expect("match");
        assert_eq!(m.positions, vec![6, 7]);
        let m = fuzzy_match("ㅂ", "분할");
        assert!(m.is_none(), "a jamo is not a syllable");
        let m = fuzzy_match("a", "한글a").expect("match");
        assert_eq!(m.positions, vec![2]);
    }

    #[test]
    fn camel_case_humps_start_words() {
        assert!(score("gt", "goToPage") > score("gt", "agtb"));
    }

    #[test]
    fn highlight_ranges_cover_the_text_in_byte_ranges() {
        let text = "오른쪽 분할";
        let m = fuzzy_match("분할", text).expect("match");
        let ranges = highlight_ranges(text, &m.positions);
        let joined: String = ranges.iter().map(|(r, _)| &text[r.clone()]).collect();
        assert_eq!(joined, text);
        let lit: Vec<&str> = ranges
            .iter()
            .filter(|(_, on)| *on)
            .map(|(r, _)| &text[r.clone()])
            .collect();
        assert_eq!(lit, vec!["분할"]);
        // Plain text before the highlight is its own range.
        assert_eq!(ranges[0], (0..10, false));
    }

    #[test]
    fn highlight_ranges_merge_neighbours_and_handle_edges() {
        assert_eq!(highlight_ranges("", &[]), vec![]);
        assert_eq!(highlight_ranges("abc", &[]), vec![(0..3, false)]);
        assert_eq!(highlight_ranges("abc", &[0, 1, 2]), vec![(0..3, true)]);
        assert_eq!(
            highlight_ranges("abcd", &[1, 3]),
            vec![(0..1, false), (1..2, true), (2..3, false), (3..4, true)]
        );
    }
}
