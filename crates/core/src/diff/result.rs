//! Building the result of a comparison (`docs/SPEC.md` §8.5 steps 4–7): changes, marks on the
//! pages, synchronisation points and the mapping of a location from one side to the other.
//!
//! [`diff_tokens`] is the pure part of the comparison. It takes two extracted documents
//! ([`DiffInput`]), aligns their paragraphs ([`align`](super::align)), refines the misaligned
//! regions word by word ([`refine`](super::refine)) and turns the outcome into a
//! [`DiffResult`].

use std::ops::Range;
use std::time::{Duration, Instant};

use super::align::{Segment, align_paragraphs};
use super::extract::{DocTokens, ExtractOptions, PageSource, Paragraph, extract};
use super::margins::Margins;
use super::refine::{Item, refine_region, word_span};
use crate::geom::{Rect, Size};
use crate::model::Location;
use crate::text::TextPos;

/// What the user can switch in a comparison.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiffOptions {
    /// Ignore header and footer lines that repeat over the pages.
    pub skip_repeated_lines: bool,
    /// Compare words without regard to case.
    pub ignore_case: bool,
    /// Page edges that are left out of the comparison (zero: nothing). Values are limited to
    /// `0.0 ..= 0.4` when the documents are read.
    pub margins: Margins,
}

impl Default for DiffOptions {
    /// Skip repeated lines, keep case, leave no margin out.
    fn default() -> Self {
        Self {
            skip_repeated_lines: true,
            ignore_case: false,
            margins: Margins::default(),
        }
    }
}

impl DiffOptions {
    /// The extraction options these comparison options stand for.
    pub fn extract_options(&self) -> ExtractOptions {
        ExtractOptions {
            skip_repeated_lines: self.skip_repeated_lines,
            ignore_case: self.ignore_case,
            margins: self.margins.clamped(),
        }
    }
}

/// Time limits of a comparison. [`DiffLimits::default`] is what the app uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffLimits {
    /// Time for aligning the paragraphs. When it runs out the alignment is rougher (more and
    /// larger misaligned regions), never incomplete. Default 2 s.
    pub paragraph_timeout: Duration,
    /// Time for the word comparison of one misaligned region. A region that runs out of time is
    /// reported as removed on the left and added on the right. Default 1 s.
    pub word_timeout: Duration,
    /// Time for the word comparisons of all regions together; once spent, every remaining
    /// region is reported as removed and added. Default 5 s.
    pub total_word_timeout: Duration,
}

impl Default for DiffLimits {
    fn default() -> Self {
        Self {
            paragraph_timeout: Duration::from_secs(2),
            word_timeout: Duration::from_secs(1),
            total_word_timeout: Duration::from_secs(5),
        }
    }
}

/// One side of a comparison: the extracted document and its page sizes.
#[derive(Clone, Debug, Default)]
pub struct DiffInput {
    /// The words and paragraphs of the document.
    pub tokens: DocTokens,
    /// Page sizes in pt, one per page (the length is the page count).
    pub page_sizes: Vec<Size>,
}

impl DiffInput {
    /// Extracts `pages` with `opts` and keeps their sizes.
    pub fn from_pages(pages: &[PageSource], opts: &ExtractOptions) -> DiffInput {
        DiffInput {
            tokens: extract(pages, opts),
            page_sizes: pages.iter().map(|p| p.size).collect(),
        }
    }
}

/// The kind of a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// Paragraphs only in the right (newer) document.
    Added,
    /// Paragraphs only in the left (older) document.
    Removed,
    /// Paragraphs present on both sides with some words changed.
    Modified,
}

/// A range of text in a document, from the first character to the boundary after the last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// The first character.
    pub start: TextPos,
    /// The boundary position after the last character (as in a [`Selection`](crate::Selection)).
    pub end: TextPos,
}

/// One change between the two documents.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// What happened.
    pub kind: ChangeKind,
    /// The paragraphs of the change on the left; `None` for [`ChangeKind::Added`].
    pub left: Option<Span>,
    /// The paragraphs of the change on the right; `None` for [`ChangeKind::Removed`].
    pub right: Option<Span>,
    /// The beginning of the changed paragraph on the left (at most 200 characters; empty for
    /// [`ChangeKind::Added`]). For a modification, the paragraph holding the first changed word.
    pub left_text: String,
    /// The same on the right (empty for [`ChangeKind::Removed`]).
    pub right_text: String,
    /// Where the change is on the left and on the right. For an addition the left location is
    /// the end of the last paragraph both documents share before it (the document start if
    /// there is none); a removal is the same the other way round. A modification starts at its
    /// first paragraph on each side.
    pub anchor: (Location, Location),
}

/// How a mark is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkKind {
    /// Removed text (left side).
    Removed,
    /// Added text (right side).
    Added,
    /// A paragraph that holds a modification (both sides).
    ModifiedBlock,
    /// A changed word inside a modification: gone on the left, new on the right.
    ModifiedWord,
}

/// A rectangle to highlight on a page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mark {
    /// Page space, inside the page, never empty. Neighbouring words of one line are one mark.
    pub rect: Rect,
    /// How to draw it.
    pub kind: MarkKind,
    /// Index into [`DiffResult::changes`].
    pub change: u32,
}

/// The left or the right document of a comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// The older document.
    Left,
    /// The newer document.
    Right,
}

/// The result of a comparison.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiffResult {
    /// All changes in document order.
    pub changes: Vec<Change>,
    /// Marks of the left document by page. Within a page, the paragraph marks of a modification
    /// come before its word marks, so drawing in order puts the words on top.
    pub left_marks: Vec<Vec<Mark>>,
    /// Marks of the right document by page.
    pub right_marks: Vec<Vec<Mark>>,
    /// Corresponding points of the two documents, strictly increasing on both sides (by page,
    /// then y). The first is the start of both documents and the last their ends.
    pub sync: Vec<(Location, Location)>,
    /// Number of [`ChangeKind::Added`] changes.
    pub added: u32,
    /// Number of [`ChangeKind::Removed`] changes.
    pub removed: u32,
    /// Number of [`ChangeKind::Modified`] changes.
    pub modified: u32,
    /// Page sizes of the left document, for [`DiffResult::map_location`].
    pub left_page_sizes: Vec<Size>,
    /// Page sizes of the right document.
    pub right_page_sizes: Vec<Size>,
    /// A time limit was reached: some regions are reported as wholly removed and added, or the
    /// paragraph alignment is rougher than it could be.
    pub approximate: bool,
    /// The generations (`DocumentInfo::generation`) of the left and right document the result
    /// was computed from (rule R-7); `(0, 0)` from [`diff_tokens`], set by [`DiffJob`].
    ///
    /// [`DiffJob`]: super::DiffJob
    pub generations: (u32, u32),
}

impl DiffResult {
    /// The marks of one page of one side (empty for a page that does not exist).
    pub fn marks(&self, side: Side, page: u32) -> &[Mark] {
        let pages = match side {
            Side::Left => &self.left_marks,
            Side::Right => &self.right_marks,
        };
        pages.get(page as usize).map_or(&[], Vec::as_slice)
    }

    /// The location on the other side that corresponds to `loc` on side `from`.
    ///
    /// The vertical position is interpolated between the two neighbouring [`sync`] points (the
    /// pages are taken as stacked without gaps); `x` is kept. Before the first or after the
    /// last point the offset to that point is kept, within the document. Comparing a document
    /// with itself maps every location to itself.
    ///
    /// [`sync`]: DiffResult::sync
    pub fn map_location(&self, from: Side, loc: Location) -> Location {
        let (src_sizes, dst_sizes) = match from {
            Side::Left => (&self.left_page_sizes, &self.right_page_sizes),
            Side::Right => (&self.right_page_sizes, &self.left_page_sizes),
        };
        let pick = |p: &(Location, Location)| match from {
            Side::Left => (p.0, p.1),
            Side::Right => (p.1, p.0),
        };
        if src_sizes == dst_sizes && self.sync.iter().all(|(a, b)| a == b) {
            return loc;
        }
        let src = Stack::new(src_sizes);
        let dst = Stack::new(dst_sizes);
        let (Some(first), Some(last)) = (self.sync.first(), self.sync.last()) else {
            return dst.location(src.y(loc), loc.x);
        };
        let y = src.y(loc);
        let k = self.sync.partition_point(|p| src.y(pick(p).0) <= y);
        let out = if k == 0 {
            let (a, b) = pick(first);
            dst.y(b) + (y - src.y(a))
        } else if k >= self.sync.len() {
            let (a, b) = pick(last);
            dst.y(b) + (y - src.y(a))
        } else {
            let (a0, b0) = pick(&self.sync[k - 1]);
            let (a1, b1) = pick(&self.sync[k]);
            let (a0, a1, b0, b1) = (src.y(a0), src.y(a1), dst.y(b0), dst.y(b1));
            if a1 > a0 {
                b0 + (y - a0) * (b1 - b0) / (a1 - a0)
            } else {
                b0
            }
        };
        dst.location(out, loc.x)
    }
}

/// The pages of a document stacked vertically without gaps: one continuous y in pt.
struct Stack {
    /// `tops[p]` is the y of the top of page `p`; the last entry is the total height.
    tops: Vec<f64>,
}

impl Stack {
    fn new(sizes: &[Size]) -> Stack {
        let mut tops = Vec::with_capacity(sizes.len() + 1);
        let mut y = 0.0;
        tops.push(y);
        for s in sizes {
            y += f64::from(s.h.max(0.0));
            tops.push(y);
        }
        Stack { tops }
    }

    fn pages(&self) -> usize {
        self.tops.len() - 1
    }

    /// The stacked y of `loc` (its page clamped to the document, its y to the page).
    fn y(&self, loc: Location) -> f64 {
        let n = self.pages();
        if n == 0 {
            return 0.0;
        }
        let p = (loc.page as usize).min(n - 1);
        let h = self.tops[p + 1] - self.tops[p];
        self.tops[p] + f64::from(loc.y).clamp(0.0, h)
    }

    /// The location at stacked y `y` (clamped to the document).
    fn location(&self, y: f64, x: f32) -> Location {
        let n = self.pages();
        if n == 0 {
            return Location::new(0, x, 0.0);
        }
        let total = self.tops[n];
        let y = y.clamp(0.0, total);
        // The page whose top is the last one at or above `y`; the very end belongs to the last.
        let p = (self.tops[1..].partition_point(|&t| t <= y)).min(n - 1);
        let local = (y - self.tops[p]) as f32;
        Location::new(to_u32(p), x, local)
    }

    /// The end of the document: the bottom of the last page.
    fn end(&self) -> Location {
        match self.pages() {
            0 => Location::new(0, 0.0, 0.0),
            n => Location::new(to_u32(n - 1), 0.0, (self.tops[n] - self.tops[n - 1]) as f32),
        }
    }
}

/// `usize` → `u32`, saturating (page and change counts never come near `u32::MAX`).
fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The most characters of [`Change::left_text`] and [`Change::right_text`].
const EXCERPT_CHARS: usize = 200;

/// Compares two extracted documents. Pure apart from reading the clock for the time limits.
pub fn diff_tokens(left: &DiffInput, right: &DiffInput, limits: &DiffLimits) -> DiffResult {
    let start = Instant::now();
    let lkeys: Vec<u64> = left.tokens.paragraphs.iter().map(|p| p.key).collect();
    let rkeys: Vec<u64> = right.tokens.paragraphs.iter().map(|p| p.key).collect();
    let (segments, late) =
        align_paragraphs(&lkeys, &rkeys, start.checked_add(limits.paragraph_timeout));

    let mut b = Builder::new(left, right);
    b.approximate = late;
    let words_end = Instant::now().checked_add(limits.total_word_timeout);
    for seg in segments {
        match seg {
            Segment::Same { left: l, right: r } => {
                for (a, c) in l.zip(r) {
                    b.same_pair(a, c);
                }
            }
            Segment::Differ { left: l, right: r } => {
                let own = Instant::now().checked_add(limits.word_timeout);
                let deadline = match (own, words_end) {
                    (Some(a), Some(e)) => Some(a.min(e)),
                    (a, e) => a.or(e),
                };
                let (items, late) = refine_region(&left.tokens, &right.tokens, l, r, deadline);
                b.approximate |= late;
                for item in items {
                    b.item(item);
                }
            }
        }
    }
    b.finish()
}

/// Collects the result while the alignment is walked in document order.
struct Builder<'a> {
    left: &'a DiffInput,
    right: &'a DiffInput,
    changes: Vec<Change>,
    left_marks: Vec<Vec<Mark>>,
    right_marks: Vec<Vec<Mark>>,
    /// Candidate synchronisation points, in walk order.
    sync: Vec<(Location, Location)>,
    /// One past the last left / right paragraph that has a counterpart (equal or modified).
    common_l: usize,
    common_r: usize,
    approximate: bool,
}

impl<'a> Builder<'a> {
    fn new(left: &'a DiffInput, right: &'a DiffInput) -> Builder<'a> {
        let pages = |d: &DiffInput| d.page_sizes.len().max(d.tokens.pages.len());
        Builder {
            left,
            right,
            changes: Vec::new(),
            left_marks: vec![Vec::new(); pages(left)],
            right_marks: vec![Vec::new(); pages(right)],
            sync: Vec::new(),
            common_l: 0,
            common_r: 0,
            approximate: false,
        }
    }

    /// Two equal paragraphs.
    fn same_pair(&mut self, l: usize, r: usize) {
        self.same(l..l + 1, r..r + 1);
    }

    /// Paragraphs with the same words on both sides.
    fn same(&mut self, l: Range<usize>, r: Range<usize>) {
        let (lt, rt) = (&self.left.tokens, &self.right.tokens);
        if let (Some(lp), Some(rp)) = (lt.paragraphs.get(l.start), rt.paragraphs.get(r.start)) {
            self.sync
                .push((start_loc(lt, lp.start), start_loc(rt, rp.start)));
        }
        let last = |d: &'a DocTokens, rg: &Range<usize>| {
            rg.end.checked_sub(1).and_then(|i| d.paragraphs.get(i))
        };
        if let (Some(lp), Some(rp)) = (last(lt, &l), last(rt, &r)) {
            self.sync.push((end_loc(lt, lp.end), end_loc(rt, rp.end)));
        }
        self.common_l = l.end;
        self.common_r = r.end;
    }

    fn item(&mut self, item: Item) {
        match item {
            Item::Same { left, right } => self.same(left, right),
            Item::Removed { left } => self.removed(left),
            Item::Added { right } => self.added(right),
            Item::Modified {
                left,
                right,
                left_words,
                right_words,
            } => self.modified(left, right, &left_words, &right_words),
        }
    }

    /// The point on `doc` where something was inserted after its paragraph `common - 1`.
    fn insertion_point(doc: &DocTokens, common: usize) -> Location {
        common
            .checked_sub(1)
            .and_then(|i| doc.paragraphs.get(i))
            .map_or(Location::new(0, 0.0, 0.0), |p| end_loc(doc, p.end))
    }

    fn next_index(&self) -> u32 {
        to_u32(self.changes.len())
    }

    fn removed(&mut self, l: Range<usize>) {
        let lt = &self.left.tokens;
        let Some(span) = paragraphs_span(lt, &l) else {
            return;
        };
        let change = self.next_index();
        let words = word_span(lt, &l);
        add_marks(
            &mut self.left_marks,
            self.left,
            words,
            MarkKind::Removed,
            change,
        );
        let anchor = (
            start_loc(lt, span.start),
            Self::insertion_point(&self.right.tokens, self.common_r),
        );
        self.sync.push(anchor);
        self.changes.push(Change {
            kind: ChangeKind::Removed,
            left: Some(span),
            right: None,
            left_text: excerpt(lt, l.start),
            right_text: String::new(),
            anchor,
        });
    }

    fn added(&mut self, r: Range<usize>) {
        let rt = &self.right.tokens;
        let Some(span) = paragraphs_span(rt, &r) else {
            return;
        };
        let change = self.next_index();
        let words = word_span(rt, &r);
        add_marks(
            &mut self.right_marks,
            self.right,
            words,
            MarkKind::Added,
            change,
        );
        let anchor = (
            Self::insertion_point(&self.left.tokens, self.common_l),
            start_loc(rt, span.start),
        );
        self.sync.push(anchor);
        self.changes.push(Change {
            kind: ChangeKind::Added,
            left: None,
            right: Some(span),
            left_text: String::new(),
            right_text: excerpt(rt, r.start),
            anchor,
        });
    }

    fn modified(
        &mut self,
        l: Range<usize>,
        r: Range<usize>,
        left_words: &[usize],
        right_words: &[usize],
    ) {
        let (lt, rt) = (&self.left.tokens, &self.right.tokens);
        let (Some(lspan), Some(rspan)) = (paragraphs_span(lt, &l), paragraphs_span(rt, &r)) else {
            return;
        };
        let change = self.next_index();
        for (marks, input, paras, words) in [
            (&mut self.left_marks, self.left, &l, left_words),
            (&mut self.right_marks, self.right, &r, right_words),
        ] {
            let all = word_span(&input.tokens, paras);
            add_marks(marks, input, all, MarkKind::ModifiedBlock, change);
            for run in runs(words) {
                add_marks(marks, input, run, MarkKind::ModifiedWord, change);
            }
        }
        let anchor = (start_loc(lt, lspan.start), start_loc(rt, rspan.start));
        self.sync.push(anchor);
        let text = |d: &DocTokens, paras: &Range<usize>, words: &[usize]| {
            let p = words
                .first()
                .map_or(paras.start, |&w| paragraph_of_word(d, w))
                .clamp(paras.start, paras.end.saturating_sub(1).max(paras.start));
            excerpt(d, p)
        };
        let left_text = text(lt, &l, left_words);
        let right_text = text(rt, &r, right_words);
        self.changes.push(Change {
            kind: ChangeKind::Modified,
            left: Some(lspan),
            right: Some(rspan),
            left_text,
            right_text,
            anchor,
        });
        // The ends of the passages correspond as well.
        if let (Some(a), Some(c)) = (
            l.end.checked_sub(1).and_then(|i| lt.paragraphs.get(i)),
            r.end.checked_sub(1).and_then(|i| rt.paragraphs.get(i)),
        ) {
            self.sync.push((end_loc(lt, a.end), end_loc(rt, c.end)));
        }
        self.common_l = l.end;
        self.common_r = r.end;
    }

    fn finish(self) -> DiffResult {
        let count = |k: ChangeKind| to_u32(self.changes.iter().filter(|c| c.kind == k).count());
        let added = count(ChangeKind::Added);
        let removed = count(ChangeKind::Removed);
        let modified = count(ChangeKind::Modified);
        let sync = monotone_sync(
            self.sync,
            &Stack::new(&self.left.page_sizes),
            &Stack::new(&self.right.page_sizes),
        );
        DiffResult {
            changes: self.changes,
            left_marks: self.left_marks,
            right_marks: self.right_marks,
            sync,
            added,
            removed,
            modified,
            left_page_sizes: self.left.page_sizes.clone(),
            right_page_sizes: self.right.page_sizes.clone(),
            approximate: self.approximate,
            generations: (0, 0),
        }
    }
}

/// The span from the first character of paragraph `paras.start` to the end of the last one.
fn paragraphs_span(doc: &DocTokens, paras: &Range<usize>) -> Option<Span> {
    let first: &Paragraph = doc.paragraphs.get(paras.start)?;
    let last: &Paragraph = doc.paragraphs.get(paras.end.checked_sub(1)?)?;
    (paras.start < paras.end).then_some(Span {
        start: first.start,
        end: last.end,
    })
}

/// The index of the paragraph holding word `w`.
fn paragraph_of_word(doc: &DocTokens, w: usize) -> usize {
    doc.paragraphs
        .partition_point(|p| (p.words.end as usize) <= w)
        .min(doc.paragraphs.len().saturating_sub(1))
}

/// Splits ascending word indices into runs of consecutive indices.
fn runs(words: &[usize]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    for &w in words {
        match out.last_mut() {
            Some(r) if r.end == w => r.end = w + 1,
            _ => out.push(w..w + 1),
        }
    }
    out
}

/// The first words of paragraph `p`, at most [`EXCERPT_CHARS`] characters, ending in "…" when
/// cut.
fn excerpt(doc: &DocTokens, p: usize) -> String {
    let Some(para) = doc.paragraphs.get(p) else {
        return String::new();
    };
    let mut out = String::new();
    let mut len = 0;
    for w in doc.paragraph_words(para) {
        if len > 0 {
            out.push(' ');
            len += 1;
        }
        let text = doc.word_text(w);
        len += text.chars().count();
        out.push_str(&text);
        if len > EXCERPT_CHARS {
            let mut cut: String = out.chars().take(EXCERPT_CHARS - 1).collect();
            cut.push('…');
            return cut;
        }
    }
    out
}

/// The box of the character at `pos`, if it exists.
fn char_box(doc: &DocTokens, pos: TextPos) -> Option<Rect> {
    doc.pages
        .get(pos.page as usize)?
        .chars
        .get(pos.ch as usize)
        .map(|c| c.quad.bounds())
}

/// The top left of the character at `pos` (the page top if it does not exist).
fn start_loc(doc: &DocTokens, pos: TextPos) -> Location {
    char_box(doc, pos).map_or(Location::new(pos.page, 0.0, 0.0), |b| {
        Location::new(pos.page, b.x0, b.y0)
    })
}

/// The bottom right of the character before the boundary `end`.
fn end_loc(doc: &DocTokens, end: TextPos) -> Location {
    let last = TextPos {
        page: end.page,
        ch: end.ch.saturating_sub(1),
    };
    char_box(doc, last).map_or(Location::new(end.page, 0.0, 0.0), |b| {
        Location::new(end.page, b.x1, b.y1)
    })
}

/// Adds marks of `kind` for the words `words` of `input`, one rectangle per run of words on a
/// line, clipped to the page.
fn add_marks(
    marks: &mut [Vec<Mark>],
    input: &DiffInput,
    words: Range<usize>,
    kind: MarkKind,
    change: u32,
) {
    let doc = &input.tokens;
    let Some(words) = doc.words.get(words) else {
        return;
    };
    for w in words {
        for (page, chars) in w.segments() {
            let (Some(pt), Some(list)) =
                (doc.pages.get(page as usize), marks.get_mut(page as usize))
            else {
                continue;
            };
            let size = input.page_sizes.get(page as usize).copied();
            for rect in pt.rects(chars) {
                let rect = match size {
                    Some(s) => Rect::new(
                        rect.x0.clamp(0.0, s.w),
                        rect.y0.clamp(0.0, s.h),
                        rect.x1.clamp(0.0, s.w),
                        rect.y1.clamp(0.0, s.h),
                    ),
                    None => rect,
                };
                // Also rejects NaN.
                if !(rect.width() > 0.0 && rect.height() > 0.0) {
                    continue;
                }
                push_mark(list, Mark { rect, kind, change });
            }
        }
    }
}

/// Appends `m`, or widens the last mark when it is of the same kind and change and `m` follows
/// it on the same line.
fn push_mark(list: &mut Vec<Mark>, m: Mark) {
    if let Some(last) = list.last_mut()
        && last.kind == m.kind
        && last.change == m.change
        && same_line_neighbours(&last.rect, &m.rect)
    {
        last.rect = last.rect.union(&m.rect);
        return;
    }
    list.push(m);
}

/// Whether `b` continues `a` on the same line: they overlap vertically by at least half the
/// lower height, and `b` starts at most one line height right of where `a` ends (and not left
/// of where `a` starts).
fn same_line_neighbours(a: &Rect, b: &Rect) -> bool {
    let overlap = a.y1.min(b.y1) - a.y0.max(b.y0);
    let h = a.height().min(b.height());
    let gap = b.x0 - a.x1;
    overlap >= 0.5 * h && b.x0 >= a.x0 && gap <= a.height().max(b.height())
}

/// Keeps the largest set of candidates that is strictly increasing on both sides, and adds the
/// start and the end of both documents.
fn monotone_sync(
    candidates: Vec<(Location, Location)>,
    left: &Stack,
    right: &Stack,
) -> Vec<(Location, Location)> {
    let start = (Location::new(0, 0.0, 0.0), Location::new(0, 0.0, 0.0));
    let end = (left.end(), right.end());
    let (le, re) = (left.y(end.0), right.y(end.1));
    let mut pts: Vec<(f64, f64, (Location, Location))> = candidates
        .into_iter()
        .map(|p| (left.y(p.0), right.y(p.1), p))
        .filter(|&(a, b, _)| a > 0.0 && b > 0.0 && a < le && b < re)
        .collect();
    // Ascending on the left; on ties descending on the right, so that a strictly increasing
    // run on the right picks at most one of them.
    pts.sort_by(|x, y| x.0.total_cmp(&y.0).then(y.1.total_cmp(&x.1)));

    // Longest strictly increasing subsequence on the right side.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; pts.len()];
    for i in 0..pts.len() {
        let b = pts[i].1;
        let k = tails.partition_point(|&t| pts[t].1 < b);
        prev[i] = k.checked_sub(1).map(|k| tails[k]);
        if k == tails.len() {
            tails.push(i);
        } else {
            tails[k] = i;
        }
    }
    let mut chosen = Vec::with_capacity(tails.len());
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        chosen.push(pts[i].2);
        cur = prev[i];
    }
    chosen.reverse();

    let mut out = Vec::with_capacity(chosen.len() + 2);
    out.push(start);
    out.extend(chosen);
    if le > 0.0 && re > 0.0 {
        out.push(end);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::diff::test_pages::{PAGE_H, PAGE_W, block_page, source};
    use crate::text::PageText;

    const KEEP: ExtractOptions = ExtractOptions {
        skip_repeated_lines: false,
        ignore_case: false,
        margins: Margins {
            top: 0.0,
            bottom: 0.0,
            left: 0.0,
            right: 0.0,
        },
    };

    /// One document from pages of blocks; each block is a list of lines.
    fn input(pages: &[&[&[&str]]]) -> DiffInput {
        let sources: Vec<PageSource> = pages
            .iter()
            .map(|blocks| {
                let list: Vec<(f32, &[&str])> = blocks
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (60.0 + i as f32 * 100.0, *b))
                    .collect();
                source(block_page(&list))
            })
            .collect();
        DiffInput::from_pages(&sources, &KEEP)
    }

    fn run(l: &DiffInput, r: &DiffInput) -> DiffResult {
        let res = diff_tokens(l, r, &DiffLimits::default());
        check_invariants(&res);
        res
    }

    fn kinds(res: &DiffResult) -> Vec<ChangeKind> {
        res.changes.iter().map(|c| c.kind).collect()
    }

    fn all_marks(res: &DiffResult) -> Vec<(Side, u32, Mark)> {
        let mut out = Vec::new();
        for (side, pages) in [
            (Side::Left, &res.left_marks),
            (Side::Right, &res.right_marks),
        ] {
            for (p, list) in pages.iter().enumerate() {
                out.extend(list.iter().map(|m| (side, p as u32, *m)));
            }
        }
        out
    }

    /// The properties every result must have.
    fn check_invariants(res: &DiffResult) {
        let sizes = |s: Side| match s {
            Side::Left => &res.left_page_sizes,
            Side::Right => &res.right_page_sizes,
        };
        for (side, page, m) in all_marks(res) {
            let s = sizes(side)[page as usize];
            let r = m.rect;
            assert!(r.width() > 0.0 && r.height() > 0.0, "{m:?}");
            assert!(
                r.x0 >= 0.0 && r.y0 >= 0.0 && r.x1 <= s.w && r.y1 <= s.h,
                "{m:?}"
            );
            assert!((m.change as usize) < res.changes.len());
        }
        for (i, c) in res.changes.iter().enumerate() {
            assert!(
                all_marks(res)
                    .iter()
                    .any(|(_, _, m)| m.change as usize == i),
                "change {i} has no mark"
            );
            assert!(c.left_text.chars().count() <= EXCERPT_CHARS);
            assert!(c.right_text.chars().count() <= EXCERPT_CHARS);
            match c.kind {
                ChangeKind::Added => assert!(c.left.is_none() && c.right.is_some()),
                ChangeKind::Removed => assert!(c.left.is_some() && c.right.is_none()),
                ChangeKind::Modified => assert!(c.left.is_some() && c.right.is_some()),
            }
        }
        let n = |k| res.changes.iter().filter(|c| c.kind == k).count() as u32;
        assert_eq!(res.added, n(ChangeKind::Added));
        assert_eq!(res.removed, n(ChangeKind::Removed));
        assert_eq!(res.modified, n(ChangeKind::Modified));
        let (ls, rs) = (
            Stack::new(&res.left_page_sizes),
            Stack::new(&res.right_page_sizes),
        );
        for w in res.sync.windows(2) {
            assert!(ls.y(w[1].0) > ls.y(w[0].0), "{w:?}");
            assert!(rs.y(w[1].1) > rs.y(w[0].1), "{w:?}");
        }
        assert_eq!(
            res.sync.first(),
            Some(&(Location::new(0, 0.0, 0.0), Location::new(0, 0.0, 0.0)))
        );
        if !res.left_page_sizes.is_empty() && !res.right_page_sizes.is_empty() {
            assert_eq!(res.sync.last(), Some(&(ls.end(), rs.end())));
        }
    }

    #[test]
    fn identical_documents_have_no_changes_and_map_to_themselves() {
        let d = input(&[
            &[&["Intro text of the page."], &["Second paragraph here."]],
            &[&["Third one on page two."]],
        ]);
        let res = run(&d, &d);
        assert!(res.changes.is_empty());
        assert!(all_marks(&res).is_empty());
        assert!(res.sync.len() > 2);
        for loc in [
            Location::new(0, 10.0, 0.0),
            Location::new(0, 72.0, 333.3),
            Location::new(1, 5.0, PAGE_H),
        ] {
            assert_eq!(res.map_location(Side::Left, loc), loc);
            assert_eq!(res.map_location(Side::Right, loc), loc);
        }
    }

    #[test]
    fn added_paragraph_is_one_added_change_with_marks_on_the_right() {
        let l = input(&[&[&["alpha beta gamma."], &["delta epsilon zeta."]]]);
        let r = input(&[&[
            &["alpha beta gamma."],
            &["new words in a new paragraph."],
            &["delta epsilon zeta."],
        ]]);
        let res = run(&l, &r);
        assert_eq!(kinds(&res), [ChangeKind::Added]);
        let c = &res.changes[0];
        assert_eq!(c.right_text, "new words in a new paragraph.");
        assert!(res.left_marks[0].is_empty());
        // One line: one merged rectangle over the whole line, at the block's place (top 160).
        assert_eq!(res.right_marks[0].len(), 1);
        let m = res.right_marks[0][0];
        assert_eq!(m.kind, MarkKind::Added);
        assert_eq!(m.rect, Rect::new(72.0, 160.0, 72.0 + 29.0 * 6.0, 172.0));
        // The left anchor is the end of the paragraph before; the right one the new paragraph.
        assert_eq!(c.anchor.0, Location::new(0, 72.0 + 17.0 * 6.0, 72.0));
        assert_eq!(c.anchor.1, Location::new(0, 72.0, 160.0));
    }

    #[test]
    fn removed_paragraph_is_one_removed_change() {
        let l = input(&[&[&["a b c."], &["gone gone gone."], &["d e f."]]]);
        let r = input(&[&[&["a b c."], &["d e f."]]]);
        let res = run(&l, &r);
        assert_eq!(kinds(&res), [ChangeKind::Removed]);
        assert_eq!(res.left_marks[0].len(), 1);
        assert_eq!(res.left_marks[0][0].kind, MarkKind::Removed);
        assert!(res.right_marks[0].is_empty());
        assert_eq!(res.changes[0].left_text, "gone gone gone.");
    }

    #[test]
    fn changed_word_marks_that_word_on_both_sides_and_the_block() {
        let l = input(&[&[
            &["first para."],
            &["the quick brown fox jumps over", "the lazy dog."],
        ]]);
        let r = input(&[&[
            &["first para."],
            &["the quick red fox jumps over", "the lazy dog."],
        ]]);
        let res = run(&l, &r);
        assert_eq!(kinds(&res), [ChangeKind::Modified]);
        for (side, word_x0, word_len) in [(Side::Left, 10.0, 5.0), (Side::Right, 10.0, 3.0)] {
            let marks = res.marks(side, 0);
            let blocks: Vec<_> = marks
                .iter()
                .filter(|m| m.kind == MarkKind::ModifiedBlock)
                .collect();
            let words: Vec<_> = marks
                .iter()
                .filter(|m| m.kind == MarkKind::ModifiedWord)
                .collect();
            // Two lines of the second block.
            assert_eq!(blocks.len(), 2, "{side:?}");
            assert!(blocks.iter().all(|m| m.rect.y0 >= 160.0));
            assert_eq!(words.len(), 1);
            let x0 = 72.0 + word_x0 * 6.0;
            assert_eq!(
                words[0].rect,
                Rect::new(x0, 160.0, x0 + word_len * 6.0, 172.0)
            );
        }
        let c = &res.changes[0];
        assert!(c.left_text.starts_with("the quick brown"));
        assert!(c.right_text.starts_with("the quick red"));
    }

    #[test]
    fn inserted_word_has_no_word_mark_on_the_left() {
        let l = input(&[&[&["one two three four five six."]]]);
        let r = input(&[&[&["one two three extra four five six."]]]);
        let res = run(&l, &r);
        assert_eq!(kinds(&res), [ChangeKind::Modified]);
        assert!(
            res.left_marks[0]
                .iter()
                .all(|m| m.kind == MarkKind::ModifiedBlock)
        );
        let words: Vec<_> = res.right_marks[0]
            .iter()
            .filter(|m| m.kind == MarkKind::ModifiedWord)
            .collect();
        assert_eq!(words.len(), 1);
    }

    #[test]
    fn three_consecutive_added_paragraphs_are_one_change() {
        let l = input(&[&[&["keep one."], &["keep two."]]]);
        let r = input(&[&[
            &["keep one."],
            &["new a."],
            &["new b."],
            &["new c."],
            &["keep two."],
        ]]);
        let res = run(&l, &r);
        assert_eq!(kinds(&res), [ChangeKind::Added]);
        assert_eq!(res.right_marks[0].len(), 3);
    }

    #[test]
    fn paragraph_split_differently_is_no_change() {
        let l = input(&[&[
            &["start."],
            &["alpha beta gamma", "delta epsilon."],
            &["end."],
        ]]);
        let r = input(&[&[
            &["start."],
            &["alpha beta"],
            &["gamma delta epsilon."],
            &["end."],
        ]]);
        let res = run(&l, &r);
        assert!(res.changes.is_empty(), "{:?}", res.changes);
    }

    #[test]
    fn word_broken_over_a_page_is_marked_on_both_pages() {
        let l = input(&[&[&["intro."]], &[&["outro."]]]);
        let r = input(&[
            &[&["intro."], &["the new speci-"]],
            &[&["fication text."], &["outro."]],
        ]);
        let res = run(&l, &r);
        assert_eq!(kinds(&res), [ChangeKind::Added]);
        assert_eq!(res.right_marks[0].len(), 1);
        assert_eq!(res.right_marks[1].len(), 1);
        assert_eq!(
            res.changes[0].right.unwrap().end,
            TextPos { page: 1, ch: 14 }
        );
    }

    #[test]
    fn empty_documents() {
        let empty = input(&[&[]]);
        let full = input(&[&[&["some text."], &["more text."]]]);
        let res = run(&empty, &empty);
        assert!(res.changes.is_empty());
        assert_eq!(res.sync.len(), 2);

        let res = run(&empty, &full);
        assert_eq!(kinds(&res), [ChangeKind::Added]);
        assert_eq!(res.changes[0].anchor.0, Location::new(0, 0.0, 0.0));
        let res = run(&full, &empty);
        assert_eq!(kinds(&res), [ChangeKind::Removed]);

        // No pages at all.
        let none = DiffInput::default();
        let res = run(&none, &none);
        assert!(res.changes.is_empty());
        assert_eq!(res.sync.len(), 1);
        assert_eq!(
            res.map_location(Side::Left, Location::new(3, 1.0, 2.0)),
            Location::new(3, 1.0, 2.0)
        );
    }

    #[test]
    fn mapping_interpolates_between_sync_points() {
        // Right has an extra paragraph near the top: everything after moves down by 100 pt.
        let l = input(&[&[&["a a a."], &["b b b."], &["c c c."]]]);
        let r = input(&[&[&["a a a."], &["new new."], &["b b b."], &["c c c."]]]);
        let res = run(&l, &r);
        // Start of "b" (left top 160) → start of "b" on the right (top 260).
        let m = res.map_location(Side::Left, Location::new(0, 72.0, 160.0));
        assert_eq!(m, Location::new(0, 72.0, 260.0));
        let back = res.map_location(Side::Right, Location::new(0, 72.0, 260.0));
        assert_eq!(back, Location::new(0, 72.0, 160.0));
        // Half-way inside "c" (a sync pair from its start to its end) keeps the offset.
        let m = res.map_location(Side::Left, Location::new(0, 72.0, 266.0));
        assert!((m.y - 366.0).abs() < 1e-3, "{m:?}");
        // Past the last paragraph, towards the page end.
        let m = res.map_location(Side::Left, Location::new(0, 0.0, PAGE_H));
        assert_eq!(m, Location::new(0, 0.0, PAGE_H));
    }

    #[test]
    fn sync_drops_points_that_break_the_order() {
        let ls = Stack::new(&[Size::new(PAGE_W, PAGE_H); 2]);
        let rs = Stack::new(&[Size::new(PAGE_W, PAGE_H); 2]);
        let p = |a: f32, b: f32| (Location::new(0, 0.0, a), Location::new(0, 0.0, b));
        let out = monotone_sync(
            vec![
                p(10.0, 10.0),
                p(20.0, 500.0),
                p(30.0, 30.0),
                p(40.0, 40.0),
                p(40.0, 45.0),
            ],
            &ls,
            &rs,
        );
        let ys: Vec<(f32, f32)> = out[1..out.len() - 1]
            .iter()
            .map(|(a, b)| (a.y, b.y))
            .collect();
        // One of the two points at left 40 is kept, not both.
        assert_eq!(ys.len(), 3);
        assert_eq!(ys[..2], [(10.0, 10.0), (30.0, 30.0)]);
        assert_eq!(ys[2].0, 40.0);
        assert_eq!(out[out.len() - 1], (ls.end(), rs.end()));
    }

    #[test]
    fn stack_round_trips_locations() {
        let s = Stack::new(&[Size::new(100.0, 200.0), Size::new(100.0, 300.0)]);
        for loc in [
            Location::new(0, 1.0, 0.0),
            Location::new(0, 1.0, 199.5),
            Location::new(1, 1.0, 0.0),
            Location::new(1, 1.0, 300.0),
        ] {
            assert_eq!(s.location(s.y(loc), 1.0), loc);
        }
        assert_eq!(s.location(-5.0, 0.0), Location::new(0, 0.0, 0.0));
        assert_eq!(s.location(1e9, 0.0), Location::new(1, 0.0, 300.0));
    }

    #[test]
    fn excerpts_are_cut_at_200_characters() {
        let long: String = (0..100)
            .map(|i| format!("w{i:03}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut pt = PageText::default();
        crate::diff::test_pages::add_block(&mut pt, 100.0, &[&long]);
        let d = DiffInput::from_pages(&[source(pt)], &KEEP);
        let e = excerpt(&d.tokens, 0);
        assert_eq!(e.chars().count(), EXCERPT_CHARS);
        assert!(e.ends_with('…'));
        assert!(e.starts_with("w000 w001"));
    }

    #[test]
    fn neighbouring_rects_merge_only_on_one_line() {
        let a = Rect::new(72.0, 100.0, 100.0, 112.0);
        assert!(same_line_neighbours(
            &a,
            &Rect::new(106.0, 100.0, 130.0, 112.0)
        ));
        assert!(!same_line_neighbours(
            &a,
            &Rect::new(72.0, 116.0, 100.0, 128.0)
        ));
        assert!(!same_line_neighbours(
            &a,
            &Rect::new(300.0, 100.0, 330.0, 112.0)
        ));
        assert!(!same_line_neighbours(
            &a,
            &Rect::new(10.0, 100.0, 30.0, 112.0)
        ));
    }
}
