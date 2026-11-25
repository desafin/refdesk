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

use egui::{
    Color32, CursorIcon, Key, Modifiers, PointerButton, Pos2, Rect, Sense, Stroke, StrokeKind, Ui,
    Vec2, pos2, vec2,
};
use refdesk_core::layout::{MAX_ZOOM, MIN_ZOOM, PIXELS_PER_POINT_AT_100};
use refdesk_core::{
    DocStatus, DocumentHandle, DocumentInfo, LayoutParams, Location, PageLayout, Point, Priority,
    RenderKey, Size, ViewState, ZoomMode,
};

use crate::action::Action;
use crate::commands;
use crate::i18n::tr;
use crate::panels::status_bar::GotoInput;
use crate::state::{DocId, PdfTab, TabId};
use crate::views::selection::{self, Released, SelectionState};
use crate::views::textures::{Pick, TextureCache};

/// Vertical space between pages, in logical pixels.
pub const PAGE_GAP: f32 = 8.0;
/// Space around the document, in logical pixels.
pub const MARGIN: f32 = 12.0;
/// Pages before and after the visible ones that are rendered sharp ahead of time while the view
/// stands still.
pub const PREFETCH_STILL: u32 = 6;
/// While scrolling: pages rendered ahead in the direction of the scroll...
pub const PREFETCH_AHEAD: u32 = 10;
/// ...and behind it.
pub const PREFETCH_BEHIND: u32 = 3;
/// A zoom gesture counts as finished this long after its last change.
pub const ZOOM_SETTLE_SECS: f64 = 0.08;
/// The view counts as scrolling until this long after the scroll offset last changed.
pub const SCROLL_SETTLE_SECS: f64 = 0.3;
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

/// Which way the view is moving, for choosing the pages rendered ahead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollMotion {
    /// Not scrolling.
    #[default]
    Still,
    /// Scrolling towards the end of the document.
    Down,
    /// Scrolling towards the start of the document.
    Up,
}

/// Tracks vertical scrolling from one frame to the next.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollTracker {
    last_y: Option<f32>,
    direction: ScrollMotion,
    last_change: Option<f64>,
}

impl ScrollTracker {
    /// Records the vertical scroll offset of a frame at time `now` (seconds).
    pub fn note(&mut self, y: f32, now: f64) {
        if let Some(last) = self.last_y
            && y != last
        {
            self.direction = if y > last {
                ScrollMotion::Down
            } else {
                ScrollMotion::Up
            };
            self.last_change = Some(now);
        }
        self.last_y = Some(y);
    }

    /// The motion at time `now`: the last direction within [`SCROLL_SETTLE_SECS`] of the last
    /// change, else still.
    pub fn motion(&self, now: f64) -> ScrollMotion {
        match self.remaining(now) {
            Some(_) => self.direction,
            None => ScrollMotion::Still,
        }
    }

    /// Time until the view counts as still; `None` when it already does.
    pub fn remaining(&self, now: f64) -> Option<Duration> {
        let left = SCROLL_SETTLE_SECS - (now - self.last_change?);
        (left > 0.0).then(|| Duration::from_secs_f64(left))
    }
}

/// The password field shown for an encrypted document.
#[derive(Clone, Debug, Default)]
pub struct PasswordInput {
    /// What has been typed.
    pub text: String,
    /// A password was submitted at least once.
    pub tried: bool,
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
    /// Recent scrolling, for the pages rendered ahead. Boxed to keep tabs small.
    pub scroll_motion: Box<ScrollTracker>,
    /// The page across the vertical middle of the viewport, as of the last frame.
    pub current_page: Option<u32>,
    /// The actual zoom factor (1.0 = 100%) as of the last frame; it differs from the zoom mode
    /// for the fit modes.
    pub effective_zoom: Option<f32>,
    /// The password field of an encrypted document. Boxed: a `Tab` must stay small.
    pub password: Box<PasswordInput>,
    /// The page number field of the status bar, while it is open.
    pub goto: Option<GotoInput>,
    /// The selected text and the press that is changing it (SEL-1). Cleared when the document is
    /// reloaded. Boxed to keep tabs small.
    pub selection: Box<SelectionState>,
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

    /// Whether some text is selected.
    pub fn has_selection(&self) -> bool {
        self.selection.has_selection()
    }

    /// The viewport size of the last laid-out frame; zero before the first one.
    pub fn viewport(&self) -> Size {
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

    /// The renders to request this frame, most important first: visible pages, then the
    /// neighbors (see [`plan_pages`]), skipping pages whose exact texture is there. Nothing while
    /// a zoom gesture lasts.
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
        let keys: Vec<(RenderKey, Priority)> = plan_pages(
            self.visible_pages(),
            g.layout.page_count(),
            self.scroll_motion.motion(now),
        )
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

/// Which pages to request, most important first: the visible ones at `Visible`, then their
/// neighbors at `Prefetch`, nearest first (on a tie the one ahead). Standing still that is
/// [`PREFETCH_STILL`] pages on each side; scrolling, [`PREFETCH_AHEAD`] in the direction of the
/// scroll and [`PREFETCH_BEHIND`] the other way. Never past the ends of the document.
pub fn plan_pages(
    visible: Range<u32>,
    page_count: u32,
    motion: ScrollMotion,
) -> Vec<(u32, Priority)> {
    let start = visible.start.min(page_count);
    let end = visible.end.min(page_count).max(start);
    let mut out: Vec<(u32, Priority)> = (start..end).map(|p| (p, Priority::Visible)).collect();
    let (after_n, before_n) = match motion {
        ScrollMotion::Still => (PREFETCH_STILL, PREFETCH_STILL),
        ScrollMotion::Down => (PREFETCH_AHEAD, PREFETCH_BEHIND),
        ScrollMotion::Up => (PREFETCH_BEHIND, PREFETCH_AHEAD),
    };
    let after = |d: u32| {
        (d <= after_n)
            .then(|| end + d - 1)
            .filter(|p| *p < page_count)
    };
    let before = |d: u32| (d <= before_n).then(|| start.checked_sub(d)).flatten();
    for d in 1..=after_n.max(before_n) {
        let pair = match motion {
            ScrollMotion::Up => [before(d), after(d)],
            _ => [after(d), before(d)],
        };
        out.extend(pair.into_iter().flatten().map(|p| (p, Priority::Prefetch)));
    }
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
            egui::TextEdit::singleline(&mut tab.ui.password.text)
                .password(true)
                .desired_width(240.0),
        );
        let entered = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if (ui.button(tr("doc.password_submit")).clicked() || entered)
            && !tab.ui.password.text.is_empty()
        {
            doc.authenticate(&tab.ui.password.text);
            tab.ui.password.text.clear();
            tab.ui.password.tried = true;
        }
        if tab.ui.password.tried {
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
    let response = ui.interact(view_rect, id, Sense::click_and_drag());
    if response.clicked() && !cx.focused {
        cx.queue.push(Action::FocusTab(cx.tab_id));
    }

    let viewport = Size::new(view_rect.width(), view_rect.height());
    let state = &mut tab.ui;
    state.sync(&tab.view, &info.page_sizes, info.generation, viewport);
    state.selection.sync_generation(info.generation);

    let now = ui.input(|i| i.time);
    let texts = |page: u32| doc.page_text(page);
    if handle_pointer(ui, &response, view_rect, state, &texts, now) && !cx.focused {
        cx.queue.push(Action::FocusTab(cx.tab_id));
    }
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
    autoscroll_and_follow(ui, view_rect, state, &texts);
    scroll_bars(ui, id, outer, view_rect, state);
    state.scroll_motion.note(state.scroll().y, now);
    state.commit(&mut tab.view);

    let pixels = PixelInfo {
        pixels_per_point: ui.ctx().pixels_per_point(),
        max_texture_side: ui.input(|i| i.max_texture_side),
    };
    let requests = state.frame_requests(info.generation, pixels, now, |k| {
        cx.textures.contains(tab.doc, k)
    });
    // Within one priority the engine serves the most recent request first: the most important
    // page is requested last.
    for (key, prio) in requests.into_iter().rev() {
        doc.request_render(key, prio);
    }
    // Run a frame when the zoom gesture or the scrolling counts as finished, so the requests
    // for that state go out.
    for left in [
        state.zoom_gesture.remaining(now),
        state.scroll_motion.remaining(now),
    ]
    .into_iter()
    .flatten()
    {
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
    paint_selection(ui, view_rect, state, &texts);
    show_copy_notice(ui, view_rect, state, now);

    // Right click: the menu of the selection, built from the command module's table.
    if commands::SELECTION_MENU.iter().any(|e| (e.enabled)(tab)) {
        response.context_menu(|ui| {
            for entry in commands::SELECTION_MENU {
                let mut button = egui::Button::new(tr(entry.name_key));
                if let Some(keys) = entry
                    .command_id
                    .and_then(|id| commands::shortcut_text(ui.ctx(), id))
                {
                    button = button.shortcut_text(keys);
                }
                if ui.add_enabled((entry.enabled)(tab), button).clicked() {
                    cx.queue.push((entry.action)(cx.tab_id));
                    ui.close();
                }
            }
        });
    }
}

/// One primary-button event, in the order it arrived.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PointerEvent {
    Press { pos: Pos2, shift: bool },
    Move(Pos2),
    Release(Pos2),
}

fn pointer_event(event: &egui::Event) -> Option<PointerEvent> {
    match event {
        egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers,
        } => Some(PointerEvent::Press {
            pos: *pos,
            shift: modifiers.shift,
        }),
        egui::Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: false,
            ..
        } => Some(PointerEvent::Release(*pos)),
        egui::Event::PointerMoved(pos) => Some(PointerEvent::Move(*pos)),
        _ => None,
    }
}

/// The document position under a position relative to the top-left corner of the view.
fn hit_at(state: &PdfViewUiState, viewport_pos: Point) -> Option<selection::Hit> {
    selection::locate(state.layout()?, state.scroll(), viewport_pos)
}

fn viewport_pos(view_rect: Rect, pos: Pos2) -> Point {
    let v = pos - view_rect.min;
    Point::new(v.x, v.y)
}

/// Applies this frame's primary button events to the selection (SEL-1) and sets the cursor.
/// Returns true when the button went down in the view.
///
/// The decision between a click and a drag is made by `SelectionState`: [`Released::Click`] is a
/// single press and release without movement, anything else is a selection gesture.
fn handle_pointer(
    ui: &Ui,
    response: &egui::Response,
    view_rect: Rect,
    state: &mut PdfViewUiState,
    texts: selection::PageTexts,
    now: f64,
) -> bool {
    let events: Vec<PointerEvent> =
        ui.input(|i| i.events.iter().filter_map(pointer_event).collect());
    let hovered = response.hovered();
    let mut pressed_here = false;
    for event in events {
        match event {
            PointerEvent::Press { pos, shift } => {
                if !(hovered && view_rect.contains(pos)) {
                    continue;
                }
                pressed_here = true;
                let vp = viewport_pos(view_rect, pos);
                if let Some(hit) = hit_at(state, vp) {
                    state.selection.press(texts, vp, hit, now, shift);
                }
            }
            PointerEvent::Move(pos) => {
                if state.selection.is_dragging() {
                    let vp = viewport_pos(view_rect, pos);
                    if let Some(hit) = hit_at(state, vp) {
                        state.selection.drag_to(texts, vp, hit);
                    }
                }
            }
            PointerEvent::Release(pos) => {
                if !state.selection.is_dragging() {
                    continue;
                }
                let vp = viewport_pos(view_rect, pos);
                if let Some(hit) = hit_at(state, vp) {
                    state.selection.drag_to(texts, vp, hit);
                }
                match state.selection.release() {
                    Some(Released::Click { .. }) => {
                        // LINK HOOK: a click that did not drag. `hit.loc` is the page-space
                        // position; following a link under it belongs here.
                    }
                    Some(Released::Selected) | None => {}
                }
            }
        }
    }

    // The I-beam over text, and while selecting.
    let over_text = hovered
        && ui
            .input(|i| i.pointer.hover_pos())
            .and_then(|p| hit_at(state, viewport_pos(view_rect, p)))
            .is_some_and(|hit| selection::over_text(texts, &hit));
    if over_text || state.selection.is_dragging() {
        ui.ctx().set_cursor_icon(CursorIcon::Text);
    }
    pressed_here
}

/// While a selection is being dragged: scrolls the view when the pointer is at its top or bottom
/// edge, and moves the end of the selection to the pointer (which points at other text after a
/// scroll even if it did not move).
fn autoscroll_and_follow(
    ui: &Ui,
    view_rect: Rect,
    state: &mut PdfViewUiState,
    texts: selection::PageTexts,
) {
    if !state.selection.is_dragging() {
        return;
    }
    let (down, latest, dt) = ui.input(|i| {
        (
            i.pointer.primary_down(),
            i.pointer.latest_pos(),
            i.stable_dt,
        )
    });
    if !down {
        // The release happened where this view saw no event.
        let _ = state.selection.release();
        return;
    }
    let pos = latest
        .map(|p| viewport_pos(view_rect, p))
        .or_else(|| state.selection.pointer());
    let Some(pos) = pos else {
        state.selection.frame(texts);
        return;
    };
    if state.selection.is_selecting() {
        let speed = selection::autoscroll_speed(pos.y, view_rect.height());
        if speed != 0.0 {
            // A long frame must not throw the view far.
            state.scroll_by(Point::new(0.0, speed * dt.min(0.05)));
            ui.ctx().request_repaint();
        }
    }
    match hit_at(state, pos) {
        Some(hit) => state.selection.drag_to(texts, pos, hit),
        None => state.selection.frame(texts),
    }
}

/// Tints the selected lines of the visible pages with the theme's selection color.
fn paint_selection(ui: &Ui, view_rect: Rect, state: &PdfViewUiState, texts: selection::PageTexts) {
    let (Some(sel), Some(layout)) = (state.selection.active(), state.layout()) else {
        return;
    };
    let base = ui.visuals().selection.bg_fill;
    // Translucent, so the text stays readable.
    let color = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), 110);
    let painter = ui.painter_at(view_rect);
    for h in selection::highlight_rects(&sel, layout, state.scroll(), state.viewport(), texts) {
        let rect = Rect::from_min_max(
            view_rect.min + vec2(h.rect.x0, h.rect.y0),
            view_rect.min + vec2(h.rect.x1, h.rect.y1),
        );
        painter.rect_filled(rect, 0.0, color);
    }
}

/// Copies the selected text of a view to the clipboard and announces how much was copied. Does
/// nothing without a selection. A selection that spans pages whose text is not there yet is
/// copied by a background thread once the text has been extracted (R-1).
pub fn copy_selection(ctx: &egui::Context, doc: &DocumentHandle, state: &mut PdfViewUiState) {
    let Some(sel) = state.selection.active() else {
        return;
    };
    let inbox = state.selection.copy_inbox();
    let ctx = ctx.clone();
    selection::start_copy(doc, sel, move |text| {
        let chars = text.chars().count();
        if chars > 0 {
            ctx.copy_text(text);
            inbox.post(chars);
        }
        ctx.request_repaint();
    });
}

/// Shows the "copied" notice for a moment at the bottom of the view.
fn show_copy_notice(ui: &Ui, view_rect: Rect, state: &mut PdfViewUiState, now: f64) {
    let Some((chars, left)) = state.selection.copy_notice(now) else {
        return;
    };
    ui.ctx().request_repaint_after(left);
    let painter = ui.painter_at(view_rect);
    let text = tr("sel.copied").replace("{}", &chars.to_string());
    let galley = painter.layout_no_wrap(text, egui::FontId::proportional(14.0), Color32::WHITE);
    let pad = vec2(12.0, 6.0);
    let size = galley.size() + pad * 2.0;
    let rect = Rect::from_min_size(
        pos2(
            view_rect.center().x - size.x / 2.0,
            view_rect.max.y - size.y - 16.0,
        ),
        size,
    );
    painter.rect_filled(rect, 6.0, Color32::from_black_alpha(210));
    painter.galley(rect.min + pad, galley, Color32::WHITE);
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

    fn prefetched(plan: &[(u32, Priority)]) -> Vec<u32> {
        plan.iter()
            .filter(|(_, p)| *p == Priority::Prefetch)
            .map(|(page, _)| *page)
            .collect()
    }

    fn visible(plan: &[(u32, Priority)]) -> Vec<u32> {
        plan.iter()
            .filter(|(_, p)| *p == Priority::Visible)
            .map(|(page, _)| *page)
            .collect()
    }

    #[test]
    fn standing_still_prefetches_six_pages_each_side_nearest_first() {
        let plan = plan_pages(50..52, 100, ScrollMotion::Still);
        assert_eq!(visible(&plan), vec![50, 51]);
        assert_eq!(
            prefetched(&plan),
            vec![52, 49, 53, 48, 54, 47, 55, 46, 56, 45, 57, 44]
        );
        // Visible pages come first in the list.
        assert!(plan[..2].iter().all(|(_, p)| *p == Priority::Visible));
    }

    #[test]
    fn scrolling_down_prefetches_ten_ahead_and_three_behind() {
        let plan = plan_pages(50..52, 100, ScrollMotion::Down);
        assert_eq!(visible(&plan), vec![50, 51]);
        assert_eq!(
            prefetched(&plan),
            vec![52, 49, 53, 48, 54, 47, 55, 56, 57, 58, 59, 60, 61]
        );
    }

    #[test]
    fn scrolling_up_prefetches_ten_above_and_three_below() {
        let plan = plan_pages(50..52, 100, ScrollMotion::Up);
        assert_eq!(
            prefetched(&plan),
            vec![49, 52, 48, 53, 47, 54, 46, 45, 44, 43, 42, 41, 40]
        );
    }

    #[test]
    fn plan_requests_stay_inside_the_document() {
        assert_eq!(
            plan_pages(0..1, 3, ScrollMotion::Still),
            vec![
                (0, Priority::Visible),
                (1, Priority::Prefetch),
                (2, Priority::Prefetch)
            ]
        );
        let at_end = plan_pages(9..10, 10, ScrollMotion::Down);
        assert_eq!(visible(&at_end), vec![9]);
        assert_eq!(prefetched(&at_end), vec![8, 7, 6]);
        let at_start = plan_pages(0..2, 100, ScrollMotion::Up);
        assert_eq!(prefetched(&at_start), vec![2, 3, 4]);
        assert!(plan_pages(0..0, 0, ScrollMotion::Still).is_empty());
        // Only a gap visible: the neighbors are still prefetched.
        let gap = plan_pages(3..3, 10, ScrollMotion::Still);
        assert!(visible(&gap).is_empty());
        assert_eq!(prefetched(&gap), vec![3, 2, 4, 1, 5, 0, 6, 7, 8]);
        // A visible range past the end is cut to the document.
        assert_eq!(
            visible(&plan_pages(8..20, 10, ScrollMotion::Still)),
            vec![8, 9]
        );
    }

    #[test]
    fn scroll_tracker_reports_the_direction_until_scrolling_settles() {
        let mut t = ScrollTracker::default();
        t.note(100.0, 1.0);
        assert_eq!(
            t.motion(1.0),
            ScrollMotion::Still,
            "a first value is no motion"
        );
        t.note(150.0, 1.1);
        assert_eq!(t.motion(1.1), ScrollMotion::Down);
        assert_eq!(t.motion(1.39), ScrollMotion::Down);
        assert!(t.remaining(1.39).is_some());
        // Standing still for the settle time.
        t.note(150.0, 1.2);
        assert_eq!(t.motion(1.41), ScrollMotion::Still);
        assert_eq!(t.remaining(1.41), None);
        t.note(120.0, 2.0);
        assert_eq!(t.motion(2.0), ScrollMotion::Up);
    }

    #[test]
    fn frame_requests_skip_pages_with_an_exact_texture() {
        let (s, _, _) = synced(10, ZoomMode::Custom(1.0));
        let all = s.frame_requests(0, pixels(), 10.0, |_| false);
        let pages: Vec<(u32, Priority)> = all.iter().map(|(k, p)| (k.page, *p)).collect();
        assert_eq!(visible(&pages), vec![0]);
        assert_eq!(prefetched(&pages), vec![1, 2, 3, 4, 5, 6]);
        assert!(all.iter().all(|(k, _)| k.scale_milli == 1333));

        let without_first = s.frame_requests(0, pixels(), 10.0, |k| k.page == 0);
        assert!(without_first.iter().all(|(k, _)| k.page != 0));
        assert_eq!(without_first.len(), 6);
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
    use crate::views::preload;
    use crate::views::test_support::{Repaints, TIMEOUT, watch_repaints};
    use crate::views::textures::collect_rendered;
    use refdesk_core::testutil::{PdfBuilder, TempPdf};

    const SCREEN: Vec2 = vec2(800.0, 600.0);
    /// Where the view starts on screen: the central panel has no frame and nothing above it.
    const VIEW_ORIGIN: Pos2 = Pos2::ZERO;

    /// A test app: state, context, and a clock that advances one 60 Hz frame per run.
    struct Harness {
        ctx: egui::Context,
        repaints: Repaints,
        state: AppState,
        time: f64,
        /// What the frames asked the platform to copy to the clipboard, oldest first.
        copied: Vec<String>,
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
                copied: Vec::new(),
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
            preload::step(&self.ctx, &mut self.state);
            let state = &mut self.state;
            let mut queue = Vec::new();
            let output = self.ctx.run_ui(input, |ui| {
                queue.extend(crate::commands::handle_shortcuts(ui.ctx(), state));
                egui::Panel::bottom("status").show(ui, |ui| {
                    status_bar::show(ui, state, state.focus, &mut queue);
                });
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| draw_active(ui, state, &mut queue));
            });
            for command in &output.platform_output.commands {
                if let egui::OutputCommand::CopyText(text) = command {
                    self.copied.push(text.clone());
                }
            }
            output.drop_without_applying_deltas();
            self.state.actions.append(&mut queue);
            dispatch_queued(&mut self.state, &self.ctx);
        }

        /// Runs frames until `check` holds, waiting for a repaint request between frames.
        fn frames_until(&mut self, what: &str, mut check: impl FnMut(&Harness) -> bool) {
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

        /// Whether every page of the document of `tab` has its preview texture.
        fn all_previews(&self, tab: TabId) -> bool {
            let doc = self.tab(tab).doc;
            let count = self
                .state
                .docs
                .get(doc)
                .and_then(|d| d.info())
                .map_or(0, |i| i.page_sizes.len());
            count > 0 && self.state.textures.preview_count(doc) == count
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
    fn every_page_gets_a_preview_without_scrolling_and_the_last_page_is_never_blank() {
        let pdf = document(40, "view-preload");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("all 40 previews", |h| h.all_previews(tab));
        let doc = h.tab(tab).doc;
        assert_eq!(h.tab(tab).view.anchor.page, 0, "nothing scrolled");
        assert_eq!(h.state.textures.preview_count(doc), 40);
        assert!(h.state.textures.preview_bytes() > 0);
        // Finished: the status bar has nothing to show.
        assert_eq!(h.state.preload.progress(doc), None);

        // Jump to the end: in that very first frame the page already has a texture.
        let press = egui::Event::Key {
            key: Key::End,
            physical_key: Some(Key::End),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        h.frame(vec![press]);
        assert_eq!(h.tab(tab).ui.visible_pages().end, 40);
        assert!(h.no_blank_pages(tab));
        assert!(!h.wanted_keys(tab).is_empty());
    }

    #[test]
    fn previews_fill_for_a_document_that_is_not_shown_and_go_when_it_closes() {
        let hidden_pdf = document(12, "view-preload-hidden");
        let shown_pdf = document(5, "view-preload-shown");
        let mut h = Harness::new();
        let hidden = h.open(hidden_pdf.path());
        let shown = h.open(shown_pdf.path());
        assert_eq!(h.state.main_window().dock.focused_tab(), Some(shown));
        h.frames_until("the hidden document's previews", |h| {
            h.all_previews(hidden) && h.all_previews(shown)
        });
        let hidden_doc = h.tab(hidden).doc;
        assert_eq!(h.state.textures.preview_count(hidden_doc), 12);
        // The hidden tab was never drawn.
        assert!(h.tab(hidden).ui.layout().is_none());

        let before = h.state.textures.preview_bytes();
        h.state.actions.push(Action::CloseTab(hidden));
        let ctx = h.ctx.clone();
        dispatch_queued(&mut h.state, &ctx);
        h.frame(Vec::new());
        assert_eq!(h.state.textures.preview_count(hidden_doc), 0);
        assert!(h.state.textures.preview_bytes() < before);
        assert_eq!(h.state.preload.progress(hidden_doc), None);
        let shown_doc = h.tab(shown).doc;
        assert_eq!(h.state.textures.preview_count(shown_doc), 5);
    }

    #[test]
    fn pages_that_come_into_view_while_zooming_out_are_not_blank() {
        // Small pages, so that zooming out brings many of them into view.
        let mut b = PdfBuilder::new();
        for i in 0..40 {
            b = b.page(100.0, 140.0, |p| {
                p.text(10.0, 30.0, 12.0, &format!("P{}", i + 1));
            });
        }
        let pdf = b.write_temp("view-preload-zoom-out");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("all previews", |h| h.all_previews(tab));
        let before = h.tab(tab).ui.visible_pages();
        // Zoom out in one gesture: many more pages become visible, none rendered sharp yet.
        for _ in 0..10 {
            h.frame(wheel(
                vec2(0.0, -150.0),
                pos2(400.0, 300.0),
                Modifiers::CTRL,
            ));
            assert!(h.no_blank_pages(tab));
            if h.tab(tab).ui.visible_pages().end > 1 + PREFETCH_STILL {
                break;
            }
        }
        // Still within the gesture: no sharp render at the new scale was asked for yet.
        assert!(h.tab(tab).ui.zoom_gesture.in_progress(h.time));
        let after = h.tab(tab).ui.visible_pages();
        // Past the sharp pages rendered ahead around the first screen (0..=6): only previews
        // can cover these.
        assert!(after.end > 1 + PREFETCH_STILL, "{after:?}");
        assert!(
            after.len() > before.len() + 2,
            "{before:?} -> {after:?}: more pages are visible"
        );
    }

    #[test]
    fn preload_progress_is_reported_while_it_runs() {
        let pdf = document(30, "view-preload-progress");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the document", |h| h.tab(tab).ui.layout().is_some());
        let doc = h.tab(tab).doc;
        // Progress only counts up and ends at 30 / 30, then disappears. It is seen at least
        // once: 30 previews take several frames to upload.
        let mut last = 0;
        let mut seen = false;
        h.frames_until("the preload", |h| match h.state.preload.progress(doc) {
            Some(p) => {
                assert_eq!(p.total, 30);
                assert!(p.done >= last && p.done < 30);
                last = p.done;
                seen = true;
                false
            }
            None => h.all_previews(tab),
        });
        assert!(seen, "the progress was reported");
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

    // ---- selection (WP-52) ----

    /// Four pages, each with three lines of one paragraph and a hyphenated word.
    fn text_document(stem: &str) -> TempPdf {
        let mut b = PdfBuilder::new();
        for n in 1..=4 {
            b = b.page(595.0, 842.0, |p| {
                p.text(72.0, 100.0, 12.0, &format!("Alpha beta gamma page{n}"));
                p.text(72.0, 116.0, 12.0, "delta epsilon speci-");
                p.text(72.0, 132.0, 12.0, "fication zeta");
            });
        }
        b.write_temp(stem)
    }

    fn pointer_button(pos: Pos2, pressed: bool, button: egui::PointerButton) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    impl Harness {
        /// The text of a page, waiting for the engine (a test may block).
        fn page_text(&self, tab: TabId, page: u32) -> std::sync::Arc<refdesk_core::PageText> {
            let doc = self.state.docs.get(self.tab(tab).doc).expect("doc");
            doc.page_text_wait(page).expect("page text")
        }

        /// The screen position of a page-space point in the view of `tab` as drawn last.
        fn screen_of(&self, tab: TabId, page: u32, x: f32, y: f32) -> Pos2 {
            let pdf = self.tab(tab);
            let layout = pdf.ui.layout().expect("layout");
            let origin = VIEW_ORIGIN;
            let r = layout.page_rect(page);
            let s = layout.scale();
            let scroll = pdf.ui.scroll();
            origin + vec2(r.x0 + x * s - scroll.x, r.y0 + y * s - scroll.y)
        }

        /// Screen position just inside the left or right edge of character `i`.
        fn char_pos(&self, tab: TabId, page: u32, i: u32, right: bool) -> Pos2 {
            let b = self.page_text(tab, page).chars[i as usize].quad.bounds();
            let x = if right { b.x1 - 0.3 } else { b.x0 + 0.3 };
            self.screen_of(tab, page, x, (b.y0 + b.y1) / 2.0)
        }

        fn selected_text(&self, tab: TabId) -> Option<String> {
            let sel = self.tab(tab).ui.selection.active()?;
            let doc = self.state.docs.get(self.tab(tab).doc)?;
            Some(selection::selection_string(&sel, &|p| doc.page_text(p)))
        }
    }

    /// Presses at `from`, moves to `to` over a frame, waits until the selection exists, releases.
    fn drag_select(h: &mut Harness, from: Pos2, to: Pos2) {
        h.frame(vec![
            egui::Event::PointerMoved(from),
            pointer_button(from, true, egui::PointerButton::Primary),
        ]);
        h.frame(vec![egui::Event::PointerMoved(to)]);
        h.frames_until("the selection", |h| {
            h.state
                .focused_pdf()
                .is_some_and(|(_, p)| p.ui.has_selection())
        });
        h.frame(vec![pointer_button(
            to,
            false,
            egui::PointerButton::Primary,
        )]);
    }

    #[test]
    fn dragging_in_the_view_selects_text_across_pages_and_draws_without_panicking() {
        let pdf = text_document("view-select");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the first screen", |h| h.all_visible_rendered(tab));
        let beta = 6;
        let from = h.char_pos(tab, 0, beta, false);
        let to = h.char_pos(tab, 0, beta + 3, true);
        drag_select(&mut h, from, to);
        assert_eq!(h.selected_text(tab).as_deref(), Some("beta"));
        // More frames with the selection on screen.
        for _ in 0..3 {
            h.frame(Vec::new());
        }
        assert!(h.tab(tab).ui.selection.has_selection());
        assert!(!h.tab(tab).ui.selection.is_dragging());
    }

    #[test]
    fn a_drag_that_starts_before_the_text_has_arrived_continues_when_it_does() {
        let pdf = text_document("view-select-pending");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the first screen", |h| h.all_visible_rendered(tab));
        // Nothing asked for the text yet, so the press finds none.
        let (from, to) = (
            h.screen_of(tab, 0, 73.0, 95.0),
            h.screen_of(tab, 0, 200.0, 111.0),
        );
        h.frame(vec![
            egui::Event::PointerMoved(from),
            pointer_button(from, true, egui::PointerButton::Primary),
        ]);
        h.frame(vec![egui::Event::PointerMoved(to)]);
        h.frames_until("the selection", |h| h.tab(tab).ui.has_selection());
        h.frame(vec![pointer_button(
            to,
            false,
            egui::PointerButton::Primary,
        )]);
        let text = h.selected_text(tab).expect("selection");
        assert!(text.starts_with("Alpha"), "{text}");
        assert!(text.contains("delta"), "{text}");
    }

    #[test]
    fn a_click_without_a_drag_clears_the_selection_and_a_double_click_selects_a_word() {
        let pdf = text_document("view-select-click");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the first screen", |h| h.all_visible_rendered(tab));
        let (from, to) = (h.char_pos(tab, 0, 6, false), h.char_pos(tab, 0, 9, true));
        drag_select(&mut h, from, to);
        assert!(h.tab(tab).ui.has_selection());

        let at = h.char_pos(tab, 0, 14, false); // inside "gamma"
        let click = |h: &mut Harness| {
            h.frame(vec![
                egui::Event::PointerMoved(at),
                pointer_button(at, true, egui::PointerButton::Primary),
            ]);
            h.frame(vec![pointer_button(
                at,
                false,
                egui::PointerButton::Primary,
            )]);
        };
        click(&mut h);
        assert!(!h.tab(tab).ui.has_selection(), "a plain click clears");
        click(&mut h);
        assert_eq!(h.selected_text(tab).as_deref(), Some("gamma"));
    }

    #[test]
    fn ctrl_c_copies_the_selected_text_and_announces_it() {
        let pdf = text_document("view-select-copy");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the first screen", |h| h.all_visible_rendered(tab));
        // From "epsilon" through the broken word of the next line.
        let text = h.page_text(tab, 0);
        let all: String = text.chars.iter().map(|c| c.ch).collect();
        let epsilon = all.find("epsilon").expect("epsilon") as u32;
        let zeta = all.find("zeta").expect("zeta") as u32;
        let (from, to) = (
            h.char_pos(tab, 0, epsilon, false),
            h.char_pos(tab, 0, zeta + 3, true),
        );
        drag_select(&mut h, from, to);
        assert!(h.copied.is_empty());

        // egui-winit delivers Ctrl+C as `Event::Copy`.
        h.frame(vec![egui::Event::Copy]);
        h.frame(Vec::new());
        assert_eq!(h.copied, vec!["epsilon specification zeta".to_owned()]);
        // The notice is up, with the character count.
        assert_eq!(
            h.tab(tab).ui.selection.notice_chars(),
            Some("epsilon specification zeta".chars().count())
        );
    }

    #[test]
    fn the_copy_action_without_a_selection_copies_nothing() {
        let (mut h, _pdf, tab) = rendered(3, "view-copy-none");
        h.state.actions.push(Action::CopySelection(tab));
        let ctx = h.ctx.clone();
        dispatch_queued(&mut h.state, &ctx);
        h.frame(Vec::new());
        assert!(h.copied.is_empty());
        assert!(h.tab(tab).ui.selection.notice_chars().is_none());
    }

    #[test]
    fn a_right_click_with_a_selection_opens_the_menu_and_without_one_does_not() {
        let pdf = text_document("view-select-menu");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the first screen", |h| h.all_visible_rendered(tab));
        let at = h.char_pos(tab, 0, 14, false);
        let right_click = |h: &mut Harness| {
            h.frame(vec![
                egui::Event::PointerMoved(at),
                pointer_button(at, true, egui::PointerButton::Secondary),
            ]);
            h.frame(vec![pointer_button(
                at,
                false,
                egui::PointerButton::Secondary,
            )]);
            h.frame(Vec::new());
        };
        right_click(&mut h);
        assert!(
            !egui::Popup::is_any_open(&h.ctx),
            "nothing selected, no menu"
        );

        let (from, to) = (h.char_pos(tab, 0, 6, false), h.char_pos(tab, 0, 9, true));
        drag_select(&mut h, from, to);
        right_click(&mut h);
        assert!(egui::Popup::is_any_open(&h.ctx), "the menu is open");
        assert!(
            h.tab(tab).ui.has_selection(),
            "a right click keeps the selection"
        );
    }

    #[test]
    fn dragging_to_the_bottom_edge_scrolls_the_view_and_extends_the_selection() {
        let pdf = text_document("view-select-autoscroll");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the first screen", |h| h.all_visible_rendered(tab));
        let from = h.char_pos(tab, 0, 6, false);
        let edge = pos2(from.x, VIEW_ORIGIN.y + h.tab(tab).ui.viewport().h - 1.0);
        h.frame(vec![
            egui::Event::PointerMoved(from),
            pointer_button(from, true, egui::PointerButton::Primary),
        ]);
        h.frame(vec![egui::Event::PointerMoved(edge)]);
        let y0 = h.tab(tab).ui.scroll().y;
        for _ in 0..20 {
            h.frame(Vec::new());
        }
        assert!(
            h.tab(tab).ui.scroll().y > y0 + 100.0,
            "the view scrolled down"
        );
        // The pointer never moved, yet the selection reaches the pages now under it.
        h.frames_until("the selection to reach a later page", |h| {
            h.tab(tab)
                .ui
                .selection
                .selection()
                .is_some_and(|s| s.focus.page > 0)
        });
        h.frame(vec![pointer_button(
            edge,
            false,
            egui::PointerButton::Primary,
        )]);
        let scroll = h.tab(tab).ui.scroll().y;
        for _ in 0..5 {
            h.frame(Vec::new());
        }
        assert_eq!(
            h.tab(tab).ui.scroll().y,
            scroll,
            "it stops when the button is released"
        );
    }

    #[test]
    fn a_reload_clears_the_selection() {
        let pdf = text_document("view-select-reload");
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the first screen", |h| h.all_visible_rendered(tab));
        let (from, to) = (h.char_pos(tab, 0, 6, false), h.char_pos(tab, 0, 9, true));
        drag_select(&mut h, from, to);
        assert!(h.tab(tab).ui.has_selection());
        let generation = |h: &Harness| {
            h.state
                .docs
                .get(h.tab(tab).doc)
                .and_then(|d| d.info())
                .map(|i| i.generation)
        };
        let before = generation(&h);
        h.state.docs.get(h.tab(tab).doc).expect("doc").reload();
        h.frames_until("the new generation", |h| generation(h) != before);
        h.frame(Vec::new());
        assert!(!h.tab(tab).ui.has_selection());
    }
}
