//! The text model of a page and text selection (types only). See `docs/ARCHITECTURE.md` §4.7.
//!
//! The methods of [`PageText`] are added by the text work package (WP-50).

use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::geom::{Quad, Rect};

/// One character with its position on the page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextChar {
    /// The character.
    pub ch: char,
    /// Its glyph area in page space.
    pub quad: Quad,
    /// Font size in points.
    pub size: f32,
}

/// A line of text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextLine {
    /// Indices into [`PageText::chars`].
    pub chars: Range<u32>,
    /// Bounding rectangle in page space.
    pub bounds: Rect,
}

/// A block (paragraph or column piece) of text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    /// Indices into [`PageText::lines`].
    pub lines: Range<u32>,
    /// Bounding rectangle in page space.
    pub bounds: Rect,
}

/// All text of one page, in the reading order the engine produced.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PageText {
    /// Characters in reading order.
    pub chars: Vec<TextChar>,
    /// Lines, each covering a range of `chars`.
    pub lines: Vec<TextLine>,
    /// Blocks, each covering a range of `lines`.
    pub blocks: Vec<TextBlock>,
}

/// A character position in a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TextPos {
    /// Zero-based page index.
    pub page: u32,
    /// Index into that page's [`PageText::chars`].
    pub ch: u32,
}

/// A text selection. It may span pages, and `focus` may come before `anchor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// Where the selection started.
    pub anchor: TextPos,
    /// Where the pointer is now.
    pub focus: TextPos,
}

/// How selected text is turned into a string.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyOptions {
    /// Join the lines of a paragraph into one line.
    pub join_lines: bool,
    /// Remove hyphens at line ends and join the word.
    pub dehyphenate: bool,
}
