//! The PDF view widget (`docs/ARCHITECTURE.md` §5.5).
//!
//! The saved state of a view is `ViewState.anchor` (the document location at the top-left corner
//! of the viewport) and `zoom`. Between frames the view keeps its scroll offset in content-space
//! pixels in [`PdfViewUiState`] so that scrolling does not jitter through rounding. The offset is
//! recomputed from the anchor only when
//! (a) the layout changed (zoom, viewport size, document info), or
//! (b) someone else changed the anchor (a page jump, a history step).
//! Otherwise the offset is the source and the anchor is rewritten from it every frame.
//!
//! The decisions (layout and scroll bookkeeping, which pages to request at which priority, the
//! zoom gesture, scroll bar geometry, key steps) are plain functions on plain data, tested
//! without egui (rule R-10). The egui part reads input and paints.

use std::ops::Range;
use std::time::Duration;

use egui::{Color32, Key, Modifiers, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};
use refdesk_core::layout::{MAX_ZOOM, MIN_ZOOM, PIXELS_PER_POINT_AT_100};
use refdesk_core::{
    DocStatus, DocumentHandle, DocumentInfo, LayoutParams, Location, PageLayout, Point, Priority,
    RenderKey, Size, ViewState, ZoomMode,
};

use crate::action::Action;
use crate::i18n::tr;
use crate::panels::status_bar::GotoInput;
use crate::state::{DocId, PdfTab, TabId};
use crate::views::textures::{Pick, TextureCache};

/// Vertical space between pages, in logical pixels.
pub const PAGE_GAP: f32 = 8.0;
/// Space around the document, in logical pixels.
pub const MARGIN: f32 = 12.0;
/// Pages before and after the visible ones that are rendered ahead of time.
pub const PREFETCH_PAGES: u32 = 2;
/// A zoom gesture counts as finished this long after its last change.
pub const ZOOM_SETTLE_SECS: f64 = 0.08;
/// Thickness of the scroll bars.
const SCROLLBAR: f32 = 12.0;
/// Shortest scroll bar thumb.
const MIN_THUMB: f32 = 24.0;
/// Scroll distance of one arrow key press.
const ARROW_STEP: f32 = 48.0;
/// Fraction of the viewport height that Space and PgUp/PgDn move.
const PAGE_STEP: f32 = 0.9;

/// What a layout depends on. A different key means a new layout.
#[derive(Clone, Copy, Debug, PartialEq)]
struct LayoutKey {
    generation: u32,
    pages: usize,
    zoom: ZoomMode,
    viewport: Size,
    fit_page: u32,
}

/// A layout and what it was made from.
#[derive(Clone, Debug)]
struct Geometry {
    key: LayoutKey,
    layout: PageLayout,
}

/// Tracks a continuous zoom gesture (Ctrl+wheel, pinch). While it lasts no new scale is
/// requested; existing textures are stretched instead.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ZoomGesture {
    last_change: Option<f64>,
}

impl ZoomGesture {
    /// Records a zoom change at time `now` (seconds).
    pub fn note(&mut self, now: f64) {
        self.last_change = Some(now);
    }

    /// True within [`ZOOM_SETTLE_SECS`] of the last change.
    pub fn in_progress(&self, now: f64) -> bool {
        self.remaining(now).is_some()
    }

    /// Time until the gesture counts as finished; `None` when it already has.
    pub fn remaining(&self, now: f64) -> Option<Duration> {
        let left = ZOOM_SETTLE_SECS - (now - self.last_change?);
        (left > 0.0).then(|| Duration::from_secs_f64(left))
    }
}

/// The unsaved state of one PDF view.
#[derive(Clone, Debug, Default)]
pub struct PdfViewUiState {
    /// Boxed: tabs are moved around by value and a layout is large.
    geometry: Option<Box<Geometry>>,
    /// Scroll offset in content space (logical pixels).
    scroll: Point,
    /// The anchor this view wrote last; a different `ViewState.anchor` was set from outside.
    written_anchor: Option<Location>,
    /// The page `ZoomMode::FitPage` fits, fixed when that mode is chosen.
    fit_page: u32,
    /// The ongoing zoom gesture.
    pub zoom_gesture: ZoomGesture,
    /// The page across the vertical middle of the viewport, as of the last frame.
    pub current_page: Option<u32>,
    /// The actual zoom factor (1.0 = 100%) as of the last frame; it differs from the zoom mode
    /// for the fit modes.
    pub effective_zoom: Option<f32>,
    /// The password being typed for an encrypted document.
    pub password: String,
    /// A password was submitted at least once.
    pub password_tried: bool,
    /// The page number field of the status bar, while it is open.
    pub goto: Option<GotoInput>,
}

/// The unclamped document location at a content-space point: the nearest page, and the offset
/// from its top-left corner (negative in the margin above or left of it). Unlike
/// [`PageLayout::location_at`] it round-trips through [`PageLayout::content_pos`] exactly, so
/// the margin above the first page survives a resize.
pub fn anchor_at(layout: &PageLayout, content: Point) -> Location {
    let page = layout.location_at(content).page;
    let rect = layout.page_rect(page);
    let s = layout.scale();
    Location::new(page, (content.x - rect.x0) / s, (content.y - rect.y0) / s)
}

impl PdfViewUiState {
    /// The current layout, once [`PdfViewUiState::sync`] ran.
    pub fn layout(&self) -> Option<&PageLayout> {
        self.geometry.as_ref().map(|g| &g.layout)
    }

    /// The current scroll offset in content space.
    pub fn scroll(&self) -> Point {
        self.scroll
    }

    fn viewport(&self) -> Size {
        self.geometry
            .as_ref()
            .map_or(Size::default(), |g| g.key.viewport)
    }

    /// Brings the layout and the scroll offset up to date for a frame: see the module comment
    /// for when the offset follows the anchor. A zoom change made outside the view (menu,
    /// shortcut) keeps the document point at the middle of the viewport in place.
    pub fn sync(&mut self, view: &ViewState, page_sizes: &[Size], generation: u32, viewport: Size) {
        let previous_zoom = self.geometry.as_ref().map(|g| g.key.zoom);
        if view.zoom == ZoomMode::FitPage && previous_zoom != Some(ZoomMode::FitPage) {
            self.fit_page = self.current_page.unwrap_or(view.anchor.page);
        }
        let key = LayoutKey {
            generation,
            pages: page_sizes.len(),
            zoom: view.zoom,
            viewport,
            fit_page: if view.zoom == ZoomMode::FitPage {
                self.fit_page
            } else {
                0
            },
        };
        let anchor_moved = self.written_anchor != Some(view.anchor);
        match self.geometry.take() {
            Some(old) if old.key == key => {
                if anchor_moved {
                    self.scroll = old.layout.content_pos(view.anchor);
                }
                self.scroll = old.layout.clamp_scroll(self.scroll, viewport);
                self.geometry = Some(old);
            }
            Some(old) if old.key.zoom != key.zoom && !anchor_moved => {
                let old_view = old.key.viewport;
                let middle = Point::new(
                    self.scroll.x + old_view.w / 2.0,
                    self.scroll.y + old_view.h / 2.0,
                );
                let loc = anchor_at(&old.layout, middle);
                let layout = PageLayout::new(page_sizes, params(key));
                let keep_at = Point::new(viewport.w / 2.0, viewport.h / 2.0);
                self.scroll = layout.scroll_to_keep(loc, keep_at, viewport);
                self.geometry = Some(Box::new(Geometry { key, layout }));
            }
            _ => {
                let layout = PageLayout::new(page_sizes, params(key));
                self.scroll = layout.clamp_scroll(layout.content_pos(view.anchor), viewport);
                self.geometry = Some(Box::new(Geometry { key, layout }));
            }
        }
    }

    /// Moves the scroll offset by `delta` content pixels, within the valid range.
    pub fn scroll_by(&mut self, delta: Point) {
        self.scroll_to(Point::new(self.scroll.x + delta.x, self.scroll.y + delta.y));
    }

    /// Sets the scroll offset, limited to the valid range.
    pub fn scroll_to(&mut self, scroll: Point) {
        if let Some(g) = &self.geometry {
            self.scroll = g.layout.clamp_scroll(scroll, g.key.viewport);
        }
    }

    /// Zooms by `factor` around `pointer` (a position inside the viewport): the document point
    /// under it stays under it. The zoom becomes `Custom`. Returns false when nothing changed
    /// (at the zoom limits, or before the first layout).
    pub fn zoom_at(
        &mut self,
        view: &mut ViewState,
        page_sizes: &[Size],
        factor: f32,
        pointer: Point,
        now: f64,
    ) -> bool {
        let Some(old) = &self.geometry else {
            return false;
        };
        if !(factor.is_finite() && factor > 0.0) {
            return false;
        }
        let current = old.layout.scale() / PIXELS_PER_POINT_AT_100;
        let zoom = (current * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (zoom - current).abs() < 1e-5 {
            return false;
        }
        let under = anchor_at(
            &old.layout,
            Point::new(self.scroll.x + pointer.x, self.scroll.y + pointer.y),
        );
        let key = LayoutKey {
            zoom: ZoomMode::Custom(zoom),
            fit_page: 0,
            ..old.key
        };
        let layout = PageLayout::new(page_sizes, params(key));
        self.scroll = layout.scroll_to_keep(under, pointer, key.viewport);
        self.geometry = Some(Box::new(Geometry { key, layout }));
        view.zoom = key.zoom;
        self.zoom_gesture.note(now);
        true
    }

    /// Ends a frame's changes: writes the anchor from the scroll offset and updates what the
    /// status bar shows.
    pub fn commit(&mut self, view: &mut ViewState) {
        let Some(g) = &self.geometry else {
            return;
        };
        view.anchor = anchor_at(&g.layout, self.scroll);
        self.written_anchor = Some(view.anchor);
        self.current_page = (g.layout.page_count() > 0)
            .then(|| current_page(&g.layout, self.scroll, g.key.viewport));
        self.effective_zoom = Some(g.layout.scale() / PIXELS_PER_POINT_AT_100);
    }

    /// The render key a page is wanted at now.
    pub fn render_key(&self, page: u32, generation: u32, pixels: PixelInfo) -> Option<RenderKey> {
        let g = self.geometry.as_ref()?;
        let rect = g.layout.page_rect(page);
        let page_pt = Size::new(
            rect.width() / g.layout.scale(),
            rect.height() / g.layout.scale(),
        );
        Some(RenderKey {
            page,
            scale_milli: render_scale_milli(g.layout.scale(), page_pt, pixels),
            generation,
        })
    }

    /// The pages overlapping the viewport.
    pub fn visible_pages(&self) -> Range<u32> {
        match &self.geometry {
            Some(g) => g.layout.visible_pages(self.scroll, g.key.viewport),
            None => 0..0,
        }
    }

    /// The renders to request this frame: visible pages, then the neighbors, skipping pages
    /// whose exact texture is there. Nothing while a zoom gesture lasts.
    pub fn frame_requests(
        &self,
        generation: u32,
        pixels: PixelInfo,
        now: f64,
        has_exact: impl Fn(RenderKey) -> bool,
    ) -> Vec<(RenderKey, Priority)> {
        let Some(g) = &self.geometry else {
            return Vec::new();
        };
        if self.zoom_gesture.in_progress(now) {
            return Vec::new();
        }
        let keys: Vec<(RenderKey, Priority)> =
            plan_pages(self.visible_pages(), g.layout.page_count())
                .into_iter()
                .filter_map(|(page, prio)| Some((self.render_key(page, generation, pixels)?, prio)))
                .collect();
        keys.into_iter().filter(|(k, _)| !has_exact(*k)).collect()
    }
}

/// The layout parameters for a key.
fn params(key: LayoutKey) -> LayoutParams {
    LayoutParams {
        viewport: key.viewport,
        zoom: key.zoom,
        gap: PAGE_GAP,
        margin: MARGIN,
        fit_page: key.fit_page,
    }
}

/// The page across the vertical middle of the viewport (the nearest one in a gap).
pub fn current_page(layout: &PageLayout, scroll: Point, viewport: Size) -> u32 {
    layout
        .location_at(Point::new(
            scroll.x + viewport.w / 2.0,
            scroll.y + viewport.h / 2.0,
        ))
        .page
}

/// Device facts the render scale depends on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelInfo {
    /// Physical pixels per logical pixel.
    pub pixels_per_point: f32,
    /// Largest texture side the renderer accepts.
    pub max_texture_side: usize,
}

/// The rendering scale for a page, times 1000: layout scale × pixels per point, lowered so no
/// side of the texture exceeds `max_texture_side` (the texture is then stretched).
pub fn render_scale_milli(layout_scale: f32, page_pt: Size, pixels: PixelInfo) -> u32 {
    let mut scale = layout_scale * pixels.pixels_per_point;
    let longest = page_pt.w.max(page_pt.h);
    if longest > 0.0 && longest * scale > pixels.max_texture_side as f32 {
        // Round down so the rendered side stays within the limit.
        let limit = (pixels.max_texture_side as f32 / longest * 1000.0).floor() / 1000.0;
        scale = scale.min(limit);
    }
    ((scale * 1000.0).round() as u32).max(1)
}

/// Which pages to request: the visible ones at `Visible`, then up to [`PREFETCH_PAGES`] on each
/// side at `Prefetch`, never past the ends of the document.
pub fn plan_pages(visible: Range<u32>, page_count: u32) -> Vec<(u32, Priority)> {
    let start = visible.start.min(page_count);
    let end = visible.end.min(page_count).max(start);
    let mut out: Vec<(u32, Priority)> = (start..end).map(|p| (p, Priority::Visible)).collect();
    let after = end..end.saturating_add(PREFETCH_PAGES).min(page_count);
    let before = start.saturating_sub(PREFETCH_PAGES)..start;
    out.extend(after.chain(before.rev()).map(|p| (p, Priority::Prefetch)));
    out
}

/// A keyboard movement of the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavKey {
    /// PgDn, Space.
    PageDown,
    /// PgUp, Shift+Space.
    PageUp,
    /// Down arrow.
    Down,
    /// Up arrow.
    Up,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Home: start of the document.
    Home,
    /// End: end of the document.
    End,
}

/// The scroll offset after a key, before clamping (End uses the given maximum).
pub fn key_scroll(key: NavKey, scroll: Point, viewport: Size, max: Point) -> Point {
    let page = viewport.h * PAGE_STEP;
    match key {
        NavKey::PageDown => Point::new(scroll.x, scroll.y + page),
        NavKey::PageUp => Point::new(scroll.x, scroll.y - page),
        NavKey::Down => Point::new(scroll.x, scroll.y + ARROW_STEP),
        NavKey::Up => Point::new(scroll.x, scroll.y - ARROW_STEP),
        NavKey::Left => Point::new(scroll.x - ARROW_STEP, scroll.y),
        NavKey::Right => Point::new(scroll.x + ARROW_STEP, scroll.y),
        NavKey::Home => Point::new(scroll.x, 0.0),
        NavKey::End => Point::new(scroll.x, max.y),
    }
}

/// The keys the view reacts to, most specific first (`consume_key` ignores extra Shift).
const KEYS: &[(Modifiers, Key, NavKey)] = &[
    (Modifiers::SHIFT, Key::Space, NavKey::PageUp),
    (Modifiers::NONE, Key::Space, NavKey::PageDown),
    (Modifiers::NONE, Key::PageDown, NavKey::PageDown),
    (Modifiers::NONE, Key::PageUp, NavKey::PageUp),
    (Modifiers::NONE, Key::ArrowDown, NavKey::Down),
    (Modifiers::NONE, Key::ArrowUp, NavKey::Up),
    (Modifiers::NONE, Key::ArrowLeft, NavKey::Left),
    (Modifiers::NONE, Key::ArrowRight, NavKey::Right),
    (Modifiers::NONE, Key::Home, NavKey::Home),
    (Modifiers::NONE, Key::End, NavKey::End),
];

/// A scroll bar thumb: offset from the start of the track and length.
pub fn thumb(track: f32, viewport: f32, content: f32, scroll: f32) -> (f32, f32) {
    if content <= viewport || track <= 0.0 {
        return (0.0, track.max(0.0));
    }
    let len = (track * viewport / content).clamp(MIN_THUMB.min(track), track);
    let max_scroll = content - viewport;
    let start = (track - len) * (scroll / max_scroll).clamp(0.0, 1.0);
    (start, len)
}

/// The scroll offset that puts the thumb's start at `thumb_start` on the track.
pub fn scroll_for_thumb(track: f32, viewport: f32, content: f32, thumb_start: f32) -> f32 {
    let (_, len) = thumb(track, viewport, content, 0.0);
    let free = track - len;
    if free <= 0.0 {
        return 0.0;
    }
    (thumb_start / free).clamp(0.0, 1.0) * (content - viewport).max(0.0)
}

// ------------------------------------------------------------------------------------------------
// Drawing
// ------------------------------------------------------------------------------------------------

/// What the view needs from its surroundings for one frame.
pub struct ViewContext<'a> {
    /// The tab being drawn.
    pub tab_id: TabId,
    /// Whether it is the tab commands apply to.
    pub focused: bool,
    /// Shared textures.
    pub textures: &'a mut TextureCache,
    /// Where widgets put their actions.
    pub queue: &'a mut Vec<Action>,
}

/// Draws a PDF tab: the document, or its loading, error or password state.
pub fn show(ui: &mut Ui, tab: &mut PdfTab, doc: &DocumentHandle, cx: ViewContext<'_>) {
    match doc.status() {
        DocStatus::Ready => match doc.info() {
            Some(info) => show_document(ui, tab, doc, &info, cx),
            None => show_loading(ui, doc),
        },
        DocStatus::Loading => show_loading(ui, doc),
        DocStatus::NeedsPassword => show_password(ui, tab, doc),
        DocStatus::Failed(message) => show_failed(ui, doc, &message),
    }
}

fn file_name(doc: &DocumentHandle) -> String {
    doc.path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| doc.path().display().to_string())
}

fn show_loading(ui: &mut Ui, doc: &DocumentHandle) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.3);
        ui.spinner();
        ui.label(tr("doc.loading"));
        ui.strong(file_name(doc));
    });
}

fn show_failed(ui: &mut Ui, doc: &DocumentHandle, message: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.3);
        ui.colored_label(
            ui.visuals().error_fg_color,
            tr("doc.failed").replace("{}", message),
        );
        ui.label(format!("{}: {}", tr("doc.path"), doc.path().display()));
    });
}

fn show_password(ui: &mut Ui, tab: &mut PdfTab, doc: &DocumentHandle) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.3);
        ui.strong(file_name(doc));
        ui.label(tr("doc.needs_password"));
        let field = ui.add(
            egui::TextEdit::singleline(&mut tab.ui.password)
                .password(true)
                .desired_width(240.0),
        );
        let entered = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if (ui.button(tr("doc.password_submit")).clicked() || entered)
            && !tab.ui.password.is_empty()
        {
            doc.authenticate(&tab.ui.password);
            tab.ui.password.clear();
            tab.ui.password_tried = true;
        }
        if tab.ui.password_tried {
            ui.colored_label(ui.visuals().error_fg_color, tr("doc.password_wrong"));
        }
    });
}

/// The view background: a dark middle tone of the theme.
fn background(visuals: &egui::Visuals) -> Color32 {
    if visuals.dark_mode {
        Color32::from_gray(52)
    } else {
        Color32::from_gray(140)
    }
}

fn to_vec(p: Point) -> Vec2 {
    vec2(p.x, p.y)
}

fn show_document(
    ui: &mut Ui,
    tab: &mut PdfTab,
    doc: &DocumentHandle,
    info: &DocumentInfo,
    cx: ViewContext<'_>,
) {
    let outer = ui.available_rect_before_wrap();
    ui.allocate_rect(outer, Sense::hover());
    let view_rect = Rect::from_min_max(outer.min, outer.max - vec2(SCROLLBAR, SCROLLBAR));
    if view_rect.width() < 1.0 || view_rect.height() < 1.0 {
        return;
    }
    let id = ui.scope_id().with(("pdf_view", cx.tab_id));
    let response = ui.interact(view_rect, id, Sense::click());
    if response.clicked() && !cx.focused {
        cx.queue.push(Action::FocusTab(cx.tab_id));
    }

    let viewport = Size::new(view_rect.width(), view_rect.height());
    let state = &mut tab.ui;
    state.sync(&tab.view, &info.page_sizes, info.generation, viewport);

    let now = ui.input(|i| i.time);
    let hovered = response.contains_pointer();
    if hovered {
        let (zoom, pointer, delta) = ui.input_mut(|i| {
            let delta = i.smooth_scroll_delta;
            i.smooth_scroll_delta = Vec2::ZERO;
            (i.zoom_delta(), i.pointer.hover_pos(), delta)
        });
        if zoom != 1.0
            && let Some(p) = pointer
        {
            let at = p - view_rect.min;
            state.zoom_at(
                &mut tab.view,
                &info.page_sizes,
                zoom,
                Point::new(at.x, at.y),
                now,
            );
        }
        if delta != Vec2::ZERO {
            // The delta says how the content moves; the offset moves the other way.
            state.scroll_by(Point::new(-delta.x, -delta.y));
        }
    }
    if (hovered || cx.focused) && !ui.ctx().text_edit_focused() {
        let pressed: Vec<NavKey> = ui.input_mut(|i| {
            KEYS.iter()
                .filter(|(m, k, _)| i.consume_key(*m, *k))
                .map(|(_, _, nav)| *nav)
                .collect()
        });
        for nav in pressed {
            if let Some(layout) = state.layout() {
                let max = layout.max_scroll(viewport);
                let to = key_scroll(nav, state.scroll(), viewport, max);
                state.scroll_to(to);
            }
        }
    }
    scroll_bars(ui, id, outer, view_rect, state);
    state.commit(&mut tab.view);

    let pixels = PixelInfo {
        pixels_per_point: ui.ctx().pixels_per_point(),
        max_texture_side: ui.input(|i| i.max_texture_side),
    };
    for (key, prio) in state.frame_requests(info.generation, pixels, now, |k| {
        cx.textures.contains(tab.doc, k)
    }) {
        doc.request_render(key, prio);
    }
    if let Some(left) = state.zoom_gesture.remaining(now) {
        ui.ctx().request_repaint_after(left);
    }

    paint_pages(
        ui,
        view_rect,
        state,
        tab.doc,
        info.generation,
        pixels,
        cx.textures,
    );
}

fn paint_pages(
    ui: &Ui,
    view_rect: Rect,
    state: &PdfViewUiState,
    doc: DocId,
    generation: u32,
    pixels: PixelInfo,
    textures: &mut TextureCache,
) {
    let painter = ui.painter_at(view_rect);
    painter.rect_filled(view_rect, 0.0, background(ui.visuals()));
    let Some(layout) = state.layout() else {
        return;
    };
    let origin = view_rect.min - to_vec(state.scroll());
    let border = Stroke::new(1.0, Color32::from_black_alpha(90));
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    for page in state.visible_pages() {
        let r = layout.page_rect(page);
        let rect = Rect::from_min_max(origin + vec2(r.x0, r.y0), origin + vec2(r.x1, r.y1));
        let texture = state.render_key(page, generation, pixels).and_then(|key| {
            match textures.pick(doc, key) {
                Pick::Exact(t) | Pick::Stretched(t) => Some(t.id()),
                Pick::Blank => None,
            }
        });
        match texture {
            Some(id) => {
                painter.image(id, rect, uv, Color32::WHITE);
            }
            None => {
                painter.rect_filled(rect, 0.0, Color32::WHITE);
            }
        }
        painter.rect_stroke(rect, 0.0, border, StrokeKind::Outside);
    }
}

/// Draws both scroll bars and applies dragging and clicking on them.
fn scroll_bars(ui: &Ui, id: egui::Id, outer: Rect, view_rect: Rect, state: &mut PdfViewUiState) {
    let Some(layout) = state.layout() else {
        return;
    };
    let content = layout.content_size();
    let viewport = state.viewport();
    let vertical = Rect::from_min_max(
        pos2(view_rect.max.x, outer.min.y),
        pos2(outer.max.x, view_rect.max.y),
    );
    let horizontal = Rect::from_min_max(
        pos2(outer.min.x, view_rect.max.y),
        pos2(view_rect.max.x, outer.max.y),
    );
    let scroll = state.scroll();
    if let Some(y) = scroll_bar(
        ui,
        id.with("vbar"),
        vertical,
        true,
        viewport.h,
        content.h,
        scroll.y,
    ) {
        state.scroll_to(Point::new(scroll.x, y));
    }
    let scroll = state.scroll();
    if let Some(x) = scroll_bar(
        ui,
        id.with("hbar"),
        horizontal,
        false,
        viewport.w,
        content.w,
        scroll.x,
    ) {
        state.scroll_to(Point::new(x, scroll.y));
    }
}

/// One scroll bar. Returns the new scroll offset along its axis when the user moved it.
fn scroll_bar(
    ui: &Ui,
    id: egui::Id,
    track: Rect,
    vertical: bool,
    viewport: f32,
    content: f32,
    scroll: f32,
) -> Option<f32> {
    let along = |p: Pos2| {
        if vertical {
            p.y - track.min.y
        } else {
            p.x - track.min.x
        }
    };
    let track_len = if vertical {
        track.height()
    } else {
        track.width()
    };
    let response = ui.interact(track, id, Sense::click_and_drag());
    let (start, len) = thumb(track_len, viewport, content, scroll);

    let mut result = None;
    if content > viewport {
        if (response.drag_started() || response.clicked())
            && let Some(p) = response.interact_pointer_pos()
        {
            let at = along(p);
            if at < start || at > start + len {
                // Pressed beside the thumb: jump so the thumb is centered there.
                result = Some(scroll_for_thumb(
                    track_len,
                    viewport,
                    content,
                    at - len / 2.0,
                ));
            }
        } else if response.dragged() {
            let d = response.drag_delta();
            let moved = if vertical { d.y } else { d.x };
            result = Some(scroll_for_thumb(
                track_len,
                viewport,
                content,
                start + moved,
            ));
        }
    }

    let visuals = ui.visuals();
    let painter = ui.painter();
    painter.rect_filled(track, 0.0, visuals.extreme_bg_color);
    let (start, len) = match result {
        Some(s) => thumb(track_len, viewport, content, s),
        None => (start, len),
    };
    let thumb_rect = if vertical {
        Rect::from_min_size(track.min + vec2(2.0, start), vec2(track.width() - 4.0, len))
    } else {
        Rect::from_min_size(
            track.min + vec2(start, 2.0),
            vec2(len, track.height() - 4.0),
        )
    };
    let fill = if response.dragged() {
        visuals.widgets.active.bg_fill
    } else if response.hovered() {
        visuals.widgets.hovered.bg_fill
    } else {
        visuals.widgets.inactive.bg_fill
    };
    if content > viewport {
        painter.rect_filled(thumb_rect, 4.0, fill);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const A4: Size = Size { w: 595.0, h: 842.0 };

    fn pixels() -> PixelInfo {
        PixelInfo {
            pixels_per_point: 1.0,
            max_texture_side: 8192,
        }
    }

    fn view() -> ViewState {
        ViewState::new(PathBuf::from("/refdesk-test/a.pdf"))
    }

    fn pages(n: usize) -> Vec<Size> {
        vec![A4; n]
    }

    /// A synced state for `n` A4 pages in an 800x600 viewport at the given zoom.
    fn synced(n: usize, zoom: ZoomMode) -> (PdfViewUiState, ViewState, Vec<Size>) {
        let sizes = pages(n);
        let mut v = view();
        v.zoom = zoom;
        let mut s = PdfViewUiState::default();
        s.sync(&v, &sizes, 0, Size::new(800.0, 600.0));
        s.commit(&mut v);
        (s, v, sizes)
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn plan_requests_visible_then_two_neighbors_each_side() {
        assert_eq!(
            plan_pages(5..7, 100),
            vec![
                (5, Priority::Visible),
                (6, Priority::Visible),
                (7, Priority::Prefetch),
                (8, Priority::Prefetch),
                (4, Priority::Prefetch),
                (3, Priority::Prefetch),
            ]
        );
    }

    #[test]
    fn plan_requests_stay_inside_the_document() {
        assert_eq!(
            plan_pages(0..1, 2),
            vec![(0, Priority::Visible), (1, Priority::Prefetch)]
        );
        assert_eq!(
            plan_pages(9..10, 10),
            vec![
                (9, Priority::Visible),
                (8, Priority::Prefetch),
                (7, Priority::Prefetch)
            ]
        );
        assert!(plan_pages(0..0, 0).is_empty());
        // Only a gap visible: the neighbors are still prefetched.
        assert_eq!(
            plan_pages(3..3, 10)
                .iter()
                .filter(|(_, p)| *p == Priority::Visible)
                .count(),
            0
        );
        assert_eq!(plan_pages(3..3, 10).len(), 4);
    }

    #[test]
    fn frame_requests_skip_pages_with_an_exact_texture() {
        let (s, _, _) = synced(10, ZoomMode::Custom(1.0));
        let all = s.frame_requests(0, pixels(), 10.0, |_| false);
        let visible: Vec<u32> = all
            .iter()
            .filter(|(_, p)| *p == Priority::Visible)
            .map(|(k, _)| k.page)
            .collect();
        assert_eq!(visible, vec![0]);
        assert_eq!(all.len(), 3, "page 0 visible, 1 and 2 prefetched");
        assert!(all.iter().all(|(k, _)| k.scale_milli == 1333));

        let without_first = s.frame_requests(0, pixels(), 10.0, |k| k.page == 0);
        assert!(without_first.iter().all(|(k, _)| k.page != 0));
        assert_eq!(without_first.len(), 2);
    }

    #[test]
    fn render_scale_follows_layout_and_pixel_density_and_texture_limit() {
        let p = pixels();
        assert_eq!(render_scale_milli(1.0, A4, p), 1000);
        let hidpi = PixelInfo {
            pixels_per_point: 2.0,
            ..p
        };
        assert_eq!(render_scale_milli(1.0, A4, hidpi), 2000);
        let small = PixelInfo {
            pixels_per_point: 1.0,
            max_texture_side: 2048,
        };
        let s = render_scale_milli(10.0, A4, small);
        assert!(s < 10_000);
        assert!(
            (A4.h * s as f32 / 1000.0) <= 2048.0,
            "the long side fits: {s}"
        );
    }

    #[test]
    fn sync_puts_the_anchor_at_the_top_left_and_commit_round_trips() {
        let sizes = pages(10);
        let mut v = view();
        v.zoom = ZoomMode::Custom(1.0);
        v.anchor = Location::new(3, 0.0, 100.0);
        let mut s = PdfViewUiState::default();
        s.sync(&v, &sizes, 0, Size::new(800.0, 600.0));
        let layout = s.layout().expect("layout").clone();
        let p = layout.content_pos(v.anchor);
        assert!(close(s.scroll().y, p.y));
        s.commit(&mut v);
        assert_eq!(v.anchor.page, 3);
        assert!(close(v.anchor.y, 100.0));
    }

    #[test]
    fn scrolling_moves_the_anchor() {
        let (mut s, mut v, sizes) = synced(10, ZoomMode::Custom(1.0));
        let before = v.anchor;
        s.sync(&v, &sizes, 0, Size::new(800.0, 600.0));
        s.scroll_by(Point::new(0.0, 2000.0));
        s.commit(&mut v);
        assert_ne!(v.anchor, before);
        let layout = s.layout().expect("layout");
        assert!(close(layout.content_pos(v.anchor).y, s.scroll().y));
        assert!(v.anchor.page >= 1);
    }

    #[test]
    fn the_scroll_offset_is_kept_between_frames() {
        let (mut s, mut v, sizes) = synced(10, ZoomMode::Custom(1.0));
        s.scroll_by(Point::new(0.0, 1234.5));
        s.commit(&mut v);
        let scroll = s.scroll();
        for _ in 0..3 {
            s.sync(&v, &sizes, 0, Size::new(800.0, 600.0));
            s.commit(&mut v);
        }
        assert_eq!(s.scroll(), scroll);
    }

    #[test]
    fn resizing_the_viewport_keeps_the_anchor() {
        let (mut s, mut v, sizes) = synced(10, ZoomMode::FitWidth);
        s.scroll_by(Point::new(0.0, 3000.0));
        s.commit(&mut v);
        let anchor = v.anchor;

        // Fit width: the scale changes with the width, the document point stays at the top.
        s.sync(&v, &sizes, 0, Size::new(500.0, 900.0));
        s.commit(&mut v);
        assert_eq!(v.anchor.page, anchor.page);
        assert!(close(v.anchor.y, anchor.y), "{v:?} vs {anchor:?}");

        // Even the margin above the first page survives.
        let (mut s, mut v, sizes) = synced(10, ZoomMode::FitWidth);
        s.scroll_to(Point::new(0.0, 0.0));
        s.commit(&mut v);
        assert!(
            v.anchor.y < 0.0,
            "the anchor lies in the margin above page 0"
        );
        s.sync(&v, &sizes, 0, Size::new(640.0, 480.0));
        s.commit(&mut v);
        // The margin is kept in points, so it shrinks with the fit scale, but the top of the
        // first page stays visible.
        let top = s.layout().expect("layout").page_rect(0).y0;
        assert!(s.scroll().y < top, "{} < {top}", s.scroll().y);
    }

    #[test]
    fn an_anchor_set_from_outside_moves_the_view() {
        let (mut s, mut v, sizes) = synced(10, ZoomMode::Custom(1.0));
        // What `GoToPage` does.
        v.anchor = Location::new(4, 0.0, 0.0);
        s.sync(&v, &sizes, 0, Size::new(800.0, 600.0));
        s.commit(&mut v);
        assert_eq!(v.anchor.page, 4);
        assert!(close(v.anchor.y, 0.0));
        let layout = s.layout().expect("layout");
        assert!(close(s.scroll().y, layout.page_rect(4).y0));
    }

    #[test]
    fn ctrl_wheel_zoom_keeps_the_point_under_the_pointer() {
        let (mut s, mut v, sizes) = synced(10, ZoomMode::Custom(1.0));
        s.scroll_by(Point::new(0.0, 1500.0));
        s.commit(&mut v);
        let pointer = Point::new(300.0, 250.0);
        let layout = s.layout().expect("layout").clone();
        let under = anchor_at(
            &layout,
            Point::new(s.scroll().x + pointer.x, s.scroll().y + pointer.y),
        );

        assert!(s.zoom_at(&mut v, &sizes, 1.5, pointer, 1.0));
        assert_eq!(v.zoom, ZoomMode::Custom(1.5));
        let layout = s.layout().expect("layout");
        let p = layout.content_pos(under);
        assert!(close(p.x - s.scroll().x, pointer.x), "x");
        assert!(close(p.y - s.scroll().y, pointer.y), "y");

        // The next frame does not move anything: the zoom is the view's own.
        s.commit(&mut v);
        let scroll = s.scroll();
        s.sync(&v, &sizes, 0, Size::new(800.0, 600.0));
        assert_eq!(s.scroll(), scroll);
    }

    #[test]
    fn zoom_is_limited() {
        let (mut s, mut v, sizes) = synced(3, ZoomMode::Custom(MAX_ZOOM));
        assert!(!s.zoom_at(&mut v, &sizes, 2.0, Point::new(10.0, 10.0), 0.0));
        assert_eq!(v.zoom, ZoomMode::Custom(MAX_ZOOM));
        assert!(!s.zoom_at(&mut v, &sizes, 1000.0, Point::new(10.0, 10.0), 0.0));
        assert!(s.zoom_at(&mut v, &sizes, 0.0001, Point::new(10.0, 10.0), 0.0));
        assert_eq!(v.zoom, ZoomMode::Custom(MIN_ZOOM));
    }

    #[test]
    fn no_new_scale_is_requested_while_zooming_and_it_is_after() {
        let (mut s, mut v, sizes) = synced(10, ZoomMode::Custom(1.0));
        assert!(s.zoom_at(&mut v, &sizes, 1.25, Point::new(100.0, 100.0), 5.0));
        s.commit(&mut v);
        assert!(s.frame_requests(0, pixels(), 5.0, |_| false).is_empty());
        assert!(s.frame_requests(0, pixels(), 5.07, |_| false).is_empty());
        assert!(s.zoom_gesture.remaining(5.07).is_some());
        // A further change restarts the wait.
        assert!(s.zoom_at(&mut v, &sizes, 1.1, Point::new(100.0, 100.0), 5.07));
        assert!(s.frame_requests(0, pixels(), 5.12, |_| false).is_empty());
        // Settled: the new scale is requested.
        let after = s.frame_requests(0, pixels(), 5.16, |_| false);
        assert!(!after.is_empty());
        let expected = render_scale_milli(1.375 * PIXELS_PER_POINT_AT_100, A4, pixels());
        assert!(after.iter().all(|(k, _)| k.scale_milli == expected));
        assert_eq!(s.zoom_gesture.remaining(5.16), None);
    }

    #[test]
    fn a_zoom_set_from_outside_keeps_the_middle_of_the_viewport() {
        let (mut s, mut v, sizes) = synced(10, ZoomMode::Custom(1.0));
        let viewport = Size::new(800.0, 600.0);
        s.scroll_by(Point::new(0.0, 2500.0));
        s.commit(&mut v);
        let layout = s.layout().expect("layout").clone();
        let middle = anchor_at(
            &layout,
            Point::new(s.scroll().x + 400.0, s.scroll().y + 300.0),
        );

        // What `SetZoom` does.
        v.zoom = ZoomMode::Custom(2.0);
        s.sync(&v, &sizes, 0, viewport);
        s.commit(&mut v);
        let layout = s.layout().expect("layout");
        let p = layout.content_pos(middle);
        assert!(close(p.y - s.scroll().y, 300.0));
        assert!(close(p.x - s.scroll().x, 400.0));
        assert_eq!(s.effective_zoom.map(|z| (z * 100.0).round()), Some(200.0));
    }

    #[test]
    fn effective_zoom_reports_the_fit_scale() {
        let (s, _, _) = synced(3, ZoomMode::FitWidth);
        let expected = (800.0 - 2.0 * MARGIN) / A4.w / PIXELS_PER_POINT_AT_100;
        assert!(close(s.effective_zoom.expect("zoom"), expected));
    }

    #[test]
    fn current_page_is_the_one_across_the_middle() {
        let (mut s, mut v, sizes) = synced(10, ZoomMode::Custom(1.0));
        assert_eq!(s.current_page, Some(0));
        let layout = s.layout().expect("layout").clone();
        // Page 2's top sits just above the middle of the viewport.
        let y = layout.page_rect(2).y0 - 290.0;
        s.scroll_to(Point::new(0.0, y));
        s.commit(&mut v);
        assert_eq!(s.current_page, Some(2));
        assert_eq!(
            current_page(&layout, Point::new(0.0, y - 20.0), Size::new(800.0, 600.0)),
            1
        );
        let _ = sizes;
    }

    #[test]
    fn keys_move_by_their_steps() {
        let vp = Size::new(800.0, 600.0);
        let max = Point::new(100.0, 9000.0);
        let at = Point::new(10.0, 1000.0);
        assert_eq!(key_scroll(NavKey::PageDown, at, vp, max).y, 1540.0);
        assert_eq!(key_scroll(NavKey::PageUp, at, vp, max).y, 460.0);
        assert_eq!(key_scroll(NavKey::Down, at, vp, max).y, 1048.0);
        assert_eq!(key_scroll(NavKey::Up, at, vp, max).y, 952.0);
        assert_eq!(key_scroll(NavKey::Left, at, vp, max).x, -38.0);
        assert_eq!(key_scroll(NavKey::Right, at, vp, max).x, 58.0);
        assert_eq!(key_scroll(NavKey::Home, at, vp, max), Point::new(10.0, 0.0));
        assert_eq!(
            key_scroll(NavKey::End, at, vp, max),
            Point::new(10.0, 9000.0)
        );
    }

    #[test]
    fn scroll_bar_thumb_geometry() {
        // Content fits: the thumb fills the track.
        assert_eq!(thumb(500.0, 600.0, 400.0, 0.0), (0.0, 500.0));
        // Half of the content visible: half the track.
        assert_eq!(thumb(500.0, 600.0, 1200.0, 0.0), (0.0, 250.0));
        assert_eq!(thumb(500.0, 600.0, 1200.0, 600.0), (250.0, 250.0));
        // A huge document still gets a usable thumb.
        assert_eq!(thumb(500.0, 600.0, 1.0e7, 0.0).1, MIN_THUMB);
        // Inverse.
        assert_eq!(scroll_for_thumb(500.0, 600.0, 1200.0, 125.0), 300.0);
        assert_eq!(scroll_for_thumb(500.0, 600.0, 1200.0, -5.0), 0.0);
        assert_eq!(scroll_for_thumb(500.0, 600.0, 1200.0, 900.0), 600.0);
        assert_eq!(scroll_for_thumb(500.0, 600.0, 400.0, 10.0), 0.0);
    }

    #[test]
    fn fit_page_fits_the_page_chosen_when_the_mode_is_set() {
        let mut sizes = pages(5);
        sizes[3] = Size::new(300.0, 300.0);
        let mut v = view();
        v.zoom = ZoomMode::Custom(1.0);
        v.anchor = Location::new(3, 0.0, 0.0);
        let mut s = PdfViewUiState::default();
        let vp = Size::new(800.0, 600.0);
        s.sync(&v, &sizes, 0, vp);
        s.commit(&mut v);
        v.zoom = ZoomMode::FitPage;
        s.sync(&v, &sizes, 0, vp);
        s.commit(&mut v);
        let fit = s.layout().expect("layout").scale();
        assert!(close(fit, (600.0 - 2.0 * MARGIN) / 300.0));
        // Scrolling to another page does not change the scale.
        s.scroll_to(Point::new(0.0, 0.0));
        s.commit(&mut v);
        s.sync(&v, &sizes, 0, vp);
        assert!(close(s.layout().expect("layout").scale(), fit));
    }

    #[test]
    fn zoom_gesture_timing() {
        let mut g = ZoomGesture::default();
        assert!(!g.in_progress(0.0));
        g.note(1.0);
        assert!(g.in_progress(1.0));
        assert!(g.in_progress(1.079));
        assert!(!g.in_progress(1.081));
    }
}

/// The widget driven frame by frame without a window, on real documents.
#[cfg(test)]
mod widget_tests {
    use super::*;
    use std::time::Instant;

    use crate::action::{OpenTarget, dispatch, dispatch_queued};
    use crate::panels::status_bar;
    use crate::state::{AppState, Tab};
    use crate::views::test_support::{Repaints, TIMEOUT, watch_repaints};
    use crate::views::textures::collect_rendered;
    use refdesk_core::testutil::{PdfBuilder, TempPdf};

    const SCREEN: Vec2 = vec2(800.0, 600.0);

    /// A test app: state, context, and a clock that advances one 60 Hz frame per run.
    struct Harness {
        ctx: egui::Context,
        repaints: Repaints,
        state: AppState,
        time: f64,
    }

    impl Harness {
        fn new() -> Harness {
            let ctx = egui::Context::default();
            let repaints = watch_repaints(&ctx);
            Harness {
                ctx,
                repaints,
                state: AppState::new(),
                time: 0.0,
            }
        }

        fn open(&mut self, path: &std::path::Path) -> TabId {
            let target = OpenTarget {
                path: path.to_path_buf(),
                page: None,
            };
            dispatch(&mut self.state, Action::OpenFiles(vec![target]), &self.ctx);
            self.state.focus.expect("the new tab has the focus")
        }

        /// One frame like the app's: collect renders, status bar, the active tab, dispatch.
        fn frame(&mut self, events: Vec<egui::Event>) {
            self.time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            collect_rendered(&self.ctx, &self.state.docs, &mut self.state.textures);
            let state = &mut self.state;
            let mut queue = Vec::new();
            self.ctx
                .run_ui(input, |ui| {
                    egui::Panel::bottom("status").show(ui, |ui| {
                        status_bar::show(ui, state, &mut queue);
                    });
                    egui::CentralPanel::default().show(ui, |ui| draw_active(ui, state, &mut queue));
                })
                .drop_without_applying_deltas();
            self.state.actions.append(&mut queue);
            dispatch_queued(&mut self.state, &self.ctx);
        }

        /// Runs frames until `check` holds, waiting for a repaint request between frames.
        fn frames_until(&mut self, what: &str, check: impl Fn(&Harness) -> bool) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.frame(Vec::new());
                if check(self) {
                    return;
                }
                self.repaints.wait_one(what, deadline);
            }
        }

        fn tab(&self, tab: TabId) -> &PdfTab {
            self.state.tabs.pdf(tab).expect("pdf tab")
        }

        /// The texture key each visible page of `tab` wants.
        fn wanted_keys(&self, tab: TabId) -> Vec<RenderKey> {
            let pdf = self.tab(tab);
            let generation = self
                .state
                .docs
                .get(pdf.doc)
                .and_then(|d| d.info())
                .map_or(0, |i| i.generation);
            let pixels = PixelInfo {
                pixels_per_point: 1.0,
                max_texture_side: self.ctx.input(|i| i.max_texture_side),
            };
            pdf.ui
                .visible_pages()
                .filter_map(|p| pdf.ui.render_key(p, generation, pixels))
                .collect()
        }

        fn all_visible_rendered(&self, tab: TabId) -> bool {
            let doc = self.tab(tab).doc;
            let keys = self.wanted_keys(tab);
            !keys.is_empty() && keys.iter().all(|k| self.state.textures.contains(doc, *k))
        }

        /// Every visible page has some texture (exact or another scale) to draw.
        fn no_blank_pages(&mut self, tab: TabId) -> bool {
            let doc = self.tab(tab).doc;
            let keys = self.wanted_keys(tab);
            keys.iter()
                .all(|k| !matches!(self.state.textures.pick(doc, *k), Pick::Blank))
        }
    }

    fn draw_active(ui: &mut Ui, state: &mut AppState, queue: &mut Vec<Action>) {
        let Some(active) = state.main_window().dock.focused_tab() else {
            return;
        };
        let AppState {
            docs,
            tabs,
            textures,
            focus,
            ..
        } = state;
        if let Some(Tab::Pdf(tab)) = tabs.get_mut(active)
            && let Some(doc) = docs.get(tab.doc)
        {
            let cx = ViewContext {
                tab_id: active,
                focused: *focus == Some(active),
                textures,
                queue,
            };
            show(ui, tab, doc, cx);
        }
    }

    fn document(pages: usize, stem: &str) -> TempPdf {
        let mut b = PdfBuilder::new();
        for i in 0..pages {
            b = b.page(595.0, 842.0, |p| {
                p.text(72.0, 100.0, 24.0, &format!("Page {}", i + 1));
            });
        }
        b.write_temp(stem)
    }

    fn wheel(delta: Vec2, at: Pos2, modifiers: Modifiers) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta,
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Unknown,
                modifiers,
            },
        ]
    }

    /// A harness with a document of `pages` pages open and its first screen rendered.
    fn rendered(pages: usize, stem: &str) -> (Harness, TempPdf, TabId) {
        let pdf = document(pages, stem);
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the visible pages", |h| h.all_visible_rendered(tab));
        (h, pdf, tab)
    }

    #[test]
    fn an_opened_document_gets_its_visible_pages_as_textures() {
        let (h, _pdf, tab) = rendered(30, "view-render");
        let pdf = h.tab(tab);
        assert_eq!(pdf.ui.visible_pages().start, 0);
        assert!(h.state.textures.bytes() > 0);
        // The status bar shows "1 / 30".
        assert_eq!(pdf.ui.current_page, Some(0));
        let expected = crate::i18n::tr("status.page")
            .replace("{page}", "1")
            .replace("{count}", "30");
        assert_eq!(
            status_bar::page_text(pdf.ui.current_page, 30, &[]),
            expected
        );
        // Ctrl+G is possible now that the document is open.
        let goto = crate::commands::COMMANDS
            .iter()
            .find(|c| c.id == "view.goto_page")
            .expect("command");
        assert_eq!(goto.run(&h.state), Some(Action::ShowGoToPage(tab)));
    }

    #[test]
    fn wheel_scrolling_moves_the_anchor() {
        let (mut h, _pdf, tab) = rendered(30, "view-wheel");
        let before = h.tab(tab).view.anchor;
        h.frame(wheel(
            vec2(0.0, -400.0),
            pos2(400.0, 300.0),
            Modifiers::NONE,
        ));
        // The wheel is smoothed over a few frames.
        for _ in 0..30 {
            h.frame(Vec::new());
        }
        let after = h.tab(tab).view.anchor;
        assert_ne!(after, before);
        let layout = h.tab(tab).ui.layout().expect("layout");
        assert!(layout.content_pos(after).y > layout.content_pos(before).y + 100.0);
    }

    #[test]
    fn keys_scroll_the_focused_view() {
        let (mut h, _pdf, tab) = rendered(30, "view-keys");
        let press = |key| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        h.frame(vec![press(Key::End)]);
        assert_eq!(h.tab(tab).view.anchor.page, 29);
        h.frame(vec![press(Key::Home)]);
        assert_eq!(h.tab(tab).view.anchor.page, 0);
        let y0 = h.tab(tab).ui.scroll().y;
        h.frame(vec![press(Key::PageDown)]);
        assert!(h.tab(tab).ui.scroll().y > y0 + 100.0);
    }

    #[test]
    fn ctrl_wheel_zooms_around_the_pointer_without_blank_pages() {
        let (mut h, _pdf, tab) = rendered(30, "view-zoom");
        // (That the point under the pointer stays is checked exactly by
        // `ctrl_wheel_zoom_keeps_the_point_under_the_pointer`; here the input path is covered.)
        let pointer = pos2(300.0, 200.0);
        let before_zoom = h.tab(tab).ui.effective_zoom.expect("zoom");
        h.frame(wheel(vec2(0.0, 120.0), pointer, Modifiers::CTRL));
        for _ in 0..3 {
            h.frame(Vec::new());
            // While the gesture lasts the old textures are stretched: no white page.
            assert!(h.no_blank_pages(tab));
        }
        let pdf = h.tab(tab);
        assert!(matches!(pdf.view.zoom, ZoomMode::Custom(_)));
        assert!(pdf.ui.effective_zoom.expect("zoom") > before_zoom);
        // After the gesture the new scale arrives.
        h.frames_until("the new scale", |h| h.all_visible_rendered(tab));
    }

    #[test]
    fn a_broken_file_shows_an_error_and_does_not_panic() {
        let dir = std::env::temp_dir().join(format!("refdesk-wp15-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("broken.pdf");
        std::fs::write(&path, b"%PDF-1.7\nthis is not a pdf").expect("write");
        let mut h = Harness::new();
        let tab = h.open(&path);
        h.frames_until("the failure", |h| {
            let doc = h.state.docs.get(h.tab(tab).doc).expect("doc");
            matches!(doc.status(), DocStatus::Failed(_))
        });
        // A few more frames of the error state.
        for _ in 0..3 {
            h.frame(Vec::new());
        }
        assert!(h.tab(tab).ui.layout().is_none());
        assert_eq!(h.state.textures.bytes(), 0);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn go_to_page_puts_that_page_at_the_top() {
        let (mut h, _pdf, tab) = rendered(30, "view-goto");
        h.state.actions.push(Action::GoToPage { tab, page: 9 });
        let ctx = h.ctx.clone();
        dispatch_queued(&mut h.state, &ctx);
        h.frame(Vec::new());
        let pdf = h.tab(tab);
        assert_eq!(pdf.view.anchor.page, 9);
        assert!(pdf.view.anchor.y.abs() < 0.01);
        assert_eq!(pdf.ui.visible_pages().start, 9);
    }

    #[test]
    fn clicking_an_unfocused_view_focuses_its_tab() {
        let (mut h, _pdf, tab) = rendered(3, "view-click");
        h.state.focus = None;
        let at = pos2(400.0, 300.0);
        h.frame(vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ]);
        h.frame(vec![egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        }]);
        assert_eq!(h.state.focus, Some(tab));
    }
}
