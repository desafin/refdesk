//! Pinned link previews (LINK-14): the pin button of a link preview popup turns it into a small
//! window over the view that stays until it is closed.
//!
//! A pinned preview has a title line (the text of the link), a "Go there" button, a close button,
//! and the destination pages below. Dragging the title line moves it, dragging the bottom-right
//! corner resizes it (never smaller than [`MIN_W`] × [`MIN_H`], never out of the view). The wheel
//! over it scrolls its own content through the pages of the document; Ctrl+wheel zooms it. A tab
//! keeps up to [`MAX_PINNED`] of them (the oldest goes) as long as it lives; they are not saved.
//! Place and size are kept as fractions of the view, so they follow when the window is resized.
//!
//! The content is drawn like the link preview: parts of the textures in the cache, with a sharp
//! render asked for when there is none at the scale shown (R-1: nothing blocks). Positions are in
//! page space (R-5).
//!
//! The decisions are plain functions, tested without egui (rule R-10): moving and resizing
//! ([`moved`], [`resized`], [`fit`]), fractions and view rectangles ([`to_frac`], [`from_frac`]),
//! the content's layout and scrolling ([`content_layout`], [`scrolled`], [`zoomed`]) and the
//! limit on their number ([`PinnedPreviews::pin`]).

use egui::{Color32, CursorIcon, FontId, Id, Order, Sense, Stroke, StrokeKind, Ui, WidgetInfo};
use refdesk_core::layout::PIXELS_PER_POINT_AT_100;
use refdesk_core::{
    DocumentHandle, DocumentInfo, LayoutParams, Location, PageLayout, Point, Rect, RenderKey, Size,
    ZoomMode,
};

use crate::action::{Action, OpenHow};
use crate::i18n::tr;
use crate::overlays::link_preview::{self, HEADER_HEIGHT};
use crate::state::{AppState, DocId, Tab, TabId};
use crate::views::pdf_view::{PdfViewUiState, PixelInfo, anchor_at, render_scale_milli};
use crate::views::textures::TextureCache;

/// A tab keeps at most this many pinned previews.
pub const MAX_PINNED: usize = 4;
/// The smallest width of a pinned preview (logical pixels).
pub const MIN_W: f32 = 240.0;
/// The smallest height of a pinned preview (logical pixels).
pub const MIN_H: f32 = 160.0;
/// The side of the resize grip at the bottom-right corner.
pub const GRIP: f32 = 14.0;
/// The zoom of the content, relative to "the widest page fills the width", stays in this range.
pub const MIN_ZOOM: f32 = 0.5;
/// See [`MIN_ZOOM`].
pub const MAX_ZOOM: f32 = 4.0;
/// Space between pages in the content.
pub const PAGE_GAP: f32 = 6.0;

// ------------------------------------------------------------------------------------------------
// Place and size
// ------------------------------------------------------------------------------------------------

/// A rectangle of a view of size `view` as fractions of it.
pub fn to_frac(rect: Rect, view: Size) -> Rect {
    let (w, h) = (view.w.max(1.0), view.h.max(1.0));
    Rect::new(rect.x0 / w, rect.y0 / h, rect.x1 / w, rect.y1 / h)
}

/// The rectangle that fractions stand for in a view of size `view`, made to fit it ([`fit`]).
pub fn from_frac(frac: Rect, view: Size) -> Rect {
    fit(
        Rect::new(
            frac.x0 * view.w,
            frac.y0 * view.h,
            frac.x1 * view.w,
            frac.y1 * view.h,
        ),
        view,
    )
}

/// A rectangle made to fit a view of size `view`: at least [`MIN_W`] × [`MIN_H`] (unless the view
/// is smaller), at most the view, and moved inside it.
pub fn fit(rect: Rect, view: Size) -> Rect {
    let w = rect.width().max(MIN_W).min(view.w.max(0.0));
    let h = rect.height().max(MIN_H).min(view.h.max(0.0));
    let x0 = rect.x0.min(view.w - w).max(0.0);
    let y0 = rect.y0.min(view.h - h).max(0.0);
    Rect::new(x0, y0, x0 + w, y0 + h)
}

/// The rectangle moved by `(dx, dy)`, kept inside the view.
pub fn moved(rect: Rect, dx: f32, dy: f32, view: Size) -> Rect {
    fit(
        Rect::new(rect.x0 + dx, rect.y0 + dy, rect.x1 + dx, rect.y1 + dy),
        view,
    )
}

/// The rectangle with its bottom-right corner moved by `(dx, dy)`: the top-left corner stays,
/// the size stays within [`MIN_W`] × [`MIN_H`] and the view's edge.
pub fn resized(rect: Rect, dx: f32, dy: f32, view: Size) -> Rect {
    let w = (rect.width() + dx).min(view.w - rect.x0).max(MIN_W);
    let h = (rect.height() + dy).min(view.h - rect.y0).max(MIN_H);
    fit(Rect::new(rect.x0, rect.y0, rect.x0 + w, rect.y0 + h), view)
}

// ------------------------------------------------------------------------------------------------
// The content
// ------------------------------------------------------------------------------------------------

/// The layout of the content of a pinned preview whose page area is `area`: the pages one under
/// the other, the widest as wide as the area times `zoom`.
pub fn content_layout(page_sizes: &[Size], area: Size, zoom: f32) -> PageLayout {
    let widest = page_sizes.iter().map(|s| s.w).fold(0.0_f32, f32::max);
    let scale = if widest > 0.0 {
        area.w / widest * zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        PIXELS_PER_POINT_AT_100
    };
    PageLayout::new(
        page_sizes,
        LayoutParams {
            viewport: area,
            zoom: ZoomMode::Custom(scale / PIXELS_PER_POINT_AT_100),
            gap: PAGE_GAP,
            margin: 0.0,
            fit_page: 0,
        },
    )
}

/// The scroll offset of the content for the place `anchor` at its top-left corner, within the
/// document (no further up than its start, no further down than its end).
pub fn content_scroll(layout: &PageLayout, anchor: Location, area: Size) -> Point {
    layout.clamp_scroll(layout.content_pos(anchor), area)
}

/// The place at the top-left corner after scrolling the content by `delta` content pixels
/// (positive y moves towards the end of the document), stopping at the start and the end.
pub fn scrolled(layout: &PageLayout, anchor: Location, delta: Point, area: Size) -> Location {
    let s = content_scroll(layout, anchor, area);
    let to = layout.clamp_scroll(Point::new(s.x + delta.x, s.y + delta.y), area);
    anchor_at(layout, to)
}

/// The zoom and the place at the top-left corner after zooming the content by `factor` around
/// `pointer` (relative to the page area): the document point under the pointer stays there.
pub fn zoomed(
    page_sizes: &[Size],
    area: Size,
    anchor: Location,
    zoom: f32,
    factor: f32,
    pointer: Point,
) -> (Location, f32) {
    let new_zoom = (zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
    let old = content_layout(page_sizes, area, zoom);
    let s = content_scroll(&old, anchor, area);
    let under = anchor_at(&old, Point::new(s.x + pointer.x, s.y + pointer.y));
    let new = content_layout(page_sizes, area, new_zoom);
    let to = new.scroll_to_keep(under, pointer, area);
    (anchor_at(&new, to), new_zoom)
}

// ------------------------------------------------------------------------------------------------
// The state of one view
// ------------------------------------------------------------------------------------------------

/// One pinned preview. Page space and fractions only (R-5).
#[derive(Clone, Debug, PartialEq)]
pub struct Pinned {
    id: u64,
    /// The text of the link: "Page 12 — 7.3 State management".
    pub title: String,
    /// Where the link leads.
    pub dest: Location,
    /// The place at the top-left corner of the content when it was pinned.
    start: Location,
    /// The place at the top-left corner of the content.
    pub anchor: Location,
    /// The zoom of the content (1 = the widest page fills the width).
    pub zoom: f32,
    /// Place and size as fractions of the view.
    pub frac: Rect,
}

impl Pinned {
    /// Where "Go there" goes: the destination, or the place the content was scrolled to.
    pub fn go_target(&self) -> Location {
        if self.anchor == self.start {
            self.dest
        } else {
            Location::new(self.anchor.page, 0.0, self.anchor.y.max(0.0))
        }
    }
}

/// What the link preview hands over when its pin is pressed.
#[derive(Clone, Debug, PartialEq)]
pub struct PinRequest {
    /// The title line.
    pub title: String,
    /// Where the link leads.
    pub dest: Location,
    /// The page space y at the top of the picture.
    pub top: f32,
    /// The popup, relative to the view.
    pub rect: Rect,
    /// The size of the view.
    pub view: Size,
}

/// The pinned previews of one PDF view, oldest first. Not saved; a copy of a view starts without
/// them.
#[derive(Debug, Default)]
pub struct PinnedPreviews {
    items: Vec<Pinned>,
    next_id: u64,
}

impl Clone for PinnedPreviews {
    fn clone(&self) -> PinnedPreviews {
        PinnedPreviews::default()
    }
}

impl PinnedPreviews {
    /// Pins a preview; beyond [`MAX_PINNED`] the oldest is closed.
    pub fn pin(&mut self, req: PinRequest) {
        let id = self.next_id;
        self.next_id += 1;
        let start = Location::new(req.dest.page, 0.0, req.top);
        self.items.push(Pinned {
            id,
            title: req.title,
            dest: req.dest,
            start,
            anchor: start,
            zoom: 1.0,
            frac: to_frac(req.rect, req.view),
        });
        if self.items.len() > MAX_PINNED {
            let excess = self.items.len() - MAX_PINNED;
            self.items.drain(..excess);
        }
    }

    /// The pinned previews, oldest first.
    #[cfg(test)]
    pub fn items(&self) -> &[Pinned] {
        &self.items
    }

    /// How many there are.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Closes all of them.
    pub fn close_all(&mut self) {
        self.items.clear();
    }

    fn close(&mut self, id: u64) {
        self.items.retain(|p| p.id != id);
    }
}

// ------------------------------------------------------------------------------------------------
// Commands (called by `dispatch`)
// ------------------------------------------------------------------------------------------------

/// "Close All Pinned Previews" of a PDF tab.
pub fn close_all(state: &mut AppState, tab: TabId) {
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        pdf.ui.links.pinned.close_all();
    }
}

/// Whether the focused PDF view has pinned previews.
pub fn has_pinned(state: &AppState) -> bool {
    state
        .focused_pdf()
        .is_some_and(|(_, pdf)| !pdf.ui.links.pinned.is_empty())
}

// ------------------------------------------------------------------------------------------------
// egui
// ------------------------------------------------------------------------------------------------

/// What [`show`] needs besides the state.
pub struct Env<'a> {
    /// The tab, for ids and for the jump of "Go there".
    pub tab: TabId,
    /// The view on screen.
    pub view_rect: egui::Rect,
    /// The document in the texture cache.
    pub doc_id: DocId,
    /// The document.
    pub doc: &'a DocumentHandle,
    /// Its pages.
    pub info: &'a DocumentInfo,
    /// Device facts for the scale of the renders.
    pub pixels: PixelInfo,
}

fn to_egui(view_rect: egui::Rect, r: Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        view_rect.min + egui::vec2(r.x0, r.y0),
        view_rect.min + egui::vec2(r.x1, r.y1),
    )
}

/// The parts of a pinned preview on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Parts {
    /// The whole window.
    pub rect: egui::Rect,
    /// The title line.
    pub header: egui::Rect,
    /// The part of the title line that moves the window.
    pub handle: egui::Rect,
    /// The "Go there" button.
    pub go: egui::Rect,
    /// The close button.
    pub close: egui::Rect,
    /// The pages.
    pub content: egui::Rect,
    /// The resize grip.
    pub grip: egui::Rect,
}

/// Where the parts of a pinned preview are, for a window at `rect` whose "Go there" button is
/// `go_w` wide.
pub fn parts(rect: egui::Rect, go_w: f32) -> Parts {
    let header = egui::Rect::from_min_max(
        rect.min,
        egui::pos2(rect.max.x, (rect.min.y + HEADER_HEIGHT).min(rect.max.y)),
    );
    let close = egui::Rect::from_min_max(
        egui::pos2(header.max.x - HEADER_HEIGHT, header.min.y),
        header.max,
    );
    let go = egui::Rect::from_min_max(
        egui::pos2(close.min.x - go_w, header.min.y),
        egui::pos2(close.min.x, header.max.y),
    );
    let handle = egui::Rect::from_min_max(header.min, egui::pos2(go.min.x, header.max.y));
    let content = egui::Rect::from_min_max(egui::pos2(rect.min.x, header.max.y), rect.max);
    let grip = egui::Rect::from_min_max(rect.max - egui::vec2(GRIP, GRIP), rect.max);
    Parts {
        rect,
        header,
        handle,
        go,
        close,
        content,
        grip,
    }
}

/// The width of the "Go there" button.
fn go_width(ui: &Ui) -> f32 {
    let galley = ui.painter().layout_no_wrap(
        tr("pin.go").to_owned(),
        FontId::proportional(12.0),
        Color32::WHITE,
    );
    galley.size().x + 14.0
}

/// The parts of the pinned previews of a view on screen, oldest first (tests).
#[cfg(test)]
pub(crate) fn screen_parts(
    ui_ctx: &egui::Context,
    state: &PdfViewUiState,
    view_rect: egui::Rect,
) -> Vec<Parts> {
    let view = Size::new(view_rect.width(), view_rect.height());
    let go_w = ui_ctx.fonts_mut(|f| {
        f.layout_no_wrap(
            tr("pin.go").to_owned(),
            FontId::proportional(12.0),
            Color32::WHITE,
        )
        .size()
        .x
    }) + 14.0;
    state
        .links
        .pinned
        .items()
        .iter()
        .map(|p| parts(to_egui(view_rect, from_frac(p.frac, view)), go_w))
        .collect()
}

/// A button drawn by hand: a hover background, the text in the middle, an accessible name.
fn header_button(ui: &Ui, rect: egui::Rect, id: Id, text: &str, name: &str) -> bool {
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(egui::accesskit::Role::Button, true, name));
    let visuals = ui.visuals();
    if response.hovered() {
        ui.painter()
            .rect_filled(rect.shrink(2.0), 3.0, visuals.widgets.hovered.weak_bg_fill);
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        FontId::proportional(12.0),
        visuals.text_color(),
    );
    response.clicked()
}

/// Draws the pinned previews of a view and applies what is done to them: moving, resizing,
/// scrolling and zooming the content, "Go there" (an action) and closing.
pub fn show(
    ui: &Ui,
    state: &mut PdfViewUiState,
    textures: &mut TextureCache,
    queue: &mut Vec<Action>,
    env: &Env,
) {
    if state.links.pinned.is_empty() {
        return;
    }
    let view = Size::new(env.view_rect.width(), env.view_rect.height());
    let go_w = go_width(ui);
    let blank = crate::theme::blank_page(textures.page_colors());
    let visuals = ui.visuals().clone();
    let sizes = &env.info.page_sizes;
    let mut closed = None;
    for p in &mut state.links.pinned.items {
        let rect = from_frac(p.frac, view);
        let screen = to_egui(env.view_rect, rect);
        let parts = parts(screen, go_w);
        let base = Id::unique(("pinned_preview", env.tab, p.id));
        let area = Size::new(parts.content.width(), parts.content.height());
        let mut drag = egui::Vec2::ZERO;
        let mut grow = egui::Vec2::ZERO;
        let mut go = false;
        let mut close = false;
        egui::Area::new(base)
            .order(Order::Middle)
            .fixed_pos(screen.min)
            .constrain(false)
            .show(ui.ctx(), |ui| {
                let (_, bg) = ui.allocate_exact_size(screen.size(), Sense::click());
                let painter = ui.painter().with_clip_rect(screen);
                painter.rect_filled(screen, 4.0, visuals.window_fill);

                // The pages.
                let layout = content_layout(sizes, area, p.zoom);
                let scroll = content_scroll(&layout, p.anchor, area);
                let content_painter = painter.with_clip_rect(parts.content);
                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                for page in layout.visible_pages(scroll, area) {
                    let r = layout.page_rect(page);
                    let at = parts.content.min + egui::vec2(r.x0 - scroll.x, r.y0 - scroll.y);
                    let page_rect =
                        egui::Rect::from_min_size(at, egui::vec2(r.width(), r.height()));
                    let page_pt = sizes.get(page as usize).copied().unwrap_or_default();
                    let key = RenderKey {
                        page,
                        scale_milli: render_scale_milli(layout.scale(), page_pt, env.pixels),
                        generation: env.info.generation,
                    };
                    match link_preview::page_texture(textures, env.doc_id, env.doc, key) {
                        Some(id) => {
                            content_painter.image(id, page_rect, uv, Color32::WHITE);
                        }
                        None => {
                            content_painter.rect_filled(page_rect, 0.0, blank);
                        }
                    }
                }

                // The title line.
                painter.rect_filled(
                    parts.header,
                    0.0,
                    visuals.widgets.noninteractive.weak_bg_fill,
                );
                link_preview::paint_title(&painter, parts.handle, &p.title, visuals.text_color());
                let handle = ui.interact(parts.handle, base.with("handle"), Sense::drag());
                handle.widget_info(|| {
                    WidgetInfo::labeled(
                        egui::accesskit::Role::Label,
                        true,
                        tr("pin.window").replace("{title}", &p.title),
                    )
                });
                if handle.hovered() || handle.dragged() {
                    ui.ctx().set_cursor_icon(CursorIcon::Grab);
                }
                if handle.dragged() {
                    drag = handle.drag_delta();
                }
                go = header_button(ui, parts.go, base.with("go"), tr("pin.go"), tr("pin.go"));
                close = header_button(ui, parts.close, base.with("close"), "×", tr("pin.close"));

                // The resize grip: three short diagonal lines.
                let grip = ui.interact(parts.grip, base.with("grip"), Sense::drag());
                grip.widget_info(|| {
                    WidgetInfo::labeled(egui::accesskit::Role::Button, true, tr("pin.resize"))
                });
                if grip.hovered() || grip.dragged() {
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeNwSe);
                }
                if grip.dragged() {
                    grow = grip.drag_delta();
                }
                let stroke = Stroke::new(1.0, visuals.weak_text_color());
                for k in [4.0, 8.0, 12.0] {
                    painter.line_segment(
                        [
                            egui::pos2(parts.grip.max.x - k, parts.grip.max.y - 2.0),
                            egui::pos2(parts.grip.max.x - 2.0, parts.grip.max.y - k),
                        ],
                        stroke,
                    );
                }
                painter.rect_stroke(
                    screen,
                    4.0,
                    Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
                    StrokeKind::Inside,
                );

                // The wheel over it scrolls its own content; Ctrl+wheel zooms it.
                if bg.contains_pointer() || handle.contains_pointer() {
                    let (delta, factor, pointer) = ui.input_mut(|i| {
                        let d = i.smooth_scroll_delta;
                        i.smooth_scroll_delta = egui::Vec2::ZERO;
                        (d, i.zoom_delta(), i.pointer.hover_pos())
                    });
                    if factor != 1.0
                        && let Some(pos) = pointer
                    {
                        let at = pos - parts.content.min;
                        let (anchor, zoom) = zoomed(
                            sizes,
                            area,
                            p.anchor,
                            p.zoom,
                            factor,
                            Point::new(at.x, at.y),
                        );
                        p.anchor = anchor;
                        p.zoom = zoom;
                    }
                    if delta != egui::Vec2::ZERO {
                        let layout = content_layout(sizes, area, p.zoom);
                        p.anchor =
                            scrolled(&layout, p.anchor, Point::new(-delta.x, -delta.y), area);
                    }
                }
            });
        if drag != egui::Vec2::ZERO {
            p.frac = to_frac(moved(rect, drag.x, drag.y, view), view);
        }
        if grow != egui::Vec2::ZERO {
            p.frac = to_frac(resized(rect, grow.x, grow.y, view), view);
        }
        if go {
            queue.push(Action::Navigate {
                tab: env.tab,
                to: p.go_target(),
                how: OpenHow::Here,
            });
        }
        if close {
            closed = Some(p.id);
        }
    }
    if let Some(id) = closed {
        state.links.pinned.close(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: Size = Size {
        w: 1000.0,
        h: 800.0,
    };

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    fn request(title: &str) -> PinRequest {
        PinRequest {
            title: title.to_owned(),
            dest: Location::new(2, 72.0, 300.0),
            top: 284.0,
            rect: Rect::new(100.0, 100.0, 500.0, 340.0),
            view: VIEW,
        }
    }

    #[test]
    fn a_fifth_pin_closes_the_oldest() {
        let mut p = PinnedPreviews::default();
        for i in 0..4 {
            p.pin(request(&format!("p{i}")));
        }
        assert_eq!(p.len(), 4);
        p.pin(request("p4"));
        assert_eq!(p.len(), MAX_PINNED);
        let titles: Vec<&str> = p.items().iter().map(|x| x.title.as_str()).collect();
        assert_eq!(titles, vec!["p1", "p2", "p3", "p4"]);
        // Ids stay distinct after the drop.
        let mut ids: Vec<u64> = p.items().iter().map(|x| x.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), 4);
        // A copy of the view starts without them.
        assert!(p.clone().is_empty());
        p.close_all();
        assert!(p.is_empty());
    }

    #[test]
    fn a_pin_starts_where_the_popup_was_and_shows_the_same_place() {
        let mut p = PinnedPreviews::default();
        p.pin(request("x"));
        let x = &p.items()[0];
        assert_eq!(x.frac, Rect::new(0.1, 0.125, 0.5, 0.425));
        assert_eq!(x.anchor, Location::new(2, 0.0, 284.0));
        // Not scrolled: "Go there" goes to the destination itself.
        assert_eq!(x.go_target(), Location::new(2, 72.0, 300.0));
        let mut y = x.clone();
        y.anchor = Location::new(3, 0.0, 40.0);
        assert_eq!(y.go_target(), Location::new(3, 0.0, 40.0));
    }

    #[test]
    fn moving_keeps_the_window_inside_the_view() {
        let r = Rect::new(100.0, 100.0, 400.0, 300.0);
        let m = moved(r, 50.0, -20.0, VIEW);
        assert_eq!(m, Rect::new(150.0, 80.0, 450.0, 280.0));
        let m = moved(r, -500.0, -500.0, VIEW);
        assert_eq!(m, Rect::new(0.0, 0.0, 300.0, 200.0));
        let m = moved(r, 2000.0, 2000.0, VIEW);
        assert_eq!(m, Rect::new(700.0, 600.0, 1000.0, 800.0));
    }

    #[test]
    fn resizing_stops_at_the_smallest_size_and_at_the_view_edge() {
        let r = Rect::new(100.0, 100.0, 400.0, 300.0);
        let s = resized(r, 100.0, 50.0, VIEW);
        assert_eq!(s, Rect::new(100.0, 100.0, 500.0, 350.0));
        let s = resized(r, -500.0, -500.0, VIEW);
        assert!(close(s.width(), MIN_W) && close(s.height(), MIN_H));
        assert_eq!((s.x0, s.y0), (100.0, 100.0));
        let s = resized(r, 5000.0, 5000.0, VIEW);
        assert_eq!(s, Rect::new(100.0, 100.0, 1000.0, 800.0));
    }

    #[test]
    fn fractions_follow_a_resized_view() {
        let r = Rect::new(100.0, 100.0, 500.0, 340.0);
        let f = to_frac(r, VIEW);
        assert_eq!(from_frac(f, VIEW), r);
        // Twice as large a view: twice as large a window, at the same relative place.
        let big = from_frac(f, Size::new(2000.0, 1600.0));
        assert_eq!(big, Rect::new(200.0, 200.0, 1000.0, 680.0));
        // A small view: the window keeps its smallest size and stays inside.
        let small = from_frac(f, Size::new(400.0, 300.0));
        assert!(close(small.width(), MIN_W) && close(small.height(), MIN_H));
        assert!(small.x1 <= 400.0 && small.y1 <= 300.0);
        // A view smaller than the smallest window: as large as the view.
        let tiny = from_frac(f, Size::new(200.0, 100.0));
        assert_eq!(tiny, Rect::new(0.0, 0.0, 200.0, 100.0));
    }

    fn pages() -> Vec<Size> {
        vec![Size::new(612.0, 792.0); 4]
    }

    #[test]
    fn the_content_scrolls_through_the_pages_and_stops_at_both_ends() {
        let sizes = pages();
        let area = Size::new(400.0, 300.0);
        let layout = content_layout(&sizes, area, 1.0);
        // The page is as wide as the area.
        assert!(close(layout.page_rect(0).width(), 400.0));
        let start = Location::new(1, 0.0, 700.0);
        // Down past the bottom of page 2: on to page 3.
        let a = scrolled(&layout, start, Point::new(0.0, 200.0), area);
        assert_eq!(a.page, 2);
        // Far up: the start of the document.
        let top = scrolled(&layout, start, Point::new(0.0, -1e6), area);
        assert_eq!(top.page, 0);
        assert!(close(top.y, 0.0));
        // Far down: the bottom of the last page is at the bottom of the area, no further.
        let end = scrolled(&layout, start, Point::new(0.0, 1e6), area);
        let s = content_scroll(&layout, end, area);
        assert!(close(s.y, layout.max_scroll(area).y));
        assert_eq!(end.page, 3);
        let again = scrolled(&layout, end, Point::new(0.0, 500.0), area);
        assert_eq!(again, end);
    }

    #[test]
    fn zooming_the_content_keeps_the_point_under_the_pointer() {
        let sizes = pages();
        let area = Size::new(400.0, 300.0);
        let anchor = Location::new(1, 0.0, 100.0);
        let pointer = Point::new(200.0, 150.0);
        let old = content_layout(&sizes, area, 1.0);
        let s = content_scroll(&old, anchor, area);
        let under = anchor_at(&old, Point::new(s.x + pointer.x, s.y + pointer.y));
        let (a, z) = zoomed(&sizes, area, anchor, 1.0, 2.0, pointer);
        assert!(close(z, 2.0));
        let new = content_layout(&sizes, area, z);
        let s2 = content_scroll(&new, a, area);
        let p = new.content_pos(under);
        assert!(close(p.x - s2.x, pointer.x) && close(p.y - s2.y, pointer.y));
        // Limited.
        assert!(close(
            zoomed(&sizes, area, anchor, 1.0, 100.0, pointer).1,
            MAX_ZOOM
        ));
        assert!(close(
            zoomed(&sizes, area, anchor, 1.0, 0.01, pointer).1,
            MIN_ZOOM
        ));
    }

    #[test]
    fn the_parts_of_the_window_do_not_overlap() {
        let r = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(300.0, 200.0));
        let p = parts(r, 60.0);
        assert!(close(p.header.height(), HEADER_HEIGHT));
        assert!(p.handle.max.x <= p.go.min.x && p.go.max.x <= p.close.min.x);
        assert!(close(p.close.max.x, r.max.x));
        assert!(close(p.content.min.y, p.header.max.y) && close(p.content.max.y, r.max.y));
        assert!(r.contains_rect(p.grip));
    }
}

/// Pinned previews in the drawn view, frame by frame.
#[cfg(test)]
mod widget_tests {
    use super::*;
    use egui::accesskit::Role;
    use egui::{Event, Modifiers, PointerButton, Pos2, pos2, vec2};
    use refdesk_core::testutil::TempPdf;

    use crate::action::dispatch;
    use crate::overlays::link_preview::tests::{
        Harness, LINK, document, hover_from, opened_document,
    };

    fn rig(stem: &str) -> (Harness, TabId, TempPdf) {
        let (mut h, tab, pdf) = opened_document(stem);
        h.accesskit = true;
        (h, tab, pdf)
    }

    fn pinned(h: &Harness, tab: TabId) -> &PinnedPreviews {
        &h.tab(tab).ui.links.pinned
    }

    fn parts_of(h: &Harness, tab: TabId) -> Vec<Parts> {
        screen_parts(&h.ctx, &h.tab(tab).ui, h.view_rect())
    }

    /// Opens the popup of the first link from the time `at` and presses its pin. Returns the
    /// popup's rectangle.
    fn pin_at(h: &mut Harness, tab: TabId, at: f64) -> egui::Rect {
        let pos = h.link_pos(tab, 0, LINK);
        hover_from(h, at, pos);
        h.frame_at(at + 0.4, Vec::new());
        h.frame_at(at + 0.45, Vec::new());
        assert!(h.is_open(tab));
        let (popup, _) = h.popup(tab).expect("popup");
        let header = egui::Rect::from_min_size(popup.min, vec2(popup.width(), HEADER_HEIGHT));
        assert!(h.has_node(Role::Button, tr("pin.button")), "{:?}", h.nodes);
        h.click_at(at + 0.5, link_preview::pin_rect(header).center());
        // (egui sizes a new area in its first frame without drawing it.)
        h.frame_at(at + 0.6, Vec::new());
        h.frame_at(at + 0.62, Vec::new());
        popup
    }

    #[test]
    fn the_pin_turns_the_popup_into_a_preview_that_stays_after_the_pointer_leaves() {
        let (mut h, tab, _pdf) = rig("pin-stay");
        let popup = pin_at(&mut h, tab, 100.0);
        assert_eq!(pinned(&h, tab).len(), 1);
        assert!(!h.is_open(tab), "the popup became the pinned preview");
        let p = &pinned(&h, tab).items()[0];
        let title = crate::i18n::tr("link.page_title")
            .replace("{page}", "3")
            .replace("{title}", "7.3.2 Transitions");
        assert_eq!(p.title, title);
        assert_eq!(p.anchor.page, 2);
        // Where the popup was.
        let r = parts_of(&h, tab)[0].rect;
        assert!((r.min - popup.min).length() < 0.5 && (r.size() - popup.size()).length() < 0.5);
        // The pointer goes far away; time passes; the view scrolls, a key, a click: it stays.
        h.frame_at(101.0, vec![Event::PointerMoved(pos2(700.0, 560.0))]);
        h.frame_at(105.0, Vec::new());
        h.key_at(105.1, egui::Key::ArrowDown, Modifiers::NONE);
        h.click_at(105.2, pos2(700.0, 560.0));
        h.frame_at(106.0, Vec::new());
        assert_eq!(pinned(&h, tab).len(), 1);
        // Its title line and buttons are in the accessibility tree.
        assert!(h.drew(&title), "{:?}", h.drawn);
        let window = tr("pin.window").replace("{title}", &title);
        assert!(h.has_node(Role::Label, &window), "{:?}", h.nodes);
        assert!(h.has_node(Role::Button, tr("pin.go")));
        assert!(h.has_node(Role::Button, tr("pin.close")));
        assert!(h.has_node(Role::Button, tr("pin.resize")));
    }

    #[test]
    fn the_wheel_over_it_scrolls_only_its_content() {
        let (mut h, tab, _pdf) = rig("pin-wheel");
        pin_at(&mut h, tab, 100.0);
        let before = pinned(&h, tab).items()[0].anchor;
        let view_scroll = h.tab(tab).ui.scroll();
        let inside = parts_of(&h, tab)[0].content.center();
        let wheel = Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, -120.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: Modifiers::NONE,
        };
        h.frame_at(101.0, vec![Event::PointerMoved(inside), wheel]);
        for i in 1..30 {
            h.frame_at(101.0 + f64::from(i) / 60.0, Vec::new());
        }
        let after = pinned(&h, tab).items()[0].anchor;
        assert!(
            after.page > before.page || after.y > before.y + 10.0,
            "{before:?} -> {after:?}"
        );
        assert_eq!(h.tab(tab).ui.scroll(), view_scroll, "the view did not move");
        assert_eq!(pinned(&h, tab).len(), 1);
    }

    #[test]
    fn go_there_jumps_to_the_destination_and_back_returns() {
        let (mut h, tab, _pdf) = rig("pin-go");
        pin_at(&mut h, tab, 100.0);
        let go = parts_of(&h, tab)[0].go.center();
        h.click_at(101.0, go);
        h.frame_at(101.1, Vec::new());
        let at = h.tab(tab).view.anchor;
        assert_eq!(at.page, 2);
        assert!(h.tab(tab).view.history.can_back());
        assert_eq!(pinned(&h, tab).len(), 1, "it stays pinned");
        dispatch(&mut h.state, Action::Back(tab), &h.ctx);
        h.frame_at(101.5, Vec::new());
        assert_eq!(h.tab(tab).view.anchor.page, 0);
    }

    #[test]
    fn the_close_button_and_the_command_close_them() {
        let (mut h, tab, _pdf) = rig("pin-close");
        pin_at(&mut h, tab, 100.0);
        let close = parts_of(&h, tab)[0].close.center();
        h.click_at(101.0, close);
        h.frame_at(101.1, Vec::new());
        assert!(pinned(&h, tab).is_empty());

        let command = crate::commands::command("view.close_pinned").expect("command");
        assert!(command.run(&h.state).is_none(), "nothing to close");
        pin_at(&mut h, tab, 110.0);
        h.frame_at(111.0, vec![Event::PointerMoved(pos2(700.0, 560.0))]);
        pin_at(&mut h, tab, 120.0);
        assert_eq!(pinned(&h, tab).len(), 2);
        let action = command.run(&h.state).expect("possible");
        assert_eq!(action, Action::CloseAllPinned(tab));
        dispatch(&mut h.state, action, &h.ctx);
        assert!(pinned(&h, tab).is_empty());
    }

    fn drag(h: &mut Harness, at: f64, from: Pos2, to: Pos2) {
        let button = |pos, pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        h.frame_at(at, vec![Event::PointerMoved(from), button(from, true)]);
        let mid = from + (to - from) / 2.0;
        h.frame_at(at + 0.02, vec![Event::PointerMoved(mid)]);
        h.frame_at(at + 0.04, vec![Event::PointerMoved(to)]);
        h.frame_at(at + 0.06, vec![button(to, false)]);
        h.frame_at(at + 0.08, Vec::new());
    }

    #[test]
    fn the_title_line_moves_it_and_the_corner_resizes_it_within_the_view() {
        let (mut h, tab, _pdf) = rig("pin-move");
        pin_at(&mut h, tab, 100.0);
        let before = parts_of(&h, tab)[0];
        let from = before.handle.left_center() + vec2(20.0, 0.0);
        drag(&mut h, 101.0, from, from + vec2(40.0, 30.0));
        let after = parts_of(&h, tab)[0];
        assert!(
            (after.rect.min - (before.rect.min + vec2(40.0, 30.0))).length() < 1.0,
            "{before:?} -> {after:?}"
        );
        assert!((after.rect.size() - before.rect.size()).length() < 0.5);
        assert!(
            !h.tab(tab).ui.has_selection(),
            "the drag selected nothing below"
        );

        // Far out of the view: it stops at the edge.
        let from = after.handle.left_center() + vec2(20.0, 0.0);
        drag(&mut h, 102.0, from, from + vec2(-2000.0, 2000.0));
        let r = parts_of(&h, tab)[0].rect;
        let view = h.view_rect();
        assert!(
            (r.min.x - view.min.x).abs() < 0.5 && (r.max.y - view.max.y).abs() < 0.5,
            "{r:?}"
        );

        // The corner: far smaller than allowed gives the smallest size.
        let grip = parts_of(&h, tab)[0].grip.center();
        drag(&mut h, 103.0, grip, grip + vec2(-1000.0, -1000.0));
        let r = parts_of(&h, tab)[0].rect;
        assert!(
            (r.width() - MIN_W).abs() < 0.5 && (r.height() - MIN_H).abs() < 0.5,
            "{r:?}"
        );
    }

    #[test]
    fn the_pointer_over_it_does_not_hover_the_link_below() {
        let (mut h, tab, _pdf) = rig("pin-cover");
        pin_at(&mut h, tab, 100.0);
        // Put it over the link.
        let link = h.link_pos(tab, 0, LINK);
        let view = Size::new(h.view_rect().width(), h.view_rect().height());
        if let Some(Tab::Pdf(pdf)) = h.state.tabs.get_mut(tab) {
            let r = Rect::new(
                link.x - 150.0,
                link.y - 40.0,
                link.x + 150.0,
                link.y + 160.0,
            );
            pdf.ui.links.pinned.items[0].frac = to_frac(r, view);
        }
        h.frame_at(101.0, Vec::new());
        h.frame_at(101.1, Vec::new());
        for t in [101.2, 101.5, 102.0] {
            h.frame_at(t, vec![Event::PointerMoved(link)]);
            assert_eq!(h.tab(tab).ui.links.hover_text, None, "at {t}");
            assert!(!h.is_open(tab), "at {t}");
        }
    }

    #[test]
    fn they_stay_with_their_tab_while_another_tab_is_shown() {
        let (mut h, tab, _pdf) = rig("pin-tabs");
        pin_at(&mut h, tab, 100.0);
        let other = document("pin-tabs-2");
        let second = h.open(other.path());
        h.frame_at(101.0, Vec::new());
        assert_eq!(h.state.focus, Some(second));
        assert!(pinned(&h, second).is_empty());
        h.frame_at(101.5, Vec::new());
        dispatch(&mut h.state, Action::FocusTab(tab), &h.ctx);
        h.frame_at(102.0, Vec::new());
        h.frame_at(102.1, Vec::new());
        assert_eq!(pinned(&h, tab).len(), 1);
        assert!(h.has_node(Role::Button, tr("pin.close")));
        // A copy of the tab starts without them.
        dispatch(&mut h.state, Action::DuplicateTab(tab), &h.ctx);
        let copy = h.state.focus.expect("focus");
        assert!(pinned(&h, copy).is_empty());
    }
}
