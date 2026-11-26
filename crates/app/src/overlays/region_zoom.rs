//! Zooming and panning with the pointer in the PDF view: the region zoom (VIEW-14) and dragging
//! the view with the middle button (VIEW-6).
//!
//! Region zoom: the "Zoom to Area" command arms the focused view. A drag then draws a rectangle,
//! and on release the view zooms so that the rectangle fills it (the smaller of the two factors,
//! stopping at the largest zoom) with the rectangle in the middle. The zoom mode and the place
//! before the first region zoom are remembered; "Previous Zoom" goes back to them.
//!
//! Middle-button pan: pressing the wheel button on a page and moving more than [`PAN_SLOP`]
//! drags the view along. A press and release without that movement stays a middle click (a link
//! under it opens in a new tab, LINK-6).
//!
//! The decisions are plain functions on plain data, tested without egui (rule R-10): the new
//! zoom and the place kept in the middle ([`plan`]), the drag rectangle ([`drag_rect`]) and the
//! pan offset ([`Pan`]). [`input`] and [`paint`] are the egui part.

use egui::{Color32, CursorIcon, Key, Modifiers, PointerButton, Stroke, StrokeKind, Ui};
use refdesk_core::layout::{MAX_ZOOM, MIN_ZOOM, PIXELS_PER_POINT_AT_100};
use refdesk_core::{DocumentInfo, Location, PageLayout, Point, Rect, Size, ViewState, ZoomMode};

use crate::i18n::tr;
use crate::state::{AppState, StatusNotice, Tab, TabId};
use crate::views::pdf_view::{PdfViewUiState, anchor_at};

/// A rectangle with a side shorter than this (logical pixels) is ignored.
pub const MIN_SIDE: f32 = 8.0;
/// The middle button has to move this far (logical pixels) before a press becomes a pan.
pub const PAN_SLOP: f32 = 4.0;
/// How long the status bar keeps the hint of an armed view (seconds). The hint goes earlier
/// when the mode ends.
pub const HINT_SECS: f64 = 60.0;

// ------------------------------------------------------------------------------------------------
// Decisions
// ------------------------------------------------------------------------------------------------

/// The zoom factor (1.0 = 100%) that makes a rectangle of the viewport fill the viewport, from
/// the factor `current`: the smaller of the horizontal and vertical ratio, limited to the zoom
/// range. `None` for a rectangle with a side shorter than [`MIN_SIDE`].
pub fn fit_zoom(rect: Rect, viewport: Size, current: f32) -> Option<f32> {
    if !(rect.width() >= MIN_SIDE && rect.height() >= MIN_SIDE) {
        return None;
    }
    let factor = (viewport.w / rect.width()).min(viewport.h / rect.height());
    if !(factor.is_finite() && factor > 0.0 && current.is_finite() && current > 0.0) {
        return None;
    }
    Some((current * factor).clamp(MIN_ZOOM, MAX_ZOOM))
}

/// What a region zoom does: the new zoom factor and the document place that goes to the middle
/// of the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// The new zoom factor (1.0 = 100%).
    pub zoom: f32,
    /// The middle of the rectangle, in page space.
    pub keep: Location,
}

/// The region zoom for `rect` (relative to the top-left corner of the viewport) in a view laid
/// out as `layout`, scrolled to `scroll`, of size `viewport`.
pub fn plan(layout: &PageLayout, scroll: Point, rect: Rect, viewport: Size) -> Option<Plan> {
    let current = layout.scale() / PIXELS_PER_POINT_AT_100;
    let zoom = fit_zoom(rect, viewport, current)?;
    let middle = Point::new(
        scroll.x + (rect.x0 + rect.x1) / 2.0,
        scroll.y + (rect.y0 + rect.y1) / 2.0,
    );
    Some(Plan {
        zoom,
        keep: anchor_at(layout, middle),
    })
}

/// The scroll offset after a region zoom, in the new layout: the kept place in the middle of the
/// viewport, as far as the document allows.
pub fn scroll_after(layout: &PageLayout, plan: &Plan, viewport: Size) -> Point {
    layout.scroll_to_keep(
        plan.keep,
        Point::new(viewport.w / 2.0, viewport.h / 2.0),
        viewport,
    )
}

/// The rectangle of a drag that started at the document place `start` and is at `pointer`
/// (relative to the viewport), limited to the viewport.
pub fn drag_rect(
    layout: &PageLayout,
    scroll: Point,
    start: Location,
    pointer: Point,
    viewport: Size,
) -> Rect {
    let p = layout.content_pos(start);
    let a = Point::new(p.x - scroll.x, p.y - scroll.y);
    let r = Rect::from_points(a, pointer);
    Rect::new(
        r.x0.clamp(0.0, viewport.w),
        r.y0.clamp(0.0, viewport.h),
        r.x1.clamp(0.0, viewport.w),
        r.y1.clamp(0.0, viewport.h),
    )
}

/// A pan in progress or about to start: where the middle button went down (relative to the
/// viewport) and the scroll offset then. Not saved; only held while the button is down.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pan {
    press: Option<(Point, Point)>,
    active: bool,
}

impl Pan {
    /// The middle button went down at `at` with the view scrolled to `scroll`.
    pub fn press(&mut self, at: Point, scroll: Point) {
        self.press = Some((at, scroll));
        self.active = false;
    }

    /// The pointer is at `at`. Returns the scroll offset the view moves to once the pointer
    /// went [`PAN_SLOP`] away from the press (and from then on), else `None`.
    pub fn moved(&mut self, at: Point) -> Option<Point> {
        let (from, scroll) = self.press?;
        let (dx, dy) = (at.x - from.x, at.y - from.y);
        if !self.active && dx.hypot(dy) < PAN_SLOP {
            return None;
        }
        self.active = true;
        Some(Point::new(scroll.x - dx, scroll.y - dy))
    }

    /// The button went up. Returns whether it was a pan (else it is a click).
    pub fn release(&mut self) -> bool {
        let was = self.active;
        *self = Pan::default();
        was
    }

    /// Whether the view is being dragged.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Whether the middle button is down after a press in the view.
    pub fn is_pressed(&self) -> bool {
        self.press.is_some()
    }
}

// ------------------------------------------------------------------------------------------------
// The state of one view
// ------------------------------------------------------------------------------------------------

/// What the region zoom is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Mode {
    /// Not zooming.
    #[default]
    Off,
    /// Waiting for a drag.
    Armed,
    /// A drag is going on.
    Dragging {
        /// Where the button went down, in page space.
        start: Location,
        /// The pointer relative to the top-left corner of the view.
        pointer: Point,
    },
}

/// The zoom and place before the first region zoom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Remembered {
    /// The zoom mode (fit width, fit page or a factor).
    pub zoom: ZoomMode,
    /// The top-left corner of the view.
    pub anchor: Location,
}

/// The region zoom and the middle-button pan of one PDF view. Not saved; a copy of a view keeps
/// only the remembered zoom.
#[derive(Debug, Default)]
pub struct RegionZoom {
    mode: Mode,
    /// What "Previous Zoom" goes back to.
    pub previous: Option<Remembered>,
    pan: Pan,
}

impl Clone for RegionZoom {
    fn clone(&self) -> RegionZoom {
        RegionZoom {
            previous: self.previous,
            ..RegionZoom::default()
        }
    }
}

impl RegionZoom {
    /// The mode.
    #[cfg(test)]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether the view waits for or draws a rectangle.
    pub fn is_active(&self) -> bool {
        self.mode != Mode::Off
    }

    /// Whether the view is being dragged with the middle button.
    pub fn panning(&self) -> bool {
        self.pan.is_active()
    }

    /// Arms the view, or disarms it when it is armed.
    pub fn toggle(&mut self) {
        self.mode = if self.is_active() {
            Mode::Off
        } else {
            Mode::Armed
        };
    }

    /// Leaves the mode, abandoning a drag.
    pub fn cancel(&mut self) {
        self.mode = Mode::Off;
    }

    /// Records the zoom before a region zoom, unless one is remembered already (repeated region
    /// zooms go back to the first one's).
    pub fn remember(&mut self, view: &ViewState) {
        if self.previous.is_none() {
            self.previous = Some(Remembered {
                zoom: view.zoom,
                anchor: view.anchor,
            });
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Commands (called by `dispatch`)
// ------------------------------------------------------------------------------------------------

/// "Zoom to Area": arms the region zoom of a PDF view (or disarms it), and tells how in the
/// status bar. The capture mode and the selection are left, since the drag now draws the area.
pub fn start(state: &mut AppState, tab: TabId, ctx: &egui::Context) {
    let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) else {
        return;
    };
    pdf.ui.selection.clear();
    pdf.ui.capture.cancel();
    pdf.ui.region_zoom.toggle();
    if pdf.ui.region_zoom.is_active() {
        state.status_notice = Some(StatusNotice {
            text: tr("zoom_region.hint").to_owned(),
            until: ctx.input(|i| i.time) + HINT_SECS,
        });
    }
    ctx.request_repaint();
}

/// "Previous Zoom": the zoom mode and place before the first region zoom, which is forgotten.
pub fn back(state: &mut AppState, tab: TabId) {
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab)
        && let Some(prev) = pdf.ui.region_zoom.previous.take()
    {
        pdf.view.zoom = prev.zoom;
        pdf.view.anchor = prev.anchor;
    }
}

/// Whether the focused PDF view remembers a zoom to go back to.
pub fn can_go_back(state: &AppState) -> bool {
    state
        .focused_pdf()
        .is_some_and(|(_, pdf)| pdf.ui.region_zoom.previous.is_some())
}

/// Called after each frame's actions: the hint goes from the status bar once no view is armed.
pub fn settle(state: &mut AppState) {
    let hint = tr("zoom_region.hint");
    if state.status_notice.as_ref().is_none_or(|n| n.text != hint) {
        return;
    }
    let armed = state
        .tabs
        .iter()
        .any(|(_, tab)| matches!(tab, Tab::Pdf(p) if p.ui.region_zoom.is_active()));
    if !armed {
        state.status_notice = None;
    }
}

// ------------------------------------------------------------------------------------------------
// egui
// ------------------------------------------------------------------------------------------------

fn viewport_pos(view_rect: egui::Rect, pos: egui::Pos2) -> Point {
    let v = pos - view_rect.min;
    Point::new(v.x, v.y)
}

/// Reads this frame's input for the middle-button pan and the region zoom. Returns true while
/// either is going on: the pointer is then not for hovering links or words.
///
/// Called right after the capture mode's input. While the region zoom is armed the primary
/// button's events are taken away from the rest of the view (no selection, no link is followed);
/// a middle release that ends a pan is taken away too, so it does not count as a click.
pub fn input(
    ui: &Ui,
    response: &egui::Response,
    view_rect: egui::Rect,
    focused: bool,
    view: &mut ViewState,
    state: &mut PdfViewUiState,
    info: &DocumentInfo,
) -> bool {
    let panning = pan_input(ui, response, view_rect, state);
    if !state.region_zoom.is_active() {
        return panning;
    }
    if !focused || state.capture.is_active() {
        state.region_zoom.cancel();
        return panning;
    }
    if !ui.ctx().text_edit_focused()
        && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
    {
        state.region_zoom.cancel();
        return panning;
    }
    let Some(layout) = state.layout().cloned() else {
        return panning;
    };
    let viewport = state.viewport();
    let events = ui.input(|i| i.events.clone());
    for event in events {
        let scroll = state.scroll();
        match (event, state.region_zoom.mode) {
            (
                egui::Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    ..
                },
                Mode::Armed,
            ) if response.hovered() && view_rect.contains(pos) => {
                let vp = viewport_pos(view_rect, pos);
                state.region_zoom.mode = Mode::Dragging {
                    start: anchor_at(&layout, Point::new(scroll.x + vp.x, scroll.y + vp.y)),
                    pointer: vp,
                };
            }
            (egui::Event::PointerMoved(pos), Mode::Dragging { start, .. }) => {
                state.region_zoom.mode = Mode::Dragging {
                    start,
                    pointer: viewport_pos(view_rect, pos),
                };
            }
            (
                egui::Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    ..
                },
                Mode::Dragging { start, .. },
            ) => {
                state.region_zoom.mode = Mode::Off;
                let rect = drag_rect(
                    &layout,
                    scroll,
                    start,
                    viewport_pos(view_rect, pos),
                    viewport,
                );
                if let Some(plan) = plan(&layout, scroll, rect, viewport) {
                    apply(view, state, info, &plan);
                }
            }
            (
                egui::Event::PointerButton {
                    pos,
                    button: PointerButton::Secondary,
                    pressed: true,
                    ..
                },
                _,
            ) if view_rect.contains(pos) => state.region_zoom.cancel(),
            _ => {}
        }
        if !state.region_zoom.is_active() {
            break;
        }
    }
    if let Mode::Dragging { start, .. } = state.region_zoom.mode {
        // The pointer may be still while the view scrolls under it.
        if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
            state.region_zoom.mode = Mode::Dragging {
                start,
                pointer: viewport_pos(view_rect, pos),
            };
        }
        // The release happened where this view saw no event.
        if !ui.input(|i| i.pointer.primary_down()) {
            state.region_zoom.cancel();
        }
    }
    ui.input_mut(|i| {
        i.events.retain(|e| {
            !matches!(
                e,
                egui::Event::PointerButton {
                    button: PointerButton::Primary | PointerButton::Secondary,
                    ..
                } | egui::Event::PointerMoved(_)
            )
        });
    });
    true
}

/// Zooms the view as planned: remembers the zoom before, sets the new factor and scrolls the kept
/// place to the middle.
fn apply(view: &mut ViewState, state: &mut PdfViewUiState, info: &DocumentInfo, plan: &Plan) {
    let viewport = state.viewport();
    state.region_zoom.remember(view);
    view.zoom = ZoomMode::Custom(plan.zoom);
    state.sync(view, &info.page_sizes, info.generation, viewport);
    if let Some(layout) = state.layout() {
        let to = scroll_after(layout, plan, viewport);
        state.scroll_to(to);
    }
}

/// The middle-button pan: a press in the view starts it, moving past [`PAN_SLOP`] drags the
/// view, the release ends it. Returns whether the view is being dragged.
fn pan_input(
    ui: &Ui,
    response: &egui::Response,
    view_rect: egui::Rect,
    state: &mut PdfViewUiState,
) -> bool {
    let events = ui.input(|i| i.events.clone());
    let mut drop_release = false;
    for event in events {
        match event {
            egui::Event::PointerButton {
                pos,
                button: PointerButton::Middle,
                pressed: true,
                ..
            } if response.hovered() && view_rect.contains(pos) => {
                let scroll = state.scroll();
                state
                    .region_zoom
                    .pan
                    .press(viewport_pos(view_rect, pos), scroll);
            }
            egui::Event::PointerMoved(pos) => {
                if let Some(to) = state.region_zoom.pan.moved(viewport_pos(view_rect, pos)) {
                    state.scroll_to(to);
                    // The release that ends the drag is no click on a link.
                    state.links.middle_down = false;
                }
            }
            egui::Event::PointerButton {
                button: PointerButton::Middle,
                pressed: false,
                ..
            } => {
                drop_release |= state.region_zoom.pan.release();
            }
            _ => {}
        }
    }
    // The release happened where this view saw no event.
    if state.region_zoom.pan.is_pressed() && !ui.input(|i| i.pointer.middle_down()) {
        state.region_zoom.pan.release();
    }
    if drop_release {
        ui.input_mut(|i| {
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::PointerButton {
                        button: PointerButton::Middle,
                        pressed: false,
                        ..
                    }
                )
            });
        });
    }
    state.region_zoom.panning()
}

/// Draws the region zoom (the translucent rectangle being dragged) and sets the cursor: the
/// crosshair while armed, the grabbing hand while panning.
pub fn paint(ui: &Ui, view_rect: egui::Rect, state: &PdfViewUiState, hovered: bool) {
    if state.region_zoom.panning() {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        return;
    }
    if !state.region_zoom.is_active() {
        return;
    }
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
    }
    let (Mode::Dragging { start, pointer }, Some(layout)) =
        (state.region_zoom.mode, state.layout())
    else {
        return;
    };
    let r = drag_rect(layout, state.scroll(), start, pointer, state.viewport());
    let rect = egui::Rect::from_min_max(
        view_rect.min + egui::vec2(r.x0, r.y0),
        view_rect.min + egui::vec2(r.x1, r.y1),
    );
    let blue = Color32::from_rgb(90, 170, 255);
    let painter = ui.painter_at(view_rect);
    painter.rect_filled(rect, 0.0, blue.gamma_multiply(0.2));
    painter.rect_stroke(rect, 0.0, Stroke::new(1.5, blue), StrokeKind::Outside);
}

#[cfg(test)]
mod tests {
    use super::*;
    use refdesk_core::LayoutParams;

    use crate::views::pdf_view::{MARGIN, PAGE_GAP};

    const A4: Size = Size { w: 595.0, h: 842.0 };
    const VIEW: Size = Size { w: 800.0, h: 600.0 };

    fn layout(zoom: f32, pages: usize) -> PageLayout {
        PageLayout::new(
            &vec![A4; pages],
            LayoutParams {
                viewport: VIEW,
                zoom: ZoomMode::Custom(zoom),
                gap: PAGE_GAP,
                margin: MARGIN,
                fit_page: 0,
            },
        )
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn a_wide_rectangle_is_fitted_by_its_width() {
        // 400 x 100 in 800 x 600: twice as wide fits, six times as high does not matter.
        let z = fit_zoom(Rect::new(10.0, 10.0, 410.0, 110.0), VIEW, 1.0).expect("zoom");
        assert!(close(z, 2.0), "{z}");
    }

    #[test]
    fn a_tall_rectangle_is_fitted_by_its_height() {
        let z = fit_zoom(Rect::new(10.0, 10.0, 110.0, 310.0), VIEW, 1.5).expect("zoom");
        // 600 / 300 = 2 beats 800 / 100 = 8.
        assert!(close(z, 3.0), "{z}");
    }

    #[test]
    fn the_zoom_stops_at_the_largest_factor() {
        let z = fit_zoom(Rect::new(0.0, 0.0, 10.0, 10.0), VIEW, 4.0).expect("zoom");
        assert!(close(z, MAX_ZOOM));
    }

    #[test]
    fn rectangles_with_a_side_under_eight_pixels_are_ignored() {
        assert_eq!(fit_zoom(Rect::new(0.0, 0.0, 7.9, 300.0), VIEW, 1.0), None);
        assert_eq!(fit_zoom(Rect::new(0.0, 0.0, 300.0, 7.0), VIEW, 1.0), None);
        assert_eq!(fit_zoom(Rect::new(5.0, 5.0, 5.0, 5.0), VIEW, 1.0), None);
        assert!(fit_zoom(Rect::new(0.0, 0.0, 8.0, 8.0), VIEW, 1.0).is_some());
    }

    #[test]
    fn the_middle_of_the_rectangle_ends_in_the_middle_of_the_view() {
        let old = layout(1.0, 5);
        let scroll = Point::new(0.0, 900.0);
        let rect = Rect::new(200.0, 100.0, 400.0, 250.0);
        let p = plan(&old, scroll, rect, VIEW).expect("plan");
        // 800 / 200 = 4, 600 / 150 = 4.
        assert!(close(p.zoom, 4.0));
        let new = layout(p.zoom, 5);
        let s = scroll_after(&new, &p, VIEW);
        let mid = new.content_pos(p.keep);
        assert!(
            close(mid.x - s.x, 400.0) && close(mid.y - s.y, 300.0),
            "{s:?}"
        );
        // The same document point as the middle of the rectangle before.
        let before = old.content_pos(p.keep);
        assert!(close(before.x - scroll.x, 300.0) && close(before.y - scroll.y, 175.0));
    }

    #[test]
    fn a_rectangle_at_the_top_left_of_the_first_page_stays_in_view() {
        let old = layout(1.0, 5);
        let page = old.page_rect(0);
        // The page corner, in the view (not scrolled).
        let rect = Rect::new(page.x0, page.y0, page.x0 + 100.0, page.y0 + 80.0);
        let p = plan(&old, Point::new(0.0, 0.0), rect, VIEW).expect("plan");
        let new = layout(p.zoom, 5);
        let s = scroll_after(&new, &p, VIEW);
        // The view cannot scroll above the document: the corner is still on screen.
        assert!(s.y >= 0.0 && s.x >= 0.0);
        let corner = new.content_pos(Location::new(0, 0.0, 0.0));
        let at = Point::new(corner.x - s.x, corner.y - s.y);
        assert!(
            at.x >= 0.0 && at.x < VIEW.w && at.y >= 0.0 && at.y < VIEW.h,
            "{at:?}"
        );
    }

    #[test]
    fn the_drag_rectangle_follows_the_page_and_stays_in_the_view() {
        let l = layout(1.0, 3);
        let start = anchor_at(&l, Point::new(100.0, 150.0));
        let r = drag_rect(
            &l,
            Point::new(0.0, 0.0),
            start,
            Point::new(300.0, 50.0),
            VIEW,
        );
        assert!(
            close(r.x0, 100.0) && close(r.y0, 50.0) && close(r.x1, 300.0) && close(r.y1, 150.0)
        );
        // Scrolled by 40: the start moved up with the page.
        let r = drag_rect(
            &l,
            Point::new(0.0, 40.0),
            start,
            Point::new(300.0, 300.0),
            VIEW,
        );
        assert!(close(r.y0, 110.0) && close(r.y1, 300.0));
        // Past the view edge: cut there.
        let r = drag_rect(
            &l,
            Point::new(0.0, 0.0),
            start,
            Point::new(900.0, 700.0),
            VIEW,
        );
        assert!(close(r.x1, VIEW.w) && close(r.y1, VIEW.h));
    }

    #[test]
    fn a_middle_press_becomes_a_pan_after_four_pixels() {
        let mut pan = Pan::default();
        pan.press(Point::new(100.0, 100.0), Point::new(0.0, 500.0));
        assert_eq!(pan.moved(Point::new(102.0, 102.0)), None);
        assert!(!pan.is_active());
        assert!(!pan.release(), "a click");

        pan.press(Point::new(100.0, 100.0), Point::new(10.0, 500.0));
        assert_eq!(
            pan.moved(Point::new(100.0, 96.0)),
            Some(Point::new(10.0, 504.0))
        );
        assert!(pan.is_active());
        // Once panning, small moves count too, always from the press.
        assert_eq!(
            pan.moved(Point::new(99.0, 99.0)),
            Some(Point::new(11.0, 501.0))
        );
        assert!(pan.release(), "a pan");
        assert!(!pan.is_active() && !pan.is_pressed());
        // No press, no pan.
        assert_eq!(pan.moved(Point::new(0.0, 0.0)), None);
    }

    #[test]
    fn the_first_region_zoom_is_remembered_and_a_copy_keeps_only_that() {
        let mut v = ViewState::new("/refdesk-test/a.pdf".into());
        v.zoom = ZoomMode::FitPage;
        v.anchor = Location::new(3, 0.0, 40.0);
        let mut rz = RegionZoom::default();
        rz.remember(&v);
        v.zoom = ZoomMode::Custom(3.0);
        v.anchor = Location::new(3, 100.0, 200.0);
        rz.remember(&v);
        assert_eq!(
            rz.previous,
            Some(Remembered {
                zoom: ZoomMode::FitPage,
                anchor: Location::new(3, 0.0, 40.0)
            })
        );
        rz.toggle();
        assert!(rz.is_active());
        let copy = rz.clone();
        assert!(!copy.is_active());
        assert_eq!(copy.previous, rz.previous);
        rz.toggle();
        assert!(!rz.is_active());
    }
}

/// The region zoom and the pan in the drawn view, frame by frame, with the shortcut table running.
#[cfg(test)]
mod widget_tests {
    use super::*;
    use egui::{Event, Modifiers, Pos2, pos2};
    use refdesk_core::testutil::TempPdf;

    use crate::action::{Action, dispatch};
    use crate::overlays::link_preview::tests::{Harness, LINK, opened_document};
    use crate::state::KeyGrab;

    fn rig(stem: &str) -> (Harness, TabId, TempPdf) {
        let (mut h, tab, pdf) = opened_document(stem);
        h.shortcuts = true;
        // The text of the first page, so that a primary drag would select something.
        let doc = h.state.docs.get(h.tab(tab).doc).cloned().expect("doc");
        h.frames_until("the text", |_| doc.page_text(0).is_some());
        h.frame_at(10.0, Vec::new());
        (h, tab, pdf)
    }

    fn button(pos: Pos2, button: PointerButton, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    /// A primary drag from `a` to `b` in three frames from the time `at`.
    fn drag(h: &mut Harness, at: f64, a: Pos2, b: Pos2) {
        h.frame_at(
            at,
            vec![
                Event::PointerMoved(a),
                button(a, PointerButton::Primary, true),
            ],
        );
        h.frame_at(at + 0.02, vec![Event::PointerMoved(b)]);
        h.frame_at(at + 0.04, vec![button(b, PointerButton::Primary, false)]);
        h.frame_at(at + 0.06, Vec::new());
    }

    fn zoom_of(h: &Harness, tab: TabId) -> ZoomMode {
        h.tab(tab).view.zoom
    }

    fn effective(h: &Harness, tab: TabId) -> f32 {
        h.tab(tab).ui.effective_zoom.expect("laid out")
    }

    #[test]
    fn z_arms_the_view_and_a_drag_zooms_so_the_area_fills_it() {
        let (mut h, tab, _pdf) = rig("rzoom-drag");
        assert_eq!(zoom_of(&h, tab), ZoomMode::FitWidth);
        let before = effective(&h, tab);
        let start_anchor = h.tab(tab).view.anchor;
        h.key_at(10.1, Key::Z, Modifiers::NONE);
        assert_eq!(h.tab(tab).ui.region_zoom.mode(), Mode::Armed);
        assert_eq!(
            h.state.status_notice.as_ref().map(|n| n.text.as_str()),
            Some(tr("zoom_region.hint"))
        );
        // The crosshair over the view.
        h.frame_at(10.15, vec![Event::PointerMoved(pos2(300.0, 300.0))]);
        assert_eq!(h.cursor, CursorIcon::Crosshair);

        // 200 x 150 in a 788 x 588 view: 3.92 times.
        drag(&mut h, 10.2, pos2(100.0, 100.0), pos2(300.0, 250.0));
        let after = effective(&h, tab);
        let factor = (788.0_f32 / 200.0).min(588.0 / 150.0);
        assert!(
            (after / before - factor).abs() < 0.01,
            "{before} -> {after}"
        );
        assert!(matches!(zoom_of(&h, tab), ZoomMode::Custom(_)));
        assert!(!h.tab(tab).ui.region_zoom.is_active());
        assert!(!h.tab(tab).ui.has_selection(), "the drag selected nothing");
        assert!(
            h.state.status_notice.is_none(),
            "the hint went with the mode"
        );
        assert_eq!(
            h.tab(tab).ui.region_zoom.previous,
            Some(Remembered {
                zoom: ZoomMode::FitWidth,
                anchor: start_anchor
            })
        );
    }

    #[test]
    fn shift_z_goes_back_to_the_zoom_and_place_before_the_first_region_zoom() {
        let (mut h, tab, _pdf) = rig("rzoom-back");
        let back = crate::commands::command("view.zoom_back").expect("command");
        assert!(back.run(&h.state).is_none(), "nothing remembered yet");
        let start = h.tab(tab).view.anchor;
        h.key_at(10.1, Key::Z, Modifiers::NONE);
        drag(&mut h, 10.2, pos2(100.0, 100.0), pos2(400.0, 300.0));
        let first = effective(&h, tab);
        // A second region zoom, and another zoom command in between: still the first one's.
        h.key_at(10.5, Key::Equals, Modifiers::CTRL);
        h.key_at(10.6, Key::Z, Modifiers::NONE);
        drag(&mut h, 10.7, pos2(200.0, 200.0), pos2(400.0, 400.0));
        assert!(effective(&h, tab) > first);
        assert_eq!(back.run(&h.state), Some(Action::ZoomBack(tab)));

        h.key_at(11.0, Key::Z, Modifiers::SHIFT);
        h.frame_at(11.1, Vec::new());
        assert_eq!(zoom_of(&h, tab), ZoomMode::FitWidth);
        let at = h.tab(tab).view.anchor;
        assert_eq!(at.page, start.page);
        assert!(
            (at.y - start.y).abs() < 0.05 && (at.x - start.x).abs() < 0.05,
            "{at:?} vs {start:?}"
        );
        assert!(h.tab(tab).ui.region_zoom.previous.is_none());
        assert!(back.run(&h.state).is_none(), "forgotten");
    }

    #[test]
    fn esc_a_tiny_rectangle_and_another_tab_end_the_mode_without_zooming() {
        let (mut h, tab, _pdf) = rig("rzoom-cancel");
        let before = zoom_of(&h, tab);
        h.key_at(10.1, Key::Z, Modifiers::NONE);
        h.key_at(10.2, Key::Escape, Modifiers::NONE);
        assert!(!h.tab(tab).ui.region_zoom.is_active());
        assert!(h.state.status_notice.is_none());

        h.key_at(10.3, Key::Z, Modifiers::NONE);
        drag(&mut h, 10.4, pos2(100.0, 100.0), pos2(105.0, 300.0));
        assert!(!h.tab(tab).ui.region_zoom.is_active());
        assert_eq!(zoom_of(&h, tab), before);
        assert!(!h.tab(tab).ui.has_selection());

        // Z twice: off again.
        h.key_at(10.6, Key::Z, Modifiers::NONE);
        h.key_at(10.7, Key::Z, Modifiers::NONE);
        assert!(!h.tab(tab).ui.region_zoom.is_active());
        // The capture mode takes over from it.
        h.key_at(10.8, Key::Z, Modifiers::NONE);
        dispatch(&mut h.state, Action::StartCapture(tab), &h.ctx);
        h.frame_at(10.9, Vec::new());
        assert!(!h.tab(tab).ui.region_zoom.is_active());
        assert_eq!(h.state.key_grab, None::<KeyGrab>);
    }

    #[test]
    fn dragging_with_the_middle_button_moves_the_view_without_a_click() {
        let (mut h, tab, _pdf) = rig("pan-drag");
        let at = pos2(400.0, 300.0);
        h.frame_at(
            10.1,
            vec![
                Event::PointerMoved(at),
                button(at, PointerButton::Middle, true),
            ],
        );
        // The view is scrolled a little first so it can move both ways.
        let s0 = h.tab(tab).ui.scroll();
        h.frame_at(10.12, vec![Event::PointerMoved(pos2(400.0, 250.0))]);
        assert!(h.tab(tab).ui.region_zoom.panning());
        assert_eq!(h.cursor, CursorIcon::Grabbing);
        let s1 = h.tab(tab).ui.scroll();
        assert!((s1.y - (s0.y + 50.0)).abs() < 0.01, "{s0:?} -> {s1:?}");
        h.frame_at(10.14, vec![Event::PointerMoved(pos2(400.0, 200.0))]);
        assert!((h.tab(tab).ui.scroll().y - (s0.y + 100.0)).abs() < 0.01);
        h.frame_at(
            10.16,
            vec![button(pos2(400.0, 200.0), PointerButton::Middle, false)],
        );
        h.frame_at(10.2, Vec::new());
        assert!(!h.tab(tab).ui.region_zoom.panning());
        assert_eq!(h.state.tabs.len(), 1, "no click");
        assert!(!h.tab(tab).ui.has_selection());
    }

    #[test]
    fn panning_over_a_link_neither_hovers_it_nor_opens_it() {
        let (mut h, tab, _pdf) = rig("pan-link");
        let link = h.link_pos(tab, 0, LINK);
        // Press below the link, drag up over it and release there.
        let from = link + egui::vec2(0.0, 60.0);
        h.frame_at(
            10.1,
            vec![
                Event::PointerMoved(from),
                button(from, PointerButton::Middle, true),
            ],
        );
        h.frame_at(10.12, vec![Event::PointerMoved(link)]);
        for t in [10.2, 10.5, 11.0] {
            h.frame_at(t, Vec::new());
            assert_eq!(h.tab(tab).ui.links.hover_text, None, "at {t}");
            assert!(!h.is_open(tab), "at {t}");
        }
        h.frame_at(11.1, vec![button(link, PointerButton::Middle, false)]);
        h.frame_at(11.2, Vec::new());
        assert_eq!(
            h.state.tabs.len(),
            1,
            "the release over the link is no click"
        );
    }

    #[test]
    fn a_middle_click_that_wobbles_less_than_four_pixels_opens_the_link_in_a_new_tab() {
        let (mut h, tab, _pdf) = rig("pan-click");
        let link = h.link_pos(tab, 0, LINK);
        h.frame_at(
            10.1,
            vec![
                Event::PointerMoved(link),
                button(link, PointerButton::Middle, true),
            ],
        );
        let wobble = link + egui::vec2(2.0, 1.0);
        h.frame_at(10.12, vec![Event::PointerMoved(wobble)]);
        assert!(!h.tab(tab).ui.region_zoom.panning());
        h.frame_at(10.14, vec![button(wobble, PointerButton::Middle, false)]);
        h.frame_at(10.2, Vec::new());
        assert_eq!(h.state.tabs.len(), 2);
        let new = h.state.focus.expect("focus");
        assert_ne!(new, tab);
        assert_eq!(h.tab(new).view.anchor.page, 2);
    }
}
