//! Document comparison. See `docs/ARCHITECTURE.md` §4.9 and `docs/SPEC.md` §8.5.
//!
//! Two versions of a document are compared as one flow of paragraphs and words each, not page by
//! page, so content that moves to another page still matches. This module turns one document
//! into that flow:
//!
//! 1. [`extract`] takes the text of every page ([`PageSource`]),
//! 2. drops repeated header and footer lines ([`noise`]),
//! 3. and produces normalised words and paragraphs ([`DocTokens`]) that still point at the
//!    original characters, so a result can be drawn back onto the pages.
//!
//! Two flows are then compared ([`diff_tokens`]):
//!
//! 4. the paragraphs are aligned by their keys (`align`),
//! 5. misaligned regions are compared word by word, so that text whose paragraph boundaries
//!    differ is not a change (`refine`),
//! 6. and the outcome becomes changes, marks on the pages and synchronisation points
//!    ([`DiffResult`]).
//!
//! [`DiffJob`] runs all of it on a background thread for two open documents.

mod align;
pub mod extract;
mod job;
pub mod noise;
mod refine;
mod result;

pub use extract::{
    DocTokens, ExtractOptions, PageSource, Paragraph, RemovedLine, Word, extract, normalize_key,
};
pub use job::DiffJob;
pub use result::{
    Change, ChangeKind, DiffInput, DiffLimits, DiffOptions, DiffResult, Mark, MarkKind, Side, Span,
    diff_tokens,
};

/// Hand-made page texts for the unit tests of this module.
#[cfg(test)]
pub(crate) mod test_pages {
    use std::sync::Arc;

    use super::PageSource;
    use crate::geom::{Point, Quad, Rect, Size};
    use crate::text::{PageText, TextBlock, TextChar, TextLine};

    /// Page size of every test page (US Letter).
    pub const PAGE_W: f32 = 612.0;
    pub const PAGE_H: f32 = 792.0;
    /// Width of a character, height of a character box, distance between line tops.
    const CW: f32 = 6.0;
    const BOX_H: f32 = 12.0;
    const PITCH: f32 = 16.0;

    /// Appends one block with `lines`, the top of the first line at `top`.
    pub fn add_block(pt: &mut PageText, top: f32, lines: &[&str]) {
        let first_line = pt.lines.len() as u32;
        let mut bounds: Option<Rect> = None;
        for (i, text) in lines.iter().enumerate() {
            let y = top + i as f32 * PITCH;
            let first = pt.chars.len() as u32;
            for (k, ch) in text.chars().enumerate() {
                let x = 72.0 + k as f32 * CW;
                pt.chars.push(TextChar {
                    ch,
                    quad: Quad {
                        ul: Point::new(x, y),
                        ur: Point::new(x + CW, y),
                        ll: Point::new(x, y + BOX_H),
                        lr: Point::new(x + CW, y + BOX_H),
                    },
                    size: 10.0,
                });
            }
            let width = text.chars().count() as f32 * CW;
            let line_bounds = Rect::new(72.0, y, 72.0 + width, y + BOX_H);
            bounds = Some(bounds.map_or(line_bounds, |b| b.union(&line_bounds)));
            pt.lines.push(TextLine {
                chars: first..pt.chars.len() as u32,
                bounds: line_bounds,
            });
        }
        pt.blocks.push(TextBlock {
            lines: first_line..pt.lines.len() as u32,
            bounds: bounds.unwrap_or_default(),
        });
    }

    /// A page with one single-line block per `(top, text)`.
    pub fn line_page(lines: &[(f32, &str)]) -> PageText {
        let mut pt = PageText::default();
        for (top, text) in lines {
            add_block(&mut pt, *top, &[text]);
        }
        pt
    }

    /// A page with one block per `(top, lines)`.
    pub fn block_page(blocks: &[(f32, &[&str])]) -> PageText {
        let mut pt = PageText::default();
        for (top, lines) in blocks {
            add_block(&mut pt, *top, lines);
        }
        pt
    }

    /// A US Letter page source.
    pub fn source(text: PageText) -> PageSource {
        PageSource {
            text: Arc::new(text),
            size: Size::new(PAGE_W, PAGE_H),
        }
    }
}
