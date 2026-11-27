//! The text model of a page and text selection. See `docs/ARCHITECTURE.md` §4.7.
//!
//! A selection is a range of character indices, not a screen rectangle, so that dragging inside a
//! two-column page selects one column only. Search and document comparison use the same model.

use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::geom::{Point, Quad, Rect};

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
///
/// The characters include the spaces between words, but no line-break characters: the border
/// between two lines is the line break. All coordinates are in page space.
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
///
/// `TextPos::ch` is read as a boundary position (`0 ..= char count`): the selection contains the
/// characters from the earlier position up to, but not including, the later one.
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

/// How far (in line heights) a point may be from a line and still count as "on" a character.
const CHAR_AT_MAX_LINE_HEIGHTS: f32 = 2.0;

/// Hyphen characters that may be removed when a word is broken at a line end.
fn is_hyphen(c: char) -> bool {
    matches!(c, '-' | '\u{2010}' | '\u{00AD}')
}

/// Horizontal distance from `x` to the span of `r` (0 inside).
fn dx_to(r: &Rect, x: f32) -> f32 {
    (r.x0 - x).max(0.0).max(x - r.x1)
}

/// Vertical distance from `y` to the span of `r` (0 inside).
fn dy_to(r: &Rect, y: f32) -> f32 {
    (r.y0 - y).max(0.0).max(y - r.y1)
}

impl PageText {
    /// The characters of `line`, clamped to what exists.
    fn line_chars(&self, line: &TextLine) -> Range<usize> {
        self.clamp(&line.chars)
    }

    /// Clamps a range of character indices to `0 ..= chars.len()` and makes it well-formed.
    fn clamp(&self, r: &Range<u32>) -> Range<usize> {
        let len = self.chars.len();
        let end = (r.end as usize).min(len);
        let start = (r.start as usize).min(end);
        start..end
    }

    fn char_value(&self, i: usize) -> char {
        self.chars.get(i).map_or('\0', |c| c.ch)
    }

    fn char_bounds(&self, i: usize) -> Option<Rect> {
        self.chars.get(i).map(|c| c.quad.bounds())
    }

    /// The character of `range` whose box is horizontally closest to `x`, with that distance.
    /// The first one wins on ties.
    fn closest_in(&self, range: Range<usize>, x: f32) -> Option<(usize, f32)> {
        let mut best: Option<(usize, f32)> = None;
        for i in range {
            let Some(b) = self.char_bounds(i) else {
                continue;
            };
            let d = dx_to(&b, x);
            if best.is_none_or(|(_, bd)| d.total_cmp(&bd).is_lt()) {
                best = Some((i, d));
            }
        }
        best
    }

    /// The character that contains `p`. Failing that, the horizontally closest character of a line
    /// that overlaps `p` vertically, including the left and right margins of the line, unless it
    /// is farther than twice the line height. `None` between lines, over figures, and on a page
    /// without text.
    ///
    /// Use this to ask "is the pointer over text?" (links, double click).
    pub fn char_at(&self, p: Point) -> Option<u32> {
        // A character box that contains the point wins outright.
        for line in &self.lines {
            for i in self.line_chars(line) {
                if self.char_bounds(i).is_some_and(|b| b.contains(p)) {
                    return Some(i as u32);
                }
            }
        }
        // Otherwise the closest character on a line the point shares a row with.
        let mut best: Option<(usize, f32)> = None;
        for line in &self.lines {
            let b = &line.bounds;
            if p.y < b.y0 || p.y > b.y1 {
                continue;
            }
            let Some((i, d)) = self.closest_in(self.line_chars(line), p.x) else {
                continue;
            };
            if d > CHAR_AT_MAX_LINE_HEIGHTS * b.height() {
                continue;
            }
            if best.is_none_or(|(_, bd)| d.total_cmp(&bd).is_lt()) {
                best = Some((i, d));
            }
        }
        best.map(|(i, _)| i as u32)
    }

    /// The closest character to `p`, always, if the page has any text. The vertically closest
    /// line is chosen first (ties go to the horizontally closer line, so a two-column page works),
    /// then the horizontally closest character in it. Use this for the end points of a drag.
    pub fn nearest_char(&self, p: Point) -> Option<u32> {
        let mut best: Option<(&TextLine, f32, f32)> = None;
        for line in &self.lines {
            if self.line_chars(line).is_empty() {
                continue;
            }
            let dy = dy_to(&line.bounds, p.y);
            let dx = dx_to(&line.bounds, p.x);
            let better =
                best.is_none_or(|(_, by, bx)| dy.total_cmp(&by).then(dx.total_cmp(&bx)).is_lt());
            if better {
                best = Some((line, dy, dx));
            }
        }
        let (line, _, _) = best?;
        self.closest_in(self.line_chars(line), p.x)
            .map(|(i, _)| i as u32)
    }

    /// The boundary position (`0 ..= chars.len()`) a drag at `p` selects up to. It is the index
    /// of [`PageText::nearest_char`], plus one when `p` is right of the middle of that character.
    pub fn caret_at(&self, p: Point) -> Option<u32> {
        let i = self.nearest_char(p)?;
        let mid = self
            .char_bounds(i as usize)
            .map_or(p.x, |b| (b.x0 + b.x1) / 2.0);
        Some(if p.x > mid { i + 1 } else { i })
    }

    /// Whether character `i` belongs to a word, given the line `start..end` it is in. Word
    /// characters are letters, digits and `_`; a dot between two digits ("7.3.2") also counts.
    fn in_word(&self, i: usize, start: usize, end: usize) -> bool {
        let c = self.char_value(i);
        if c.is_alphanumeric() || c == '_' {
            return true;
        }
        c == '.'
            && i > start
            && i + 1 < end
            && self.char_value(i - 1).is_numeric()
            && self.char_value(i + 1).is_numeric()
    }

    /// The word around character `ch`. A word never crosses a line. On punctuation and spaces the
    /// result is that single character; an index past the end gives an empty range at the end.
    pub fn word_at(&self, ch: u32) -> Range<u32> {
        let len = self.chars.len() as u32;
        if ch >= len {
            return len..len;
        }
        let line = self.line_of(ch);
        let (start, end) = (line.start as usize, line.end as usize);
        let i = ch as usize;
        if !self.in_word(i, start, end) {
            return ch..ch + 1;
        }
        let mut a = i;
        while a > start && self.in_word(a - 1, start, end) {
            a -= 1;
        }
        let mut b = i + 1;
        while b < end && self.in_word(b, start, end) {
            b += 1;
        }
        a as u32..b as u32
    }

    /// Index of the line that contains character `ch`.
    fn line_index_of(&self, ch: u32) -> Option<usize> {
        let ch = ch as usize;
        self.lines
            .iter()
            .position(|l| self.line_chars(l).contains(&ch))
    }

    /// The characters of the line that contains `ch`. If no line contains it (inconsistent data)
    /// the result is the character alone; an index past the end gives an empty range at the end.
    pub fn line_of(&self, ch: u32) -> Range<u32> {
        let len = self.chars.len() as u32;
        if ch >= len {
            return len..len;
        }
        match self.line_index_of(ch).and_then(|i| self.lines.get(i)) {
            Some(l) => {
                let r = self.line_chars(l);
                r.start as u32..r.end as u32
            }
            None => ch..ch + 1,
        }
    }

    /// The characters of the whole block (paragraph) that contains `ch`. Falls back to
    /// [`PageText::line_of`] if no block contains the line.
    pub fn block_of(&self, ch: u32) -> Range<u32> {
        let Some(li) = self.line_index_of(ch) else {
            return self.line_of(ch);
        };
        let block = self
            .blocks
            .iter()
            .find(|b| (b.lines.start as usize..b.lines.end as usize).contains(&li));
        let Some(block) = block else {
            return self.line_of(ch);
        };
        let first = (block.lines.start as usize).min(self.lines.len());
        let last = (block.lines.end as usize).min(self.lines.len());
        let lines = self.lines.get(first..last).unwrap_or(&[]);
        let start = lines.iter().map(|l| self.line_chars(l).start).min();
        let end = lines.iter().map(|l| self.line_chars(l).end).max();
        match (start, end) {
            (Some(s), Some(e)) => s as u32..e as u32,
            _ => self.line_of(ch),
        }
    }

    /// One rectangle per line the range touches: the union of the boxes of the characters of that
    /// line that are in the range. An empty range gives an empty list; indices past the end are
    /// cut off.
    pub fn rects(&self, range: Range<u32>) -> Vec<Rect> {
        let range = self.clamp(&range);
        let mut out = Vec::new();
        for line in &self.lines {
            let lr = self.line_chars(line);
            let sub = lr.start.max(range.start)..lr.end.min(range.end);
            let mut acc: Option<Rect> = None;
            for i in sub {
                if let Some(b) = self.char_bounds(i) {
                    acc = Some(acc.map_or(b, |a| a.union(&b)));
                }
            }
            out.extend(acc);
        }
        out
    }

    /// The text of the characters in `range` (cut to what exists).
    ///
    /// Without `join_lines` every line end becomes `\n`. With it, a line end inside a block
    /// becomes one space (none if the line already ends with a space) and a block end becomes
    /// `\n`; lines that belong to no block are never joined. With `dehyphenate` as well, a line
    /// that ends in a letter and a hyphen, followed by a line that starts with a lowercase
    /// letter, loses the hyphen and is joined without a space. Trailing whitespace of the result
    /// is removed.
    pub fn text(&self, range: Range<u32>, opts: CopyOptions) -> String {
        let range = self.clamp(&range);
        let mut line_block: Vec<Option<usize>> = vec![None; self.lines.len()];
        for (bi, b) in self.blocks.iter().enumerate() {
            let first = (b.lines.start as usize).min(line_block.len());
            let last = (b.lines.end as usize).min(line_block.len());
            if let Some(slots) = line_block.get_mut(first..last) {
                slots.fill(Some(bi));
            }
        }

        let mut out = String::new();
        let mut prev_block: Option<Option<usize>> = None;
        for (li, line) in self.lines.iter().enumerate() {
            let lr = self.line_chars(line);
            let sub = lr.start.max(range.start)..lr.end.min(range.end);
            if sub.is_empty() {
                continue;
            }
            let piece: String = sub.map(|i| self.char_value(i)).collect();
            let block = line_block.get(li).copied().flatten();
            if let Some(pb) = prev_block {
                let same = block.is_some() && pb == block;
                if !opts.join_lines {
                    out.push('\n');
                } else if !same {
                    let kept = out.trim_end_matches([' ', '\t']).len();
                    out.truncate(kept);
                    out.push('\n');
                } else if opts.dehyphenate && Self::hyphen_break(&out, &piece) {
                    out.pop();
                } else if !out.ends_with(char::is_whitespace) {
                    out.push(' ');
                }
            }
            out.push_str(&piece);
            prev_block = Some(block);
        }
        out.truncate(out.trim_end().len());
        out
    }

    /// Whether `before` ends in "letter-" and `next` starts with a lowercase letter.
    fn hyphen_break(before: &str, next: &str) -> bool {
        let mut rev = before.chars().rev();
        let hyphen = rev.next().is_some_and(is_hyphen);
        let letter = rev.next().is_some_and(char::is_alphabetic);
        hyphen && letter && next.chars().next().is_some_and(char::is_lowercase)
    }
}

impl Selection {
    /// The two ends in document order (page, then character index).
    pub fn ordered(&self) -> (TextPos, TextPos) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    /// Whether the selection contains nothing (both ends are the same position).
    pub fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }

    /// The pages the selection touches, first to last.
    pub fn pages(&self) -> RangeInclusive<u32> {
        let (a, b) = self.ordered();
        a.page..=b.page
    }

    /// The part of the selection on `page`, as a range of character indices of a page that has
    /// `page_chars` characters. Pages between the ends are covered completely. `None` for pages
    /// outside the selection and when nothing on the page is selected (an empty range).
    pub fn range_on(&self, page: u32, page_chars: u32) -> Option<Range<u32>> {
        let (a, b) = self.ordered();
        if page < a.page || page > b.page {
            return None;
        }
        let start = if page == a.page {
            a.ch.min(page_chars)
        } else {
            0
        };
        let end = if page == b.page {
            b.ch.min(page_chars)
        } else {
            page_chars
        };
        (start < end).then_some(start..end)
    }
}

/// The text of a selection that may span pages. `page` gives the text of a page, or `None` if it
/// is not available yet (that page is skipped). The texts of different pages are separated by
/// `\n`.
pub fn selection_text(
    sel: &Selection,
    page: impl Fn(u32) -> Option<Arc<PageText>>,
    opts: CopyOptions,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for n in sel.pages() {
        let Some(pt) = page(n) else {
            continue;
        };
        let Some(range) = sel.range_on(n, pt.chars.len() as u32) else {
            continue;
        };
        let t = pt.text(range, opts);
        if !t.is_empty() {
            parts.push(t);
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CW: f32 = 10.0; // width of a character
    const BOX_H: f32 = 12.0; // height of a character box and of a line
    const PITCH: f32 = 16.0; // distance between the tops of two lines
    const BLOCK_GAP: f32 = 8.0; // extra space before a new block

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::new(x0, y0, x1, y1)
    }

    /// Appends one block made of `lines` to `pt`; the first line has its top-left at `(x0, y0)`.
    /// Returns the y of the top of the next line after the block.
    fn add_block(pt: &mut PageText, x0: f32, y0: f32, lines: &[&str]) -> f32 {
        let line_start = pt.lines.len() as u32;
        let mut y = y0;
        let mut block_bounds: Option<Rect> = None;
        for text in lines {
            let first = pt.chars.len() as u32;
            for (i, ch) in text.chars().enumerate() {
                let x = x0 + i as f32 * CW;
                let (l, r, t, b) = (x, x + CW, y, y + BOX_H);
                pt.chars.push(TextChar {
                    ch,
                    quad: Quad {
                        ul: Point::new(l, t),
                        ur: Point::new(r, t),
                        ll: Point::new(l, b),
                        lr: Point::new(r, b),
                    },
                    size: 10.0,
                });
            }
            let last = pt.chars.len() as u32;
            let bounds = rect(x0, y, x0 + text.chars().count() as f32 * CW, y + BOX_H);
            block_bounds = Some(block_bounds.map_or(bounds, |a| a.union(&bounds)));
            pt.lines.push(TextLine {
                chars: first..last,
                bounds,
            });
            y += PITCH;
        }
        pt.blocks.push(TextBlock {
            lines: line_start..pt.lines.len() as u32,
            bounds: block_bounds.unwrap_or_default(),
        });
        y + BLOCK_GAP
    }

    /// A page whose blocks are stacked at `(x0, y0)`, one slice of lines per block.
    fn page_at(x0: f32, y0: f32, blocks: &[&[&str]]) -> PageText {
        let mut pt = PageText::default();
        let mut y = y0;
        for b in blocks {
            y = add_block(&mut pt, x0, y, b);
        }
        pt
    }

    /// A page at `(72, 72)`.
    fn page(blocks: &[&[&str]]) -> PageText {
        page_at(72.0, 72.0, blocks)
    }

    /// Index of column `col` of line `line`.
    fn at(pt: &PageText, line: usize, col: u32) -> u32 {
        pt.lines[line].chars.start + col
    }

    /// Centre point of the box of character `i`.
    fn centre(pt: &PageText, i: u32) -> Point {
        let b = pt.chars[i as usize].quad.bounds();
        Point::new((b.x0 + b.x1) / 2.0, (b.y0 + b.y1) / 2.0)
    }

    const PLAIN: CopyOptions = CopyOptions {
        join_lines: false,
        dehyphenate: false,
    };
    const JOIN: CopyOptions = CopyOptions {
        join_lines: true,
        dehyphenate: false,
    };
    const JOIN_DEHYPHEN: CopyOptions = CopyOptions {
        join_lines: true,
        dehyphenate: true,
    };

    fn pos(page: u32, ch: u32) -> TextPos {
        TextPos { page, ch }
    }

    // ---- char_at ----

    #[test]
    fn char_at_on_a_character() {
        let pt = page(&[&["hello world"]]);
        // Line top is 72; the box of column 3 spans x 102..112.
        assert_eq!(pt.char_at(Point::new(105.0, 78.0)), Some(3));
        assert_eq!(pt.char_at(Point::new(72.0, 72.0)), Some(0), "corner");
    }

    #[test]
    fn char_at_right_margin_near_and_far() {
        let pt = page(&[&["hello"]]); // x 72..122, height 12
        let y = 78.0;
        assert_eq!(
            pt.char_at(Point::new(130.0, y)),
            Some(4),
            "8pt past the end"
        );
        assert_eq!(
            pt.char_at(Point::new(122.0 + 24.0, y)),
            Some(4),
            "exactly 2 heights"
        );
        assert_eq!(pt.char_at(Point::new(122.0 + 24.5, y)), None, "too far");
        assert_eq!(pt.char_at(Point::new(500.0, y)), None);
    }

    #[test]
    fn char_at_left_margin() {
        let pt = page(&[&["hello"]]);
        assert_eq!(pt.char_at(Point::new(60.0, 78.0)), Some(0));
        assert_eq!(pt.char_at(Point::new(40.0, 78.0)), None);
    }

    #[test]
    fn char_at_gap_between_lines_is_none() {
        let pt = page(&[&["hello", "world"]]);
        // Line 0 spans y 72..84, line 1 spans 88..100.
        assert_eq!(pt.char_at(Point::new(80.0, 86.0)), None);
        assert_eq!(pt.char_at(Point::new(80.0, 40.0)), None, "above the text");
        assert_eq!(pt.char_at(Point::new(80.0, 200.0)), None, "below the text");
    }

    #[test]
    fn char_at_page_without_text_is_none() {
        let pt = PageText::default();
        assert_eq!(pt.char_at(Point::new(10.0, 10.0)), None);
    }

    #[test]
    fn char_at_two_columns_picks_the_column_of_the_point() {
        let mut pt = page_at(72.0, 72.0, &[&["left col"]]);
        add_block(&mut pt, 300.0, 72.0, &["right col"]);
        assert_eq!(pt.char_at(Point::new(310.0, 78.0)), Some(8));
        // In the gap, closer to the right column (300) than to the left one (152).
        assert_eq!(pt.char_at(Point::new(290.0, 78.0)), Some(8));
    }

    // ---- nearest_char / caret_at ----

    #[test]
    fn nearest_char_outside_the_text() {
        let pt = page(&[&["hello", "world"]]);
        assert_eq!(pt.nearest_char(Point::new(0.0, 0.0)), Some(0), "top left");
        assert_eq!(
            pt.nearest_char(Point::new(90.0, 0.0)),
            Some(1),
            "above, over col 1"
        );
        assert_eq!(
            pt.nearest_char(Point::new(5.0, 80.0)),
            Some(0),
            "left of line 0"
        );
        assert_eq!(
            pt.nearest_char(Point::new(5.0, 95.0)),
            Some(5),
            "left of line 1"
        );
        assert_eq!(
            pt.nearest_char(Point::new(500.0, 500.0)),
            Some(9),
            "bottom right"
        );
        assert_eq!(
            pt.nearest_char(Point::new(90.0, 500.0)),
            Some(6),
            "below, over col 1"
        );
    }

    #[test]
    fn nearest_char_in_the_gap_between_lines_picks_the_closer_line() {
        let pt = page(&[&["hello", "world"]]);
        assert_eq!(pt.nearest_char(Point::new(80.0, 85.0)), Some(0));
        assert_eq!(pt.nearest_char(Point::new(80.0, 87.0)), Some(5));
    }

    #[test]
    fn nearest_char_without_text_is_none() {
        assert_eq!(PageText::default().nearest_char(Point::new(1.0, 1.0)), None);
        assert_eq!(PageText::default().caret_at(Point::new(1.0, 1.0)), None);
    }

    #[test]
    fn caret_at_left_and_right_half_of_a_character() {
        let pt = page(&[&["hello"]]); // column 2 spans x 92..102, middle 97
        let y = 78.0;
        assert_eq!(pt.caret_at(Point::new(93.0, y)), Some(2), "left half");
        assert_eq!(pt.caret_at(Point::new(96.9, y)), Some(2));
        assert_eq!(pt.caret_at(Point::new(97.1, y)), Some(3), "right half");
        assert_eq!(pt.caret_at(Point::new(101.0, y)), Some(3));
    }

    #[test]
    fn caret_at_page_corners_and_margins() {
        let pt = page(&[&["hello", "world"]]);
        assert_eq!(pt.caret_at(Point::new(0.0, 0.0)), Some(0), "top left");
        assert_eq!(
            pt.caret_at(Point::new(500.0, 500.0)),
            Some(10),
            "after the last char"
        );
        assert_eq!(
            pt.caret_at(Point::new(500.0, 78.0)),
            Some(5),
            "right of line 0"
        );
        assert_eq!(
            pt.caret_at(Point::new(0.0, 95.0)),
            Some(5),
            "left of line 1"
        );
    }

    // ---- two columns ----

    #[test]
    fn two_column_range_stays_in_the_left_column() {
        // Reading order: the whole left column, then the whole right column.
        let mut pt = page_at(72.0, 72.0, &[&["left one", "left two", "left three"]]);
        add_block(
            &mut pt,
            320.0,
            72.0,
            &["right one", "right two", "right three"],
        );
        // Drag from line 0 of the left column to line 2 of the left column, ending the pointer
        // far over in the right column's x range on the same row as line 2.
        let a = pt.caret_at(Point::new(80.0, 78.0)).unwrap_or(0);
        let b = pt.caret_at(Point::new(150.0, 112.0)).unwrap_or(0);
        assert!(a < b);
        let rects = pt.rects(a..b);
        assert_eq!(rects.len(), 3);
        for r in &rects {
            assert!(
                r.x0 >= 72.0 && r.x1 <= 72.0 + 10.0 * CW,
                "rect {r:?} left the column"
            );
        }
    }

    #[test]
    fn two_column_drag_end_in_the_gutter_picks_nearest_column() {
        let mut pt = page_at(72.0, 72.0, &[&["left one", "left two"]]);
        add_block(&mut pt, 320.0, 72.0, &["right one", "right two"]);
        // Row of line 1 (y 88..100), x in the gutter, closer to the left column (ends at 152).
        let end = pt.caret_at(Point::new(200.0, 94.0));
        assert_eq!(end, Some(16), "after the last char of 'left two'");
    }

    // ---- word_at ----

    fn word(pt: &PageText, line: usize, col: u32) -> String {
        let r = pt.word_at(at(pt, line, col));
        pt.chars[r.start as usize..r.end as usize]
            .iter()
            .map(|c| c.ch)
            .collect()
    }

    #[test]
    fn word_at_ordinary_word() {
        let pt = page(&[&["the quick fox"]]);
        assert_eq!(word(&pt, 0, 5), "quick");
        assert_eq!(word(&pt, 0, 4), "quick", "first letter");
        assert_eq!(word(&pt, 0, 8), "quick", "last letter");
        assert_eq!(pt.word_at(5), 4..9);
    }

    #[test]
    fn word_at_stops_before_the_full_stop() {
        let pt = page(&[&["It ends here. Next"]]);
        assert_eq!(word(&pt, 0, 9), "here");
        assert_eq!(pt.word_at(at(&pt, 0, 12)), 12..13, "the dot itself");
    }

    #[test]
    fn word_at_dotted_number_is_one_word() {
        let pt = page(&[&["see 7.3.2 for"]]);
        assert_eq!(word(&pt, 0, 4), "7.3.2");
        assert_eq!(word(&pt, 0, 5), "7.3.2", "on the dot");
        assert_eq!(word(&pt, 0, 8), "7.3.2");
    }

    #[test]
    fn word_at_trailing_dot_after_a_number_is_not_included() {
        let pt = page(&[&["in 7.3. Then"]]);
        assert_eq!(word(&pt, 0, 3), "7.3");
        assert_eq!(pt.word_at(at(&pt, 0, 6)), 6..7);
    }

    #[test]
    fn word_at_identifier_with_underscores() {
        let pt = page(&[&["id SWS_EM_01004 ok"]]);
        assert_eq!(word(&pt, 0, 3), "SWS_EM_01004");
        assert_eq!(word(&pt, 0, 9), "SWS_EM_01004");
    }

    #[test]
    fn word_at_two_words_around_a_dot() {
        let pt = page(&[&["foo.bar"]]);
        assert_eq!(word(&pt, 0, 1), "foo");
        assert_eq!(word(&pt, 0, 5), "bar");
        assert_eq!(pt.word_at(3), 3..4);
    }

    #[test]
    fn word_at_space_and_punctuation_is_one_char() {
        let pt = page(&[&["a  (b)"]]);
        assert_eq!(pt.word_at(1), 1..2, "space");
        assert_eq!(pt.word_at(2), 2..3, "second space");
        assert_eq!(pt.word_at(3), 3..4, "paren");
    }

    #[test]
    fn word_at_does_not_cross_lines() {
        let pt = page(&[&["state", "machine"]]);
        assert_eq!(word(&pt, 0, 4), "state", "word at the end of a line");
        assert_eq!(word(&pt, 1, 0), "machine", "word at the start of a line");
        assert_eq!(pt.word_at(4), 0..5);
        assert_eq!(pt.word_at(5), 5..12);
    }

    #[test]
    fn word_at_unicode_letters_and_out_of_range() {
        let pt = page(&[&["한글 text"]]);
        assert_eq!(pt.word_at(1), 0..2);
        assert_eq!(pt.word_at(99), 7..7);
    }

    // ---- line_of / block_of ----

    #[test]
    fn line_of_gives_the_whole_line() {
        let pt = page(&[&["hello", "big", "world"]]);
        assert_eq!(pt.line_of(0), 0..5);
        assert_eq!(pt.line_of(4), 0..5);
        assert_eq!(pt.line_of(5), 5..8);
        assert_eq!(pt.line_of(12), 8..13);
        assert_eq!(pt.line_of(13), 13..13, "past the end");
    }

    #[test]
    fn block_of_gives_the_whole_paragraph() {
        let pt = page(&[&["one two", "three"], &["four"], &["five", "six"]]);
        // Characters: 0..7 | 7..12 | 12..16 | 16..20 | 20..23
        assert_eq!(pt.block_of(0), 0..12);
        assert_eq!(pt.block_of(11), 0..12);
        assert_eq!(pt.block_of(12), 12..16);
        assert_eq!(pt.block_of(22), 16..23);
        assert_eq!(pt.block_of(99), 23..23);
    }

    // ---- rects ----

    #[test]
    fn rects_part_of_one_line() {
        let pt = page(&[&["hello world"]]);
        let r = pt.rects(2..5);
        assert_eq!(r, vec![rect(92.0, 72.0, 122.0, 84.0)]);
    }

    #[test]
    fn rects_over_three_lines_gives_three_rects() {
        let pt = page(&[&["hello", "big world", "end"]]);
        // Characters: 0..5 | 5..14 | 14..17. From col 3 of line 0 to col 2 of line 2.
        let r = pt.rects(3..16);
        assert_eq!(
            r,
            vec![
                rect(102.0, 72.0, 122.0, 84.0),
                rect(72.0, 88.0, 162.0, 100.0),
                rect(72.0, 104.0, 92.0, 116.0),
            ]
        );
    }

    #[test]
    fn rects_empty_and_out_of_range() {
        let pt = page(&[&["hello"]]);
        assert!(pt.rects(2..2).is_empty());
        let (hi, lo) = (4, 2);
        assert!(pt.rects(hi..lo).is_empty(), "reversed range");
        assert!(pt.rects(5..9).is_empty(), "entirely past the end");
        assert_eq!(
            pt.rects(3..99),
            vec![rect(102.0, 72.0, 122.0, 84.0)],
            "cut off"
        );
        assert!(PageText::default().rects(0..3).is_empty());
    }

    // ---- text ----

    #[test]
    fn text_without_joining_keeps_every_line_break() {
        let pt = page(&[&["one two", "three"], &["four"]]);
        assert_eq!(pt.text(0..16, PLAIN), "one two\nthree\nfour");
    }

    #[test]
    fn text_part_of_a_line_and_trailing_whitespace_is_removed() {
        let pt = page(&[&["one two ", "three"]]);
        assert_eq!(pt.text(4..8, PLAIN), "two");
        assert_eq!(pt.text(0..0, PLAIN), "");
        assert_eq!(pt.text(0..99, PLAIN), "one two \nthree");
    }

    #[test]
    fn text_joining_lines_within_a_paragraph() {
        let pt = page(&[&["one two", "three"], &["four", "five"]]);
        assert_eq!(pt.text(0..21, JOIN), "one two three\nfour five");
    }

    #[test]
    fn text_joining_does_not_double_a_space_at_the_line_end() {
        let pt = page(&[&["one two ", "three"]]);
        assert_eq!(pt.text(0..13, JOIN), "one two three");
    }

    #[test]
    fn text_joining_keeps_unblocked_lines_apart() {
        let mut pt = page(&[&["one", "two"]]);
        pt.blocks.clear();
        assert_eq!(pt.text(0..6, JOIN), "one\ntwo");
    }

    #[test]
    fn text_dehyphenate_joins_the_word() {
        let pt = page(&[&["the speci-", "fication says"]]);
        assert_eq!(pt.text(0..23, JOIN_DEHYPHEN), "the specification says");
        assert_eq!(pt.text(0..23, JOIN), "the speci- fication says");
        assert_eq!(pt.text(0..23, PLAIN), "the speci-\nfication says");
    }

    #[test]
    fn text_dehyphenate_keeps_hyphen_before_uppercase() {
        let pt = page(&[&["use Wi-", "Fi now"]]);
        assert_eq!(pt.text(0..13, JOIN_DEHYPHEN), "use Wi- Fi now");
    }

    #[test]
    fn text_dehyphenate_keeps_numeric_ranges() {
        let pt = page(&[&["pages 1-", "2 only"]]);
        assert_eq!(pt.text(0..14, JOIN_DEHYPHEN), "pages 1- 2 only");
        let pt = page(&[&["pages x-", "2 only"]]);
        assert_eq!(pt.text(0..14, JOIN_DEHYPHEN), "pages x- 2 only");
    }

    #[test]
    fn text_dehyphenate_does_not_join_across_paragraphs() {
        let pt = page(&[&["a speci-"], &["fication"]]);
        assert_eq!(pt.text(0..16, JOIN_DEHYPHEN), "a speci-\nfication");
    }

    #[test]
    fn text_dehyphenate_needs_a_letter_before_the_hyphen() {
        let pt = page(&[&["a -", "b"]]);
        assert_eq!(pt.text(0..4, JOIN_DEHYPHEN), "a - b");
    }

    #[test]
    fn text_out_of_range_is_cut() {
        let pt = page(&[&["abc"]]);
        assert_eq!(pt.text(1..100, PLAIN), "bc");
        assert_eq!(pt.text(50..100, PLAIN), "");
    }

    // ---- Selection ----

    #[test]
    fn selection_ordered_when_dragged_backwards() {
        let sel = Selection {
            anchor: pos(3, 5),
            focus: pos(2, 40),
        };
        assert_eq!(sel.ordered(), (pos(2, 40), pos(3, 5)));
        assert_eq!(sel.pages(), 2..=3);
        let same_page = Selection {
            anchor: pos(1, 9),
            focus: pos(1, 2),
        };
        assert_eq!(same_page.ordered(), (pos(1, 2), pos(1, 9)));
        let fwd = Selection {
            anchor: pos(0, 1),
            focus: pos(0, 4),
        };
        assert_eq!(fwd.ordered(), (pos(0, 1), pos(0, 4)));
    }

    #[test]
    fn selection_is_empty() {
        let p = pos(1, 3);
        assert!(
            Selection {
                anchor: p,
                focus: p
            }
            .is_empty()
        );
        assert!(
            !Selection {
                anchor: p,
                focus: pos(1, 4)
            }
            .is_empty()
        );
        assert!(
            !Selection {
                anchor: p,
                focus: pos(2, 3)
            }
            .is_empty()
        );
    }

    #[test]
    fn selection_range_on_a_single_page() {
        let sel = Selection {
            anchor: pos(4, 8),
            focus: pos(4, 3),
        };
        assert_eq!(sel.range_on(4, 20), Some(3..8));
        assert_eq!(sel.range_on(3, 20), None);
        assert_eq!(sel.range_on(5, 20), None);
        assert_eq!(sel.range_on(4, 5), Some(3..5), "clamped to the page");
        let empty = Selection {
            anchor: pos(4, 3),
            focus: pos(4, 3),
        };
        assert_eq!(empty.range_on(4, 20), None);
    }

    #[test]
    fn selection_range_on_three_pages() {
        let sel = Selection {
            anchor: pos(5, 12),
            focus: pos(3, 4),
        };
        assert_eq!(sel.range_on(2, 30), None);
        assert_eq!(sel.range_on(3, 30), Some(4..30), "first page: the tail");
        assert_eq!(sel.range_on(4, 25), Some(0..25), "middle page: all of it");
        assert_eq!(sel.range_on(5, 40), Some(0..12), "last page: the head");
        assert_eq!(sel.range_on(6, 40), None);
        assert_eq!(sel.range_on(4, 0), None, "middle page without text");
    }

    fn three_pages() -> Vec<Arc<PageText>> {
        vec![
            Arc::new(page(&[&["p0 first", "p0 second"]])), // 8 + 9 chars
            Arc::new(page(&[&["p1 only"]])),               // 7 chars
            Arc::new(page(&[&["p2 first", "p2 second"]])),
        ]
    }

    #[test]
    fn selection_text_over_three_pages() {
        let pages = three_pages();
        let get = |n: u32| pages.get(n as usize).cloned();
        let sel = Selection {
            anchor: pos(0, 3),
            focus: pos(2, 11),
        };
        assert_eq!(
            selection_text(&sel, get, PLAIN),
            "first\np0 second\np1 only\np2 first\np2"
        );
    }

    #[test]
    fn selection_text_within_one_page_and_reversed() {
        let pages = three_pages();
        let get = |n: u32| pages.get(n as usize).cloned();
        let sel = Selection {
            anchor: pos(0, 11),
            focus: pos(0, 3),
        };
        assert_eq!(selection_text(&sel, get, PLAIN), "first\np0");
        let empty = Selection {
            anchor: pos(0, 3),
            focus: pos(0, 3),
        };
        assert_eq!(selection_text(&empty, get, PLAIN), "");
    }

    #[test]
    fn selection_text_skips_pages_without_text() {
        let pages = three_pages();
        let get = |n: u32| {
            if n == 1 {
                None
            } else {
                pages.get(n as usize).cloned()
            }
        };
        let sel = Selection {
            anchor: pos(0, 8),
            focus: pos(2, 4),
        };
        assert_eq!(selection_text(&sel, get, PLAIN), "p0 second\np2 f");
    }

    #[test]
    fn selection_text_passes_the_copy_options() {
        let pages = [Arc::new(page(&[&["the speci-", "fication"]]))];
        let get = |n: u32| pages.get(n as usize).cloned();
        let sel = Selection {
            anchor: pos(0, 0),
            focus: pos(0, 18),
        };
        assert_eq!(
            selection_text(&sel, get, JOIN_DEHYPHEN),
            "the specification"
        );
    }

    #[test]
    fn centre_helper_hits_its_character() {
        // Guards the test helpers themselves: the centre of every box maps back to it.
        let pt = page(&[&["ab", "cd"]]);
        for i in 0..4 {
            assert_eq!(pt.char_at(centre(&pt, i)), Some(i));
        }
    }
}
