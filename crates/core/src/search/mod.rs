//! Text search over a document (`docs/ARCHITECTURE.md` §4.8, SPEC §7.7).
//!
//! * [`normalize`] turns the text of a page into a search string and remembers which original
//!   character every position came from (ligatures, line-end hyphens and line breaks are
//!   folded, case optionally).
//! * [`matcher`] finds matches in that string (plain, whole word, regular expression), also
//!   across a page boundary, and maps them back to character ranges and rectangles.
//! * [`job`] runs the search on a background thread over a [`crate::DocumentHandle`].
//! * [`scope`] narrows a search to a part of the document, such as a chapter (FIND-16).

mod job;
mod matcher;
mod normalize;
mod scope;

use std::ops::Range;

use crate::geom::Rect;

pub use job::{SearchJob, SearchOptions};
pub use scope::{ScopeChoice, SearchScope, document_end, scope_choices, section_scope};

/// What to look for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchQuery {
    /// The text to find, or the regular expression when `regex` is set.
    pub text: String,
    /// Compare letters exactly. Off by default in the UI.
    pub case_sensitive: bool,
    /// A match must not touch a word character (letter, digit, underscore) on either side where
    /// it ends in one.
    pub whole_word: bool,
    /// Read `text` as a regular expression (Rust `regex` syntax). It is matched against the
    /// normalized page text; the pattern itself is not normalized.
    pub regex: bool,
}

/// The part of a match that lies on the page after the one where the match started.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchPart {
    /// Zero-based page index.
    pub page: u32,
    /// Original character indices on that page.
    pub chars: Range<u32>,
    /// One rectangle per line, in page space.
    pub rects: Vec<Rect>,
}

/// One occurrence of the query.
///
/// A match belongs to the page where it starts. When a phrase runs on to the next page the rest
/// is in `cont`.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchMatch {
    /// The page where the match starts.
    pub page: u32,
    /// Original character indices (of `page`) covered by the match. A match across a line-end
    /// hyphen covers the hyphen too.
    pub chars: Range<u32>,
    /// One rectangle per line, in page space.
    pub rects: Vec<Rect>,
    /// The part on the next page, if the match crosses the page boundary.
    pub cont: Option<MatchPart>,
    /// Normalized text before the match (about 40 characters, cut at a word boundary).
    pub before: String,
    /// The normalized text that matched.
    pub matched: String,
    /// Normalized text after the match (about 40 characters, cut at a word boundary). Near the
    /// end of a page it continues with the start of the next page.
    pub after: String,
}

/// The state of a search at one moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchSnapshot {
    /// All matches found so far, always in document order.
    pub matches: Vec<SearchMatch>,
    /// Pages processed so far (pages whose text could not be read count as processed).
    pub pages_done: u32,
    /// Pages to search: those of the document, or of the scope when the search has one; 0 until
    /// the document is known.
    pub pages_total: u32,
    /// Every page has been processed (or the search stopped at the match limit).
    pub finished: bool,
    /// The search stopped because it reached [`MAX_MATCHES`].
    pub truncated: bool,
}

/// The search stops after this many matches.
pub const MAX_MATCHES: usize = 100_000;

/// Why a search could not start.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    /// The query text is empty (or only white space in a plain search).
    #[error("the search text is empty")]
    Empty,
    /// The regular expression does not compile. The message says where.
    #[error("invalid regular expression: {0}")]
    InvalidRegex(String),
    /// The search thread could not be started.
    #[error("cannot start the search thread: {0}")]
    Thread(String),
}
