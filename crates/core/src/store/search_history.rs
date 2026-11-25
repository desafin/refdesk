//! `search_history.json`: the searches the user made, newest first (FIND-13, FIND-14).
//!
//! One history is shared by the in-document search bar and the search across all open
//! documents. All the rules (what counts as "the same search", the limits, replacing a typed
//! prefix) live here so that they are tested without a window (R-10).

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::atomic::{Loaded, atomic_write, load_with};
use super::{FORMAT_VERSION, json_version, lenient_items, warn_if_newer};
use crate::search::SearchQuery;

/// The most entries the history keeps; the oldest ones go.
pub const MAX_SEARCH_HISTORY: usize = 100;
/// The most characters of search text an entry keeps; longer text is cut.
pub const MAX_SEARCH_TEXT_CHARS: usize = 1000;

/// One remembered search.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchHistoryEntry {
    /// The search text (or the regular expression), trimmed and at most
    /// [`MAX_SEARCH_TEXT_CHARS`] characters.
    pub text: String,
    /// The "match case" option.
    pub case_sensitive: bool,
    /// The "whole word" option.
    pub whole_word: bool,
    /// The "regular expression" option.
    pub regex: bool,
    /// When the search was last used, in Unix seconds.
    pub last_used: i64,
}

impl SearchHistoryEntry {
    /// The query this entry stands for.
    pub fn to_query(&self) -> SearchQuery {
        SearchQuery {
            text: self.text.clone(),
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            regex: self.regex,
        }
    }

    /// Whether the entry is `query` (text compared after [`normalize_search_text`], and all
    /// three options equal).
    pub fn is(&self, query: &SearchQuery) -> bool {
        self.text == normalize_search_text(&query.text)
            && self.case_sensitive == query.case_sensitive
            && self.whole_word == query.whole_word
            && self.regex == query.regex
    }

    fn same_options(&self, query: &SearchQuery) -> bool {
        self.case_sensitive == query.case_sensitive
            && self.whole_word == query.whole_word
            && self.regex == query.regex
    }
}

/// The text a history entry keeps for `text`: trimmed, at most [`MAX_SEARCH_TEXT_CHARS`]
/// characters. Empty when there is nothing to remember. Applying it twice changes nothing more.
pub fn normalize_search_text(text: &str) -> String {
    let trimmed = text.trim();
    let cut: String = trimmed.chars().take(MAX_SEARCH_TEXT_CHARS).collect();
    cut.trim_end().to_string()
}

/// The remembered searches, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchHistory {
    /// Format version of the file.
    pub version: u32,
    entries: Vec<SearchHistoryEntry>,
}

impl Default for SearchHistory {
    fn default() -> Self {
        SearchHistory {
            version: FORMAT_VERSION,
            entries: Vec::new(),
        }
    }
}

impl SearchHistory {
    /// The entries, the most recently used first.
    pub fn entries(&self) -> &[SearchHistoryEntry] {
        &self.entries
    }

    /// No search is remembered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The most recently used entry.
    pub fn latest(&self) -> Option<&SearchHistoryEntry> {
        self.entries.first()
    }

    /// Whether the most recent entry is `query` (see [`SearchHistoryEntry::is`]).
    pub fn is_latest(&self, query: &SearchQuery) -> bool {
        self.latest().is_some_and(|e| e.is(query))
    }

    /// Remembers `query` as used at `now` (Unix seconds). The text is trimmed and cut to
    /// [`MAX_SEARCH_TEXT_CHARS`]; an empty text does nothing and gives `false`. An entry with
    /// the same text and the same three options is removed and the new one goes first. Entries
    /// beyond [`MAX_SEARCH_HISTORY`] are dropped, oldest first.
    pub fn record(&mut self, query: &SearchQuery, now: i64) -> bool {
        let text = normalize_search_text(&query.text);
        if text.is_empty() {
            return false;
        }
        let entry = SearchHistoryEntry {
            text,
            case_sensitive: query.case_sensitive,
            whole_word: query.whole_word,
            regex: query.regex,
            last_used: now,
        };
        self.entries
            .retain(|e| !(e.text == entry.text && e.same_options(query)));
        self.entries.insert(0, entry);
        self.entries.truncate(MAX_SEARCH_HISTORY);
        true
    }

    /// Removes the entry at `index`. `false` when there is none.
    pub fn remove(&mut self, index: usize) -> bool {
        if index < self.entries.len() {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Removes the first entry when it is `older`, `newer` has the same options, and `older`'s
    /// text is a proper beginning of `newer`'s: typing "hel" then "hello" leaves "hello" only
    /// (FIND-14). Gives whether an entry was removed.
    pub fn replace_latest_if_prefix(&mut self, older: &SearchQuery, newer: &SearchQuery) -> bool {
        let (old_text, new_text) = (
            normalize_search_text(&older.text),
            normalize_search_text(&newer.text),
        );
        let same_options = older.case_sensitive == newer.case_sensitive
            && older.whole_word == newer.whole_word
            && older.regex == newer.regex;
        let proper_prefix = !old_text.is_empty()
            && old_text.len() < new_text.len()
            && new_text.starts_with(&old_text);
        if same_options && proper_prefix && self.is_latest(older) {
            self.entries.remove(0);
            true
        } else {
            false
        }
    }

    /// Reads the history. Never fails: a missing or empty file gives an empty history; a file
    /// that is not a JSON object gives an empty history and is moved to `.bad` (R-8). Unknown
    /// fields are ignored. The limits are applied again, and empty or repeated entries dropped.
    pub fn load(path: &Path) -> SearchHistory {
        load_with(path, "search history", parse)
    }

    /// Writes the history with [`atomic_write`].
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut out = self.clone();
        out.version = FORMAT_VERSION;
        let bytes = serde_json::to_vec_pretty(&out).map_err(io::Error::other)?;
        atomic_write(path, &bytes)
    }

    /// Puts a hand-edited list in order: newest first, text normalized, empty and repeated
    /// entries dropped, cut to the limit.
    fn tidy(&mut self) {
        self.entries.sort_by_key(|e| std::cmp::Reverse(e.last_used));
        let mut kept: Vec<SearchHistoryEntry> = Vec::with_capacity(self.entries.len());
        for mut entry in self.entries.drain(..) {
            entry.text = normalize_search_text(&entry.text);
            if entry.text.is_empty() {
                continue;
            }
            let repeated = kept.iter().any(|k| {
                k.text == entry.text
                    && k.case_sensitive == entry.case_sensitive
                    && k.whole_word == entry.whole_word
                    && k.regex == entry.regex
            });
            if !repeated {
                kept.push(entry);
            }
        }
        kept.truncate(MAX_SEARCH_HISTORY);
        self.entries = kept;
    }
}

fn parse(bytes: &[u8]) -> Result<Loaded<SearchHistory>, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let obj = value
        .as_object()
        .ok_or_else(|| "top level is not an object".to_string())?;
    warn_if_newer("search history", json_version(obj));
    let mut dropped = Vec::new();
    let entries = lenient_items(obj.get("entries"), "entries", &mut dropped);
    let mut history = SearchHistory {
        version: FORMAT_VERSION,
        entries,
    };
    history.tidy();
    Ok(Loaded {
        value: history,
        dropped: (!dropped.is_empty()).then(|| dropped.join("; ")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::atomic::bad_path;
    use crate::store::paths::Paths;
    use std::fs;

    fn q(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.to_string(),
            ..SearchQuery::default()
        }
    }

    fn opts(text: &str, case: bool, word: bool, regex: bool) -> SearchQuery {
        SearchQuery {
            text: text.to_string(),
            case_sensitive: case,
            whole_word: word,
            regex,
        }
    }

    fn texts(h: &SearchHistory) -> Vec<&str> {
        h.entries().iter().map(|e| e.text.as_str()).collect()
    }

    #[test]
    fn history_file_is_in_the_state_dir() {
        let paths = Paths::under(Path::new("/r"));
        assert_eq!(
            paths.search_history_file(),
            std::path::PathBuf::from("/r/state/search_history.json")
        );
    }

    #[test]
    fn newest_comes_first() {
        let mut h = SearchHistory::default();
        assert!(h.is_empty());
        assert!(h.record(&q("a"), 1));
        assert!(h.record(&q("b"), 2));
        assert!(h.record(&q("c"), 3));
        assert_eq!(texts(&h), ["c", "b", "a"]);
        assert_eq!(h.latest().map(|e| e.last_used), Some(3));
    }

    #[test]
    fn the_same_search_is_one_entry_at_the_front() {
        let mut h = SearchHistory::default();
        h.record(&q("a"), 1);
        h.record(&q("b"), 2);
        h.record(&q("a"), 3);
        assert_eq!(texts(&h), ["a", "b"]);
        assert_eq!(h.entries()[0].last_used, 3);
        assert_eq!(h.entries().len(), 2);
    }

    #[test]
    fn different_options_are_different_entries() {
        let mut h = SearchHistory::default();
        h.record(&opts("a", false, false, false), 1);
        h.record(&opts("a", true, false, false), 2);
        h.record(&opts("a", false, true, false), 3);
        h.record(&opts("a", false, false, true), 4);
        assert_eq!(h.entries().len(), 4);
        h.record(&opts("a", true, false, false), 5);
        assert_eq!(h.entries().len(), 4);
        assert!(h.entries()[0].case_sensitive);
    }

    #[test]
    fn blank_text_is_ignored_and_edges_are_trimmed() {
        let mut h = SearchHistory::default();
        assert!(!h.record(&q(""), 1));
        assert!(!h.record(&q("  \t\n "), 1));
        assert!(h.is_empty());
        assert!(h.record(&q("  hello world \n"), 2));
        assert_eq!(texts(&h), ["hello world"]);
        // The same search with other spaces around it is the same entry.
        h.record(&q("hello world  "), 3);
        assert_eq!(h.entries().len(), 1);
        assert!(h.is_latest(&q(" hello world")));
        assert!(!h.is_latest(&q("hello")));
    }

    #[test]
    fn at_most_one_hundred_entries_oldest_go() {
        let mut h = SearchHistory::default();
        for i in 0..(MAX_SEARCH_HISTORY + 20) {
            h.record(&q(&format!("term{i}")), i as i64);
        }
        assert_eq!(h.entries().len(), MAX_SEARCH_HISTORY);
        assert_eq!(
            h.entries()[0].text,
            format!("term{}", MAX_SEARCH_HISTORY + 19)
        );
        assert_eq!(h.entries()[MAX_SEARCH_HISTORY - 1].text, "term20");
    }

    #[test]
    fn long_text_is_cut_to_one_thousand_characters() {
        let mut h = SearchHistory::default();
        let long = "가".repeat(1500);
        h.record(&q(&long), 1);
        assert_eq!(h.entries()[0].text.chars().count(), MAX_SEARCH_TEXT_CHARS);
        // The same long query matches its own entry.
        assert!(h.is_latest(&q(&long)));
        h.record(&q(&long), 2);
        assert_eq!(h.entries().len(), 1);
    }

    #[test]
    fn remove_and_clear() {
        let mut h = SearchHistory::default();
        h.record(&q("a"), 1);
        h.record(&q("b"), 2);
        h.record(&q("c"), 3);
        assert!(h.remove(1));
        assert_eq!(texts(&h), ["c", "a"]);
        assert!(!h.remove(2));
        assert!(!h.remove(usize::MAX));
        assert_eq!(texts(&h), ["c", "a"]);
        h.clear();
        assert!(h.is_empty());
        assert!(h.latest().is_none());
    }

    #[test]
    fn entry_gives_its_query_back() {
        let mut h = SearchHistory::default();
        h.record(&opts("  x.*  ", true, false, true), 1);
        assert_eq!(h.entries()[0].to_query(), opts("x.*", true, false, true));
    }

    #[test]
    fn a_typed_beginning_is_replaced() {
        let mut h = SearchHistory::default();
        h.record(&q("hel"), 1);
        assert!(h.replace_latest_if_prefix(&q("hel"), &q("hello")));
        assert!(h.is_empty());
        h.record(&q("hello"), 2);
        assert_eq!(texts(&h), ["hello"]);
    }

    #[test]
    fn replacing_needs_a_proper_prefix_with_the_same_options_at_the_front() {
        let mut h = SearchHistory::default();
        h.record(&q("hel"), 1);
        // Same text: not a proper beginning.
        assert!(!h.replace_latest_if_prefix(&q("hel"), &q("hel")));
        // Not a beginning.
        assert!(!h.replace_latest_if_prefix(&q("hel"), &q("yellow")));
        // Other options on the new query.
        assert!(!h.replace_latest_if_prefix(&q("hel"), &opts("hello", true, false, false)));
        // The older query has other options than the entry.
        assert!(!h.replace_latest_if_prefix(
            &opts("hel", true, false, false),
            &opts("hello", true, false, false)
        ));
        assert_eq!(texts(&h), ["hel"]);
        // Not at the front.
        h.record(&q("other"), 2);
        assert!(!h.replace_latest_if_prefix(&q("hel"), &q("hello")));
        assert_eq!(texts(&h), ["other", "hel"]);
        // An empty older text is nothing to replace.
        assert!(!h.replace_latest_if_prefix(&q(""), &q("other")));
        // The shorter one is not a prefix of the longer one the wrong way round.
        assert!(!h.replace_latest_if_prefix(&q("other"), &q("oth")));
    }

    #[test]
    fn roundtrip_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/search_history.json");
        let mut h = SearchHistory::default();
        h.record(&opts("SWS_EM_01004", true, true, false), 10);
        h.record(&opts("fo+", false, false, true), 20);
        h.save(&path).unwrap();
        assert_eq!(SearchHistory::load(&path), h);
        assert!(!bad_path(&path).exists());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\""), "{text}");
    }

    #[test]
    fn a_missing_or_empty_file_gives_an_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search_history.json");
        assert!(SearchHistory::load(&path).is_empty());
        fs::write(&path, " \n").unwrap();
        assert!(SearchHistory::load(&path).is_empty());
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn a_damaged_file_gives_an_empty_history_and_a_bad_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search_history.json");
        let junk = b"{ \"entries\": [ {";
        fs::write(&path, junk).unwrap();
        assert!(SearchHistory::load(&path).is_empty());
        assert_eq!(fs::read(bad_path(&path)).unwrap(), junk);
        assert!(!path.exists());

        fs::write(&path, "[1]").unwrap();
        assert!(SearchHistory::load(&path).is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search_history.json");
        fs::write(
            &path,
            r#"{"version": 1, "future": [1], "entries": [
                {"text": "abc", "last_used": 5, "mystery": true},
                {"text": "def", "regex": true},
                7
            ]}"#,
        )
        .unwrap();
        let h = SearchHistory::load(&path);
        assert_eq!(texts(&h), ["abc", "def"]);
        assert!(!h.entries()[0].regex);
        assert_eq!(h.entries()[0].last_used, 5);
        assert!(h.entries()[1].regex);
        assert_eq!(h.entries()[1].last_used, 0);
        // The one bad item is noted; the original is kept as `.bad`.
        assert!(bad_path(&path).exists());
    }

    #[test]
    fn a_file_over_the_limits_or_with_repeats_is_tidied_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search_history.json");
        let mut items = Vec::new();
        for i in 0..150 {
            items.push(format!(r#"{{"text": "t{i}", "last_used": {i}}}"#));
        }
        items.push(r#"{"text": "   ", "last_used": 1000}"#.to_string());
        items.push(r#"{"text": "", "last_used": 999}"#.to_string());
        // Repeats of t149 (the newest) and of t3 (which falls under the limit anyway).
        items.push(r#"{"text": " t149 ", "last_used": 10}"#.to_string());
        items.push(format!(
            r#"{{"text": "{}", "last_used": 998}}"#,
            "x".repeat(1500)
        ));
        fs::write(
            &path,
            format!(r#"{{"version": 1, "entries": [{}]}}"#, items.join(",")),
        )
        .unwrap();
        let h = SearchHistory::load(&path);
        assert_eq!(h.entries().len(), MAX_SEARCH_HISTORY);
        // The long one (998) is the newest valid entry, cut to the limit.
        assert_eq!(h.entries()[0].text.len(), MAX_SEARCH_TEXT_CHARS);
        assert_eq!(h.entries()[1].text, "t149");
        assert_eq!(h.entries().iter().filter(|e| e.text == "t149").count(), 1);
        assert!(h.entries().iter().all(|e| !e.text.trim().is_empty()));
        let mut seen = std::collections::HashSet::new();
        assert!(h.entries().iter().all(|e| seen.insert(e.text.clone())));
        // Nothing was damaged enough to need a `.bad` copy.
        assert!(!bad_path(&path).exists());
    }
}
