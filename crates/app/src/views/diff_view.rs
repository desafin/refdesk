//! The comparison view (WP-62; `docs/SPEC.md` DIFF-1, DIFF-3, DIFF-4, part of DIFF-5;
//! `docs/ARCHITECTURE.md` §4.9).
//!
//! A comparison tab holds two ordinary PDF views (each a [`PdfTab`] with its own `ViewState`
//! and `PdfViewUiState`) side by side: the older document on the left, the newer on the right.
//! Both are drawn with [`pdf_view::show`] unchanged; the marks of the [`DiffResult`] are painted
//! on top, placed with the view's layout and scroll offset.
//!
//! The comparison itself runs in a [`DiffJob`] on its own thread (rule R-1). A finished result
//! carries the document generations it was computed from; once a document is reloaded the marks
//! are hidden and the view offers to compare again (rule R-7).
//!
//! The decisions are plain functions tested without egui (rule R-10): where a mark lands on the
//! screen ([`mark_rects`], [`change_bar`]), its colors ([`mark_fill`]), which change is the next
//! or previous one ([`pick_change`], [`reading_pos`]) and where the other side scrolls to
//! ([`place_scroll`], [`DiffTab::follow`]). The egui part draws and reads the pointer.
//!
//! WP-63 adds the change list on the right of the tab (DIFF-5) and a position strip beside the
//! scroll bar of each view (DIFF-7). Both are built in `panels::changes`; this file only gives
//! them their place: the panel is a right-hand side panel of the tab, and each side reserves a
//! strip of [`STRIP_WIDTH`] next to its view, so `pdf_view` is not touched.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use egui::{Color32, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use refdesk_core::diff::{MAX_MARGIN, MarginSide, Margins};
use refdesk_core::{
    DiffJob, DiffOptions, DiffResult, DocStatus, DocumentHandle, DocumentInfo, Location, Mark,
    MarkKind, Notifier, Outline, PageLayout, Point, Rect, SessionTab, Side, Size, ViewState,
};

use crate::action::Action;
use crate::commands;
use crate::i18n::tr;
use crate::panels::changes::{self, ChangesState, Overview, PanelContent, STRIP_WIDTH};
use crate::state::{AppState, DocId, DocStore, PdfTab, TabId};
use crate::views::pdf_view::{self, PdfViewUiState, ViewContext, anchor_at};

/// The reference line of synchronized scrolling, as a fraction of the viewport height from its
/// top: the two documents are aligned at this line (DIFF-4).
pub const SYNC_LINE: f32 = 0.25;

/// Where a change is placed when moving to it, as a fraction of the viewport height (DIFF-5).
pub const CHANGE_LINE: f32 = 1.0 / 3.0;

/// Where a click on a position strip puts the clicked place, as a fraction of the viewport
/// height from its top: the middle.
const STRIP_LINE: f32 = 0.5;

/// Two positions closer than this (pt) count as the same place when looking for the next change.
const NAV_TOLERANCE: f64 = 1.0;

/// Width of the bar beside removed and added marks, and its distance from the mark (logical
/// pixels).
const BAR_WIDTH: f32 = 3.0;
const BAR_GAP: f32 = 2.0;

/// The space between the two views (logical pixels).
const SPLIT_GAP: f32 = 4.0;

/// What a finished comparison gave: the differences, or why there are none.
pub type Outcome = Result<Arc<DiffResult>, String>;

/// The other side.
pub fn other(side: Side) -> Side {
    match side {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
    }
}

/// The part of a pair that belongs to `side`.
pub fn pick<T: Copy>(pair: (T, T), side: Side) -> T {
    match side {
        Side::Left => pair.0,
        Side::Right => pair.1,
    }
}

// ------------------------------------------------------------------------------------------------
// The tab
// ------------------------------------------------------------------------------------------------

/// A comparison tab (`Tab::Diff`).
#[derive(Clone, Debug)]
pub struct DiffTab {
    /// The older document and its view.
    pub left: PdfTab,
    /// The newer document and its view.
    pub right: PdfTab,
    /// What the comparison ignores.
    pub options: DiffOptions,
    /// The running comparison. Shared only so the tab stays `Clone`; dropping the last
    /// reference cancels it.
    pub job: Option<Arc<DiffJob>>,
    /// The finished comparison. `None` while one runs, and after a cancel.
    pub outcome: Option<Outcome>,
    /// Scrolling one side moves the other to the corresponding place (DIFF-4).
    pub sync_scroll: bool,
    /// The change moved to last (index into `DiffResult::changes`).
    pub current: Option<usize>,
    /// The side the pointer was over last; it drives synchronized scrolling and takes the keys.
    pub active: Side,
    /// A short message for the band (an i18n key), such as "this is the last change".
    pub notice: Option<&'static str>,
    /// The change list on the right of the tab (DIFF-5) and what the position strips draw from.
    pub changes: ChangesState,
    /// The excluded margins as the controls show them, which may be ahead of
    /// `options.margins` while one is being dragged (DIFF-11).
    pub margins: MarginEdit,
    /// Keep a thin line at the edge of the excluded margins while the options are closed.
    pub margin_guides: bool,
    /// The reference locations ([`SYNC_LINE`]) of both sides as of the last frame, to see which
    /// side moved. `None` until both views are laid out.
    last_ref: Option<(Location, Location)>,
    /// The view anchors right after the last move to a change: while the views are still there,
    /// next/previous continue from that change instead of from the position.
    nav_anchor: Option<(Location, Location)>,
}

/// Where a comparison tab is, as shown in its band.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Comparing; the progress from 0.0 to 1.0.
    Running(f32),
    /// The user cancelled the comparison.
    Cancelled,
    /// The documents could not be compared.
    Failed(String),
    /// A document changed since the result was computed (rule R-7).
    Stale,
    /// The result, valid for the documents as they are.
    Done(Arc<DiffResult>),
}

/// What moving to the next or previous change did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavOutcome {
    /// Both views moved to this change.
    Moved(usize),
    /// There is no change further in that direction; nothing moved.
    AtEnd,
    /// The documents have no differences.
    NoChanges,
}

/// A view of a document for one side of a comparison, at the top of the document.
pub fn side_tab(doc: DocId, path: PathBuf) -> PdfTab {
    PdfTab {
        doc,
        view: ViewState::new(path),
        ui: PdfViewUiState::default(),
        toc: Box::default(),
        preview: false,
    }
}

impl DiffTab {
    /// A comparison of two views that has not started yet (see [`DiffTab::start`]).
    pub fn new(left: PdfTab, right: PdfTab) -> DiffTab {
        DiffTab {
            left,
            right,
            options: DiffOptions::default(),
            job: None,
            outcome: None,
            sync_scroll: true,
            current: None,
            active: Side::Left,
            notice: None,
            changes: ChangesState::default(),
            margins: MarginEdit::new(Margins::default()),
            margin_guides: true,
            last_ref: None,
            nav_anchor: None,
        }
    }

    /// The view of one side.
    pub fn side(&self, side: Side) -> &PdfTab {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }

    /// The view of one side, for changing it.
    pub fn side_mut(&mut self, side: Side) -> &mut PdfTab {
        match side {
            Side::Left => &mut self.left,
            Side::Right => &mut self.right,
        }
    }

    /// The documents compared, left then right (the same id twice for a file compared with
    /// itself).
    pub fn docs(&self) -> [DocId; 2] {
        [self.left.doc, self.right.doc]
    }

    /// Starts (or restarts) comparing the two documents with the current options. A previous
    /// run is cancelled and its result dropped.
    pub fn start(&mut self, docs: &DocStore, notifier: Notifier) {
        self.job = None;
        self.outcome = None;
        self.current = None;
        self.nav_anchor = None;
        self.notice = None;
        match (docs.get(self.left.doc), docs.get(self.right.doc)) {
            (Some(left), Some(right)) => {
                let job = DiffJob::start(left.clone(), right.clone(), self.options, notifier);
                self.job = Some(Arc::new(job));
            }
            _ => self.outcome = Some(Err(tr("diff.no_document").to_owned())),
        }
    }

    /// Stops the running comparison. The band then offers to compare again.
    pub fn cancel(&mut self) {
        if self.job.take().is_some() {
            self.outcome = None;
        }
    }

    /// Takes the result of a finished comparison into the tab. Returns whether it arrived now.
    pub fn poll(&mut self) -> bool {
        let Some(outcome) = self.job.as_ref().and_then(|job| job.result()) else {
            return false;
        };
        self.job = None;
        self.outcome = Some(outcome);
        true
    }

    /// The result, if there is one (it may be stale; see [`DiffTab::fresh_result`]).
    pub fn result(&self) -> Option<&Arc<DiffResult>> {
        self.outcome.as_ref().and_then(|o| o.as_ref().ok())
    }

    /// Where the comparison is.
    pub fn phase(&self, docs: &DocStore) -> Phase {
        if let Some(job) = &self.job {
            return Phase::Running(job.progress());
        }
        match &self.outcome {
            None => Phase::Cancelled,
            Some(Err(message)) => Phase::Failed(message.clone()),
            Some(Ok(result)) => {
                if current_generations(self, docs) == Some(result.generations) {
                    Phase::Done(Arc::clone(result))
                } else {
                    Phase::Stale
                }
            }
        }
    }

    /// The result when it still describes the documents as they are now.
    pub fn fresh_result(&self, docs: &DocStore) -> Option<Arc<DiffResult>> {
        match self.phase(docs) {
            Phase::Done(result) => Some(result),
            _ => None,
        }
    }

    /// Changes the options and compares again when they differ from the current ones.
    pub fn set_options(&mut self, options: DiffOptions, docs: &DocStore, notifier: Notifier) {
        let mut options = options;
        options.margins = options.margins.clamped();
        if options != self.options {
            self.options = options;
            self.margins.follow(options.margins);
            self.start(docs, notifier);
        }
    }

    /// Makes `margins` the excluded margins before the first comparison (a new tab starts with
    /// the ones used last). They are limited to the allowed range.
    pub fn start_with_margins(&mut self, margins: Margins) {
        let margins = margins.snapped();
        self.options.margins = margins;
        self.margins = MarginEdit::new(margins);
    }

    /// The excluded margins to compare with now, if the controls moved on from the ones in use:
    /// see [`MarginEdit::due`]. `now` is the time of the frame in seconds.
    pub fn margins_due(&self, now: f64) -> Option<Margins> {
        self.margins.due(self.options.margins, now)
    }

    /// Exchanges the two sides (older becomes newer) and compares again.
    pub fn swap(&mut self, docs: &DocStore, notifier: Notifier) {
        std::mem::swap(&mut self.left, &mut self.right);
        self.active = other(self.active);
        self.last_ref = None;
        self.start(docs, notifier);
    }

    /// Turns synchronized scrolling on or off. Turning it on aligns the other side with the
    /// active one at once.
    pub fn set_sync(&mut self, on: bool, result: Option<&DiffResult>) {
        self.sync_scroll = on;
        if on {
            self.follow(self.active, result, true);
        }
    }

    /// The reference locations ([`SYNC_LINE`]) of both views; `None` until both are laid out.
    pub fn reference_points(&self) -> Option<(Location, Location)> {
        let at = |tab: &PdfTab| {
            tab.ui
                .layout()
                .map(|layout| ref_location(layout, tab.ui.scroll(), tab.ui.viewport(), SYNC_LINE))
        };
        Some((at(&self.left)?, at(&self.right)?))
    }

    /// Synchronized scrolling for one frame, after the `driver` side has taken its input: when
    /// the driver's reference location moved since the last frame (or `force`), the other side
    /// scrolls so that the corresponding location ([`DiffResult::map_location`]) is at the same
    /// height. Only the driver moves the other side, so the two never push each other back and
    /// forth. Returns whether the other side moved. Does nothing (but remember the positions)
    /// while synchronized scrolling is off or there is no valid result.
    pub fn follow(&mut self, driver: Side, result: Option<&DiffResult>, force: bool) -> bool {
        let now = self.reference_points();
        let mut moved = false;
        if let (true, Some(result), Some(now)) = (self.sync_scroll, result, now) {
            let driver_moved = self
                .last_ref
                .is_some_and(|last| pick(last, driver) != pick(now, driver));
            if driver_moved || force {
                let target = result.map_location(driver, pick(now, driver));
                let follower = self.side_mut(other(driver));
                if let Some(layout) = follower.ui.layout() {
                    let viewport = follower.ui.viewport();
                    let to =
                        place_scroll(layout, target, viewport, SYNC_LINE, follower.ui.scroll().x);
                    let before = follower.ui.scroll();
                    follower.ui.scroll_to(to);
                    follower.ui.commit(&mut follower.view);
                    moved = follower.ui.scroll() != before;
                }
            }
        }
        self.last_ref = self.reference_points();
        moved
    }

    /// With synchronized scrolling, gives the other side the zoom the `driver` has (DIFF-4) and
    /// lays it out again at once. `info` is the other side's document. Returns whether the zoom
    /// changed.
    pub fn sync_zoom(&mut self, driver: Side, info: Option<&DocumentInfo>) -> bool {
        let zoom = self.side(driver).view.zoom;
        let follower = self.side_mut(other(driver));
        if follower.view.zoom == zoom {
            return false;
        }
        follower.view.zoom = zoom;
        if let Some(info) = info {
            let viewport = follower.ui.viewport();
            follower
                .ui
                .sync(&follower.view, &info.page_sizes, info.generation, viewport);
            follower.ui.commit(&mut follower.view);
        }
        true
    }

    /// Moves both views to the next (`forward`) or previous change, counted from where the
    /// active view is now, or from the change moved to last while the views are still there.
    /// At either end nothing moves and a notice says so.
    pub fn go_to_change(&mut self, result: &DiffResult, forward: bool) -> NavOutcome {
        if result.changes.is_empty() {
            self.notice = Some("diff.no_changes");
            return NavOutcome::NoChanges;
        }
        let side = self.active;
        let sizes = page_sizes(result, side);
        let anchors: Vec<f64> = result
            .changes
            .iter()
            .map(|c| stacked_y(sizes, pick(c.anchor, side)))
            .collect();
        let at_current = self.nav_anchor.is_some()
            && self.nav_anchor == Some((self.left.view.anchor, self.right.view.anchor));
        let tab = self.side(side);
        let pos = match tab.ui.layout() {
            Some(layout) => reading_pos(
                layout,
                tab.ui.scroll(),
                tab.ui.viewport(),
                CHANGE_LINE,
                sizes,
            ),
            // Not drawn yet: it shows its anchor at the top.
            None if tab.view.anchor == Location::default() => f64::NEG_INFINITY,
            None => stacked_y(sizes, tab.view.anchor),
        };
        let at = if at_current { self.current } else { None };
        match pick_change(&anchors, pos, at, forward) {
            Some(i) => {
                self.move_to_change(result, i);
                NavOutcome::Moved(i)
            }
            None => {
                self.notice = Some(if forward {
                    "diff.last_change"
                } else {
                    "diff.first_change"
                });
                NavOutcome::AtEnd
            }
        }
    }

    /// Moves both views so that change `index` sits at [`CHANGE_LINE`] on each side (as far as
    /// the document can scroll).
    pub fn move_to_change(&mut self, result: &DiffResult, index: usize) {
        let Some(change) = result.changes.get(index) else {
            return;
        };
        let mut laid_out = true;
        for side in [Side::Left, Side::Right] {
            let loc = pick(change.anchor, side);
            let tab = self.side_mut(side);
            match tab.ui.layout() {
                Some(layout) => {
                    let to = place_scroll(
                        layout,
                        loc,
                        tab.ui.viewport(),
                        CHANGE_LINE,
                        tab.ui.scroll().x,
                    );
                    tab.ui.scroll_to(to);
                    tab.ui.commit(&mut tab.view);
                }
                None => {
                    // Not laid out yet: show the place at the top; the view scrolls there when
                    // it is first drawn.
                    tab.view.anchor = Location::new(loc.page, 0.0, loc.y);
                    laid_out = false;
                }
            }
        }
        self.current = Some(index);
        self.notice = None;
        self.nav_anchor = laid_out.then_some((self.left.view.anchor, self.right.view.anchor));
        // Both sides were placed on purpose: the next frame must not re-align them.
        self.last_ref = self.reference_points();
    }

    /// The tab as stored in the session (`SessionTab::Diff`). `None` when a document is gone.
    #[allow(dead_code)] // used by session saving (a later work package)
    pub fn to_session(&self, docs: &DocStore) -> Option<SessionTab> {
        Some(SessionTab::Diff {
            left: docs.get(self.left.doc)?.path().to_path_buf(),
            right: docs.get(self.right.doc)?.path().to_path_buf(),
        })
    }
}

/// Scrolls a view so that `loc` is on the line [`STRIP_LINE`] of its viewport. Returns whether it
/// moved; a view that is not laid out yet does not.
fn center_on(pdf: &mut PdfTab, loc: Location) -> bool {
    let Some(layout) = pdf.ui.layout() else {
        return false;
    };
    let to = place_scroll(
        layout,
        loc,
        pdf.ui.viewport(),
        STRIP_LINE,
        pdf.ui.scroll().x,
    );
    let before = pdf.ui.scroll();
    pdf.ui.scroll_to(to);
    pdf.ui.commit(&mut pdf.view);
    pdf.ui.scroll() != before
}

/// The current generations of both documents, if both are open.
fn current_generations(tab: &DiffTab, docs: &DocStore) -> Option<(u32, u32)> {
    let generation = |id: DocId| docs.get(id)?.info().map(|i| i.generation);
    Some((generation(tab.left.doc)?, generation(tab.right.doc)?))
}

/// The page sizes of one side of a result.
fn page_sizes(result: &DiffResult, side: Side) -> &[Size] {
    match side {
        Side::Left => &result.left_page_sizes,
        Side::Right => &result.right_page_sizes,
    }
}

// ------------------------------------------------------------------------------------------------
// Decisions (pure)
// ------------------------------------------------------------------------------------------------

/// The pages stacked one below the other without gaps: the y in pt of `loc` from the top of the
/// document. A page past the end counts as the last; `y` is not clamped.
pub fn stacked_y(sizes: &[Size], loc: Location) -> f64 {
    let page = (loc.page as usize).min(sizes.len().saturating_sub(1));
    let above: f64 = sizes
        .iter()
        .take(page)
        .map(|s| f64::from(s.h.max(0.0)))
        .sum();
    above + f64::from(loc.y)
}

/// The document location on the line `line` (a fraction of the viewport height from its top)
/// of a view.
pub fn ref_location(layout: &PageLayout, scroll: Point, viewport: Size, line: f32) -> Location {
    anchor_at(layout, Point::new(scroll.x, scroll.y + viewport.h * line))
}

/// The scroll offset that puts `loc` on the line `line` of the viewport, keeping the horizontal
/// offset `x`; limited to the valid range.
pub fn place_scroll(
    layout: &PageLayout,
    loc: Location,
    viewport: Size,
    line: f32,
    x: f32,
) -> Point {
    let p = layout.content_pos(loc);
    layout.clamp_scroll(Point::new(x, p.y - viewport.h * line), viewport)
}

/// The position (stacked y, see [`stacked_y`]) next/previous change counts from: the location on
/// the line `line` of the view. A view that cannot scroll further up counts as before everything,
/// one that cannot scroll further down as after everything, so the changes on the first and the
/// last screen are reachable.
pub fn reading_pos(
    layout: &PageLayout,
    scroll: Point,
    viewport: Size,
    line: f32,
    sizes: &[Size],
) -> f64 {
    let max = layout.max_scroll(viewport);
    if scroll.y <= 0.5 {
        f64::NEG_INFINITY
    } else if scroll.y >= max.y - 0.5 {
        f64::INFINITY
    } else {
        stacked_y(sizes, ref_location(layout, scroll, viewport, line))
    }
}

/// The change to move to. `anchors` are the stacked y of every change on the side counted on, in
/// document order; `pos` is the current position on that side. `at` is the change the views
/// still show from the last move: then it is simply the neighbor in the list (several changes may
/// share a place). Otherwise the first change after `pos` (or the last before it). `None` at the
/// end.
pub fn pick_change(anchors: &[f64], pos: f64, at: Option<usize>, forward: bool) -> Option<usize> {
    if let Some(i) = at.filter(|i| *i < anchors.len()) {
        return if forward {
            (i + 1 < anchors.len()).then_some(i + 1)
        } else {
            i.checked_sub(1)
        };
    }
    if forward {
        anchors.iter().position(|y| *y > pos + NAV_TOLERANCE)
    } else {
        anchors.iter().rposition(|y| *y < pos - NAV_TOLERANCE)
    }
}

/// A mark placed on the screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenMark {
    /// Relative to the top-left corner of the view, in logical pixels.
    pub rect: Rect,
    /// How it is drawn.
    pub kind: MarkKind,
    /// The change it belongs to.
    pub change: u32,
}

/// The marks of the visible pages of a view, placed relative to its top-left corner. Marks of
/// pages outside the viewport, and marks scrolled out of it, are left out. Order is kept, so a
/// page's word marks come after its paragraph marks.
pub fn mark_rects<'m>(
    layout: &PageLayout,
    scroll: Point,
    viewport: Size,
    marks: impl Fn(u32) -> &'m [Mark],
) -> Vec<ScreenMark> {
    let s = layout.scale();
    let mut out = Vec::new();
    for page in layout.visible_pages(scroll, viewport) {
        let page_rect = layout.page_rect(page);
        let (ox, oy) = (page_rect.x0 - scroll.x, page_rect.y0 - scroll.y);
        for mark in marks(page) {
            let rect = Rect::new(
                ox + mark.rect.x0 * s,
                oy + mark.rect.y0 * s,
                ox + mark.rect.x1 * s,
                oy + mark.rect.y1 * s,
            );
            let outside =
                rect.x1 < 0.0 || rect.y1 < 0.0 || rect.x0 > viewport.w || rect.y0 > viewport.h;
            if !outside {
                out.push(ScreenMark {
                    rect,
                    kind: mark.kind,
                    change: mark.change,
                });
            }
        }
    }
    out
}

/// The bar drawn beside a mark so that the kind does not rest on color alone: on the left edge
/// for removed text, on the right edge for added text, none for modifications.
pub fn change_bar(kind: MarkKind, rect: Rect) -> Option<Rect> {
    match kind {
        MarkKind::Removed => Some(Rect::new(
            rect.x0 - BAR_GAP - BAR_WIDTH,
            rect.y0,
            rect.x0 - BAR_GAP,
            rect.y1,
        )),
        MarkKind::Added => Some(Rect::new(
            rect.x1 + BAR_GAP,
            rect.y0,
            rect.x1 + BAR_GAP + BAR_WIDTH,
            rect.y1,
        )),
        MarkKind::ModifiedBlock | MarkKind::ModifiedWord => None,
    }
}

/// The translucent fill of a mark, as unmultiplied RGBA. Pages are drawn on white in both
/// themes (the PDF view paints rendered pages, or white while they render), so one palette
/// serves both: the alpha is low enough that black text under a mark stays readable.
pub fn mark_fill(kind: MarkKind) -> [u8; 4] {
    match kind {
        MarkKind::Removed => [230, 40, 40, 70],
        MarkKind::Added => [30, 170, 60, 70],
        MarkKind::ModifiedBlock => [255, 215, 0, 55],
        MarkKind::ModifiedWord => [245, 120, 0, 110],
    }
}

/// The opaque color of the bar beside a removed or added mark.
pub fn bar_color(kind: MarkKind) -> [u8; 4] {
    match kind {
        MarkKind::Removed => [200, 20, 20, 255],
        MarkKind::Added => [20, 140, 40, 255],
        MarkKind::ModifiedBlock | MarkKind::ModifiedWord => [200, 110, 0, 255],
    }
}

/// The outline of the marks of the current change.
const CURRENT_OUTLINE: [u8; 4] = [30, 90, 220, 255];

/// "3 added · 1 removed · 2 changed".
pub fn counts_text(result: &DiffResult) -> String {
    tr("diff.counts")
        .replace("{added}", &result.added.to_string())
        .replace("{removed}", &result.removed.to_string())
        .replace("{modified}", &result.modified.to_string())
}

/// "2 / 7", or "– / 7" before moving to a change.
pub fn position_text(current: Option<usize>, total: usize) -> String {
    match current.filter(|c| *c < total) {
        Some(c) => format!("{} / {total}", c + 1),
        None => format!("– / {total}"),
    }
}

/// The file name of a path, or the whole path when it has none.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The title of a comparison tab: "Compare: old.pdf ↔ new.pdf".
pub fn tab_title(tab: &DiffTab, docs: &DocStore) -> String {
    let name = |id: DocId| {
        docs.get(id)
            .map(|d| file_name(d.path()))
            .unwrap_or_else(|| tr("tab.unknown").to_owned())
    };
    tr("diff.tab_title")
        .replace("{left}", &name(tab.left.doc))
        .replace("{right}", &name(tab.right.doc))
}

/// The tooltip of a comparison tab: both full paths.
pub fn tab_tooltip(tab: &DiffTab, docs: &DocStore) -> Option<String> {
    let left = docs.get(tab.left.doc)?.path().display().to_string();
    let right = docs.get(tab.right.doc)?.path().display().to_string();
    Some(format!("{left}\n{right}"))
}

/// What the status bar shows for a comparison tab: the phase and the counts.
pub fn status_text(tab: &DiffTab, docs: &DocStore) -> String {
    match tab.phase(docs) {
        Phase::Running(p) => format!("{} {}%", tr("diff.running"), (p * 100.0).round() as u32),
        Phase::Cancelled => tr("diff.cancelled").to_owned(),
        Phase::Failed(message) => tr("diff.failed").replace("{}", &message),
        Phase::Stale => tr("diff.stale").to_owned(),
        Phase::Done(result) => counts_text(&result),
    }
}

// ------------------------------------------------------------------------------------------------
// Excluded margins (DIFF-11)
// ------------------------------------------------------------------------------------------------

/// How long a value dragged in the margin controls must stay as it is before the comparison runs
/// without waiting for the button to be let go (seconds).
pub const MARGIN_SETTLE: f64 = 0.4;

/// The excluded margins as the controls show them. While one is dragged the comparison is not
/// run again: the bands that would be left out are painted on the pages instead, and the
/// comparison runs when the drag ends or the value has stayed put for [`MARGIN_SETTLE`].
#[derive(Clone, Debug, PartialEq)]
pub struct MarginEdit {
    shown: Margins,
    /// When `shown` last changed (seconds).
    changed_at: f64,
    /// A control is being dragged right now (set by the drawing each frame).
    pub dragging: bool,
    /// The options menu with the controls is open (set by the drawing each frame).
    pub open: bool,
}

impl MarginEdit {
    /// Controls showing `margins`, which are the ones in use.
    pub fn new(margins: Margins) -> MarginEdit {
        MarginEdit {
            shown: margins,
            changed_at: 0.0,
            dragging: false,
            open: false,
        }
    }

    /// The margins the controls show.
    pub fn shown(&self) -> Margins {
        self.shown
    }

    /// Sets one side to `ratio` (a fraction of the page, snapped to the control step and
    /// limited to the allowed range). Returns whether the value changed.
    pub fn set(&mut self, side: MarginSide, ratio: f32, now: f64) -> bool {
        let mut next = self.shown;
        next.set(side, ratio);
        self.replace(next.snapped(), now)
    }

    /// Sets every side to 0. Returns whether anything changed.
    pub fn reset(&mut self, now: f64) -> bool {
        self.replace(Margins::default(), now)
    }

    fn replace(&mut self, next: Margins, now: f64) -> bool {
        if next == self.shown {
            return false;
        }
        self.shown = next;
        self.changed_at = now;
        true
    }

    /// The options changed from outside: show the margins now in use.
    pub fn follow(&mut self, applied: Margins) {
        self.shown = applied;
    }

    /// Whether the controls are ahead of the margins `applied` by the comparison.
    pub fn is_pending(&self, applied: Margins) -> bool {
        self.shown != applied
    }

    /// The margins to compare with now: `None` while they are the ones in use (the same values
    /// never compare again) or while a drag is going on and the value moved less than
    /// [`MARGIN_SETTLE`] ago.
    pub fn due(&self, applied: Margins, now: f64) -> Option<Margins> {
        if !self.is_pending(applied) {
            return None;
        }
        (!self.dragging || now - self.changed_at >= MARGIN_SETTLE).then_some(self.shown)
    }

    /// Seconds until a drag that stands still makes [`MarginEdit::due`] answer, when one is
    /// waiting for that.
    pub fn wait(&self, applied: Margins, now: f64) -> Option<f64> {
        (self.is_pending(applied) && self.dragging)
            .then(|| (self.changed_at + MARGIN_SETTLE - now).max(0.0))
    }
}

/// What is painted of the excluded margins on the pages.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarginPaint {
    /// The margins to show.
    pub margins: Margins,
    /// Paint the bands pale as well as their edges: the margins are being set.
    pub fill: bool,
}

/// What to paint for a tab: while the controls are ahead of the comparison or the options menu is
/// open, the bands and their edges; otherwise only a thin line at their edges when the guides
/// are on. Nothing when no margin is set.
pub fn margin_paint(edit: &MarginEdit, applied: Margins, guides: bool) -> Option<MarginPaint> {
    let margins = edit.shown();
    if margins.is_zero() {
        return None;
    }
    let fill = edit.is_pending(applied) || edit.open;
    (fill || guides).then_some(MarginPaint { margins, fill })
}

/// A band of an excluded margin placed on the screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenBand {
    /// Which edge of the page.
    pub side: MarginSide,
    /// Relative to the top-left corner of the view, in logical pixels.
    pub rect: Rect,
}

/// The bands of `margins` on the visible pages of a view, relative to its top-left corner. The
/// fractions apply to each page's own size, so pages of different sizes get proportional bands.
pub fn margin_bands(
    layout: &PageLayout,
    scroll: Point,
    viewport: Size,
    margins: &Margins,
) -> Vec<ScreenBand> {
    let mut out = Vec::new();
    for page in layout.visible_pages(scroll, viewport) {
        let page_rect = layout.page_rect(page);
        let (ox, oy) = (page_rect.x0 - scroll.x, page_rect.y0 - scroll.y);
        let size = Size::new(page_rect.width(), page_rect.height());
        for (side, r) in margins.bands(size) {
            out.push(ScreenBand {
                side,
                rect: Rect::new(ox + r.x0, oy + r.y0, ox + r.x1, oy + r.y1),
            });
        }
    }
    out
}

/// The edge of a band that faces the text: the line that is kept when the controls are closed.
pub fn band_edge(side: MarginSide, rect: Rect) -> (Point, Point) {
    match side {
        MarginSide::Top => (Point::new(rect.x0, rect.y1), Point::new(rect.x1, rect.y1)),
        MarginSide::Bottom => (Point::new(rect.x0, rect.y0), Point::new(rect.x1, rect.y0)),
        MarginSide::Left => (Point::new(rect.x1, rect.y0), Point::new(rect.x1, rect.y1)),
        MarginSide::Right => (Point::new(rect.x0, rect.y0), Point::new(rect.x0, rect.y1)),
    }
}

/// The translucent fill of an excluded band: a pale gray-blue over white pages.
pub const MARGIN_FILL: [u8; 4] = [90, 110, 150, 45];

/// The line along the inner edge of an excluded band.
pub const MARGIN_LINE: [u8; 4] = [70, 100, 190, 190];

/// Keeps `margins` as the ones new comparisons start with (`prefs.json`). A failed save is only
/// logged: the margins still apply until the app ends.
pub fn remember_margins(state: &mut AppState, margins: Margins) {
    if state.prefs.diff_margins == margins {
        return;
    }
    state.prefs.diff_margins = margins;
    if let Some(paths) = &state.paths
        && let Err(err) = state.prefs.save(&paths.prefs_file())
    {
        log::warn!("could not save {}: {err}", paths.prefs_file().display());
    }
}

// ------------------------------------------------------------------------------------------------
// Drawing
// ------------------------------------------------------------------------------------------------

fn color(rgba: [u8; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3])
}

fn to_screen(rect: Rect, origin: egui::Pos2) -> egui::Rect {
    egui::Rect::from_min_max(
        origin + vec2(rect.x0, rect.y0),
        origin + vec2(rect.x1, rect.y1),
    )
}

/// Draws a comparison tab: the band, then both views with their marks.
pub fn show(ui: &mut Ui, tab: &mut DiffTab, docs: &DocStore, cx: ViewContext<'_>) {
    let ViewContext {
        tab_id,
        focused,
        textures,
        queue,
    } = cx;
    tab.poll();
    band(ui, tab, docs, tab_id, queue);
    let result = tab.fresh_result(docs);
    prepare_changes(tab, docs, result.as_ref());
    show_changes_panel(ui, tab, docs, tab_id, queue);
    let overview = result.as_ref().and_then(|_| tab.changes.overview());

    let rest = ui.available_rect_before_wrap();
    if rest.width() < 2.0 * SPLIT_GAP || rest.height() < 1.0 {
        return;
    }
    ui.allocate_rect(rest, Sense::hover());
    let mid = rest.center().x;
    let rect_of = |side: Side| match side {
        Side::Left => egui::Rect::from_min_max(rest.min, pos2(mid - SPLIT_GAP / 2.0, rest.max.y)),
        Side::Right => egui::Rect::from_min_max(pos2(mid + SPLIT_GAP / 2.0, rest.min.y), rest.max),
    };
    if let Some(side) = [Side::Left, Side::Right]
        .into_iter()
        .find(|s| ui.rect_contains_pointer(rect_of(*s)))
    {
        tab.active = side;
    }
    let driver = tab.active;
    let follower = other(driver);
    let overlay = Overlay {
        marks: result.as_deref().map(|r| (r, tab.current)),
        overview: overview.as_deref(),
        margins: margin_paint(&tab.margins, tab.options.margins, tab.margin_guides),
    };

    // The driver takes its input first; the follower is then placed and drawn in the same frame.
    let doc = docs.get(tab.side(driver).doc).cloned();
    let side_cx = ViewContext {
        tab_id,
        focused,
        textures: &mut *textures,
        queue: &mut *queue,
    };
    draw_side(
        ui,
        rect_of(driver),
        driver,
        tab,
        doc.as_ref(),
        side_cx,
        overlay,
    );

    if tab.sync_scroll && result.is_some() {
        let info = docs.get(tab.side(follower).doc).and_then(|d| d.info());
        let zoomed = tab.sync_zoom(driver, info.as_deref());
        tab.follow(driver, result.as_deref(), zoomed);
    } else {
        tab.follow(driver, None, false);
    }

    let doc = docs.get(tab.side(follower).doc).cloned();
    let side_cx = ViewContext {
        tab_id,
        focused,
        textures,
        queue,
    };
    draw_side(
        ui,
        rect_of(follower),
        follower,
        tab,
        doc.as_ref(),
        side_cx,
        overlay,
    );

    ui.painter().vline(
        mid,
        rest.y_range(),
        Stroke::new(
            SPLIT_GAP,
            ui.visuals().widgets.noninteractive.bg_stroke.color,
        ),
    );
}

/// The outline of a document (empty while it is not open).
fn outline_of(info: &Option<Arc<DocumentInfo>>) -> &Outline {
    static EMPTY: std::sync::OnceLock<Outline> = std::sync::OnceLock::new();
    match info {
        Some(info) => &info.outline,
        None => EMPTY.get_or_init(Outline::new),
    }
}

/// Makes the change list and the strips ready for a result that is valid now. Nothing is
/// recomputed for a result that was prepared already.
fn prepare_changes(tab: &mut DiffTab, docs: &DocStore, result: Option<&Arc<DiffResult>>) {
    let Some(result) = result else {
        return;
    };
    let left = docs.get(tab.left.doc).and_then(|d| d.info());
    let right = docs.get(tab.right.doc).and_then(|d| d.info());
    tab.changes
        .prepare(result, outline_of(&left), outline_of(&right));
}

/// The change list, on the right of the tab under the band (DIFF-5). Its width is the user's to
/// drag; egui remembers it per tab.
fn show_changes_panel(
    ui: &mut Ui,
    tab: &mut DiffTab,
    docs: &DocStore,
    tab_id: TabId,
    queue: &mut Vec<Action>,
) {
    if !tab.changes.open {
        return;
    }
    let phase = tab.phase(docs);
    let left = docs.get(tab.left.doc).and_then(|d| d.info());
    let right = docs.get(tab.right.doc).and_then(|d| d.info());
    let stale = match &phase {
        Phase::Stale => tab.result().cloned(),
        _ => None,
    };
    let content = match &phase {
        Phase::Running(progress) => PanelContent::Running(*progress),
        Phase::Done(result) => PanelContent::Result {
            result,
            left: outline_of(&left),
            right: outline_of(&right),
            stale: false,
            margins: tab.options.margins,
        },
        Phase::Stale => match &stale {
            Some(result) => PanelContent::Result {
                result,
                left: outline_of(&left),
                right: outline_of(&right),
                stale: true,
                margins: tab.options.margins,
            },
            None => PanelContent::Unavailable,
        },
        Phase::Cancelled | Phase::Failed(_) => PanelContent::Unavailable,
    };
    let current = tab.current;
    egui::Panel::right(egui::Id::unique(("diff-changes", tab_id)))
        .resizable(true)
        .default_size(changes::DEFAULT_PANEL_WIDTH)
        .size_range(changes::PANEL_WIDTH_RANGE.0..=changes::PANEL_WIDTH_RANGE.1)
        .show(ui, |ui| {
            changes::show(ui, &mut tab.changes, content, current, tab_id, queue);
        });
}

/// What is drawn over the views of a valid result: the marks of the changes (with the current
/// one) and the position strips.
#[derive(Clone, Copy)]
struct Overlay<'a> {
    marks: Option<(&'a DiffResult, Option<usize>)>,
    overview: Option<&'a Overview>,
    margins: Option<MarginPaint>,
}

/// Draws one side: the PDF view, then the marks over it, then the position strip beside its
/// scroll bar. `rect` holds all of it. Only the active side of a focused tab takes the keys.
fn draw_side(
    ui: &mut Ui,
    rect: egui::Rect,
    side: Side,
    tab: &mut DiffTab,
    doc: Option<&DocumentHandle>,
    cx: ViewContext<'_>,
    overlay: Overlay<'_>,
) {
    let Overlay {
        marks,
        overview,
        margins,
    } = overlay;
    let salt = match side {
        Side::Left => "diff-left",
        Side::Right => "diff-right",
    };
    let tab_id = cx.tab_id;
    // The strip takes the right edge, next to the vertical scroll bar of the view.
    let strip_x = (rect.max.x - STRIP_WIDTH).max(rect.min.x);
    let view_area = egui::Rect::from_min_max(rect.min, pos2(strip_x, rect.max.y));
    let mut child = ui.new_child(UiBuilder::new().id_salt(salt).max_rect(view_area));
    let cx = ViewContext {
        focused: cx.focused && tab.active == side,
        ..cx
    };
    let pdf = tab.side_mut(side);
    let Some(doc) = doc else {
        child.label(tr("tab.unknown"));
        return;
    };
    pdf_view::show(&mut child, pdf, doc, cx);

    if !matches!(doc.status(), DocStatus::Ready) {
        return;
    }
    let Some(layout) = pdf.ui.layout() else {
        return;
    };
    let viewport = pdf.ui.viewport();
    let view_rect = egui::Rect::from_min_size(rect.min, vec2(viewport.w, viewport.h));
    let painter = child.painter_at(view_rect);
    if let Some(paint) = margins {
        paint_margins(
            &painter,
            view_rect.min,
            layout,
            pdf.ui.scroll(),
            viewport,
            paint,
        );
    }
    let Some((result, current)) = marks else {
        return;
    };
    let outline = Stroke::new(2.0, color(CURRENT_OUTLINE));
    for mark in mark_rects(layout, pdf.ui.scroll(), viewport, |p| result.marks(side, p)) {
        let r = to_screen(mark.rect, view_rect.min);
        painter.rect_filled(r, 0.0, color(mark_fill(mark.kind)));
        if let Some(bar) = change_bar(mark.kind, mark.rect) {
            painter.rect_filled(
                to_screen(bar, view_rect.min),
                0.0,
                color(bar_color(mark.kind)),
            );
        }
        if current == Some(mark.change as usize) {
            painter.rect_stroke(r.expand(1.0), 0.0, outline, StrokeKind::Outside);
        }
    }

    if let Some(overview) = overview {
        // As high as the vertical scroll bar of the view.
        let strip = egui::Rect::from_min_size(
            pos2(strip_x, rect.min.y),
            vec2(rect.max.x - strip_x, viewport.h),
        );
        let id = ui.scope_id().with(("diff-strip", tab_id, salt));
        draw_strip(ui, id, strip, side, overview, pdf);
    }
}

/// Paints the excluded margins over the visible pages of a view: the pale bands while they are
/// being set, and a thin line along the inner edge of each band.
fn paint_margins(
    painter: &egui::Painter,
    origin: egui::Pos2,
    layout: &PageLayout,
    scroll: Point,
    viewport: Size,
    paint: MarginPaint,
) {
    let line = Stroke::new(1.0, color(MARGIN_LINE));
    for band in margin_bands(layout, scroll, viewport, &paint.margins) {
        if paint.fill {
            painter.rect_filled(to_screen(band.rect, origin), 0.0, color(MARGIN_FILL));
        }
        let (a, b) = band_edge(band.side, band.rect);
        painter.line_segment([origin + vec2(a.x, a.y), origin + vec2(b.x, b.y)], line);
    }
}

/// The position strip of one side: the changes of its document as ticks, the part the view shows
/// as a pale band. A press or drag on it scrolls the view to that place.
fn draw_strip(
    ui: &mut Ui,
    id: egui::Id,
    strip: egui::Rect,
    side: Side,
    overview: &Overview,
    pdf: &mut PdfTab,
) {
    let extent = overview.extent(side);
    let (ticks, span) = {
        let Some(layout) = pdf.ui.layout() else {
            return;
        };
        let (scroll, viewport) = (pdf.ui.scroll(), pdf.ui.viewport());
        let top = ref_location(layout, scroll, viewport, 0.0);
        let bottom = ref_location(layout, scroll, viewport, 1.0);
        (
            changes::strip_ticks(
                overview.points(side),
                strip.height().floor().max(0.0) as u32,
            ),
            changes::visible_span(extent, top, bottom),
        )
    };
    if let Some(fraction) = changes::show_strip(ui, id, strip, &ticks, Some(span)) {
        center_on(pdf, extent.location_at(fraction));
    }
}

/// A button that runs a command of the table for this tab; its name and shortcut show on hover.
fn nav_button(ui: &mut Ui, enabled: bool, symbol: &str, command_id: &str) -> bool {
    let name = commands::command(command_id).map_or("", |c| tr(c.name_key));
    let hint = match commands::shortcut_text(ui.ctx(), command_id) {
        Some(keys) => format!("{name} ({keys})"),
        None => name.to_owned(),
    };
    ui.add_enabled(enabled, egui::Button::new(symbol))
        .on_hover_text(hint.clone())
        .on_disabled_hover_text(hint)
        .clicked()
}

/// The "excluded margins" section of the options menu: one control per page edge, in percent of
/// the page height (top, bottom) or width (left, right), and a button that clears them all.
fn margin_controls(ui: &mut Ui, edit: &mut MarginEdit, now: f64) {
    ui.separator();
    ui.strong(tr("diff.margins"));
    ui.weak(tr("diff.margins_hint"));
    for side in MarginSide::ALL {
        let mut percent = edit.shown().get(side) * 100.0;
        let response = ui.add(
            egui::DragValue::new(&mut percent)
                .range(0.0..=(MAX_MARGIN * 100.0))
                .speed(0.25)
                .max_decimals(1)
                .prefix(format!("{} ", tr(margin_side_key(side))))
                .suffix("%"),
        );
        // The accessible name is the edge alone; its value reads "Top 6.0%".
        ui.ctx().accesskit_node_builder(response.id, |node| {
            node.set_label(tr(margin_side_key(side)));
        });
        if response.dragged() {
            edit.dragging = true;
        }
        if response.changed() {
            edit.set(side, percent / 100.0, now);
        }
    }
    let any = !edit.shown().is_zero();
    if ui
        .add_enabled(any, egui::Button::new(tr("diff.margin_reset")))
        .clicked()
    {
        edit.reset(now);
    }
}

/// The i18n key of the name of a page edge.
pub fn margin_side_key(side: MarginSide) -> &'static str {
    match side {
        MarginSide::Top => "diff.margin_top",
        MarginSide::Bottom => "diff.margin_bottom",
        MarginSide::Left => "diff.margin_left",
        MarginSide::Right => "diff.margin_right",
    }
}

/// The band above the views: names, counts, navigation, switches, and a line about the state of
/// the comparison.
fn band(ui: &mut Ui, tab: &mut DiffTab, docs: &DocStore, tab_id: TabId, queue: &mut Vec<Action>) {
    let phase = tab.phase(docs);
    let path = |id: DocId| docs.get(id).map(|d| d.path().to_path_buf());
    let (left_path, right_path) = (path(tab.left.doc), path(tab.right.doc));
    let name = |p: &Option<PathBuf>| {
        p.as_deref()
            .map_or_else(|| tr("tab.unknown").to_owned(), file_name)
    };
    let full = |p: &Option<PathBuf>| {
        p.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    };

    ui.horizontal_wrapped(|ui| {
        ui.strong(name(&left_path)).on_hover_text(full(&left_path));
        ui.label("↔");
        ui.strong(name(&right_path))
            .on_hover_text(full(&right_path));
        ui.separator();
        let total = match &phase {
            Phase::Done(result) => {
                ui.label(counts_text(result));
                ui.separator();
                result.changes.len()
            }
            _ => 0,
        };
        let can_move = total > 0;
        if nav_button(ui, can_move, "◀", "diff.prev_change") {
            queue.push(Action::PrevChange(tab_id));
        }
        ui.label(position_text(tab.current, total));
        if nav_button(ui, can_move, "▶", "diff.next_change") {
            queue.push(Action::NextChange(tab_id));
        }
        ui.separator();
        let mut sync = tab.sync_scroll;
        if ui.checkbox(&mut sync, tr("diff.sync")).changed() {
            let result = match &phase {
                Phase::Done(result) => Some(Arc::clone(result)),
                _ => None,
            };
            tab.set_sync(sync, result.as_deref());
        }
        let name = commands::command("diff.toggle_changes").map_or("", |c| tr(c.name_key));
        let hint = match commands::shortcut_text(ui.ctx(), "diff.toggle_changes") {
            Some(keys) => format!("{name} ({keys})"),
            None => name.to_owned(),
        };
        if ui
            .selectable_label(tab.changes.open, tr("changes.title"))
            .on_hover_text(hint)
            .clicked()
        {
            queue.push(Action::ToggleChangesPanel(tab_id));
        }
        let now = ui.input(|i| i.time);
        tab.margins.dragging = false;
        let menu = ui.menu_button(tr("diff.options"), |ui| {
            let mut options = tab.options;
            let a = ui
                .checkbox(
                    &mut options.skip_repeated_lines,
                    tr("diff.opt_skip_repeated"),
                )
                .changed();
            let b = ui
                .checkbox(&mut options.ignore_case, tr("diff.opt_ignore_case"))
                .changed();
            if a || b {
                queue.push(Action::SetDiffOptions {
                    tab: tab_id,
                    options,
                });
            }
            margin_controls(ui, &mut tab.margins, now);
        });
        tab.margins.open = menu.inner.is_some();
        if let Some(margins) = tab.margins_due(now) {
            queue.push(Action::SetDiffOptions {
                tab: tab_id,
                options: DiffOptions {
                    margins,
                    ..tab.options
                },
            });
        } else if let Some(wait) = tab.margins.wait(tab.options.margins, now) {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64(wait));
        }
        if ui.button(tr("diff.swap")).clicked() {
            queue.push(Action::SwapDiffSides(tab_id));
        }
    });

    ui.horizontal_wrapped(|ui| {
        let rerun = |ui: &mut Ui, queue: &mut Vec<Action>| {
            if ui.button(tr("diff.rerun")).clicked() {
                queue.push(Action::RerunDiff(tab_id));
            }
        };
        match &phase {
            Phase::Running(progress) => {
                ui.add(
                    egui::ProgressBar::new(*progress)
                        .desired_width(200.0)
                        .show_percentage(),
                );
                ui.label(tr("diff.running"));
                if ui.button(tr("diff.cancel")).clicked() {
                    queue.push(Action::CancelDiff(tab_id));
                }
            }
            Phase::Cancelled => {
                ui.label(tr("diff.cancelled"));
                rerun(ui, queue);
            }
            Phase::Failed(message) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    tr("diff.failed").replace("{}", message),
                );
                rerun(ui, queue);
            }
            Phase::Stale => {
                ui.colored_label(ui.visuals().warn_fg_color, tr("diff.stale"));
                rerun(ui, queue);
            }
            Phase::Done(result) => {
                if result.changes.is_empty() {
                    ui.strong(tr("diff.no_changes"));
                }
                if result.approximate {
                    ui.colored_label(ui.visuals().warn_fg_color, tr("diff.approximate"));
                }
            }
        }
        if let Some(key) = tab.notice {
            ui.weak(tr(key));
        }
    });
    ui.add_space(2.0);
}

// ------------------------------------------------------------------------------------------------
// Choosing two documents
// ------------------------------------------------------------------------------------------------

/// The small window of the command "Compare Documents…": an older and a newer document, each
/// chosen from the open documents or from a file.
#[derive(Debug, Default)]
pub struct DiffDialog {
    /// The older document.
    pub left: Option<PathBuf>,
    /// The newer document.
    pub right: Option<PathBuf>,
    /// The file chooser that is open, for which side.
    picking: Option<(Side, Receiver<Option<PathBuf>>)>,
}

impl DiffDialog {
    /// A dialog with these documents chosen already.
    pub fn new(left: Option<PathBuf>, right: Option<PathBuf>) -> DiffDialog {
        DiffDialog {
            left,
            right,
            picking: None,
        }
    }

    /// The document chosen for a side.
    pub fn path(&self, side: Side) -> Option<&PathBuf> {
        match side {
            Side::Left => self.left.as_ref(),
            Side::Right => self.right.as_ref(),
        }
    }

    /// Chooses the document of a side.
    pub fn set(&mut self, side: Side, path: PathBuf) {
        match side {
            Side::Left => self.left = Some(path),
            Side::Right => self.right = Some(path),
        }
    }

    /// Both documents are chosen (they may be the same file).
    pub fn can_compare(&self) -> bool {
        self.left.is_some() && self.right.is_some()
    }

    /// The action that starts the comparison, once both are chosen.
    pub fn action(&self) -> Option<Action> {
        Some(Action::StartDiff {
            left: self.left.clone()?,
            right: self.right.clone()?,
        })
    }

    /// A file chooser is open.
    pub fn picking(&self) -> bool {
        self.picking.is_some()
    }

    /// Opens the file chooser for a side on its own thread (rule R-1).
    fn pick_file(&mut self, side: Side, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("diff-file-dialog".into())
            .spawn(move || {
                let picked = rfd::FileDialog::new()
                    .set_title(tr("diff.dialog_file_title"))
                    .add_filter(tr("dialog.filter"), &["pdf", "PDF"])
                    .pick_file();
                // The receiver is gone only when the dialog was closed meanwhile.
                let _ = tx.send(picked);
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => self.picking = Some((side, rx)),
            Err(err) => log::error!("could not start the file dialog thread: {err}"),
        }
    }

    /// Takes the file chosen in the file chooser, without waiting.
    pub fn poll(&mut self) {
        let Some((side, rx)) = &self.picking else {
            return;
        };
        let side = *side;
        match rx.try_recv() {
            Ok(picked) => {
                self.picking = None;
                if let Some(path) = picked {
                    self.set(side, path);
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.picking = None,
        }
    }
}

/// The documents the dialog offers: the open ones and the chosen ones, sorted, without repeats.
pub fn dialog_choices(dialog: &DiffDialog, open: &[PathBuf]) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = open
        .iter()
        .cloned()
        .chain(dialog.left.clone())
        .chain(dialog.right.clone())
        .collect();
    all.sort();
    all.dedup();
    all
}

/// Draws the dialog. `open` lists the paths of the open documents. Returns false when it is to
/// be closed (closed by the user, or the comparison was started).
pub fn show_dialog(
    ctx: &egui::Context,
    dialog: &mut DiffDialog,
    open: &[PathBuf],
    queue: &mut Vec<Action>,
) -> bool {
    dialog.poll();
    let choices = dialog_choices(dialog, open);
    let mut window_open = true;
    let mut done = false;
    egui::Window::new(tr("diff.dialog_title"))
        .open(&mut window_open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            egui::Grid::new("diff-dialog-grid")
                .num_columns(3)
                .show(ui, |ui| {
                    for (side, label) in [
                        (Side::Left, "diff.dialog_left"),
                        (Side::Right, "diff.dialog_right"),
                    ] {
                        ui.label(tr(label));
                        let chosen = dialog.path(side).cloned();
                        let shown = chosen
                            .as_deref()
                            .map_or_else(|| tr("diff.dialog_pick").to_owned(), file_name);
                        egui::ComboBox::from_id_salt(("diff-dialog", label))
                            .selected_text(shown)
                            .width(260.0)
                            .show_ui(ui, |ui| {
                                for path in &choices {
                                    let selected = chosen.as_ref() == Some(path);
                                    if ui
                                        .selectable_label(selected, file_name(path))
                                        .on_hover_text(path.display().to_string())
                                        .clicked()
                                    {
                                        dialog.set(side, path.clone());
                                    }
                                }
                            });
                        let button = egui::Button::new(tr("diff.dialog_from_file"));
                        if ui.add_enabled(!dialog.picking(), button).clicked() {
                            dialog.pick_file(side, ui.ctx());
                        }
                        ui.end_row();
                    }
                });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let compare = egui::Button::new(tr("diff.dialog_compare"));
                if ui.add_enabled(dialog.can_compare(), compare).clicked() {
                    queue.extend(dialog.action());
                    done = true;
                }
                if ui.button(tr("diff.dialog_close")).clicked() {
                    done = true;
                }
            });
        });
    window_open && !done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, dispatch};
    use crate::state::{AppState, Tab};
    use crate::views::test_support::{Repaints, watch_repaints};
    use refdesk_core::testutil::{FlowOptions, TempPdf, flow_document};
    use refdesk_core::{ChangeKind, LayoutParams, ZoomMode};

    // --------------------------------------------------------------------------------------------
    // Pure decisions
    // --------------------------------------------------------------------------------------------

    fn layout(sizes: &[Size], zoom: f32, viewport: Size) -> PageLayout {
        PageLayout::new(
            sizes,
            LayoutParams {
                viewport,
                zoom: ZoomMode::Custom(zoom),
                gap: pdf_view::PAGE_GAP,
                margin: pdf_view::MARGIN,
                fit_page: 0,
            },
        )
    }

    fn mark(page_rect: Rect, kind: MarkKind, change: u32) -> Mark {
        Mark {
            rect: page_rect,
            kind,
            change,
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn marks_land_on_the_page_whatever_the_zoom_and_scroll() {
        let sizes = vec![Size::new(600.0, 800.0); 3];
        let viewport = Size::new(900.0, 700.0);
        let marks = [
            vec![mark(
                Rect::new(72.0, 100.0, 300.0, 112.0),
                MarkKind::Removed,
                0,
            )],
            vec![mark(
                Rect::new(72.0, 400.0, 200.0, 412.0),
                MarkKind::Added,
                1,
            )],
            vec![],
        ];
        for (zoom, scroll_y) in [(1.0, 0.0), (2.0, 1900.0), (0.5, 120.0)] {
            let l = layout(&sizes, zoom, viewport);
            let scroll = l.clamp_scroll(Point::new(10.0, scroll_y), viewport);
            let placed = mark_rects(&l, scroll, viewport, |p| {
                marks.get(p as usize).map_or(&[][..], Vec::as_slice)
            });
            for sm in &placed {
                let page = if sm.change == 0 { 0 } else { 1 };
                let m = marks[page][0].rect;
                let pr = l.page_rect(page as u32);
                let s = l.scale();
                assert!(close(sm.rect.x0, pr.x0 + m.x0 * s - scroll.x), "{zoom}");
                assert!(close(sm.rect.y0, pr.y0 + m.y0 * s - scroll.y), "{zoom}");
                assert!(close(sm.rect.x1, pr.x0 + m.x1 * s - scroll.x), "{zoom}");
                assert!(close(sm.rect.y1, pr.y0 + m.y1 * s - scroll.y), "{zoom}");
            }
            // Exactly the marks whose rectangle is in the viewport are there.
            let expected: Vec<u32> = [(0usize, 0u32), (1, 1)]
                .iter()
                .filter(|(page, _)| {
                    let m = marks[*page][0].rect;
                    let pr = l.page_rect(*page as u32);
                    let y0 = pr.y0 + m.y0 * l.scale() - scroll.y;
                    let y1 = pr.y0 + m.y1 * l.scale() - scroll.y;
                    y1 >= 0.0 && y0 <= viewport.h
                })
                .map(|(_, c)| *c)
                .collect();
            let got: Vec<u32> = placed.iter().map(|m| m.change).collect();
            assert_eq!(got, expected, "zoom {zoom}, scroll {scroll_y}");
        }
    }

    #[test]
    fn pages_outside_the_viewport_give_no_marks() {
        let sizes = vec![Size::new(600.0, 800.0); 3];
        let viewport = Size::new(900.0, 500.0);
        let l = layout(&sizes, 1.0, viewport);
        let asked = std::cell::RefCell::new(Vec::new());
        let marks = vec![mark(Rect::new(10.0, 10.0, 20.0, 20.0), MarkKind::Added, 0)];
        let placed = mark_rects(&l, Point::new(0.0, 0.0), viewport, |p| {
            asked.borrow_mut().push(p);
            marks.as_slice()
        });
        assert_eq!(*asked.borrow(), vec![0]);
        assert_eq!(placed.len(), 1);
        // Scrolled to the third page: the first two are not asked for.
        asked.borrow_mut().clear();
        let scroll = Point::new(0.0, l.page_rect(2).y0);
        mark_rects(&l, scroll, viewport, |p| {
            asked.borrow_mut().push(p);
            marks.as_slice()
        });
        assert_eq!(*asked.borrow(), vec![2]);
    }

    #[test]
    fn removed_has_a_bar_on_the_left_and_added_on_the_right() {
        let r = Rect::new(100.0, 50.0, 200.0, 62.0);
        let left = change_bar(MarkKind::Removed, r).expect("bar");
        assert!(left.x1 < r.x0 && left.y0 == r.y0 && left.y1 == r.y1);
        let right = change_bar(MarkKind::Added, r).expect("bar");
        assert!(right.x0 > r.x1 && right.y0 == r.y0 && right.y1 == r.y1);
        assert_eq!(change_bar(MarkKind::ModifiedBlock, r), None);
        assert_eq!(change_bar(MarkKind::ModifiedWord, r), None);
    }

    /// Relative luminance of an sRGB color (WCAG).
    fn luminance(rgb: [f64; 3]) -> f64 {
        let lin = |c: f64| {
            let c = c / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(rgb[0]) + 0.7152 * lin(rgb[1]) + 0.0722 * lin(rgb[2])
    }

    /// `over` with alpha composed on `under`.
    fn blend(over: [u8; 4], under: [f64; 3]) -> [f64; 3] {
        let a = f64::from(over[3]) / 255.0;
        [0, 1, 2].map(|i| f64::from(over[i]) * a + under[i] * (1.0 - a))
    }

    #[test]
    fn black_text_under_every_mark_stays_readable_and_kinds_differ() {
        let kinds = [
            MarkKind::Removed,
            MarkKind::Added,
            MarkKind::ModifiedBlock,
            MarkKind::ModifiedWord,
        ];
        for kind in kinds {
            let fill = mark_fill(kind);
            let background = luminance(blend(fill, [255.0; 3]));
            let text = luminance(blend(fill, [0.0; 3]));
            let contrast = (background + 0.05) / (text + 0.05);
            assert!(contrast >= 4.5, "{kind:?}: contrast {contrast}");
            assert!(fill[3] > 0 && fill[3] < 128, "{kind:?} is translucent");
        }
        for (i, a) in kinds.iter().enumerate() {
            for b in &kinds[i + 1..] {
                assert_ne!(mark_fill(*a), mark_fill(*b));
            }
        }
        // The word marks are stronger than the paragraph marks they sit on.
        assert!(mark_fill(MarkKind::ModifiedWord)[3] > mark_fill(MarkKind::ModifiedBlock)[3]);
    }

    #[test]
    fn pick_change_from_a_position_and_from_the_current_change() {
        let anchors = [100.0, 500.0, 500.0, 900.0];
        // From the start, forward: the first.
        assert_eq!(
            pick_change(&anchors, f64::NEG_INFINITY, None, true),
            Some(0)
        );
        // From the end, backward: the last.
        assert_eq!(pick_change(&anchors, f64::INFINITY, None, false), Some(3));
        // From a position, the first after it / the last before it.
        assert_eq!(pick_change(&anchors, 100.0, None, true), Some(1));
        assert_eq!(pick_change(&anchors, 700.0, None, false), Some(2));
        assert_eq!(pick_change(&anchors, 950.0, None, true), None);
        assert_eq!(pick_change(&anchors, 50.0, None, false), None);
        // From the current change: neighbors, even at the same place.
        assert_eq!(pick_change(&anchors, 500.0, Some(1), true), Some(2));
        assert_eq!(pick_change(&anchors, 500.0, Some(2), false), Some(1));
        assert_eq!(pick_change(&anchors, 900.0, Some(3), true), None);
        assert_eq!(pick_change(&anchors, 100.0, Some(0), false), None);
        assert_eq!(pick_change(&[], 0.0, None, true), None);
    }

    #[test]
    fn reading_position_is_infinite_at_the_ends_of_the_scroll() {
        let sizes = vec![Size::new(600.0, 800.0); 4];
        let viewport = Size::new(700.0, 600.0);
        let l = layout(&sizes, 1.0, viewport);
        let pos = |y: f32| reading_pos(&l, Point::new(0.0, y), viewport, CHANGE_LINE, &sizes);
        assert_eq!(pos(0.0), f64::NEG_INFINITY);
        assert_eq!(pos(l.max_scroll(viewport).y), f64::INFINITY);
        let mid = l.page_rect(2).y0;
        let expected = stacked_y(
            &sizes,
            ref_location(&l, Point::new(0.0, mid), viewport, CHANGE_LINE),
        );
        assert_eq!(pos(mid), expected);
        assert!(expected > 1600.0 && expected < 2400.0, "{expected}");
    }

    #[test]
    fn place_scroll_puts_the_location_on_the_line() {
        let sizes = vec![Size::new(600.0, 800.0); 4];
        let viewport = Size::new(700.0, 600.0);
        let l = layout(&sizes, 1.5, viewport);
        let loc = Location::new(2, 72.0, 300.0);
        let scroll = place_scroll(&l, loc, viewport, CHANGE_LINE, 7.0);
        assert_eq!(scroll.x, l.clamp_scroll(Point::new(7.0, 0.0), viewport).x);
        let back = ref_location(&l, scroll, viewport, CHANGE_LINE);
        assert_eq!(back.page, 2);
        assert!((back.y - 300.0).abs() < 0.01, "{back:?}");
        // Near the start it is limited to the top.
        let top = place_scroll(&l, Location::new(0, 0.0, 10.0), viewport, CHANGE_LINE, 0.0);
        assert_eq!(top.y, 0.0);
    }

    #[test]
    fn texts_of_the_band() {
        assert_eq!(position_text(None, 4), "– / 4");
        assert_eq!(position_text(Some(1), 4), "2 / 4");
        assert_eq!(position_text(Some(9), 4), "– / 4");
        let result = DiffResult {
            added: 3,
            removed: 1,
            modified: 2,
            ..DiffResult::default()
        };
        let text = counts_text(&result);
        assert!(
            text.contains('3') && text.contains('1') && text.contains('2'),
            "{text}"
        );
    }

    #[test]
    fn the_dialog_compares_the_two_chosen_documents() {
        let mut dialog = DiffDialog::new(Some(PathBuf::from("/refdesk-test/a.pdf")), None);
        assert!(!dialog.can_compare());
        assert_eq!(dialog.action(), None);
        dialog.set(Side::Right, PathBuf::from("/refdesk-test/a.pdf"));
        assert!(
            dialog.can_compare(),
            "the same file on both sides is allowed"
        );
        dialog.set(Side::Right, PathBuf::from("/refdesk-test/x.pdf"));
        assert_eq!(
            dialog.action(),
            Some(Action::StartDiff {
                left: PathBuf::from("/refdesk-test/a.pdf"),
                right: PathBuf::from("/refdesk-test/x.pdf"),
            })
        );
        let choices = dialog_choices(
            &dialog,
            &[
                PathBuf::from("/refdesk-test/b.pdf"),
                PathBuf::from("/refdesk-test/a.pdf"),
            ],
        );
        assert_eq!(
            choices,
            vec![
                PathBuf::from("/refdesk-test/a.pdf"),
                PathBuf::from("/refdesk-test/b.pdf"),
                PathBuf::from("/refdesk-test/x.pdf"),
            ]
        );
    }

    #[test]
    fn the_dialog_draws_without_a_window() {
        let ctx = egui::Context::default();
        let mut dialog = DiffDialog::new(Some(PathBuf::from("/refdesk-test/a.pdf")), None);
        let mut queue = Vec::new();
        for _ in 0..2 {
            let mut keep = false;
            ctx.run_ui(egui::RawInput::default(), |ui| {
                keep = show_dialog(
                    ui.ctx(),
                    &mut dialog,
                    &[PathBuf::from("/refdesk-test/b.pdf")],
                    &mut queue,
                );
            })
            .drop_without_applying_deltas();
            assert!(keep);
        }
        assert!(queue.is_empty());
    }

    // --------------------------------------------------------------------------------------------
    // Comparison tabs with real documents
    // --------------------------------------------------------------------------------------------

    const WORDS: &[&str] = &[
        "the",
        "engine",
        "renders",
        "visible",
        "pages",
        "before",
        "background",
        "work",
        "and",
        "keeps",
        "extracted",
        "text",
        "while",
        "document",
        "stays",
        "open",
        "worker",
        "thread",
        "owns",
        "each",
        "request",
        "carries",
        "generation",
        "number",
        "results",
        "from",
        "older",
        "layout",
        "places",
        "vertically",
        "with",
        "small",
        "gap",
        "between",
    ];

    /// `n` distinct paragraphs of 30 to 60 lowercase words.
    fn paragraphs(n: usize, tag: &str) -> Vec<String> {
        (0..n)
            .map(|i| {
                let len = 30 + (i * 7) % 31;
                let words: Vec<&str> = (0..len)
                    .map(|k| WORDS[(i * 5 + k * 3) % WORDS.len()])
                    .collect();
                format!("Paragraph {tag}{i} {}.", words.join(" "))
            })
            .collect()
    }

    /// A paragraph of `words` words, long enough to fill most of a page when large.
    fn long_paragraph(words: usize) -> String {
        let body: Vec<&str> = (0..words).map(|k| WORDS[(k * 11) % WORDS.len()]).collect();
        format!("Inserted {}.", body.join(" "))
    }

    struct Fixture {
        ctx: egui::Context,
        repaints: Repaints,
        state: AppState,
        tab: TabId,
        files: Vec<TempPdf>,
    }

    fn write(paras: &[String], stem: &str) -> TempPdf {
        flow_document(paras, &FlowOptions::default()).write_temp(stem)
    }

    fn start(left: &Path, right: &Path, files: Vec<TempPdf>) -> Fixture {
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        dispatch(
            &mut state,
            Action::StartDiff {
                left: left.to_path_buf(),
                right: right.to_path_buf(),
            },
            &ctx,
        );
        let tab = state.focus.expect("the comparison tab is focused");
        Fixture {
            ctx,
            repaints,
            state,
            tab,
            files,
        }
    }

    fn compare(left: &[String], right: &[String]) -> Fixture {
        let lf = write(left, "wp62-left");
        let rf = write(right, "wp62-right");
        let (l, r) = (lf.path().to_path_buf(), rf.path().to_path_buf());
        start(&l, &r, vec![lf, rf])
    }

    fn diff(state: &AppState, tab: TabId) -> &DiffTab {
        match state.tabs.get(tab) {
            Some(Tab::Diff(d)) => d,
            other => panic!("not a comparison tab: {other:?}"),
        }
    }

    fn diff_mut(state: &mut AppState, tab: TabId) -> &mut DiffTab {
        match state.tabs.get_mut(tab) {
            Some(Tab::Diff(d)) => d,
            _ => panic!("not a comparison tab"),
        }
    }

    impl Fixture {
        fn diff(&self) -> &DiffTab {
            diff(&self.state, self.tab)
        }

        fn diff_mut(&mut self) -> &mut DiffTab {
            diff_mut(&mut self.state, self.tab)
        }

        /// Waits (without sleeping) until `check` holds. egui calls the repaint callback only
        /// once until the next frame, so an empty frame runs before every check.
        fn wait_until(&mut self, what: &str, mut check: impl FnMut(&mut AppState) -> bool) {
            let (ctx, state) = (&self.ctx, &mut self.state);
            self.repaints.wait_until(what, || {
                ctx.run_ui(egui::RawInput::default(), |_| {})
                    .drop_without_applying_deltas();
                check(state)
            });
        }

        /// Waits for the comparison to finish and returns its outcome.
        fn finish(&mut self) -> Outcome {
            let tab = self.tab;
            self.wait_until("the comparison", |state| {
                let d = diff_mut(state, tab);
                d.poll();
                d.job.is_none()
            });
            self.diff().outcome.clone().expect("finished")
        }

        fn result(&mut self) -> Arc<DiffResult> {
            self.finish().expect("the comparison succeeds")
        }

        fn dispatch(&mut self, action: Action) {
            dispatch(&mut self.state, action, &self.ctx);
        }

        fn doc(&self, side: Side) -> DocumentHandle {
            let id = self.diff().side(side).doc;
            self.state.docs.get(id).expect("open document").clone()
        }

        /// Lays out both views as the drawing would, for a viewport of this size.
        fn lay_out(&mut self, viewport: Size) {
            let AppState { tabs, docs, .. } = &mut self.state;
            let Some(Tab::Diff(d)) = tabs.get_mut(self.tab) else {
                panic!("not a comparison tab");
            };
            for side in [Side::Left, Side::Right] {
                let p = d.side_mut(side);
                let info = docs.get(p.doc).and_then(|doc| doc.info()).expect("info");
                p.ui.sync(&p.view, &info.page_sizes, info.generation, viewport);
                p.ui.commit(&mut p.view);
            }
        }

        /// The location on the line `line` of one view.
        fn at_line(&self, side: Side, line: f32) -> Location {
            let p = self.diff().side(side);
            ref_location(
                p.ui.layout().expect("laid out"),
                p.ui.scroll(),
                p.ui.viewport(),
                line,
            )
        }

        /// Scrolls one view by `dy` content pixels, as the user's wheel would.
        fn scroll(&mut self, side: Side, dy: f32) {
            let p = self.diff_mut().side_mut(side);
            p.ui.scroll_by(Point::new(0.0, dy));
            p.ui.commit(&mut p.view);
        }

        /// Draws the main window for one frame and returns the queued actions.
        fn frame(&mut self) -> Vec<Action> {
            let mut queue = Vec::new();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let state = &mut self.state;
            self.ctx
                .run_ui(input, |ui| {
                    crate::windows::show_window(ui, state, 0, &mut queue)
                })
                .drop_without_applying_deltas();
            queue
        }
    }

    const VIEWPORT: Size = Size { w: 560.0, h: 700.0 };

    /// 80 paragraphs, and the same with a long paragraph inserted after the 15th.
    fn inserted_pair() -> (Vec<String>, Vec<String>) {
        let left = paragraphs(80, "p");
        let mut right = left.clone();
        right.insert(15, long_paragraph(420));
        (left, right)
    }

    #[test]
    fn start_diff_opens_both_documents_in_a_focused_comparison_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(
            &mut state,
            Action::StartDiff {
                left: PathBuf::from("/refdesk-test/old.pdf"),
                right: PathBuf::from("/refdesk-test/new.pdf"),
            },
            &ctx,
        );
        assert_eq!(state.docs.len(), 2);
        let tabs = state.main_window().dock.tabs();
        assert_eq!(tabs.len(), 1, "the start page made room");
        assert_eq!(state.focus, Some(tabs[0]));
        let d = diff(&state, tabs[0]);
        let path = |id| state.docs.get(id).expect("doc").path().to_path_buf();
        assert_eq!(path(d.left.doc), PathBuf::from("/refdesk-test/old.pdf"));
        assert_eq!(path(d.right.doc), PathBuf::from("/refdesk-test/new.pdf"));
        assert!(d.job.is_some(), "the comparison runs");
        let title = crate::windows::tab_title(&state, tabs[0]);
        assert!(
            title.contains("old.pdf") && title.contains("new.pdf"),
            "{title}"
        );
        assert!(d.to_session(&state.docs).is_some_and(|s| s
            == SessionTab::Diff {
                left: PathBuf::from("/refdesk-test/old.pdf"),
                right: PathBuf::from("/refdesk-test/new.pdf"),
            }));
    }

    #[test]
    fn an_open_document_is_shared_and_closing_the_tab_releases_it() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(
            &mut state,
            Action::OpenFiles(vec![refdesk_core::OpenTarget::parse(
                "/refdesk-test/old.pdf",
            )]),
            &ctx,
        );
        let (pdf, p) = state.focused_pdf().expect("pdf tab");
        let shared = p.doc;
        dispatch(
            &mut state,
            Action::StartDiff {
                left: PathBuf::from("/refdesk-test/old.pdf"),
                right: PathBuf::from("/refdesk-test/new.pdf"),
            },
            &ctx,
        );
        let tab = state.focus.expect("comparison");
        assert_eq!(state.docs.len(), 2);
        assert_eq!(diff(&state, tab).left.doc, shared);
        let right = diff(&state, tab).right.doc;

        // The comparison still uses the shared document after its own tab is closed.
        dispatch(&mut state, Action::CloseTab(pdf), &ctx);
        assert!(state.docs.get(shared).is_some());
        // Opening it again adds a tab on the same document instead of opening it twice.
        dispatch(
            &mut state,
            Action::OpenFiles(vec![refdesk_core::OpenTarget::parse(
                "/refdesk-test/old.pdf",
            )]),
            &ctx,
        );
        let (again, p) = state.focused_pdf().expect("pdf tab");
        assert_eq!(p.doc, shared);
        assert_eq!(state.docs.len(), 2);

        dispatch(&mut state, Action::CloseTab(tab), &ctx);
        assert!(
            state.docs.get(right).is_none(),
            "only the comparison used it"
        );
        assert!(state.docs.get(shared).is_some(), "a pdf tab still shows it");
        dispatch(&mut state, Action::CloseTab(again), &ctx);
        assert_eq!(state.docs.len(), 0);
    }

    #[test]
    fn a_file_compared_with_itself_has_no_changes() {
        let f = write(&paragraphs(8, "s"), "wp62-same");
        let path = f.path().to_path_buf();
        let mut fx = start(&path, &path, vec![f]);
        assert_eq!(fx.state.docs.len(), 1);
        let result = fx.result();
        assert!(result.changes.is_empty());
        assert_eq!(fx.diff().phase(&fx.state.docs), Phase::Done(result));
        assert_eq!(
            fx.diff_mut().go_to_change(&DiffResult::default(), true),
            NavOutcome::NoChanges
        );
        fx.frame();
        let tab = fx.tab;
        fx.dispatch(Action::CloseTab(tab));
        assert_eq!(fx.state.docs.len(), 0);
        assert!(!fx.files.is_empty());
    }

    #[test]
    fn an_inserted_paragraph_is_one_addition_marked_on_the_right() {
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        let result = fx.result();
        assert_eq!((result.added, result.removed, result.modified), (1, 0, 0));
        let change = &result.changes[0];
        assert_eq!(change.kind, ChangeKind::Added);

        fx.lay_out(VIEWPORT);
        assert_eq!(
            fx.diff_mut().go_to_change(&result, true),
            NavOutcome::Moved(0)
        );
        let p = &fx.diff().right;
        let layout = p.ui.layout().expect("laid out");
        let placed = mark_rects(layout, p.ui.scroll(), p.ui.viewport(), |page| {
            result.marks(Side::Right, page)
        });
        assert!(
            placed
                .iter()
                .any(|m| m.kind == MarkKind::Added && m.change == 0),
            "{placed:?}"
        );
        let page = change.anchor.1.page;
        assert!(
            result
                .marks(Side::Right, page)
                .iter()
                .any(|m| m.kind == MarkKind::Added)
        );
        assert!(
            result
                .marks(Side::Left, change.anchor.0.page)
                .iter()
                .all(|m| m.kind != MarkKind::Added)
        );
    }

    #[test]
    fn next_and_previous_change_move_both_views_to_the_anchor() {
        let mut left = paragraphs(60, "p");
        let mut right = left.clone();
        right.insert(12, long_paragraph(60));
        // A changed word further on.
        left[30] = left[30].replacen("engine", "motor", 1);
        if left[30] == right[31] {
            left[30] = format!("{} extra", left[30]);
        }
        let mut fx = compare(&left, &right);
        let result = fx.result();
        assert_eq!(result.changes.len(), 2, "{:?}", result.changes);
        fx.lay_out(VIEWPORT);

        let check_at = |fx: &Fixture, i: usize| {
            for side in [Side::Left, Side::Right] {
                let sizes = page_sizes(&result, side);
                let at = stacked_y(sizes, fx.at_line(side, CHANGE_LINE));
                let want = stacked_y(sizes, pick(result.changes[i].anchor, side));
                assert!(
                    (at - want).abs() < 0.5,
                    "change {i} {side:?}: {at} vs {want}"
                );
            }
        };
        let d = fx.diff_mut();
        assert_eq!(d.go_to_change(&result, true), NavOutcome::Moved(0));
        check_at(&fx, 0);
        assert_eq!(
            fx.diff_mut().go_to_change(&result, true),
            NavOutcome::Moved(1)
        );
        check_at(&fx, 1);
        let anchors = (fx.diff().left.view.anchor, fx.diff().right.view.anchor);
        assert_eq!(fx.diff_mut().go_to_change(&result, true), NavOutcome::AtEnd);
        assert_eq!(fx.diff().notice, Some("diff.last_change"));
        assert_eq!(fx.diff().current, Some(1));
        assert_eq!(
            (fx.diff().left.view.anchor, fx.diff().right.view.anchor),
            anchors
        );
        assert_eq!(
            fx.diff_mut().go_to_change(&result, false),
            NavOutcome::Moved(0)
        );
        check_at(&fx, 0);
        assert_eq!(
            fx.diff_mut().go_to_change(&result, false),
            NavOutcome::AtEnd
        );
        assert_eq!(fx.diff().notice, Some("diff.first_change"));

        // After scrolling away by hand, "next" counts from where the view is.
        fx.scroll(Side::Left, 100_000.0);
        assert_eq!(
            fx.diff_mut().go_to_change(&result, false),
            NavOutcome::Moved(1)
        );
    }

    #[test]
    fn synchronized_scrolling_follows_the_moved_side() {
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        let result = fx.result();
        fx.lay_out(VIEWPORT);
        let d = fx.diff_mut();
        d.follow(Side::Left, Some(&result), false);

        // Far into the document, past the inserted paragraph.
        fx.scroll(Side::Left, 4000.0);
        assert!(fx.diff_mut().follow(Side::Left, Some(&result), false));
        let from = fx.at_line(Side::Left, SYNC_LINE);
        let want = result.map_location(Side::Left, from);
        let got = fx.at_line(Side::Right, SYNC_LINE);
        let rs = &result.right_page_sizes;
        assert!(
            (stacked_y(rs, got) - stacked_y(rs, want)).abs() < 0.5,
            "{got:?} vs {want:?}"
        );
        // The right document has the inserted paragraph (most of a page) before this place.
        let ls = &result.left_page_sizes;
        let ahead = stacked_y(rs, got) - stacked_y(ls, from);
        assert!(ahead > 400.0, "the right side is further on by {ahead} pt");

        // The follower does not push back: neither the same driver nor the other side moves
        // anything when nothing was scrolled.
        let before = (fx.diff().left.view.anchor, fx.diff().right.view.anchor);
        assert!(!fx.diff_mut().follow(Side::Left, Some(&result), false));
        assert!(!fx.diff_mut().follow(Side::Right, Some(&result), false));
        assert_eq!(
            (fx.diff().left.view.anchor, fx.diff().right.view.anchor),
            before
        );

        // Scrolling the right side moves the left one back the corresponding amount.
        fx.scroll(Side::Right, -3000.0);
        assert!(fx.diff_mut().follow(Side::Right, Some(&result), false));
        let from = fx.at_line(Side::Right, SYNC_LINE);
        let want = result.map_location(Side::Right, from);
        let got = fx.at_line(Side::Left, SYNC_LINE);
        assert!((stacked_y(ls, got) - stacked_y(ls, want)).abs() < 0.5);
    }

    #[test]
    fn without_synchronized_scrolling_the_other_side_stays() {
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        let result = fx.result();
        fx.lay_out(VIEWPORT);
        fx.diff_mut().set_sync(false, Some(&result));
        fx.diff_mut().follow(Side::Left, Some(&result), false);
        let right_before = fx.diff().right.view.anchor;
        fx.scroll(Side::Left, 5000.0);
        assert!(!fx.diff_mut().follow(Side::Left, Some(&result), false));
        assert_eq!(fx.diff().right.view.anchor, right_before);

        // Turning it on again aligns the other side at once.
        fx.diff_mut().set_sync(true, Some(&result));
        let want = result.map_location(Side::Left, fx.at_line(Side::Left, SYNC_LINE));
        let got = fx.at_line(Side::Right, SYNC_LINE);
        let rs = &result.right_page_sizes;
        assert!((stacked_y(rs, got) - stacked_y(rs, want)).abs() < 0.5);
    }

    #[test]
    fn the_zoom_follows_the_driver_while_synchronized() {
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        fx.result();
        fx.lay_out(VIEWPORT);
        let info = fx.doc(Side::Right).info();
        let d = fx.diff_mut();
        d.left.view.zoom = ZoomMode::Custom(2.0);
        assert!(d.sync_zoom(Side::Left, info.as_deref()));
        assert_eq!(d.right.view.zoom, ZoomMode::Custom(2.0));
        let scale = d.right.ui.layout().expect("laid out").scale();
        assert!((scale - 2.0 * refdesk_core::layout::PIXELS_PER_POINT_AT_100).abs() < 1e-4);
        assert!(!d.sync_zoom(Side::Left, info.as_deref()));
    }

    #[test]
    fn changing_the_options_compares_again_and_swapping_exchanges_the_kinds() {
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        let first = fx.result();
        assert_eq!((first.added, first.removed), (1, 0));
        let tab = fx.tab;

        let options = DiffOptions {
            ignore_case: true,
            ..DiffOptions::default()
        };
        fx.dispatch(Action::SetDiffOptions { tab, options });
        assert_eq!(fx.diff().options, options);
        assert!(fx.diff().job.is_some() && fx.diff().outcome.is_none());
        let again = fx.result();
        assert_eq!((again.added, again.removed), (1, 0));
        // The same options do not start another run.
        fx.dispatch(Action::SetDiffOptions { tab, options });
        assert!(fx.diff().job.is_none());

        let (l, r) = (fx.diff().left.doc, fx.diff().right.doc);
        fx.dispatch(Action::SwapDiffSides(tab));
        assert_eq!((fx.diff().left.doc, fx.diff().right.doc), (r, l));
        let swapped = fx.result();
        assert_eq!((swapped.added, swapped.removed), (0, 1));
        assert_eq!(swapped.changes[0].kind, ChangeKind::Removed);
    }

    #[test]
    fn cancelling_stops_the_comparison_and_it_can_run_again() {
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        let tab = fx.tab;
        fx.dispatch(Action::CancelDiff(tab));
        assert_eq!(fx.diff().phase(&fx.state.docs), Phase::Cancelled);
        fx.frame();
        fx.dispatch(Action::RerunDiff(tab));
        assert!(matches!(fx.diff().phase(&fx.state.docs), Phase::Running(_)));
        assert!(fx.finish().is_ok());
    }

    #[test]
    fn a_reloaded_document_makes_the_result_stale() {
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        fx.result();
        assert!(fx.diff().fresh_result(&fx.state.docs).is_some());
        let doc = fx.doc(Side::Left);
        let old = doc.info().expect("info").generation;
        doc.reload();
        fx.wait_until("the reload", |_| {
            doc.info().is_some_and(|i| i.generation != old)
        });
        assert_eq!(fx.diff().phase(&fx.state.docs), Phase::Stale);
        assert!(fx.diff().fresh_result(&fx.state.docs).is_none());
        fx.frame();
        // Comparing again gives a fresh result.
        let tab = fx.tab;
        fx.dispatch(Action::RerunDiff(tab));
        fx.result();
        assert!(fx.diff().fresh_result(&fx.state.docs).is_some());
    }

    #[test]
    fn a_missing_file_ends_in_an_error_message() {
        let f = write(&paragraphs(3, "m"), "wp62-missing");
        let path = f.path().to_path_buf();
        let mut fx = start(
            Path::new("/refdesk-test/does/not/exist.pdf"),
            &path,
            vec![f],
        );
        fx.frame();
        let outcome = fx.finish();
        let message = outcome.expect_err("an error");
        assert!(message.contains("exist.pdf"), "{message}");
        assert!(matches!(fx.diff().phase(&fx.state.docs), Phase::Failed(_)));
        for _ in 0..2 {
            assert!(fx.frame().is_empty());
        }
    }

    #[test]
    fn a_comparison_tab_draws_while_running_finished_and_failed() {
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        // Running (the first frames come before the documents are even open).
        for _ in 0..2 {
            assert!(fx.frame().is_empty(), "nothing was clicked");
        }
        fx.result();
        // Finished: drawing lays both views out and keeps them aligned.
        for _ in 0..3 {
            assert!(fx.frame().is_empty(), "nothing was clicked");
        }
        let d = fx.diff();
        assert!(d.left.ui.layout().is_some() && d.right.ui.layout().is_some());
        let result = d.result().expect("result").clone();
        let tab = fx.tab;
        fx.dispatch(Action::NextChange(tab));
        assert_eq!(fx.diff().current, Some(0));
        for _ in 0..2 {
            assert!(fx.frame().is_empty());
        }
        // The drawn frames did not move the views away from the change.
        let at = stacked_y(
            &result.right_page_sizes,
            fx.at_line(Side::Right, CHANGE_LINE),
        );
        let want = stacked_y(&result.right_page_sizes, result.changes[0].anchor.1);
        assert!((at - want).abs() < 0.5, "{at} vs {want}");
        fx.dispatch(Action::PrevChange(tab));
        assert_eq!(fx.diff().notice, Some("diff.first_change"));

        // A status bar line for the comparison.
        let text = status_text(fx.diff(), &fx.state.docs);
        assert_eq!(text, counts_text(&result));
    }

    #[test]
    fn change_navigation_commands_need_a_comparison_tab_with_changes() {
        let find = |id: &str| commands::command(id).expect("command");
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        for id in ["diff.next_change", "diff.prev_change"] {
            assert!(!(find(id).enabled)(&state), "{id} without a tab");
        }
        dispatch(
            &mut state,
            Action::OpenFiles(vec![refdesk_core::OpenTarget::parse("/refdesk-test/a.pdf")]),
            &ctx,
        );
        for id in ["diff.next_change", "diff.prev_change"] {
            assert!(!(find(id).enabled)(&state), "{id} on a pdf tab");
            assert_eq!(find(id).run(&state), None);
        }
        assert_eq!(find("diff.start").run(&state), Some(Action::ShowDiffDialog));

        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        assert!(
            !(find("diff.next_change").enabled)(&fx.state),
            "still running"
        );
        fx.result();
        let tab = fx.tab;
        assert_eq!(
            find("diff.next_change").run(&fx.state),
            Some(Action::NextChange(tab))
        );
        assert_eq!(
            find("diff.prev_change").run(&fx.state),
            Some(Action::PrevChange(tab))
        );
    }

    #[test]
    fn the_dialog_opens_with_the_focused_document_on_the_left() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(
            &mut state,
            Action::OpenFiles(vec![refdesk_core::OpenTarget::parse("/refdesk-test/a.pdf")]),
            &ctx,
        );
        dispatch(&mut state, Action::ShowDiffDialog, &ctx);
        let dialog = state.diff_dialog.as_ref().expect("dialog");
        assert_eq!(dialog.left, Some(PathBuf::from("/refdesk-test/a.pdf")));
        assert_eq!(dialog.right, None);
        // Drawing the window with the dialog does not panic.
        let mut queue = Vec::new();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            crate::windows::show_window(ui, &mut state, 0, &mut queue);
        })
        .drop_without_applying_deltas();
        assert!(state.diff_dialog.is_some());
    }

    // --------------------------------------------------------------------------------------------
    // Change list and position strips (WP-63)
    // --------------------------------------------------------------------------------------------

    /// Like [`write`], with a table of contents: "1 First" on the first page, "2 Second" on the
    /// third.
    fn write_with_contents(paras: &[String], stem: &str) -> TempPdf {
        flow_document(paras, &FlowOptions::default())
            .outline_leaf("1 First", 1, 0.0)
            .outline_leaf("2 Second", 3, 0.0)
            .write_temp(stem)
    }

    /// Two documents with contents and a few changes spread over them.
    fn changed_pair() -> (Vec<String>, Vec<String>) {
        let mut left = paragraphs(60, "p");
        let mut right = left.clone();
        right.insert(12, long_paragraph(60));
        left[30] = left[30].replacen("engine", "motor", 1);
        right.remove(50);
        (left, right)
    }

    fn compare_with_contents(left: &[String], right: &[String]) -> Fixture {
        let lf = write_with_contents(left, "wp63-left");
        let rf = write_with_contents(right, "wp63-right");
        let (l, r) = (lf.path().to_path_buf(), rf.path().to_path_buf());
        start(&l, &r, vec![lf, rf])
    }

    #[test]
    fn the_change_list_groups_by_the_contents_of_the_real_documents() {
        let (left, right) = changed_pair();
        let mut fx = compare_with_contents(&left, &right);
        let result = fx.result();
        assert!(result.changes.len() >= 3, "{:?}", result.changes);
        // Drawing prepares the list.
        for _ in 0..2 {
            fx.frame();
        }
        let d = fx.diff();
        let grouping = d.changes.grouping().expect("prepared");
        let outline = |side| {
            fx.doc(side)
                .info()
                .map(|i| i.outline.clone())
                .expect("outline")
        };
        assert_eq!(outline(Side::Left).items.len(), 2);
        let expected =
            changes::group_changes(&result.changes, &outline(Side::Left), &outline(Side::Right));
        assert_eq!(grouping.groups, expected.groups);
        assert!(
            grouping.groups.iter().all(|g| matches!(
                g.key,
                changes::GroupKey::Section(_) | changes::GroupKey::Start
            )),
            "{:?}",
            grouping.groups
        );
        // Every change is in a group and the rows hold each of them once.
        let rows = fx.diff_mut().changes.rows();
        for i in 0..result.changes.len() as u32 {
            assert!(changes::row_of_change(&rows, i).is_some(), "change {i}");
        }
    }

    #[test]
    fn clicking_a_row_moves_both_views_and_makes_it_the_current_change() {
        let (left, right) = changed_pair();
        let mut fx = compare(&left, &right);
        let result = fx.result();
        let last = result.changes.len() - 1;
        assert!(last >= 1);
        fx.lay_out(VIEWPORT);
        let tab = fx.tab;

        // The click of a row is this action; it goes where F7 goes.
        fx.dispatch(Action::GoToChange { tab, index: last });
        assert_eq!(fx.diff().current, Some(last));
        for side in [Side::Left, Side::Right] {
            let sizes = page_sizes(&result, side);
            let at = stacked_y(sizes, fx.at_line(side, CHANGE_LINE));
            let want = stacked_y(sizes, pick(result.changes[last].anchor, side));
            assert!((at - want).abs() < 0.5, "{side:?}: {at} vs {want}");
        }
        // Previous change continues from the clicked one.
        fx.dispatch(Action::PrevChange(tab));
        assert_eq!(fx.diff().current, Some(last - 1));
        // A change that does not exist is ignored.
        fx.dispatch(Action::GoToChange { tab, index: 9999 });
        assert_eq!(fx.diff().current, Some(last - 1));
    }

    #[test]
    fn a_row_cannot_be_used_once_the_result_is_stale() {
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        fx.result();
        let doc = fx.doc(Side::Right);
        let old = doc.info().expect("info").generation;
        doc.reload();
        fx.wait_until("the reload", |_| {
            doc.info().is_some_and(|i| i.generation != old)
        });
        let tab = fx.tab;
        fx.dispatch(Action::GoToChange { tab, index: 0 });
        assert_eq!(fx.diff().current, None);
    }

    #[test]
    fn moving_with_f7_unfolds_the_group_and_targets_the_row() {
        let (left, right) = changed_pair();
        let mut fx = compare(&left, &right);
        let result = fx.result();
        fx.lay_out(VIEWPORT);
        for _ in 0..2 {
            fx.frame();
        }
        let tab = fx.tab;
        // Fold the group the second change is in.
        let group = fx
            .diff()
            .changes
            .grouping()
            .and_then(|g| g.group_of(1))
            .expect("group");
        fx.diff_mut().changes.toggle_fold(group);
        assert!(changes::row_of_change(&fx.diff_mut().changes.rows(), 1).is_none());

        fx.dispatch(Action::NextChange(tab));
        fx.dispatch(Action::NextChange(tab));
        assert_eq!(fx.diff().current, Some(1));
        // The list notices on its next draw: the row comes back and is scrolled to.
        let current = fx.diff().current;
        fx.diff_mut().changes.note_current(current);
        assert_eq!(fx.diff().changes.pending_scroll(), Some(1));
        assert!(!fx.diff().changes.is_folded(group));
        assert!(changes::row_of_change(&fx.diff_mut().changes.rows(), 1).is_some());
        // Drawing uses the request up.
        fx.frame();
        assert_eq!(fx.diff().changes.pending_scroll(), None);
        assert!(!fx.diff().changes.is_folded(group));
        assert_eq!(
            result.changes.len(),
            fx.diff().changes.grouping().map_or(0, |g| g
                .groups
                .iter()
                .map(|g| g.changes.len())
                .sum())
        );
    }

    #[test]
    fn the_panel_toggles_and_the_kind_filter_only_touches_the_list() {
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        fx.result();
        for _ in 0..2 {
            assert!(fx.frame().is_empty());
        }
        let tab = fx.tab;
        assert!(fx.diff().changes.open, "open by default");
        fx.dispatch(Action::ToggleChangesPanel(tab));
        assert!(!fx.diff().changes.open);
        // The strips still have their data while the panel is closed.
        for _ in 0..2 {
            assert!(fx.frame().is_empty());
        }
        assert!(fx.diff().changes.overview().is_some());
        fx.dispatch(Action::ToggleChangesPanel(tab));
        assert!(fx.diff().changes.open);

        // Switching every kind off empties the list but not the marks or the counts.
        for kind in changes::KINDS {
            fx.diff_mut().changes.toggle_kind(kind);
        }
        assert!(fx.diff_mut().changes.rows().is_empty());
        for _ in 0..2 {
            assert!(fx.frame().is_empty());
        }
        let result = fx.result();
        assert_eq!((result.added, result.removed, result.modified), (1, 0, 0));
        assert!(!result.right_marks.iter().all(Vec::is_empty));
    }

    #[test]
    fn the_change_list_draws_while_running_finished_without_changes_and_stale() {
        // Running, then finished with changes.
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        for _ in 0..2 {
            assert!(fx.frame().is_empty());
        }
        fx.result();
        for _ in 0..3 {
            assert!(fx.frame().is_empty());
        }
        assert!(fx.diff().changes.grouping().is_some());
        // Stale: the list stays, dimmed.
        let doc = fx.doc(Side::Left);
        let old = doc.info().expect("info").generation;
        doc.reload();
        fx.wait_until("the reload", |_| {
            doc.info().is_some_and(|i| i.generation != old)
        });
        assert_eq!(fx.diff().phase(&fx.state.docs), Phase::Stale);
        for _ in 0..2 {
            assert!(fx.frame().is_empty(), "a stale list cannot be clicked");
        }
        // Cancelled: nothing to list.
        let tab = fx.tab;
        fx.dispatch(Action::CancelDiff(tab));
        for _ in 0..2 {
            assert!(fx.frame().is_empty());
        }

        // A file against itself: no differences.
        let f = write(&paragraphs(8, "s"), "wp63-same");
        let path = f.path().to_path_buf();
        let mut same = start(&path, &path, vec![f]);
        same.result();
        for _ in 0..3 {
            assert!(same.frame().is_empty());
        }
        assert_eq!(
            same.diff().changes.grouping().map(|g| g.groups.len()),
            Some(0)
        );
        assert!(same.diff_mut().changes.rows().is_empty());
    }

    #[test]
    fn a_click_on_a_strip_scrolls_that_view_to_the_place() {
        let (left, right) = inserted_pair();
        let mut fx = compare(&left, &right);
        let result = fx.result();
        for _ in 0..2 {
            fx.frame();
        }
        fx.lay_out(VIEWPORT);
        let overview = fx.diff().changes.overview().expect("overview");
        for side in [Side::Left, Side::Right] {
            let extent = overview.extent(side);
            for fraction in [0.1_f32, 0.5, 0.9] {
                let want = extent.location_at(fraction);
                let pdf = fx.diff_mut().side_mut(side);
                center_on(pdf, want);
                let sizes = page_sizes(&result, side);
                let at = fx.at_line(side, STRIP_LINE);
                let got = extent.fraction(at);
                assert!(
                    (got - fraction).abs() < 0.01,
                    "{side:?} {fraction}: scrolled to {got} ({at:?}, wanted {want:?}, {} pages)",
                    sizes.len()
                );
            }
        }
        // The right strip has the addition; the left one has nothing (it is only in the right).
        assert_eq!(overview.points(Side::Right).len(), 1);
        assert!(overview.points(Side::Left).is_empty());
        let right_extent = overview.extent(Side::Right);
        let point = overview.points(Side::Right)[0];
        let want = right_extent.fraction(result.changes[0].anchor.1);
        assert_eq!(point.fraction, want);
        // A view that is not laid out does not move.
        let mut fresh = DiffTab::new(
            side_tab(DocId::for_test(1), PathBuf::from("/refdesk-test/a.pdf")),
            side_tab(DocId::for_test(2), PathBuf::from("/refdesk-test/b.pdf")),
        );
        assert!(!center_on(&mut fresh.left, Location::new(0, 0.0, 100.0)));
    }

    #[test]
    fn copying_in_a_comparison_tab_takes_the_text_of_the_side_in_use() {
        use refdesk_core::{Selection, TextPos};
        let left = paragraphs(6, "p");
        let right = paragraphs(6, "q");
        let mut fx = compare(&left, &right);
        fx.result();
        let tab = fx.tab;
        let find = |id: &str| commands::command(id).expect("command");
        let select = |fx: &mut Fixture, side: Side| {
            fx.diff_mut()
                .side_mut(side)
                .ui
                .selection
                .set_for_test(Selection {
                    anchor: TextPos { page: 0, ch: 0 },
                    focus: TextPos { page: 0, ch: 14 },
                });
        };
        assert!(!(find("edit.copy").enabled)(&fx.state), "nothing selected");

        // Selected on the right while the left is in use: nothing to copy.
        select(&mut fx, Side::Right);
        fx.diff_mut().active = Side::Left;
        assert!(!(find("edit.copy").enabled)(&fx.state));
        fx.diff_mut().active = Side::Right;
        assert!((find("edit.copy").enabled)(&fx.state));
        assert_eq!(
            find("edit.copy").run(&fx.state),
            Some(Action::CopySelection(tab))
        );

        // The copy goes through a worker when the text is not extracted yet: wait for it.
        fx.dispatch(Action::CopySelection(tab));
        let copied = copied_text(&mut fx);
        assert!(copied.contains("q0"), "{copied:?}");
        assert!(!copied.contains("p0"), "{copied:?}");

        // The same selection on the left, in use now, copies the left text.
        select(&mut fx, Side::Left);
        fx.diff_mut().active = Side::Left;
        fx.dispatch(Action::CopySelection(tab));
        let copied = copied_text(&mut fx);
        assert!(copied.contains("p0"), "{copied:?}");
        assert!(!copied.contains("q0"), "{copied:?}");
    }

    /// Runs frames until the platform is asked to copy some text, and returns it.
    fn copied_text(fx: &mut Fixture) -> String {
        let mut text = None;
        let ctx = fx.ctx.clone();
        fx.repaints.wait_until("the copied text", || {
            let output = ctx.run_ui(egui::RawInput::default(), |_| {});
            for command in &output.platform_output.commands {
                if let egui::OutputCommand::CopyText(t) = command {
                    text = Some(t.clone());
                }
            }
            output.drop_without_applying_deltas();
            text.is_some()
        });
        text.unwrap_or_default()
    }

    #[test]
    fn the_changes_command_belongs_to_comparison_tabs() {
        let find = |id: &str| commands::command(id).expect("command");
        let toggle = find("diff.toggle_changes");
        assert_eq!(toggle.menu, commands::MenuGroup::View);
        assert_eq!(
            toggle.shortcut,
            Some(egui::KeyboardShortcut::new(
                egui::Modifiers::NONE,
                egui::Key::F8
            ))
        );
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        assert!(!(toggle.enabled)(&state), "no tab");
        assert_eq!(toggle.run(&state), None);
        dispatch(
            &mut state,
            Action::OpenFiles(vec![refdesk_core::OpenTarget::parse("/refdesk-test/a.pdf")]),
            &ctx,
        );
        assert!(!(toggle.enabled)(&state), "a pdf tab");
        assert_eq!(toggle.run(&state), None);

        let (left, right) = inserted_pair();
        let fx = compare(&left, &right);
        // Usable even while the comparison still runs.
        assert!((toggle.enabled)(&fx.state));
        assert_eq!(
            toggle.run(&fx.state),
            Some(Action::ToggleChangesPanel(fx.tab))
        );
    }

    // --------------------------------------------------------------------------------------------
    // Excluded margins (DIFF-11)
    // --------------------------------------------------------------------------------------------

    fn top(v: f32) -> Margins {
        Margins {
            top: v,
            ..Margins::default()
        }
    }

    #[test]
    fn dragging_a_margin_does_not_compare_until_it_is_let_go_or_stands_still() {
        let applied = Margins::default();
        let mut edit = MarginEdit::new(applied);
        assert_eq!(edit.due(applied, 1.0), None, "nothing changed");
        edit.dragging = true;
        assert!(edit.set(MarginSide::Top, 0.06, 1.0));
        assert!(edit.is_pending(applied));
        // Each change of the value starts the wait again.
        assert_eq!(edit.due(applied, 1.0), None);
        assert!(edit.set(MarginSide::Top, 0.065, 1.2));
        assert_eq!(edit.due(applied, 1.5), None, "0.3 s after the last change");
        assert!(
            edit.wait(applied, 1.5)
                .is_some_and(|w| (w - 0.1).abs() < 1e-9)
        );
        assert_eq!(edit.due(applied, 1.6), Some(top(0.065)));
        // Let go before that: at once.
        edit.set(MarginSide::Top, 0.07, 2.0);
        assert_eq!(edit.due(applied, 2.0), None);
        edit.dragging = false;
        assert_eq!(edit.due(applied, 2.0), Some(top(0.07)));
        assert_eq!(edit.wait(applied, 2.0), None, "nothing to wait for");
    }

    #[test]
    fn the_same_margins_never_compare_again() {
        let applied = top(0.06);
        let mut edit = MarginEdit::new(applied);
        edit.dragging = true;
        assert!(!edit.set(MarginSide::Top, 0.06, 1.0), "no change");
        assert!(!edit.set(MarginSide::Top, 0.0612, 1.0), "snaps to the same");
        assert_eq!(edit.due(applied, 9.0), None);
        // Away and back to the margins in use before the comparison started again.
        assert!(edit.set(MarginSide::Top, 0.1, 2.0));
        assert!(edit.set(MarginSide::Top, 0.06, 2.1));
        edit.dragging = false;
        assert_eq!(edit.due(applied, 3.0), None);
        // Following the options in use (changed from outside) drops what was pending.
        edit.set(MarginSide::Left, 0.2, 4.0);
        edit.follow(applied);
        assert!(!edit.is_pending(applied));
        // "Reset all" goes to zero in one step, once.
        assert!(edit.reset(5.0));
        assert!(!edit.reset(5.1));
        assert_eq!(edit.due(applied, 5.1), Some(Margins::default()));
    }

    #[test]
    fn values_of_the_controls_are_snapped_and_limited() {
        let mut edit = MarginEdit::new(Margins::default());
        edit.set(MarginSide::Bottom, 0.0537, 0.0);
        assert!((edit.shown().bottom - 0.055).abs() < 1e-6);
        edit.set(MarginSide::Left, 0.9, 0.0);
        assert_eq!(edit.shown().left, MAX_MARGIN);
        edit.set(MarginSide::Right, -0.2, 0.0);
        assert_eq!(edit.shown().right, 0.0);
        edit.set(MarginSide::Right, f32::NAN, 0.0);
        assert_eq!(edit.shown().right, 0.0);
    }

    #[test]
    fn the_pages_show_bands_while_setting_and_lines_afterwards() {
        let none = Margins::default();
        let m = top(0.1);
        let settled = MarginEdit::new(m);
        // In use, options closed: only lines, and only with the guides on.
        assert_eq!(
            margin_paint(&settled, m, true),
            Some(MarginPaint {
                margins: m,
                fill: false
            })
        );
        assert_eq!(margin_paint(&settled, m, false), None);
        // Nothing set: nothing to draw.
        assert_eq!(margin_paint(&MarginEdit::new(none), none, true), None);
        // The controls are ahead of the comparison: bands, guides or not.
        let mut dragged = MarginEdit::new(none);
        dragged.set(MarginSide::Top, 0.1, 0.0);
        let paint = margin_paint(&dragged, none, false).expect("painted");
        assert!(paint.fill && paint.margins == m);
        // The options are open: bands too.
        let mut open = MarginEdit::new(m);
        open.open = true;
        assert!(margin_paint(&open, m, false).is_some_and(|p| p.fill));
        // Dragged down to nothing: nothing.
        let mut cleared = MarginEdit::new(m);
        cleared.reset(0.0);
        assert_eq!(margin_paint(&cleared, m, true), None);
    }

    #[test]
    fn bands_follow_each_pages_size_zoom_and_scroll() {
        let sizes = vec![Size::new(600.0, 800.0), Size::new(300.0, 400.0)];
        let viewport = Size::new(700.0, 1500.0);
        let margins = Margins {
            top: 0.1,
            bottom: 0.05,
            left: 0.2,
            right: 0.0,
        };
        for zoom in [0.5, 1.0, 2.0] {
            let l = layout(&sizes, zoom, viewport);
            let scroll = Point::new(0.0, 0.0);
            let bands = margin_bands(&l, scroll, viewport, &margins);
            let visible = l.visible_pages(scroll, viewport);
            // Three bands per visible page; the right margin is 0.
            assert_eq!(bands.len(), 3 * visible.len() as usize);
            for (i, page) in visible.enumerate() {
                let pr = l.page_rect(page);
                let (w, h) = (pr.width(), pr.height());
                let of = |side| {
                    bands[3 * i..3 * i + 3]
                        .iter()
                        .find(|b| b.side == side)
                        .unwrap()
                        .rect
                };
                let top_band = of(MarginSide::Top);
                assert!(close(top_band.x0, pr.x0) && close(top_band.x1, pr.x1));
                assert!(close(top_band.y0, pr.y0) && close(top_band.y1, pr.y0 + 0.1 * h));
                let bottom_band = of(MarginSide::Bottom);
                assert!(close(bottom_band.y0, pr.y1 - 0.05 * h) && close(bottom_band.y1, pr.y1));
                let left_band = of(MarginSide::Left);
                assert!(close(left_band.x0, pr.x0) && close(left_band.x1, pr.x0 + 0.2 * w));
                assert!(close(left_band.y0, top_band.y1) && close(left_band.y1, bottom_band.y0));
            }
        }
        // Scrolled down: the same bands, shifted up by the scroll.
        let l = layout(&sizes, 1.0, viewport);
        let at_top = margin_bands(&l, Point::new(0.0, 0.0), viewport, &margins);
        let scrolled = margin_bands(&l, Point::new(0.0, 100.0), viewport, &margins);
        assert!(close(scrolled[0].rect.y0, at_top[0].rect.y0 - 100.0));
        // No margins: no bands. Pages outside the viewport: none either.
        assert!(margin_bands(&l, Point::new(0.0, 0.0), viewport, &Margins::default()).is_empty());
        let small = Size::new(700.0, 100.0);
        let first_only = margin_bands(&l, Point::new(0.0, 0.0), small, &margins);
        assert_eq!(first_only.len(), 3);
    }

    #[test]
    fn the_line_of_a_band_is_the_edge_facing_the_text() {
        let r = Rect::new(10.0, 20.0, 110.0, 70.0);
        let edge = |s| band_edge(s, r);
        assert_eq!(
            edge(MarginSide::Top),
            (Point::new(10.0, 70.0), Point::new(110.0, 70.0))
        );
        assert_eq!(
            edge(MarginSide::Bottom),
            (Point::new(10.0, 20.0), Point::new(110.0, 20.0))
        );
        assert_eq!(
            edge(MarginSide::Left),
            (Point::new(110.0, 20.0), Point::new(110.0, 70.0))
        );
        assert_eq!(
            edge(MarginSide::Right),
            (Point::new(10.0, 20.0), Point::new(10.0, 70.0))
        );
    }

    /// A document of `pages` pages, each with a header that differs from page to page (it starts
    /// with `header`) and the same body, with `body_word` in the body of the second page.
    fn margin_doc(header: &str, body_word: &str, stem: &str) -> TempPdf {
        let words = ["red", "green", "blue", "gold"];
        let mut pdf = refdesk_core::testutil::PdfBuilder::new();
        for (n, word) in words.iter().enumerate() {
            pdf = pdf.page(612.0, 792.0, |p| {
                p.text(72.0, 40.0, 10.0, &format!("{header} chapter {word} title"));
                let mid = if n == 1 { body_word } else { "steady" };
                p.text(
                    72.0,
                    300.0,
                    10.0,
                    &format!("The {mid} body text of page {n} stays here."),
                );
                p.text(
                    72.0,
                    400.0,
                    10.0,
                    &format!("A second paragraph of page {n} follows below."),
                );
            });
        }
        pdf.write_temp(stem)
    }

    fn compare_margin_docs() -> Fixture {
        let lf = margin_doc("Alpha", "quick", "wp64-left");
        let rf = margin_doc("Beta", "lazy", "wp64-right");
        let (l, r) = (lf.path().to_path_buf(), rf.path().to_path_buf());
        start(&l, &r, vec![lf, rf])
    }

    /// The text of every node of the accessibility tree of one frame.
    fn tree_texts(fx: &mut Fixture) -> Vec<String> {
        fx.ctx.enable_accesskit();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let state = &mut fx.state;
        let mut queue = Vec::new();
        let output = fx.ctx.run_ui(input, |ui| {
            crate::windows::show_window(ui, state, 0, &mut queue)
        });
        let update = output.platform_output.accesskit_update.clone();
        output.drop_without_applying_deltas();
        update
            .map(|u| u.nodes)
            .unwrap_or_default()
            .into_iter()
            .flat_map(|(_, n)| [n.label().map(str::to_owned), n.value().map(str::to_owned)])
            .flatten()
            .collect()
    }

    #[test]
    fn changing_the_margins_compares_again_with_fewer_changes_and_says_so() {
        let mut fx = compare_margin_docs();
        let before = fx.result().changes.len();
        assert!(before >= 5, "four headers and a body change: {before}");
        let tab = fx.tab;
        let options = DiffOptions {
            margins: top(0.1),
            ..fx.diff().options
        };
        fx.dispatch(Action::SetDiffOptions { tab, options });
        assert!(fx.diff().job.is_some(), "compares again");
        assert_eq!(fx.diff().margins.shown(), top(0.1), "the controls follow");
        let after = fx.result();
        assert_eq!(after.changes.len(), 1, "{:?}", after.changes);
        assert_eq!(after.changes[0].kind, ChangeKind::Modified);
        // The same margins again do not start another comparison.
        fx.dispatch(Action::SetDiffOptions { tab, options });
        assert!(fx.diff().job.is_none());

        // The summary of the change list names the margins; the panel is open.
        for _ in 0..2 {
            fx.frame();
        }
        let want = changes::margins_summary(&top(0.1)).expect("summary");
        assert!(want.contains("10%"), "{want}");
        let texts = tree_texts(&mut fx);
        assert!(texts.contains(&want), "{want} not in {texts:?}");

        // Navigation still works on the shorter list.
        fx.dispatch(Action::NextChange(tab));
        assert_eq!(fx.diff().current, Some(0));
    }

    #[test]
    fn a_frame_after_dragging_asks_to_compare_with_the_dragged_margins() {
        let mut fx = compare_margin_docs();
        fx.result();
        for _ in 0..2 {
            assert!(fx.frame().is_empty(), "nothing yet");
        }
        // While a drag is on (the control sets this each frame) nothing is asked for, but the
        // pages are drawn with the bands.
        fx.diff_mut().margins.set(MarginSide::Top, 0.1, 100.0);
        assert!(fx.diff().margins.is_pending(fx.diff().options.margins));
        let queue = fx.frame();
        // The drawing resets "dragging" because no control is being dragged: let go.
        let asked = queue.iter().find_map(|a| match a {
            Action::SetDiffOptions { options, .. } => Some(options.margins),
            _ => None,
        });
        assert_eq!(asked, Some(top(0.1)));
        // Applying it makes the frames quiet again.
        for a in queue {
            fx.dispatch(a);
        }
        fx.result();
        for _ in 0..3 {
            assert!(fx.frame().is_empty());
        }
    }

    #[test]
    fn the_guides_command_belongs_to_comparison_tabs_and_toggles_the_lines() {
        let command = commands::command("diff.toggle_margin_guides").expect("command");
        assert_eq!(command.menu, commands::MenuGroup::View);
        assert!(command.shortcut.is_none());
        assert!(commands::in_menu(commands::MenuGroup::View).any(|c| c.id == command.id));
        let ctx = egui::Context::default();
        let state = AppState::new();
        assert!(!(command.enabled)(&state));
        assert_eq!(command.run(&state), None);
        let mut fx = compare_margin_docs();
        assert!((command.enabled)(&fx.state));
        let tab = fx.tab;
        assert_eq!(
            command.run(&fx.state),
            Some(Action::ToggleDiffMarginGuides(tab))
        );
        assert!(fx.diff().margin_guides);
        fx.dispatch(Action::ToggleDiffMarginGuides(tab));
        assert!(!fx.diff().margin_guides);
        fx.dispatch(Action::ToggleDiffMarginGuides(tab));
        assert!(fx.diff().margin_guides);
        drop(ctx);
    }

    #[test]
    fn the_margins_are_remembered_for_new_comparisons_and_old_prefs_files_load() {
        use crate::action::scroll_tests::ScratchDir;
        let dir = ScratchDir::new("wp64-prefs");
        let lf = margin_doc("Alpha", "quick", "wp64-pl");
        let rf = margin_doc("Beta", "lazy", "wp64-pr");
        let (l, r) = (lf.path().to_path_buf(), rf.path().to_path_buf());
        let ctx = egui::Context::default();

        // A prefs file from before the item existed: a comparison starts without margins.
        let prefs_file = dir.paths().prefs_file();
        std::fs::create_dir_all(prefs_file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&prefs_file, br#"{"version": 1, "scroll_speed": 3.0}"#).expect("write");
        let mut state = AppState::new();
        state.paths = Some(dir.paths());
        state.prefs = refdesk_core::Prefs::load(&prefs_file);
        let open = |state: &mut AppState| {
            dispatch(
                state,
                Action::StartDiff {
                    left: l.clone(),
                    right: r.clone(),
                },
                &ctx,
            );
            state.focus.expect("tab")
        };
        let tab = open(&mut state);
        assert!(diff(&state, tab).options.margins.is_zero());

        // Change them: they are in `prefs.json` at once.
        let margins = Margins {
            top: 0.06,
            bottom: 0.05,
            ..Margins::default()
        };
        let options = DiffOptions {
            margins,
            ..diff(&state, tab).options
        };
        dispatch(&mut state, Action::SetDiffOptions { tab, options }, &ctx);
        let saved = refdesk_core::Prefs::load(&prefs_file);
        assert_eq!(saved.diff_margins, margins);
        assert_eq!(saved.scroll_speed, Some(3.0), "the rest is kept");
        drop(state);

        // A new process: the next comparison starts with them, controls included.
        let mut state = AppState::new();
        state.paths = Some(dir.paths());
        state.prefs = refdesk_core::Prefs::load(&prefs_file);
        let tab = open(&mut state);
        assert_eq!(diff(&state, tab).options.margins, margins);
        assert_eq!(diff(&state, tab).margins.shown(), margins);
        assert!(!diff(&state, tab).margins.is_pending(margins));
        assert!(diff(&state, tab).job.is_some(), "and it compares with them");

        // Margins that are not allowed are cut down before they are remembered.
        let wild = DiffOptions {
            margins: Margins {
                top: 0.9,
                ..Margins::default()
            },
            ..diff(&state, tab).options
        };
        dispatch(
            &mut state,
            Action::SetDiffOptions { tab, options: wild },
            &ctx,
        );
        assert_eq!(diff(&state, tab).options.margins, top(0.4));
        assert_eq!(state.prefs.diff_margins, top(0.4));
    }

    #[test]
    fn the_margin_controls_and_the_bands_draw_without_a_window() {
        let mut fx = compare_margin_docs();
        fx.result();
        let tab = fx.tab;
        // Set and open: bands and lines on both sides, then the lines alone.
        let options = DiffOptions {
            margins: Margins {
                top: 0.1,
                bottom: 0.1,
                left: 0.05,
                right: 0.05,
            },
            ..fx.diff().options
        };
        fx.dispatch(Action::SetDiffOptions { tab, options });
        fx.result();
        for open in [true, false] {
            fx.diff_mut().margins.open = open;
            for _ in 0..3 {
                fx.frame();
            }
        }
        fx.dispatch(Action::ToggleDiffMarginGuides(tab));
        for _ in 0..2 {
            fx.frame();
        }
    }

    #[test]
    fn the_margin_controls_have_names_for_assistive_technology() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut edit = MarginEdit::new(Margins {
            top: 0.06,
            bottom: 0.055,
            ..Margins::default()
        });
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    vec2(400.0, 400.0),
                )),
                ..Default::default()
            },
            |ui| margin_controls(ui, &mut edit, 0.0),
        );
        let update = output.platform_output.accesskit_update.clone();
        output.drop_without_applying_deltas();
        let nodes = update.expect("accessibility update").nodes;
        let named = |name: &str| {
            nodes
                .iter()
                .filter(|(_, n)| n.label() == Some(name) || n.value() == Some(name))
                .count()
        };
        // Four controls, each with the name of its edge, and the button.
        for key in [
            "diff.margin_top",
            "diff.margin_bottom",
            "diff.margin_left",
            "diff.margin_right",
        ] {
            let name = tr(key);
            let found = nodes.iter().any(|(_, n)| {
                n.role() == egui::accesskit::Role::SpinButton && n.label() == Some(name)
            });
            assert!(
                found,
                "no node named {name}: {:?}",
                nodes
                    .iter()
                    .map(|(_, n)| (
                        n.role(),
                        n.label().map(str::to_owned),
                        n.value().map(str::to_owned)
                    ))
                    .collect::<Vec<_>>()
            );
        }
        assert!(named(tr("diff.margin_reset")) >= 1);
        assert!(named(tr("diff.margins")) >= 1);
        assert!(!edit.dragging, "nothing was dragged");
    }
}
