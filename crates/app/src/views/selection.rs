//! Text selection in the PDF view (SEL-1, SEL-2, SEL-3; `docs/ARCHITECTURE.md` §4.7, §5.5).
//!
//! A selection is a range of character indices (`refdesk_core::Selection`), never a screen
//! rectangle, so that dragging inside one column of a two-column page selects that column only.
//!
//! Everything that decides something lives here as plain functions and plain data and is tested
//! without egui (rule R-10): turning a viewport position into a page and a page-space point,
//! the click and drag state machine, the highlight rectangles, the auto-scroll speed and the
//! copy. The widget (`pdf_view.rs`) only reads the pointer and paints.
//!
//! Page texts come in through a function (`PageTexts`) because the engine delivers them
//! asynchronously. A page whose text is not there yet is "pending": the request has been queued
//! by the function and the state machine tries again on the next frame.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use refdesk_core::{
    CopyOptions, DocumentHandle, Location, PageLayout, PageText, Point, Rect, Selection, Size,
    TextPos, selection_text,
};

/// How selected text is turned into a string (SEL-2). A constant for now; a setting later.
pub const COPY_OPTIONS: CopyOptions = CopyOptions {
    join_lines: true,
    dehyphenate: true,
};

/// A press that moved less than this many logical pixels is a click, not a drag.
pub const DRAG_START_DISTANCE: f32 = 4.0;
/// Two presses at most this far apart (logical pixels) and this close in time (seconds) are a
/// double click; three in a row are a triple click.
pub const MULTI_CLICK_DISTANCE: f32 = 6.0;
/// See [`MULTI_CLICK_DISTANCE`].
pub const MULTI_CLICK_SECS: f64 = 0.4;
/// While dragging, a pointer closer than this to the top or bottom edge of the view (or outside
/// of it) scrolls the view.
pub const AUTOSCROLL_EDGE: f32 = 36.0;
/// Auto-scroll speed in logical pixels per second for a pointer exactly at the edge of the view.
pub const AUTOSCROLL_SPEED: f32 = 600.0;
/// The speed grows with the distance past the inner border of the edge zone, up to this many
/// times [`AUTOSCROLL_SPEED`] (reached one and a half edge widths outside the view).
pub const AUTOSCROLL_MAX_FACTOR: f32 = 3.0;

/// The text of a page, or `None` while it is not available. The function queues the request.
pub type PageTexts<'a> = &'a dyn Fn(u32) -> Option<Arc<PageText>>;

// ------------------------------------------------------------------------------------------------
// Positions
// ------------------------------------------------------------------------------------------------

/// Where a viewport position lies on the document.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// The page (the nearest one for a position between pages) and the point on it in page space
    /// (pt). The point is not clamped: in the margin it lies outside the page.
    pub loc: Location,
    /// Whether the position is on the page itself and not in a gap or margin.
    pub on_page: bool,
}

/// The page and page-space point under `viewport_pos`, a position relative to the top-left corner
/// of the view. `scroll` is the scroll offset in content space. `None` for a document without
/// pages.
pub fn locate(layout: &PageLayout, scroll: Point, viewport_pos: Point) -> Option<Hit> {
    if layout.page_count() == 0 {
        return None;
    }
    let content = Point::new(scroll.x + viewport_pos.x, scroll.y + viewport_pos.y);
    let page = layout.location_at(content).page;
    let rect = layout.page_rect(page);
    let scale = layout.scale();
    if scale <= 0.0 {
        return None;
    }
    Some(Hit {
        loc: Location::new(
            page,
            (content.x - rect.x0) / scale,
            (content.y - rect.y0) / scale,
        ),
        on_page: rect.contains(content),
    })
}

fn page_point(loc: Location) -> Point {
    Point::new(loc.x, loc.y)
}

/// Where a boundary position lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caret {
    /// The boundary position.
    Ready(TextPos),
    /// The text of the page has not arrived yet.
    Pending,
    /// The page has no text.
    NoText,
}

/// The boundary position a drag at `loc` selects up to (`PageText::caret_at`).
pub fn caret_at(texts: PageTexts, loc: Location) -> Caret {
    let Some(text) = texts(loc.page) else {
        return Caret::Pending;
    };
    match text.caret_at(page_point(loc)) {
        Some(ch) => Caret::Ready(TextPos { page: loc.page, ch }),
        None => Caret::NoText,
    }
}

/// What a double or triple click selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Picked {
    /// This selection.
    Range(Selection),
    /// Nothing is under the pointer: the selection is cleared.
    Clear,
    /// The text of the page has not arrived yet.
    Pending,
}

/// The word (two clicks) or the line (three or more) under the pointer. Nothing is picked where
/// there is no character (`PageText::char_at`) or off the page.
pub fn pick_range(texts: PageTexts, hit: &Hit, clicks: u32) -> Picked {
    if !hit.on_page {
        return Picked::Clear;
    }
    let Some(text) = texts(hit.loc.page) else {
        return Picked::Pending;
    };
    let Some(ch) = text.char_at(page_point(hit.loc)) else {
        return Picked::Clear;
    };
    let range = if clicks >= 3 {
        text.line_of(ch)
    } else {
        text.word_at(ch)
    };
    if range.is_empty() {
        return Picked::Clear;
    }
    let page = hit.loc.page;
    Picked::Range(Selection {
        anchor: TextPos {
            page,
            ch: range.start,
        },
        focus: TextPos {
            page,
            ch: range.end,
        },
    })
}

/// Whether the pointer is over a character, for the I-beam cursor.
pub fn over_text(texts: PageTexts, hit: &Hit) -> bool {
    texts(hit.loc.page).is_some_and(|t| t.char_at(page_point(hit.loc)).is_some())
}

// ------------------------------------------------------------------------------------------------
// Clicks and drags
// ------------------------------------------------------------------------------------------------

/// Counts successive presses: 1, 2 (double click), 3 (triple click), then 1 again.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClickTracker {
    last: Option<(f64, Point, u32)>,
}

impl ClickTracker {
    /// Records a press at `time` (seconds) at `pos` (viewport position) and returns its count.
    pub fn press(&mut self, time: f64, pos: Point) -> u32 {
        let count = match self.last {
            Some((t, p, n))
                if n < 3
                    && time - t <= MULTI_CLICK_SECS
                    && distance(p, pos) <= MULTI_CLICK_DISTANCE =>
            {
                n + 1
            }
            _ => 1,
        };
        self.last = Some((time, pos, count));
        count
    }
}

fn distance(a: Point, b: Point) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// What a finished press turned out to be. This is the place where "click" and "drag" are told
/// apart; the link work package follows a link on [`Released::Click`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Released {
    /// A single click that did not move: where it was, and whether Shift was held. The selection
    /// has been cleared.
    Click {
        /// The position of the press.
        hit: Hit,
        /// Shift was held.
        shift: bool,
    },
    /// Anything else: a drag, a double or triple click, a Shift+click that extended the
    /// selection.
    Selected,
}

/// One press in progress.
#[derive(Clone, Debug)]
struct Drag {
    press: Hit,
    press_pos: Point,
    clicks: u32,
    /// Shift+click on an existing selection: the anchor stays and the focus follows the pointer.
    extend: bool,
    shift: bool,
    /// The anchor of a drag. Unknown while the text of the pressed page is pending.
    anchor: Option<TextPos>,
    /// The pointer moved far enough to count as a drag.
    started: bool,
    /// The press has been interpreted (its page text was there).
    resolved: bool,
    /// The latest pointer position.
    last: Hit,
    /// The viewport position of `last`.
    last_pos: Point,
}

impl Drag {
    /// Whether the focus follows the pointer: a single click or a Shift+click, once moved.
    fn follows(&self) -> bool {
        self.extend || self.clicks == 1
    }
}

/// The selection of one view and the press that is changing it.
#[derive(Clone, Debug, Default)]
pub struct SelectionState {
    selection: Option<Selection>,
    generation: Option<u32>,
    drag: Option<Drag>,
    clicks: ClickTracker,
    /// Where a finished copy leaves its character count.
    copy_inbox: CopyInbox,
    /// The "copied" notice being shown: the character count and when it appeared (seconds).
    notice: Option<(usize, f64)>,
}

impl SelectionState {
    /// The selection as it is, possibly empty.
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// The selection if it contains something.
    pub fn active(&self) -> Option<Selection> {
        self.selection().filter(|s| !s.is_empty())
    }

    /// Whether something is selected.
    pub fn has_selection(&self) -> bool {
        self.active().is_some()
    }

    /// Whether a press is in progress.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Whether a press is in progress that moves the end of the selection with the pointer
    /// (a drag, or a Shift+click). A held press that has not moved yet is not.
    pub fn is_selecting(&self) -> bool {
        self.drag.as_ref().is_some_and(|d| d.started && d.follows())
    }

    /// The viewport position of the pointer as of the last [`SelectionState::drag_to`], while a
    /// press is in progress.
    pub fn pointer(&self) -> Option<Point> {
        self.drag.as_ref().map(|d| d.last_pos)
    }

    /// Sets the selection, for tests of code that only reads it.
    #[cfg(test)]
    pub fn set_for_test(&mut self, selection: Selection) {
        self.selection = Some(selection);
    }

    /// The inbox a copy of this selection reports to; clones share it.
    pub fn copy_inbox(&self) -> CopyInbox {
        self.copy_inbox.clone()
    }

    /// The "copied" notice to show at time `now` (seconds): the number of characters copied and
    /// how long the notice stays. `None` when there is none or it has run out.
    pub fn copy_notice(&mut self, now: f64) -> Option<(usize, Duration)> {
        if let Some(chars) = self.copy_inbox.take() {
            self.notice = Some((chars, now));
        }
        let (chars, since) = self.notice?;
        let left = COPY_NOTICE_SECS - (now - since);
        if left <= 0.0 {
            self.notice = None;
            return None;
        }
        Some((chars, Duration::from_secs_f64(left)))
    }

    /// The character count of the notice that is up, for tests.
    #[cfg(test)]
    pub fn notice_chars(&self) -> Option<usize> {
        self.notice.map(|(chars, _)| chars)
    }

    /// Clears the selection and abandons the press in progress.
    pub fn clear(&mut self) {
        self.selection = None;
        self.drag = None;
    }

    /// Tells the state which document generation is shown. A different generation than before
    /// means the file was reloaded and character indices no longer mean the same thing: the
    /// selection goes (R-7).
    pub fn sync_generation(&mut self, generation: u32) {
        if self.generation.is_some_and(|g| g != generation) {
            self.clear();
        }
        self.generation = Some(generation);
    }

    /// The primary button went down at `pos` (viewport position, `hit` its document position),
    /// at `time` seconds. A plain press clears the selection and prepares a drag; a Shift press
    /// on an existing selection keeps its anchor and moves its end.
    pub fn press(&mut self, texts: PageTexts, pos: Point, hit: Hit, time: f64, shift: bool) {
        let clicks = self.clicks.press(time, pos);
        let base = self.selection.filter(|_| shift);
        let extend = base.is_some();
        self.drag = Some(Drag {
            press: hit,
            press_pos: pos,
            clicks: if extend { 1 } else { clicks },
            extend,
            shift,
            anchor: base.map(|s| s.anchor),
            started: extend,
            resolved: extend,
            last: hit,
            last_pos: pos,
        });
        if !extend {
            self.selection = None;
        }
        self.step(texts);
    }

    /// The pointer is at `pos` / `hit` while the button is down. Call it for every pointer move
    /// and once per frame (a stationary pointer over a scrolling view points at new text).
    pub fn drag_to(&mut self, texts: PageTexts, pos: Point, hit: Hit) {
        if let Some(d) = &mut self.drag {
            d.last = hit;
            d.last_pos = pos;
            if !d.started && distance(d.press_pos, pos) >= DRAG_START_DISTANCE {
                d.started = true;
            }
        }
        self.step(texts);
    }

    /// Per-frame upkeep without pointer news: retries a press whose page text was pending and
    /// requests the texts of every page the selection spans while a press is in progress.
    pub fn frame(&mut self, texts: PageTexts) {
        self.step(texts);
    }

    /// The button went up. `None` when no press was in progress.
    pub fn release(&mut self) -> Option<Released> {
        let d = self.drag.take()?;
        if !d.started && d.clicks == 1 && !d.extend {
            Some(Released::Click {
                hit: d.press,
                shift: d.shift,
            })
        } else {
            Some(Released::Selected)
        }
    }

    fn step(&mut self, texts: PageTexts) {
        let Some(d) = &mut self.drag else {
            return;
        };
        if !d.resolved {
            if d.clicks == 1 {
                match caret_at(texts, d.press.loc) {
                    Caret::Ready(p) => {
                        d.anchor = Some(p);
                        d.resolved = true;
                    }
                    Caret::NoText => d.resolved = true,
                    Caret::Pending => {}
                }
            } else {
                match pick_range(texts, &d.press, d.clicks) {
                    Picked::Range(sel) => {
                        self.selection = Some(sel);
                        d.resolved = true;
                    }
                    Picked::Clear => {
                        self.selection = None;
                        d.resolved = true;
                    }
                    Picked::Pending => {}
                }
            }
        }
        if d.resolved
            && d.started
            && d.follows()
            && let Some(anchor) = d.anchor
            && let Caret::Ready(focus) = caret_at(texts, d.last.loc)
        {
            self.selection = Some(Selection { anchor, focus });
        }
        // Everything between the ends is needed for the highlight and for copying.
        if let Some(sel) = self.selection {
            for page in sel.pages() {
                let _ = texts(page);
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Auto-scroll
// ------------------------------------------------------------------------------------------------

/// The vertical auto-scroll speed in logical pixels per second while dragging, for a pointer at
/// `pointer_y` (relative to the top of the view) in a view `viewport_h` high. Positive scrolls
/// down. Zero inside the view away from its top and bottom edges; grows with the distance into
/// the edge zone and beyond the view, up to [`AUTOSCROLL_MAX_FACTOR`] times [`AUTOSCROLL_SPEED`].
pub fn autoscroll_speed(pointer_y: f32, viewport_h: f32) -> f32 {
    let edge = AUTOSCROLL_EDGE.min(viewport_h / 2.0);
    if edge.is_nan() || edge <= 0.0 || !pointer_y.is_finite() {
        return 0.0;
    }
    let speed = |depth: f32| (depth / edge).min(AUTOSCROLL_MAX_FACTOR) * AUTOSCROLL_SPEED;
    let above = edge - pointer_y;
    let below = pointer_y - (viewport_h - edge);
    if above > 0.0 {
        -speed(above)
    } else if below > 0.0 {
        speed(below)
    } else {
        0.0
    }
}

// ------------------------------------------------------------------------------------------------
// Highlight
// ------------------------------------------------------------------------------------------------

/// One line of the selection on one page, in viewport coordinates (the top-left corner of the
/// view is the origin).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Highlight {
    /// The page it is on.
    pub page: u32,
    /// The area to tint, in logical pixels.
    pub rect: Rect,
}

/// The areas to tint for `sel`: one rectangle per selected line, for the visible pages only.
/// A page whose text is pending is skipped (and requested).
pub fn highlight_rects(
    sel: &Selection,
    layout: &PageLayout,
    scroll: Point,
    viewport: Size,
    texts: PageTexts,
) -> Vec<Highlight> {
    let mut out = Vec::new();
    if sel.is_empty() {
        return out;
    }
    let visible = layout.visible_pages(scroll, viewport);
    let scale = layout.scale();
    for page in sel.pages() {
        if !visible.contains(&page) {
            continue;
        }
        let Some(text) = texts(page) else {
            continue;
        };
        let Some(range) = sel.range_on(page, text.chars.len() as u32) else {
            continue;
        };
        let origin = layout.page_rect(page);
        for r in text.rects(range) {
            out.push(Highlight {
                page,
                rect: Rect::new(
                    origin.x0 + r.x0 * scale - scroll.x,
                    origin.y0 + r.y0 * scale - scroll.y,
                    origin.x0 + r.x1 * scale - scroll.x,
                    origin.y0 + r.y1 * scale - scroll.y,
                ),
            });
        }
    }
    out
}

// ------------------------------------------------------------------------------------------------
// Copy
// ------------------------------------------------------------------------------------------------

/// The text of a selection from the pages that are available; missing pages are skipped.
pub fn selection_string(sel: &Selection, texts: PageTexts) -> String {
    selection_text(sel, texts, COPY_OPTIONS)
}

/// How [`start_copy`] delivered the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyMode {
    /// Every page text was there: `done` has already run.
    Immediate,
    /// Some page text had to be fetched: `done` runs on a background thread when it is complete.
    Background,
}

/// Builds the text of `sel` and hands it to `done`. It never blocks the caller (rule R-1): when
/// every page text is there it runs `done` right away, else a background thread waits for the
/// missing pages (`page_text_wait`) and runs `done` afterwards. If the document was reloaded in
/// the meantime the text is dropped (R-7).
pub fn start_copy(
    doc: &DocumentHandle,
    sel: Selection,
    done: impl FnOnce(String) + Send + 'static,
) -> CopyMode {
    let pages: Vec<u32> = sel.pages().collect();
    let mut have: Vec<(u32, Option<Arc<PageText>>)> =
        pages.iter().map(|p| (*p, doc.page_text(*p))).collect();
    if have.iter().all(|(_, t)| t.is_some()) {
        let text = selection_string(&sel, &|p| {
            have.iter()
                .find(|(n, _)| *n == p)
                .and_then(|(_, t)| t.clone())
        });
        done(text);
        return CopyMode::Immediate;
    }
    let generation = doc.info().map(|i| i.generation);
    let doc = doc.clone();
    let spawned = std::thread::Builder::new()
        .name("copy-selection".into())
        .spawn(move || {
            for (page, text) in &mut have {
                if text.is_none() {
                    match doc.page_text_wait(*page) {
                        Ok(t) => *text = Some(t),
                        Err(e) => log::warn!("copy: no text for page {page}: {e}"),
                    }
                }
            }
            // Without a generation at the start the document was still opening: nothing to compare.
            if generation.is_some() && doc.info().map(|i| i.generation) != generation {
                log::debug!("copy: the document was reloaded, text dropped");
                return;
            }
            let text = selection_string(&sel, &|p| {
                have.iter()
                    .find(|(n, _)| *n == p)
                    .and_then(|(_, t)| t.clone())
            });
            done(text);
        });
    if let Err(e) = spawned {
        log::error!("could not start the copy thread: {e}");
    }
    CopyMode::Background
}

/// Where a finished copy leaves the number of copied characters for the view to announce.
/// Shared between the copy (possibly on a background thread) and the view.
#[derive(Clone, Debug, Default)]
pub struct CopyInbox(Arc<Mutex<Option<usize>>>);

impl CopyInbox {
    /// Records that `chars` characters were copied.
    pub fn post(&self, chars: usize) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(chars);
        }
    }

    /// Takes the pending announcement, if any.
    pub fn take(&self) -> Option<usize> {
        self.0.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// How long the "copied" notice stays visible, in seconds.
pub const COPY_NOTICE_SECS: f64 = 2.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::pdf_view::{MARGIN, PAGE_GAP};
    use refdesk_core::testutil::{PdfBuilder, TempPdf};
    use refdesk_core::{LayoutParams, ZoomMode};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::time::Duration;

    const VIEWPORT: Size = Size { w: 800.0, h: 600.0 };
    /// Far above what a slow machine needs; only reached when something is broken.
    const TIMEOUT: Duration = Duration::from_secs(60);

    /// A document opened through the engine, its page texts, and a layout of it.
    struct Fixture {
        doc: DocumentHandle,
        _pdf: TempPdf,
        texts: HashMap<u32, Arc<PageText>>,
        layout: PageLayout,
        scroll: Point,
    }

    impl Fixture {
        fn open(builder: &PdfBuilder, stem: &str) -> (DocumentHandle, TempPdf) {
            let pdf = builder.write_temp(stem);
            (DocumentHandle::open(pdf.path(), Arc::new(|| {})), pdf)
        }

        fn new(builder: &PdfBuilder, stem: &str, zoom: f32) -> Fixture {
            let (doc, pdf) = Fixture::open(builder, stem);
            let mut texts = HashMap::new();
            for page in 0..builder.page_count() as u32 {
                texts.insert(page, doc.page_text_wait(page).expect("page text"));
            }
            let info = loop {
                if let Some(info) = doc.info() {
                    break info;
                }
                // `page_text_wait` returned, so the document is open.
                std::thread::yield_now();
            };
            let layout = PageLayout::new(
                &info.page_sizes,
                LayoutParams {
                    viewport: VIEWPORT,
                    zoom: ZoomMode::Custom(zoom),
                    gap: PAGE_GAP,
                    margin: MARGIN,
                    fit_page: 0,
                },
            );
            Fixture {
                doc,
                _pdf: pdf,
                texts,
                layout,
                scroll: Point::default(),
            }
        }

        fn page(&self, page: u32) -> &PageText {
            self.texts.get(&page).expect("page")
        }

        fn lookup(&self) -> impl Fn(u32) -> Option<Arc<PageText>> + '_ {
            |p| self.texts.get(&p).cloned()
        }

        /// The viewport position of a page-space point.
        fn at(&self, page: u32, x: f32, y: f32) -> Point {
            let r = self.layout.page_rect(page);
            let s = self.layout.scale();
            Point::new(r.x0 + x * s - self.scroll.x, r.y0 + y * s - self.scroll.y)
        }

        fn hit(&self, vp: Point) -> Hit {
            locate(&self.layout, self.scroll, vp).expect("hit")
        }

        /// Just inside the left edge of character `i`, at mid height (page space).
        fn left_of(&self, page: u32, i: u32) -> Point {
            let b = self.page(page).chars[i as usize].quad.bounds();
            self.at(page, b.x0 + 0.3, (b.y0 + b.y1) / 2.0)
        }

        /// Just inside the right edge of character `i`.
        fn right_of(&self, page: u32, i: u32) -> Point {
            let b = self.page(page).chars[i as usize].quad.bounds();
            self.at(page, b.x1 - 0.3, (b.y0 + b.y1) / 2.0)
        }

        fn centre_of(&self, page: u32, i: u32) -> Point {
            let b = self.page(page).chars[i as usize].quad.bounds();
            self.at(page, (b.x0 + b.x1) / 2.0, (b.y0 + b.y1) / 2.0)
        }

        /// Index of the first character of `needle` on `page`.
        fn find(&self, page: u32, needle: &str) -> u32 {
            let all: String = self.page(page).chars.iter().map(|c| c.ch).collect();
            let byte = all.find(needle).expect("needle on the page");
            all[..byte].chars().count() as u32
        }

        /// Press at `from`, move to `to`, release; at `time`.
        fn drag(&self, st: &mut SelectionState, from: Point, to: Point, time: f64) -> Released {
            let texts = self.lookup();
            st.press(&texts, from, self.hit(from), time, false);
            st.drag_to(&texts, to, self.hit(to));
            st.release().expect("a press was in progress")
        }

        /// A click (press and release without moving) at `at`.
        fn click(&self, st: &mut SelectionState, at: Point, time: f64, shift: bool) -> Released {
            let texts = self.lookup();
            st.press(&texts, at, self.hit(at), time, shift);
            st.release().expect("a press was in progress")
        }

        fn text_of(&self, sel: &Selection) -> String {
            selection_string(sel, &self.lookup())
        }
    }

    fn lines_doc() -> PdfBuilder {
        PdfBuilder::new().page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "Alpha beta gamma");
            p.text(72.0, 116.0, 12.0, "delta epsilon zeta");
            p.text(72.0, 132.0, 12.0, "eta theta iota");
            p.text(72.0, 180.0, 12.0, "See SWS_EM_01004 for details");
            p.text(72.0, 240.0, 12.0, "A long speci-");
            p.text(72.0, 256.0, 12.0, "fication of the thing.");
        })
    }

    fn two_pages_doc() -> PdfBuilder {
        PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "First page opens here");
                p.text(72.0, 700.0, 12.0, "First page ends here");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "Second page begins now");
                p.text(72.0, 116.0, 12.0, "and continues below");
            })
    }

    fn two_columns_doc() -> PdfBuilder {
        PdfBuilder::new().page(612.0, 792.0, |p| {
            // Content stream order is reading order: the left column, then the right one.
            for (i, w) in ["left one", "left two", "left three"].iter().enumerate() {
                p.text(72.0, 100.0 + 16.0 * i as f32, 12.0, w);
            }
            for (i, w) in ["right one", "right two", "right three"].iter().enumerate() {
                p.text(330.0, 100.0 + 16.0 * i as f32, 12.0, w);
            }
        })
    }

    fn pos(page: u32, ch: u32) -> TextPos {
        TextPos { page, ch }
    }

    // ---- positions ----

    #[test]
    fn a_viewport_point_becomes_a_page_and_a_page_space_point_at_any_zoom_and_scroll() {
        for zoom in [1.0_f32, 2.0, 0.5] {
            let mut f = Fixture::new(&two_pages_doc(), "sel-locate", zoom);
            let s = f.layout.scale();
            // Scrolled so that page 1 is on screen, page 0 only partly.
            f.scroll = Point::new(0.0, f.layout.page_rect(1).y0 - 150.0);
            let at = f.at(1, 100.0, 40.0);
            let hit = f.hit(at);
            assert_eq!(hit.loc.page, 1, "zoom {zoom}");
            assert!((hit.loc.x - 100.0).abs() < 0.01, "x at zoom {zoom}");
            assert!((hit.loc.y - 40.0).abs() < 0.01, "y at zoom {zoom}");
            assert!(hit.on_page);
            // The same point on page 0 above it.
            let above = f.at(0, 30.0, 700.0);
            let hit = f.hit(above);
            assert_eq!(hit.loc.page, 0);
            assert!((hit.loc.y - 700.0).abs() < 0.01);
            // The gap between the pages belongs to the nearer page and is not "on" a page.
            let gap = Point::new(
                100.0,
                f.layout.page_rect(1).y0 - PAGE_GAP / 2.0 - f.scroll.y,
            );
            assert!(!f.hit(gap).on_page);
            let _ = s;
        }
    }

    #[test]
    fn a_point_in_the_margin_is_off_the_page_and_not_clamped() {
        let f = Fixture::new(&lines_doc(), "sel-margin", 1.0);
        let r = f.layout.page_rect(0);
        let hit = f.hit(Point::new(r.x0 - 5.0, r.y0 + 100.0));
        assert!(!hit.on_page);
        assert!(hit.loc.x < 0.0);
        assert_eq!(
            locate(&f.layout, Point::default(), Point::new(0.0, 0.0)).map(|h| h.loc.page),
            Some(0)
        );
    }

    // ---- drag ----

    #[test]
    fn dragging_inside_a_line_selects_those_characters() {
        let f = Fixture::new(&lines_doc(), "sel-line", 1.0);
        let beta = f.find(0, "beta");
        let mut st = SelectionState::default();
        let out = f.drag(&mut st, f.left_of(0, beta), f.right_of(0, beta + 3), 1.0);
        assert_eq!(out, Released::Selected);
        let sel = st.active().expect("selection");
        assert_eq!(sel.anchor, pos(0, beta));
        assert_eq!(sel.focus, pos(0, beta + 4));
        assert_eq!(f.text_of(&sel), "beta");
    }

    #[test]
    fn dragging_over_several_lines_joins_them_into_a_paragraph() {
        let f = Fixture::new(&lines_doc(), "sel-lines", 2.0);
        let gamma = f.find(0, "gamma");
        let theta = f.find(0, "theta");
        let mut st = SelectionState::default();
        f.drag(&mut st, f.left_of(0, gamma), f.right_of(0, theta + 4), 1.0);
        let sel = st.active().expect("selection");
        assert_eq!(f.text_of(&sel), "gamma delta epsilon zeta eta theta");
    }

    #[test]
    fn dragging_backwards_selects_the_same_text() {
        let f = Fixture::new(&lines_doc(), "sel-backwards", 1.0);
        let gamma = f.find(0, "gamma");
        let theta = f.find(0, "theta");
        let forward = {
            let mut st = SelectionState::default();
            f.drag(&mut st, f.left_of(0, gamma), f.right_of(0, theta + 4), 1.0);
            st.active().expect("selection")
        };
        let backward = {
            let mut st = SelectionState::default();
            f.drag(&mut st, f.right_of(0, theta + 4), f.left_of(0, gamma), 1.0);
            st.active().expect("selection")
        };
        assert_eq!(backward.anchor, forward.focus);
        assert_eq!(backward.focus, forward.anchor);
        assert_eq!(f.text_of(&backward), f.text_of(&forward));
        assert_eq!(f.text_of(&backward), "gamma delta epsilon zeta eta theta");
    }

    #[test]
    fn dragging_across_two_pages_selects_the_end_of_one_and_the_start_of_the_next() {
        let mut f = Fixture::new(&two_pages_doc(), "sel-pages", 1.0);
        let ends = f.find(0, "ends");
        let now = f.find(1, "now");
        // Both ends have to be on screen to drag; put the end of page 0 and the start of page
        // 1 in the viewport.
        f.scroll = Point::new(0.0, f.layout.page_rect(1).y0 - 400.0);
        let mut st = SelectionState::default();
        f.drag(&mut st, f.left_of(0, ends), f.right_of(1, now + 2), 1.0);
        let sel = st.active().expect("selection");
        assert_eq!(sel.anchor.page, 0);
        assert_eq!(sel.focus.page, 1);
        assert_eq!(f.text_of(&sel), "ends here\nSecond page begins now");
    }

    #[test]
    fn dragging_in_the_left_column_selects_the_left_column_only() {
        let f = Fixture::new(&two_columns_doc(), "sel-columns", 1.0);
        let one = f.find(0, "left one");
        let three = f.find(0, "left three");
        let mut st = SelectionState::default();
        // The pointer ends to the right of the left column's text, over the gap between the
        // columns, on the line of "left three".
        let end = f.right_of(0, three + 9);
        f.drag(&mut st, f.left_of(0, one), end, 1.0);
        let sel = st.active().expect("selection");
        let text = f.text_of(&sel);
        assert!(text.starts_with("left one"), "{text}");
        assert!(text.ends_with("left three"), "{text}");
        assert!(!text.contains("right"), "{text}");

        let rects = highlight_rects(&sel, &f.layout, f.scroll, VIEWPORT, &f.lookup());
        assert_eq!(rects.len(), 3, "one rectangle per line");
        let right_column_x = f.at(0, 330.0, 0.0).x;
        for h in &rects {
            assert!(h.rect.x1 < right_column_x, "{h:?} stays in the left column");
        }
    }

    #[test]
    fn a_page_without_text_selects_nothing_and_does_not_panic() {
        let b = PdfBuilder::new().page(612.0, 792.0, |_| {});
        let f = Fixture::new(&b, "sel-empty-page", 1.0);
        let mut st = SelectionState::default();
        let from = f.at(0, 100.0, 100.0);
        let to = f.at(0, 300.0, 300.0);
        f.drag(&mut st, from, to, 1.0);
        assert!(!st.has_selection());
        assert!(!over_text(&f.lookup(), &f.hit(from)));
    }

    #[test]
    fn a_small_movement_is_a_click_and_a_large_one_a_drag() {
        let f = Fixture::new(&lines_doc(), "sel-threshold", 1.0);
        let at = f.centre_of(0, 2);
        let mut st = SelectionState::default();
        let nudge = Point::new(at.x + DRAG_START_DISTANCE - 1.0, at.y);
        assert!(matches!(
            f.drag(&mut st, at, nudge, 1.0),
            Released::Click { .. }
        ));
        assert!(!st.has_selection());
        let far = Point::new(at.x + DRAG_START_DISTANCE + 1.0, at.y);
        assert_eq!(f.drag(&mut st, at, far, 5.0), Released::Selected);
    }

    // ---- clicks ----

    #[test]
    fn double_click_selects_the_word_and_a_code_identifier_whole() {
        let f = Fixture::new(&lines_doc(), "sel-word", 1.0);
        let mut st = SelectionState::default();
        let beta = f.find(0, "beta");
        let at = f.centre_of(0, beta + 1);
        assert!(matches!(
            f.click(&mut st, at, 1.0, false),
            Released::Click { .. }
        ));
        assert_eq!(f.click(&mut st, at, 1.2, false), Released::Selected);
        assert_eq!(f.text_of(&st.active().expect("selection")), "beta");

        // "SWS_EM_01004" is one word, wherever inside it the click lands.
        let id = f.find(0, "SWS_EM_01004");
        for inside in [0, 3, 7, 11] {
            let mut st = SelectionState::default();
            let at = f.centre_of(0, id + inside);
            f.click(&mut st, at, 1.0, false);
            f.click(&mut st, at, 1.2, false);
            assert_eq!(
                f.text_of(&st.active().expect("selection")),
                "SWS_EM_01004",
                "click at {inside}"
            );
        }
    }

    #[test]
    fn triple_click_selects_the_line() {
        let f = Fixture::new(&lines_doc(), "sel-triple", 1.0);
        let mut st = SelectionState::default();
        let at = f.centre_of(0, f.find(0, "epsilon") + 2);
        f.click(&mut st, at, 1.0, false);
        f.click(&mut st, at, 1.2, false);
        f.click(&mut st, at, 1.4, false);
        let sel = st.active().expect("selection");
        let line = f.page(0).lines[1].chars.clone();
        assert_eq!((sel.anchor.ch, sel.focus.ch), (line.start, line.end));
        assert_eq!(f.text_of(&sel), "delta epsilon zeta");
        // A fourth quick click starts over with a single click.
        assert!(matches!(
            f.click(&mut st, at, 1.6, false),
            Released::Click { .. }
        ));
        assert!(!st.has_selection());
    }

    #[test]
    fn double_click_where_there_is_no_text_clears_the_selection() {
        let f = Fixture::new(&lines_doc(), "sel-double-empty", 1.0);
        let mut st = SelectionState::default();
        let beta = f.find(0, "beta");
        f.drag(&mut st, f.left_of(0, beta), f.right_of(0, beta + 3), 1.0);
        assert!(st.has_selection());
        // Far below the text, on the page.
        let blank = f.at(0, 300.0, 600.0);
        f.click(&mut st, blank, 10.0, false);
        f.click(&mut st, blank, 10.2, false);
        assert!(!st.has_selection());
        // And off the page, in the margin.
        let r = f.layout.page_rect(0);
        let margin = Point::new(r.x0 - 6.0, f.centre_of(0, 3).y);
        f.click(&mut st, margin, 20.0, false);
        f.click(&mut st, margin, 20.2, false);
        assert!(!st.has_selection());
    }

    #[test]
    fn a_plain_click_clears_the_selection_and_reports_a_click() {
        let f = Fixture::new(&lines_doc(), "sel-click", 1.0);
        let mut st = SelectionState::default();
        let beta = f.find(0, "beta");
        f.drag(&mut st, f.left_of(0, beta), f.right_of(0, beta + 3), 1.0);
        assert!(st.has_selection());
        let at = f.centre_of(0, f.find(0, "iota"));
        let out = f.click(&mut st, at, 9.0, false);
        match out {
            Released::Click { hit, shift } => {
                assert!(!shift);
                assert_eq!(hit.loc.page, 0);
            }
            other => panic!("expected a click, got {other:?}"),
        }
        assert!(!st.has_selection());
        assert!(!st.is_dragging());
    }

    #[test]
    fn shift_click_extends_and_shrinks_from_the_start_of_the_selection() {
        let f = Fixture::new(&lines_doc(), "sel-shift", 1.0);
        let mut st = SelectionState::default();
        let beta = f.find(0, "beta");
        let gamma = f.find(0, "gamma");
        // "beta" selected by a double click.
        let at = f.centre_of(0, beta + 1);
        f.click(&mut st, at, 1.0, false);
        f.click(&mut st, at, 1.2, false);
        assert_eq!(f.text_of(&st.active().expect("selection")), "beta");

        // Extend to the end of "gamma".
        let out = f.click(&mut st, f.right_of(0, gamma + 4), 5.0, true);
        assert_eq!(out, Released::Selected);
        let sel = st.active().expect("selection");
        assert_eq!(sel.anchor, pos(0, beta), "the start stays");
        assert_eq!(f.text_of(&sel), "beta gamma");

        // Shrink: click inside "beta" again.
        f.click(&mut st, f.right_of(0, beta + 1), 9.0, true);
        let sel = st.active().expect("selection");
        assert_eq!(sel.anchor, pos(0, beta));
        assert_eq!(f.text_of(&sel), "be");

        // Shift+click before the start swaps the direction but keeps the anchor.
        f.click(&mut st, f.left_of(0, 2), 13.0, true);
        let sel = st.active().expect("selection");
        assert_eq!(sel.anchor, pos(0, beta));
        assert_eq!(sel.focus, pos(0, 2));
        assert_eq!(f.text_of(&sel), "pha");
    }

    #[test]
    fn shift_click_without_a_selection_is_a_plain_click() {
        let f = Fixture::new(&lines_doc(), "sel-shift-none", 1.0);
        let mut st = SelectionState::default();
        let out = f.click(&mut st, f.centre_of(0, 3), 1.0, true);
        assert_eq!(
            out,
            Released::Click {
                hit: f.hit(f.centre_of(0, 3)),
                shift: true
            }
        );
        assert!(!st.has_selection());
    }

    #[test]
    fn shift_drag_keeps_following_the_pointer() {
        let f = Fixture::new(&lines_doc(), "sel-shift-drag", 1.0);
        let mut st = SelectionState::default();
        let beta = f.find(0, "beta");
        f.drag(&mut st, f.left_of(0, beta), f.right_of(0, beta + 3), 1.0);
        let texts = f.lookup();
        let delta = f.find(0, "delta");
        st.press(
            &texts,
            f.centre_of(0, delta),
            f.hit(f.centre_of(0, delta)),
            9.0,
            true,
        );
        assert!(st.is_selecting());
        let p = f.right_of(0, delta + 4);
        st.drag_to(&texts, p, f.hit(p));
        assert_eq!(st.release(), Some(Released::Selected));
        assert_eq!(
            f.text_of(&st.active().expect("selection")),
            "beta gamma delta"
        );
    }

    #[test]
    fn click_tracker_counts_one_two_three_and_starts_over() {
        let mut t = ClickTracker::default();
        let p = Point::new(10.0, 10.0);
        assert_eq!(t.press(1.0, p), 1);
        assert_eq!(t.press(1.2, Point::new(12.0, 11.0)), 2);
        assert_eq!(t.press(1.4, p), 3);
        assert_eq!(t.press(1.5, p), 1, "a fourth press starts over");
        // Too slow, or too far.
        assert_eq!(t.press(1.5 + MULTI_CLICK_SECS + 0.1, p), 1);
        assert_eq!(
            t.press(2.2, Point::new(10.0 + MULTI_CLICK_DISTANCE + 1.0, 10.0)),
            1
        );
    }

    // ---- text that has not arrived ----

    #[test]
    fn a_press_on_a_page_without_text_waits_for_it_and_carries_on() {
        let f = Fixture::new(&lines_doc(), "sel-pending", 1.0);
        let ready = Cell::new(false);
        let asked = RefCell::new(Vec::new());
        let texts = |p: u32| {
            asked.borrow_mut().push(p);
            if ready.get() {
                f.texts.get(&p).cloned()
            } else {
                None
            }
        };
        let beta = f.find(0, "beta");
        let (from, to) = (f.left_of(0, beta), f.right_of(0, beta + 3));
        let mut st = SelectionState::default();
        st.press(&texts, from, f.hit(from), 1.0, false);
        st.drag_to(&texts, to, f.hit(to));
        assert!(!st.has_selection(), "nothing starts without text");
        assert!(asked.borrow().contains(&0), "the text was requested");
        assert!(st.is_dragging());
        // The text arrives; the next frame goes on.
        ready.set(true);
        st.frame(&texts);
        let sel = st.active().expect("selection after the text arrived");
        assert_eq!(f.text_of(&sel), "beta");
        assert_eq!(st.release(), Some(Released::Selected));
    }

    #[test]
    fn a_double_click_on_a_page_without_text_waits_for_it() {
        let f = Fixture::new(&lines_doc(), "sel-pending-word", 1.0);
        let ready = Cell::new(false);
        let texts = |p: u32| ready.get().then(|| f.texts.get(&p).cloned()).flatten();
        let at = f.centre_of(0, f.find(0, "gamma") + 1);
        let mut st = SelectionState::default();
        st.press(&texts, at, f.hit(at), 1.0, false);
        st.release();
        st.press(&texts, at, f.hit(at), 1.2, false);
        assert!(!st.has_selection());
        ready.set(true);
        st.frame(&texts);
        assert_eq!(f.text_of(&st.active().expect("selection")), "gamma");
    }

    #[test]
    fn dragging_requests_the_text_of_every_page_in_between() {
        let mut f = Fixture::new(
            &PdfBuilder::new()
                .page(612.0, 792.0, |p| {
                    p.text(72.0, 700.0, 12.0, "one");
                })
                .page(612.0, 792.0, |p| {
                    p.text(72.0, 100.0, 12.0, "two");
                })
                .page(612.0, 792.0, |p| {
                    p.text(72.0, 100.0, 12.0, "three");
                }),
            "sel-between",
            0.25,
        );
        f.scroll = Point::default();
        let asked = RefCell::new(Vec::new());
        let texts = |p: u32| {
            asked.borrow_mut().push(p);
            // The middle page has not been extracted.
            if p == 1 {
                None
            } else {
                f.texts.get(&p).cloned()
            }
        };
        let (from, to) = (f.left_of(0, 0), f.right_of(2, 2));
        let mut st = SelectionState::default();
        st.press(&texts, from, f.hit(from), 1.0, false);
        st.drag_to(&texts, to, f.hit(to));
        let sel = st.active().expect("selection");
        assert_eq!((sel.anchor.page, sel.focus.page), (0, 2));
        assert!(asked.borrow().contains(&1), "page 1 was requested");
    }

    // ---- highlight ----

    #[test]
    fn highlight_has_one_rectangle_per_line_inside_the_page() {
        for zoom in [1.0_f32, 2.0] {
            let mut f = Fixture::new(&lines_doc(), "sel-highlight", zoom);
            f.scroll = Point::new(0.0, 30.0);
            let gamma = f.find(0, "gamma");
            let theta = f.find(0, "theta");
            let mut st = SelectionState::default();
            f.drag(&mut st, f.left_of(0, gamma), f.right_of(0, theta + 4), 1.0);
            let sel = st.active().expect("selection");
            let rects = highlight_rects(&sel, &f.layout, f.scroll, VIEWPORT, &f.lookup());
            assert_eq!(rects.len(), 3, "gamma / delta epsilon zeta / eta theta");
            let page = f.layout.page_rect(0);
            for h in &rects {
                let r = h.rect;
                assert_eq!(h.page, 0);
                // Viewport coordinates, inside the page rectangle.
                assert!(r.x0 >= page.x0 - f.scroll.x - 0.01 && r.x1 <= page.x1 - f.scroll.x + 0.01);
                assert!(r.y0 >= page.y0 - f.scroll.y - 0.01 && r.y1 <= page.y1 - f.scroll.y + 0.01);
                assert!(r.width() > 0.0 && r.height() > 0.0);
            }
            // Top to bottom.
            assert!(rects.windows(2).all(|w| w[0].rect.y0 < w[1].rect.y0));
            // The first line starts at "gamma".
            let b = f.page(0).chars[gamma as usize].quad.bounds();
            let expected = f.at(0, b.x0, b.y0);
            assert!((rects[0].rect.x0 - expected.x).abs() < 0.01);
            assert!((rects[0].rect.y0 - expected.y).abs() < 0.01);
        }
    }

    #[test]
    fn highlight_skips_pages_that_are_not_visible() {
        let mut f = Fixture::new(&two_pages_doc(), "sel-highlight-visible", 1.0);
        let sel = Selection {
            anchor: pos(0, 0),
            focus: pos(1, f.page(1).chars.len() as u32),
        };
        // Only page 1 on screen.
        f.scroll = Point::new(0.0, f.layout.page_rect(1).y0 + 50.0);
        let rects = highlight_rects(&sel, &f.layout, f.scroll, VIEWPORT, &f.lookup());
        assert!(!rects.is_empty());
        assert!(rects.iter().all(|h| h.page == 1), "{rects:?}");
        // Both on screen.
        f.scroll = Point::new(0.0, f.layout.page_rect(1).y0 - 300.0);
        let rects = highlight_rects(&sel, &f.layout, f.scroll, VIEWPORT, &f.lookup());
        assert!(rects.iter().any(|h| h.page == 0) && rects.iter().any(|h| h.page == 1));
        // An empty selection has nothing.
        let empty = Selection {
            anchor: pos(0, 3),
            focus: pos(0, 3),
        };
        assert!(highlight_rects(&empty, &f.layout, f.scroll, VIEWPORT, &f.lookup()).is_empty());
        // A page whose text is pending is skipped.
        let none = |_: u32| None;
        assert!(highlight_rects(&sel, &f.layout, f.scroll, VIEWPORT, &none).is_empty());
    }

    // ---- copy ----

    #[test]
    fn copied_text_joins_lines_with_spaces_and_removes_the_hyphen_of_a_broken_word() {
        let f = Fixture::new(&lines_doc(), "sel-copy", 1.0);
        let all = Selection {
            anchor: pos(0, 0),
            focus: pos(0, f.page(0).chars.len() as u32),
        };
        let text = f.text_of(&all);
        assert!(
            text.contains("Alpha beta gamma delta epsilon zeta eta theta iota"),
            "{text}"
        );
        assert!(
            text.contains("A long specification of the thing."),
            "{text}"
        );
        assert!(!text.contains("speci-"), "{text}");
        // Paragraphs stay apart.
        assert!(
            text.contains("iota\nSee SWS_EM_01004 for details\nA long"),
            "{text}"
        );
    }

    #[test]
    fn copy_options_are_the_documented_ones() {
        const { assert!(COPY_OPTIONS.join_lines && COPY_OPTIONS.dehyphenate) };
    }

    #[test]
    fn a_copy_with_every_text_present_runs_at_once() {
        let f = Fixture::new(&two_pages_doc(), "sel-copy-now", 1.0);
        let sel = Selection {
            anchor: pos(0, 0),
            focus: pos(1, 5),
        };
        let (tx, rx) = mpsc::channel();
        let mode = start_copy(&f.doc, sel, move |t| {
            let _ = tx.send(t);
        });
        assert_eq!(mode, CopyMode::Immediate);
        let text = rx.try_recv().expect("delivered synchronously");
        assert!(text.starts_with("First page opens here"), "{text}");
        assert!(text.ends_with("\nSecon"), "{text}");
    }

    #[test]
    fn a_copy_over_pages_without_text_does_not_block_and_still_delivers_everything() {
        // Freshly opened: no page text has been requested yet, so the first `page_text` call of
        // `start_copy` is certain to miss.
        let b = two_pages_doc();
        let (doc, _pdf) = Fixture::open(&b, "sel-copy-wait");
        let sel = Selection {
            anchor: pos(0, 0),
            focus: pos(1, 22),
        };
        let (tx, rx) = mpsc::channel();
        let mode = start_copy(&doc, sel, move |t| {
            let _ = tx.send(t);
        });
        assert_eq!(mode, CopyMode::Background);
        let text = rx.recv_timeout(TIMEOUT).expect("the copy finished");
        assert_eq!(
            text,
            "First page opens here\nFirst page ends here\nSecond page begins now"
        );
    }

    #[test]
    fn copy_inbox_hands_a_count_over_once() {
        let inbox = CopyInbox::default();
        assert_eq!(inbox.take(), None);
        inbox.post(42);
        let other = inbox.clone();
        assert_eq!(other.take(), Some(42));
        assert_eq!(inbox.take(), None);
    }

    #[test]
    fn the_copy_notice_shows_for_a_while_and_then_goes() {
        let mut st = SelectionState::default();
        assert_eq!(st.copy_notice(1.0), None);
        st.copy_inbox().post(12);
        let (chars, left) = st.copy_notice(10.0).expect("notice");
        assert_eq!(chars, 12);
        assert_eq!(left, Duration::from_secs_f64(COPY_NOTICE_SECS));
        let (_, left) = st.copy_notice(10.5).expect("still up");
        assert!(left < Duration::from_secs_f64(COPY_NOTICE_SECS));
        assert_eq!(st.copy_notice(10.0 + COPY_NOTICE_SECS + 0.1), None);
        assert_eq!(st.notice_chars(), None);
    }

    // ---- auto-scroll ----

    #[test]
    fn autoscroll_is_zero_inside_and_grows_towards_and_past_the_edges() {
        let h = 600.0;
        assert_eq!(autoscroll_speed(300.0, h), 0.0);
        assert_eq!(
            autoscroll_speed(AUTOSCROLL_EDGE, h),
            0.0,
            "edge zone starts here"
        );
        assert_eq!(autoscroll_speed(h - AUTOSCROLL_EDGE, h), 0.0);
        // In the zone: towards the edge, faster.
        let near = autoscroll_speed(AUTOSCROLL_EDGE / 2.0, h);
        let at_edge = autoscroll_speed(0.0, h);
        assert!(
            near < 0.0 && at_edge < near,
            "up is negative: {near}, {at_edge}"
        );
        assert_eq!(at_edge, -AUTOSCROLL_SPEED);
        let near = autoscroll_speed(h - AUTOSCROLL_EDGE / 2.0, h);
        let at_edge = autoscroll_speed(h, h);
        assert!(
            near > 0.0 && at_edge > near,
            "down is positive: {near}, {at_edge}"
        );
        // Outside: faster still, up to the cap.
        assert!(autoscroll_speed(-20.0, h) < autoscroll_speed(0.0, h));
        assert!(autoscroll_speed(h + 20.0, h) > autoscroll_speed(h, h));
        let cap = AUTOSCROLL_SPEED * AUTOSCROLL_MAX_FACTOR;
        assert_eq!(autoscroll_speed(-10_000.0, h), -cap);
        assert_eq!(autoscroll_speed(h + 10_000.0, h), cap);
        // A tiny view still scrolls towards the nearer edge.
        assert!(autoscroll_speed(2.0, 20.0) < 0.0);
        assert!(autoscroll_speed(18.0, 20.0) > 0.0);
        assert_eq!(autoscroll_speed(10.0, 20.0), 0.0);
        assert_eq!(autoscroll_speed(f32::NAN, h), 0.0);
        assert_eq!(autoscroll_speed(5.0, 0.0), 0.0);
    }

    // ---- state ----

    #[test]
    fn a_new_generation_clears_the_selection() {
        let f = Fixture::new(&lines_doc(), "sel-generation", 1.0);
        let mut st = SelectionState::default();
        st.sync_generation(0);
        let beta = f.find(0, "beta");
        f.drag(&mut st, f.left_of(0, beta), f.right_of(0, beta + 3), 1.0);
        assert!(st.has_selection());
        st.sync_generation(0);
        assert!(st.has_selection(), "the same generation keeps it");
        st.sync_generation(1);
        assert!(!st.has_selection());
        assert!(!st.is_dragging());
        st.sync_generation(1);
        assert_eq!(st.selection(), None);
    }

    #[test]
    fn the_cursor_is_an_i_beam_only_over_text() {
        let f = Fixture::new(&lines_doc(), "sel-cursor", 1.0);
        let texts = f.lookup();
        assert!(over_text(&texts, &f.hit(f.centre_of(0, 3))));
        assert!(!over_text(&texts, &f.hit(f.at(0, 300.0, 600.0))));
        let none = |_: u32| None;
        assert!(!over_text(&none, &f.hit(f.centre_of(0, 3))));
    }
}
