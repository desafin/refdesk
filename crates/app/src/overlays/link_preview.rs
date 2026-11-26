//! The hover preview of links inside the document (LINK-8).
//!
//! Resting the pointer on a link to another place in the same document opens a small popup with
//! the title line of the link ("12쪽 — 7.3.2 제목") and a picture of the destination, so that a
//! reference like "see 7.3.2" can be checked without leaving the place being read.
//!
//! Every decision is a plain function or plain data, tested without egui (rule R-10):
//! when the popup opens and closes ([`PreviewTimer`], with the time passed in), its size and
//! place ([`popup_size`], [`place`], [`popup_geometry`]), the part of the page it shows
//! ([`shot`]) and the texture it draws ([`choose_texture`]). [`update`] and [`show`] are the
//! egui part: they read the pointer and the keys, and paint the popup in the tooltip layer.
//!
//! The popup draws a part of a texture that is in the cache already (the preview layer holds
//! every page). When there is no picture at the scale the popup needs, the preview is stretched
//! and a render is requested; the sharp one replaces it when it arrives (R-1: nothing blocks).

use std::time::Duration;

use egui::{
    Color32, CursorIcon, FontId, Id, Order, Pos2, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};
use refdesk_core::nav::dest::resolve;
use refdesk_core::{
    DocumentHandle, DocumentInfo, LinkTarget, Location, Priority, Rect, RenderKey, Size,
};

use crate::action::{Action, OpenHow};
use crate::state::{DocId, TabId};
use crate::views::links::{self, LinkHit};
use crate::views::pdf_view::{PdfViewUiState, PixelInfo, render_scale_milli};
use crate::views::textures::{Cache, Pick, TextureCache};

/// The pointer has to rest on a link this long (seconds) before the popup opens.
pub const SHOW_DELAY: f64 = 0.35;
/// The popup closes this long (seconds) after the pointer left both the link and the popup.
pub const CLOSE_DELAY: f64 = 0.2;
/// The picture starts this fraction of the page height above the destination, so that the
/// heading line is not at the very edge.
pub const REGION_ABOVE: f32 = 0.02;
/// The popup is this fraction of the view width wide...
pub const WIDTH_FRACTION: f32 = 0.6;
/// ...but at least this many logical pixels...
pub const MIN_WIDTH: f32 = 360.0;
/// ...and at most this many.
pub const MAX_WIDTH: f32 = 720.0;
/// The height of the popup is this times its width.
pub const HEIGHT_RATIO: f32 = 0.6;
/// Height of the title line at the top of the popup.
pub const HEADER_HEIGHT: f32 = 24.0;
/// Space between the link and the popup.
pub const GAP: f32 = 4.0;
/// The popup is never shorter than this, even where there is no room.
const MIN_HEIGHT: f32 = HEADER_HEIGHT + 24.0;

// ------------------------------------------------------------------------------------------------
// When it opens and closes
// ------------------------------------------------------------------------------------------------

/// Which link of which page the pointer is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LinkKey {
    /// The page the link is on (zero-based).
    pub page: u32,
    /// The index of the link in the list of its page.
    pub index: usize,
}

impl LinkKey {
    /// The key of a link found under the pointer.
    pub fn of(hit: &LinkHit) -> LinkKey {
        LinkKey {
            page: hit.page,
            index: hit.index,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Phase {
    /// No popup, and the pointer is not on a link that would open one.
    #[default]
    Idle,
    /// The pointer is on a link since `since`.
    Waiting { key: LinkKey, since: f64 },
    /// The popup is open. `away_since` is when the pointer left both the link and the popup.
    Open {
        key: LinkKey,
        away_since: Option<f64>,
    },
}

/// What one frame says about the pointer, for [`PreviewTimer::step`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    /// The time of the frame, in seconds.
    pub now: f64,
    /// The link under the pointer that would open a popup: an in-document link, with no
    /// selection being dragged. `None` otherwise.
    pub over: Option<LinkKey>,
    /// The pointer is over the popup.
    pub over_popup: bool,
    /// The view scrolled, a button was pressed (outside the popup) or a key was pressed.
    pub interrupted: bool,
    /// Something else has the user's attention: a menu, the search bar, the palette.
    pub blocked: bool,
}

/// The timing of the popup: [`SHOW_DELAY`] to open, [`CLOSE_DELAY`] to close.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PreviewTimer {
    phase: Phase,
    /// A link whose popup was closed by scrolling, a click or a key. It does not open again
    /// until the pointer has left that link.
    suppressed: Option<LinkKey>,
}

impl PreviewTimer {
    /// Advances to the frame `f`. Returns the link whose popup is open after it.
    pub fn step(&mut self, f: Frame) -> Option<LinkKey> {
        if f.blocked {
            self.phase = Phase::Idle;
            return None;
        }
        if f.interrupted {
            self.suppressed = f.over.or(self.open_key());
            self.phase = Phase::Idle;
            return None;
        }
        if self.suppressed.is_some() && self.suppressed != f.over {
            self.suppressed = None;
        }
        let over = f.over.filter(|k| Some(*k) != self.suppressed);
        self.phase = match self.phase {
            Phase::Idle => match over {
                Some(key) => Phase::Waiting { key, since: f.now },
                None => Phase::Idle,
            },
            Phase::Waiting { key, since } => match over {
                Some(k) if k == key && f.now - since >= SHOW_DELAY => Phase::Open {
                    key,
                    away_since: None,
                },
                Some(k) if k == key => Phase::Waiting { key, since },
                // Another link: the delay starts again.
                Some(k) => Phase::Waiting {
                    key: k,
                    since: f.now,
                },
                None => Phase::Idle,
            },
            Phase::Open { key, away_since } => match over {
                Some(k) if k == key => Phase::Open {
                    key,
                    away_since: None,
                },
                Some(k) => Phase::Waiting {
                    key: k,
                    since: f.now,
                },
                None if f.over_popup => Phase::Open {
                    key,
                    away_since: None,
                },
                None => {
                    let away = away_since.unwrap_or(f.now);
                    if f.now - away >= CLOSE_DELAY {
                        Phase::Idle
                    } else {
                        Phase::Open {
                            key,
                            away_since: Some(away),
                        }
                    }
                }
            },
        };
        self.open_key()
    }

    /// The link whose popup is open.
    pub fn open_key(&self) -> Option<LinkKey> {
        match self.phase {
            Phase::Open { key, .. } => Some(key),
            _ => None,
        }
    }

    /// Closes the popup now, and keeps it closed while the pointer stays on `key`.
    pub fn close(&mut self) {
        self.suppressed = self.open_key().or(self.suppressed);
        self.phase = Phase::Idle;
    }

    /// Time until something changes by itself (the popup opens or closes); `None` when nothing
    /// is pending. A frame is wanted then, and only then.
    pub fn remaining(&self, now: f64) -> Option<Duration> {
        let left = match self.phase {
            Phase::Waiting { since, .. } => SHOW_DELAY - (now - since),
            Phase::Open {
                away_since: Some(away),
                ..
            } => CLOSE_DELAY - (now - away),
            _ => return None,
        };
        Some(Duration::from_secs_f64(left.max(0.0)))
    }
}

/// Whether this frame's events close the popup at once: scrolling, any key, and a button
/// pressed anywhere but on the popup (a press on it is the start of a click on it).
pub fn interrupted_by(events: &[egui::Event], popup: Option<egui::Rect>) -> bool {
    events.iter().any(|event| match event {
        egui::Event::MouseWheel { .. } => true,
        egui::Event::Key { pressed: true, .. } => true,
        egui::Event::PointerButton {
            pressed: true, pos, ..
        } => !popup.is_some_and(|r| r.contains(*pos)),
        _ => false,
    })
}

/// Where a link leads when it may open a popup: the place in this document, limited to its
/// pages. Links to addresses and to other documents have none.
pub fn preview_target(target: &LinkTarget, page_sizes: &[Size]) -> Option<Location> {
    let LinkTarget::Internal(dest) = target else {
        return None;
    };
    let last = page_sizes.len().checked_sub(1)? as u32;
    let size = page_sizes
        .get(dest.page as usize)
        .copied()
        .unwrap_or_default();
    let mut at = resolve(dest, size);
    at.page = at.page.min(last);
    Some(at)
}

// ------------------------------------------------------------------------------------------------
// How big it is, where it is, what it shows
// ------------------------------------------------------------------------------------------------

/// The size of the popup in a view of this size: [`WIDTH_FRACTION`] of its width limited to
/// [`MIN_WIDTH`] ..= [`MAX_WIDTH`], [`HEIGHT_RATIO`] times that high. Never larger than the
/// view (then both sides shrink alike).
pub fn popup_size(view: Size) -> Size {
    let w = (view.w * WIDTH_FRACTION).clamp(MIN_WIDTH, MAX_WIDTH);
    let h = w * HEIGHT_RATIO;
    let fit = (view.w / w).min(view.h / h).clamp(0.0, 1.0);
    Size::new(w * fit, h * fit)
}

/// Which side of the link the popup is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Below the link.
    Below,
    /// Above the link.
    Above,
}

/// The place of a popup of `size` for a link whose visible part is `link`, in a view of size
/// `view` (all relative to the top-left corner of the view). Just below the link; when that
/// does not fit, above it; when neither fits, on the side with more room, with less height.
/// It does not cover the link, and does not leave the view sideways.
pub fn place(link: Rect, view: Size, size: Size) -> (Rect, Side) {
    let below = view.h - link.y1 - GAP;
    let above = link.y0 - GAP;
    let (side, height) = if below >= size.h {
        (Side::Below, size.h)
    } else if above >= size.h {
        (Side::Above, size.h)
    } else if below >= above {
        (Side::Below, below.max(MIN_HEIGHT).min(size.h))
    } else {
        (Side::Above, above.max(MIN_HEIGHT).min(size.h))
    };
    let top = match side {
        Side::Below => link.y1 + GAP,
        Side::Above => link.y0 - GAP - height,
    };
    let center = (link.x0 + link.x1) / 2.0;
    let left = (center - size.w / 2.0).min(view.w - size.w).max(0.0);
    (Rect::new(left, top, left + size.w, top + height), side)
}

/// The part of the destination page the popup shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shot {
    /// The region in page space: the whole width of the page.
    pub region: Rect,
    /// The same region as a fraction of the page, for drawing a part of its texture.
    pub uv: Rect,
    /// The height of the picture on screen, at most the height of the picture area (less when
    /// the page is shorter than the area).
    pub height: f32,
}

/// The region of a page of size `page` that fills a picture area of size `image` with the page
/// as wide as the area, starting [`REGION_ABOVE`] of the page height above `dest_y` and moved up
/// where it would run past the bottom of the page.
pub fn shot(dest_y: f32, page: Size, image: Size) -> Shot {
    if !(page.w > 0.0 && page.h > 0.0 && image.w > 0.0 && image.h > 0.0) {
        return Shot {
            region: Rect::new(0.0, 0.0, page.w.max(0.0), page.h.max(0.0)),
            uv: Rect::new(0.0, 0.0, 1.0, 1.0),
            height: 0.0,
        };
    }
    // Logical pixels per pt: the page is as wide as the area.
    let scale = image.w / page.w;
    let height_pt = (image.h / scale).min(page.h);
    let top = (dest_y - page.h * REGION_ABOVE)
        .min(page.h - height_pt)
        .max(0.0);
    Shot {
        region: Rect::new(0.0, top, page.w, top + height_pt),
        uv: Rect::new(
            0.0,
            top / page.h,
            1.0,
            ((top + height_pt) / page.h).min(1.0),
        ),
        height: height_pt * scale,
    }
}

/// Everything about how the popup looks, in the view's coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// The whole popup.
    pub rect: Rect,
    /// Where the picture is drawn: under the title line, as high as [`Shot::height`].
    pub image: Rect,
    /// The title line.
    pub header: Rect,
    /// Which part of the page is shown.
    pub shot: Shot,
    /// Logical pixels per pt of the picture (for the render that would be sharp).
    pub scale: f32,
    /// Which side of the link it is on.
    pub side: Side,
}

/// The popup for a link whose visible part is `link` leading to `dest` on a page of size
/// `page`, in a view of size `view`.
pub fn popup_geometry(link: Rect, view: Size, dest: Location, page: Size) -> Geometry {
    let (rect, side) = place(link, view, popup_size(view));
    let header = Rect::new(
        rect.x0,
        rect.y0,
        rect.x1,
        (rect.y0 + HEADER_HEIGHT).min(rect.y1),
    );
    let area = Size::new(rect.width(), (rect.y1 - header.y1).max(0.0));
    let shot = shot(dest.y, page, area);
    let image = Rect::new(rect.x0, header.y1, rect.x1, header.y1 + shot.height);
    let scale = if page.w > 0.0 { area.w / page.w } else { 1.0 };
    Geometry {
        rect,
        image,
        header,
        shot,
        scale,
        side,
    }
}

/// The texture chosen for the popup.
#[derive(Debug, PartialEq, Eq)]
pub struct Chosen<'a, H> {
    /// The texture to draw; `None` when the page has none at all (a white page is drawn).
    pub texture: Option<&'a H>,
    /// It is the one at the scale the popup wants. When it is not, a render has to be asked
    /// for and this one is drawn stretched until it arrives.
    pub sharp: bool,
}

/// The texture to draw for a popup that wants `key`: the one at exactly that scale, else the
/// one of the same page whose scale is closest (the preview layer guarantees there is one once
/// the document has been preloaded).
pub fn choose_texture<H>(cache: &mut Cache<H>, doc: DocId, key: RenderKey) -> Chosen<'_, H> {
    match cache.pick(doc, key) {
        Pick::Exact(texture) => Chosen {
            texture: Some(texture),
            sharp: true,
        },
        Pick::Stretched(texture) => Chosen {
            texture: Some(texture),
            sharp: false,
        },
        Pick::Blank => Chosen {
            texture: None,
            sharp: false,
        },
    }
}

/// The part of a link the popup stays clear of: the union of its lines that are in the view.
pub fn visible_bounds(rects: &[Rect], view: Size) -> Option<Rect> {
    let view_rect = Rect::new(0.0, 0.0, view.w, view.h);
    rects
        .iter()
        .filter(|r| r.intersects(&view_rect))
        .copied()
        .reduce(|a, b| a.union(&b))
}

// ------------------------------------------------------------------------------------------------
// The state of one view
// ------------------------------------------------------------------------------------------------

/// What is shown by the open popup: kept while it is open, because the pointer may be on the
/// popup and no longer on the link. Page space only (R-5).
#[derive(Clone, Debug, PartialEq)]
pub struct Shown {
    key: LinkKey,
    generation: u32,
    /// The place the link leads to.
    dest: Location,
    /// The size of the page there.
    page: Size,
    /// The title line.
    title: String,
    /// The lines of the link, in page space, on page `key.page`.
    rects: Vec<Rect>,
}

/// What a view remembers about the popup. Not saved.
#[derive(Clone, Debug, Default)]
pub struct PreviewState {
    timer: PreviewTimer,
    shown: Option<Shown>,
    /// The command palette (or another overlay of the window) is open: no popup.
    pub overlay_open: bool,
}

impl PreviewState {
    /// Whether a popup is open.
    pub fn is_open(&self) -> bool {
        self.shown.is_some() && self.timer.open_key().is_some()
    }

    /// What is shown.
    pub fn shown(&self) -> Option<&Shown> {
        self.shown.as_ref().filter(|_| self.is_open())
    }

    fn close(&mut self) {
        self.timer.close();
        self.shown = None;
    }
}

#[cfg(test)]
impl Shown {
    /// The place the popup shows.
    pub(crate) fn dest(&self) -> Location {
        self.dest
    }

    /// The title line of the popup.
    pub(crate) fn title(&self) -> &str {
        &self.title
    }
}

// ------------------------------------------------------------------------------------------------
// egui
// ------------------------------------------------------------------------------------------------

/// What [`update`] needs from the view for one frame.
pub struct UpdateEnv<'a> {
    /// The view on screen.
    pub view_rect: egui::Rect,
    /// The time of the frame, in seconds.
    pub now: f64,
    /// The link under the pointer.
    pub hover: Option<&'a LinkHit>,
    /// The document.
    pub info: &'a DocumentInfo,
    /// Something else has the attention (menu, search bar, selecting).
    pub blocked: bool,
}

fn to_egui(view_rect: egui::Rect, r: Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        view_rect.min + vec2(r.x0, r.y0),
        view_rect.min + vec2(r.x1, r.y1),
    )
}

/// The geometry of the open popup in the view as it is now.
fn geometry(shown: &Shown, state: &PdfViewUiState, view_rect: egui::Rect) -> Option<Geometry> {
    let layout = state.layout()?;
    if shown.key.page >= layout.page_count() {
        return None;
    }
    let screen = links::screen_rects(layout, state.scroll(), shown.key.page, &shown.rects);
    let view = Size::new(view_rect.width(), view_rect.height());
    let link = visible_bounds(&screen, view)?;
    Some(popup_geometry(link, view, shown.dest, shown.page))
}

/// Decides, at the start of the frame, whether a popup is open: runs the timer on what the
/// pointer, the keys and the wheel did, and keeps what the popup shows. Returns whether it is
/// open. Asks for a frame when something is going to open or close by itself.
pub fn update(ui: &Ui, state: &mut PdfViewUiState, env: &UpdateEnv) -> bool {
    let target = env
        .hover
        .and_then(|h| preview_target(&h.link.target, &env.info.page_sizes).map(|dest| (h, dest)));
    let over = target.map(|(h, _)| LinkKey::of(h));

    let popup = state
        .links
        .preview
        .shown
        .as_ref()
        .and_then(|s| geometry(s, state, env.view_rect))
        .map(|g| to_egui(env.view_rect, g.rect));
    let pointer = ui.input(|i| i.pointer.hover_pos());
    let over_popup = popup
        .zip(pointer)
        .is_some_and(|(rect, pos)| rect.contains(pos));
    let interrupted = ui.input(|i| interrupted_by(&i.events, popup));

    let preview = &mut state.links.preview;
    let blocked = env.blocked || preview.overlay_open;
    let open = preview.timer.step(Frame {
        now: env.now,
        over,
        over_popup,
        interrupted,
        blocked,
    });
    let stale = preview
        .shown
        .as_ref()
        .is_some_and(|s| Some(s.key) != open || s.generation != env.info.generation);
    if open.is_none() || stale {
        preview.shown = None;
    }
    if let (Some(key), None) = (open, &preview.shown) {
        match target.filter(|(h, _)| LinkKey::of(h) == key) {
            Some((hit, dest)) => {
                let info = links::describe(
                    &hit.link.target,
                    &env.info.page_sizes,
                    &env.info.page_labels,
                    &env.info.outline,
                );
                preview.shown = Some(Shown {
                    key,
                    generation: env.info.generation,
                    dest,
                    page: env
                        .info
                        .page_sizes
                        .get(dest.page as usize)
                        .copied()
                        .unwrap_or_default(),
                    title: info.text(),
                    rects: hit.link.rects.clone(),
                });
            }
            // Open without the link in sight: cannot happen, but then there is nothing to show.
            None => preview.close(),
        }
    }
    if let Some(left) = preview.timer.remaining(env.now) {
        ui.ctx().request_repaint_after(left);
    }
    preview.is_open()
}

/// The texture the popup wants: its page at the scale the picture is drawn at.
fn wanted_key(shown: &Shown, g: &Geometry, pixels: PixelInfo, generation: u32) -> RenderKey {
    RenderKey {
        page: shown.dest.page,
        scale_milli: render_scale_milli(g.scale, shown.page, pixels),
        generation,
    }
}

/// The open popup as it is now: its geometry, and its texture key and place on screen (tests).
#[cfg(test)]
pub(crate) fn current_popup(
    state: &PdfViewUiState,
    view_rect: egui::Rect,
    pixels: PixelInfo,
    generation: u32,
) -> Option<(egui::Rect, RenderKey)> {
    let shown = state.links.preview.shown()?;
    let g = geometry(shown, state, view_rect)?;
    Some((
        to_egui(view_rect, g.rect),
        wanted_key(shown, &g, pixels, generation),
    ))
}

/// What [`show`] needs besides the state.
pub struct ShowEnv<'a> {
    /// The tab, for the id of the popup and for the jump a click on it makes.
    pub tab: TabId,
    /// The view on screen.
    pub view_rect: egui::Rect,
    /// The document in the texture cache.
    pub doc_id: DocId,
    /// The document.
    pub doc: &'a DocumentHandle,
    /// Its generation.
    pub generation: u32,
    /// Device facts for the scale of the render.
    pub pixels: PixelInfo,
}

/// Paints the popup when one is open, above everything, and queues the jump when it is clicked.
/// The picture is a part of a texture of the page; a sharp one is asked for when there is none.
pub fn show(
    ui: &Ui,
    state: &mut PdfViewUiState,
    textures: &mut TextureCache,
    queue: &mut Vec<Action>,
    env: &ShowEnv,
) {
    let Some(shown) = state.links.preview.shown().cloned() else {
        return;
    };
    let Some(g) = geometry(&shown, state, env.view_rect) else {
        state.links.preview.close();
        return;
    };
    let key = wanted_key(&shown, &g, env.pixels, env.generation);
    let chosen = choose_texture(textures, env.doc_id, key);
    if !chosen.sharp {
        env.doc.request_render(key, Priority::Preview);
    }
    let texture = chosen.texture.map(|t| t.id());
    let blank = crate::theme::blank_page(textures.page_colors());

    let rect = to_egui(env.view_rect, g.rect);
    let image = to_egui(env.view_rect, g.image);
    let header = to_egui(env.view_rect, g.header);
    let uv = egui::Rect::from_min_max(
        pos2(g.shot.uv.x0, g.shot.uv.y0),
        pos2(g.shot.uv.x1, g.shot.uv.y1),
    );
    let visuals = ui.visuals().clone();
    let mut clicked = false;
    egui::Area::new(Id::unique(("link_preview", env.tab)))
        .order(Order::Tooltip)
        .fixed_pos(rect.min)
        .constrain(false)
        .show(ui.ctx(), |ui| {
            let (_, response) = ui.allocate_exact_size(rect.size(), Sense::click());
            let painter = ui.painter().with_clip_rect(rect);
            painter.rect_filled(rect, 0.0, visuals.window_fill);
            match texture {
                Some(id) => {
                    painter.image(id, image, uv, Color32::WHITE);
                }
                None => {
                    painter.rect_filled(image, 0.0, blank);
                }
            }
            painter.rect_filled(header, 0.0, visuals.widgets.noninteractive.weak_bg_fill);
            let font = FontId::proportional(13.0);
            let mut job = egui::text::LayoutJob::simple_singleline(
                shown.title.clone(),
                font,
                visuals.text_color(),
            );
            job.wrap.max_width = (header.width() - 16.0).max(0.0);
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            job.wrap.overflow_character = Some('…');
            let galley = painter.layout_job(job);
            let at = Pos2::new(
                header.min.x + 8.0,
                header.center().y - galley.size().y / 2.0,
            );
            painter.galley(at, galley, visuals.text_color());
            painter.rect_stroke(
                rect,
                0.0,
                Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
                StrokeKind::Inside,
            );
            if response.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            clicked = response.clicked();
        });
    if clicked {
        queue.push(Action::Navigate {
            tab: env.tab,
            to: shown.dest,
            how: OpenHow::Here,
        });
        state.links.preview.close();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Instant;

    use egui::Modifiers;
    use refdesk_core::testutil::{PdfBuilder, TempPdf};
    use refdesk_core::{DestKind, Destination};

    use crate::action::{OpenTarget, dispatch, dispatch_queued};
    use crate::state::{AppState, PdfTab, Tab};
    use crate::views::preload;
    use crate::views::test_support::{Repaints, TIMEOUT, watch_repaints};
    use crate::views::textures::collect_rendered;

    // ---- the timer ----

    fn key(page: u32, index: usize) -> LinkKey {
        LinkKey { page, index }
    }

    fn over(now: f64, k: Option<LinkKey>) -> Frame {
        Frame {
            now,
            over: k,
            over_popup: false,
            interrupted: false,
            blocked: false,
        }
    }

    #[test]
    fn the_popup_opens_after_350_ms_on_a_link_and_not_before() {
        let a = key(0, 0);
        let mut t = PreviewTimer::default();
        assert_eq!(t.step(over(10.0, Some(a))), None);
        assert_eq!(t.step(over(10.2, Some(a))), None);
        assert_eq!(t.step(over(10.349, Some(a))), None);
        assert_eq!(t.step(over(10.351, Some(a))), Some(a));
        assert_eq!(t.open_key(), Some(a));
    }

    #[test]
    fn moving_to_another_link_starts_the_delay_again() {
        let (a, b) = (key(0, 0), key(0, 1));
        let mut t = PreviewTimer::default();
        t.step(over(1.0, Some(a)));
        t.step(over(1.3, Some(a)));
        // B at 1.3: the 350 ms count from here, not from A.
        assert_eq!(t.step(over(1.31, Some(b))), None);
        assert_eq!(t.step(over(1.6, Some(b))), None);
        assert_eq!(t.step(over(1.67, Some(b))), Some(b));
        // Leaving the link before the delay is over cancels it.
        let mut t = PreviewTimer::default();
        t.step(over(1.0, Some(a)));
        t.step(over(1.2, None));
        assert_eq!(t.step(over(1.4, Some(a))), None);
        assert_eq!(t.step(over(1.7, Some(a))), None);
        assert_eq!(t.step(over(1.76, Some(a))), Some(a));
    }

    fn opened(a: LinkKey) -> PreviewTimer {
        let mut t = PreviewTimer::default();
        t.step(over(1.0, Some(a)));
        assert_eq!(t.step(over(1.4, Some(a))), Some(a));
        t
    }

    #[test]
    fn leaving_the_link_closes_the_popup_after_200_ms() {
        let a = key(0, 0);
        let mut t = opened(a);
        assert_eq!(t.step(over(2.0, None)), Some(a));
        assert_eq!(t.step(over(2.19, None)), Some(a));
        assert_eq!(t.step(over(2.21, None)), None);
        // And it does not come back by itself.
        assert_eq!(t.step(over(2.5, None)), None);
    }

    #[test]
    fn the_pointer_can_move_from_the_link_to_the_popup() {
        let a = key(0, 0);
        let mut t = opened(a);
        assert_eq!(t.step(over(2.0, None)), Some(a));
        let on_popup = Frame {
            over_popup: true,
            ..over(2.15, None)
        };
        assert_eq!(t.step(on_popup), Some(a));
        // It stays as long as the pointer is on it, however long.
        for i in 0..20 {
            let f = Frame {
                over_popup: true,
                ..over(2.2 + f64::from(i), None)
            };
            assert_eq!(t.step(f), Some(a));
        }
        // The 200 ms count from the moment it left the popup, not from the link.
        assert_eq!(t.step(over(30.0, None)), Some(a));
        assert_eq!(t.step(over(30.1, None)), Some(a));
        assert_eq!(t.step(over(30.21, None)), None);
    }

    #[test]
    fn coming_back_to_the_link_keeps_the_popup() {
        let a = key(0, 0);
        let mut t = opened(a);
        assert_eq!(t.step(over(2.0, None)), Some(a));
        assert_eq!(t.step(over(2.15, Some(a))), Some(a));
        // The count was reset: 2.15 + 0.15 is not 200 ms away.
        assert_eq!(t.step(over(2.3, None)), Some(a));
        assert_eq!(t.step(over(2.49, None)), Some(a));
        assert_eq!(t.step(over(2.51, None)), None);
    }

    #[test]
    fn another_link_replaces_the_open_popup_after_its_own_delay() {
        let (a, b) = (key(0, 0), key(1, 0));
        let mut t = opened(a);
        assert_eq!(t.step(over(2.0, Some(b))), None);
        assert_eq!(t.step(over(2.3, Some(b))), None);
        assert_eq!(t.step(over(2.36, Some(b))), Some(b));
    }

    #[test]
    fn scrolling_clicking_and_keys_close_it_at_once_and_it_stays_closed_on_that_link() {
        let a = key(0, 0);
        let mut t = opened(a);
        let hit = Frame {
            interrupted: true,
            ..over(1.5, Some(a))
        };
        assert_eq!(t.step(hit), None);
        // Still on the same link: no new popup however long.
        assert_eq!(t.step(over(2.0, Some(a))), None);
        assert_eq!(t.step(over(5.0, Some(a))), None);
        // After leaving the link, it works again.
        assert_eq!(t.step(over(5.1, None)), None);
        assert_eq!(t.step(over(6.0, Some(a))), None);
        assert_eq!(t.step(over(6.4, Some(a))), Some(a));
        // An interruption while still waiting also holds it back.
        let mut t = PreviewTimer::default();
        t.step(over(1.0, Some(a)));
        t.step(Frame {
            interrupted: true,
            ..over(1.1, Some(a))
        });
        assert_eq!(t.step(over(2.0, Some(a))), None);
    }

    #[test]
    fn a_popup_that_was_open_over_the_popup_closes_when_interrupted_there() {
        let a = key(0, 0);
        let mut t = opened(a);
        let f = Frame {
            over_popup: true,
            interrupted: true,
            ..over(2.0, None)
        };
        assert_eq!(t.step(f), None);
        assert_eq!(t.open_key(), None);
    }

    #[test]
    fn nothing_opens_while_blocked_and_blocking_closes_at_once() {
        let a = key(0, 0);
        let mut t = PreviewTimer::default();
        let blocked = |now| Frame {
            blocked: true,
            ..over(now, Some(a))
        };
        assert_eq!(t.step(blocked(1.0)), None);
        assert_eq!(t.step(blocked(2.0)), None);
        // The delay starts when the block is gone.
        assert_eq!(t.step(over(2.1, Some(a))), None);
        assert_eq!(t.step(over(2.4, Some(a))), None);
        assert_eq!(t.step(over(2.46, Some(a))), Some(a));
        assert_eq!(t.step(blocked(2.5)), None);
    }

    #[test]
    fn a_frame_is_wanted_only_while_a_delay_runs() {
        let a = key(0, 0);
        let mut t = PreviewTimer::default();
        assert_eq!(t.remaining(1.0), None);
        t.step(over(1.0, Some(a)));
        let left = t.remaining(1.1).expect("waiting");
        assert!((left.as_secs_f64() - 0.25).abs() < 1e-9);
        t.step(over(1.4, Some(a)));
        // Open and still on the link: nothing is pending.
        assert_eq!(t.remaining(1.5), None);
        t.step(over(2.0, None));
        let left = t.remaining(2.05).expect("closing");
        assert!((left.as_secs_f64() - 0.15).abs() < 1e-9);
        t.step(over(2.3, None));
        assert_eq!(t.remaining(2.4), None);
    }

    fn press(button_pos: egui::Pos2) -> egui::Event {
        egui::Event::PointerButton {
            pos: button_pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        }
    }

    fn key_down(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn wheel() -> egui::Event {
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -10.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn events_that_interrupt() {
        let popup = Some(egui::Rect::from_min_size(
            pos2(100.0, 100.0),
            vec2(200.0, 100.0),
        ));
        assert!(!interrupted_by(&[], popup));
        assert!(!interrupted_by(
            &[egui::Event::PointerMoved(pos2(1.0, 1.0))],
            popup
        ));
        assert!(interrupted_by(&[wheel()], popup));
        assert!(interrupted_by(&[key_down(egui::Key::A)], popup));
        // A press outside the popup, but not one on it (that is the start of a click on it).
        assert!(interrupted_by(&[press(pos2(10.0, 10.0))], popup));
        assert!(!interrupted_by(&[press(pos2(150.0, 150.0))], popup));
        assert!(interrupted_by(&[press(pos2(150.0, 150.0))], None));
        // A key that went up does not.
        let up = egui::Event::Key {
            key: egui::Key::A,
            physical_key: Some(egui::Key::A),
            pressed: false,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        assert!(!interrupted_by(&[up], popup));
    }

    // ---- which links, how big, where, what ----

    fn dest(page: u32) -> Destination {
        Destination {
            page,
            kind: DestKind::Xyz {
                left: Some(72.0),
                top: Some(300.0),
            },
        }
    }

    #[test]
    fn only_links_inside_the_document_have_a_preview() {
        let sizes = [Size::new(612.0, 792.0); 3];
        let at = preview_target(&LinkTarget::Internal(dest(2)), &sizes).expect("internal");
        assert_eq!((at.page, at.y), (2, 300.0));
        // Past the last page: the last page.
        let at = preview_target(&LinkTarget::Internal(dest(9)), &sizes).expect("internal");
        assert_eq!(at.page, 2);
        assert_eq!(
            preview_target(&LinkTarget::External("https://example.com/".into()), &sizes),
            None
        );
        let other = LinkTarget::OtherDocument {
            path: "other.pdf".into(),
            dest: Some(dest(0)),
        };
        assert_eq!(preview_target(&other, &sizes), None);
        assert_eq!(preview_target(&LinkTarget::Internal(dest(0)), &[]), None);
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn the_popup_is_60_percent_of_the_view_within_360_and_720_and_0_6_as_high() {
        let s = popup_size(Size::new(1000.0, 900.0));
        assert!(close(s.w, 600.0) && close(s.h, 360.0), "{s:?}");
        // A narrow view: the lower limit.
        let s = popup_size(Size::new(500.0, 900.0));
        assert!(close(s.w, 360.0) && close(s.h, 216.0), "{s:?}");
        // A wide view: the upper limit.
        let s = popup_size(Size::new(3000.0, 2000.0));
        assert!(close(s.w, 720.0) && close(s.h, 432.0), "{s:?}");
        // Exactly at the limits.
        assert!(close(popup_size(Size::new(600.0, 900.0)).w, 360.0));
        assert!(close(popup_size(Size::new(1200.0, 900.0)).w, 720.0));
    }

    #[test]
    fn the_popup_never_exceeds_a_small_view() {
        let s = popup_size(Size::new(300.0, 900.0));
        assert!(close(s.w, 300.0) && close(s.h, 180.0), "{s:?}");
        let s = popup_size(Size::new(1000.0, 200.0));
        assert!(
            s.h <= 200.0 + 1e-3 && close(s.h / s.w, HEIGHT_RATIO),
            "{s:?}"
        );
    }

    const VIEW: Size = Size {
        w: 1000.0,
        h: 800.0,
    };

    #[test]
    fn the_popup_is_just_below_the_link_when_there_is_room() {
        let link = Rect::new(300.0, 100.0, 500.0, 114.0);
        let size = popup_size(VIEW);
        let (r, side) = place(link, VIEW, size);
        assert_eq!(side, Side::Below);
        assert!(close(r.y0, 114.0 + GAP));
        assert!(close(r.width(), size.w) && close(r.height(), size.h));
        // Centered on the link.
        assert!(close((r.x0 + r.x1) / 2.0, 400.0));
        // The link line is not covered.
        assert!(!r.intersects(&link));
    }

    #[test]
    fn the_popup_goes_above_the_link_when_there_is_no_room_below() {
        let link = Rect::new(300.0, 700.0, 500.0, 714.0);
        let size = popup_size(VIEW);
        let (r, side) = place(link, VIEW, size);
        assert_eq!(side, Side::Above);
        assert!(close(r.y1, 700.0 - GAP));
        assert!(close(r.height(), size.h));
        assert!(r.y0 >= 0.0 && !r.intersects(&link));
    }

    #[test]
    fn with_no_room_on_either_side_the_larger_side_is_used_with_less_height() {
        let view = Size::new(1000.0, 400.0);
        let size = popup_size(view);
        assert!(close(size.h, 360.0));
        // Just below the middle: more room above than below.
        let link = Rect::new(300.0, 220.0, 500.0, 234.0);
        let (r, side) = place(link, view, size);
        assert_eq!(side, Side::Above);
        assert!(close(r.y1, 220.0 - GAP) && r.y0 >= 0.0);
        assert!(r.height() < size.h);
        assert!(!r.intersects(&link));
        // Above the middle: more room below.
        let link = Rect::new(300.0, 100.0, 500.0, 114.0);
        let (r, side) = place(link, view, size);
        assert_eq!(side, Side::Below);
        assert!(close(r.y0, 114.0 + GAP) && r.y1 <= 400.0 + 1e-3);
        assert!(!r.intersects(&link));
    }

    #[test]
    fn a_link_near_the_left_or_right_edge_keeps_the_popup_in_the_view() {
        let size = popup_size(VIEW);
        let left = Rect::new(0.0, 100.0, 40.0, 114.0);
        let (r, _) = place(left, VIEW, size);
        assert!(close(r.x0, 0.0) && close(r.width(), size.w));
        let right = Rect::new(960.0, 100.0, 1000.0, 114.0);
        let (r, _) = place(right, VIEW, size);
        assert!(close(r.x1, 1000.0) && close(r.width(), size.w));
        // A link in the middle is centered.
        let (r, _) = place(Rect::new(450.0, 100.0, 550.0, 114.0), VIEW, size);
        assert!(close((r.x0 + r.x1) / 2.0, 500.0));
    }

    #[test]
    fn a_link_of_several_lines_is_cleared_as_a_whole() {
        let lines = [
            Rect::new(600.0, 100.0, 900.0, 114.0),
            Rect::new(100.0, 116.0, 300.0, 130.0),
        ];
        let bounds = visible_bounds(&lines, VIEW).expect("in view");
        let (r, _) = place(bounds, VIEW, popup_size(VIEW));
        for line in &lines {
            assert!(!r.intersects(line));
        }
        // Lines out of the view do not count.
        let lines = [
            Rect::new(100.0, -50.0, 300.0, -36.0),
            Rect::new(100.0, 10.0, 300.0, 24.0),
        ];
        assert_eq!(
            visible_bounds(&lines, VIEW),
            Some(Rect::new(100.0, 10.0, 300.0, 24.0))
        );
        assert_eq!(visible_bounds(&lines[..1], VIEW), None);
    }

    const PAGE: Size = Size { w: 612.0, h: 792.0 };

    #[test]
    fn the_picture_starts_a_little_above_the_destination() {
        let image = Size::new(600.0, 336.0);
        let s = shot(300.0, PAGE, image);
        // 2% of the page height above the destination.
        assert!(close(s.region.y0, 300.0 - 792.0 * 0.02));
        // The whole width of the page, so that nothing is cut off at the sides.
        assert!(close(s.region.x0, 0.0) && close(s.region.x1, 612.0));
        // As high as the area shows at this scale.
        let scale = 600.0 / 612.0;
        assert!(close(s.region.height(), 336.0 / scale));
        assert!(close(s.height, 336.0));
        assert!(close(s.uv.x0, 0.0) && close(s.uv.x1, 1.0));
        assert!(close(s.uv.y0, s.region.y0 / 792.0) && close(s.uv.y1, s.region.y1 / 792.0));
    }

    #[test]
    fn near_the_bottom_of_the_page_the_picture_is_pulled_up_to_stay_on_it() {
        let image = Size::new(600.0, 336.0);
        let s = shot(780.0, PAGE, image);
        assert!(close(s.region.y1, 792.0));
        assert!(close(s.region.height(), 336.0 / (600.0 / 612.0)));
        assert!(s.region.y0 < 780.0 - 792.0 * 0.02);
        assert!(s.uv.y1 <= 1.0 && close(s.uv.y1, 1.0));
        assert!(close(s.height, 336.0));
    }

    #[test]
    fn near_the_top_of_the_page_the_picture_starts_at_the_top() {
        let s = shot(5.0, PAGE, Size::new(600.0, 336.0));
        assert!(close(s.region.y0, 0.0) && close(s.uv.y0, 0.0));
        let s = shot(-30.0, PAGE, Size::new(600.0, 336.0));
        assert!(close(s.region.y0, 0.0));
    }

    #[test]
    fn the_uv_range_is_inside_the_texture_and_matches_the_region() {
        for dest_y in [-20.0, 0.0, 14.0, 300.0, 700.0, 791.0, 792.0, 5000.0] {
            for image in [
                Size::new(360.0, 190.0),
                Size::new(600.0, 336.0),
                Size::new(720.0, 408.0),
                Size::new(300.0, 2000.0),
            ] {
                let s = shot(dest_y, PAGE, image);
                assert!(s.uv.x0 >= 0.0 && s.uv.x1 <= 1.0, "{dest_y} {image:?}");
                assert!(s.uv.y0 >= 0.0 && s.uv.y1 <= 1.0, "{dest_y} {image:?}");
                assert!(s.uv.y0 <= s.uv.y1);
                assert!(close(s.uv.y0 * PAGE.h, s.region.y0), "{dest_y} {image:?}");
                assert!(close(s.uv.y1 * PAGE.h, s.region.y1), "{dest_y} {image:?}");
                assert!(s.height <= image.h + 1e-3);
                // The picture is as wide as the area: the same scale in both directions.
                let scale = image.w / PAGE.w;
                assert!(close(s.height, s.region.height() * scale));
            }
        }
    }

    #[test]
    fn a_page_shorter_than_the_area_is_drawn_whole_and_not_stretched() {
        let short = Size::new(612.0, 200.0);
        let s = shot(50.0, short, Size::new(600.0, 336.0));
        assert!(close(s.region.y0, 0.0) && close(s.region.y1, 200.0));
        assert!(close(s.uv.y1, 1.0));
        assert!(close(s.height, 200.0 * 600.0 / 612.0));
        assert!(s.height < 336.0);
        // A page with no size gives nothing to draw.
        assert_eq!(
            shot(0.0, Size::new(0.0, 0.0), Size::new(600.0, 336.0)).height,
            0.0
        );
    }

    #[test]
    fn the_geometry_puts_the_title_line_above_the_picture_inside_the_popup() {
        let link = Rect::new(300.0, 100.0, 500.0, 114.0);
        let at = Location::new(2, 72.0, 300.0);
        let g = popup_geometry(link, VIEW, at, PAGE);
        assert_eq!(g.side, Side::Below);
        assert!(close(g.header.height(), HEADER_HEIGHT));
        assert!(close(g.header.y0, g.rect.y0) && close(g.image.y0, g.header.y1));
        assert!(g.image.y1 <= g.rect.y1 + 1e-3);
        assert!(close(g.image.width(), g.rect.width()));
        // The page is as wide as the popup.
        assert!(close(g.scale, g.rect.width() / PAGE.w));
        assert!(close(g.shot.region.y0, 300.0 - 792.0 * 0.02));
    }

    // ---- the texture ----

    fn key_at(page: u32, scale_milli: u32) -> RenderKey {
        RenderKey {
            page,
            scale_milli,
            generation: 0,
        }
    }

    #[test]
    fn the_texture_closest_in_scale_is_drawn_and_a_sharp_one_is_asked_for() {
        let doc = DocId::for_test(1);
        let mut cache: Cache<u32> = Cache::default();
        cache.set_preview_scale(doc, 0, 300);
        cache.insert_handle(doc, key_at(2, 300), 1, [10, 10]);
        // Only the preview: it is used, and it is not sharp.
        let c = choose_texture(&mut cache, doc, key_at(2, 800));
        assert_eq!((c.texture, c.sharp), (Some(&1), false));

        // A sharp one that is closer in scale wins over the preview.
        cache.insert_handle(doc, key_at(2, 1000), 2, [10, 10]);
        let c = choose_texture(&mut cache, doc, key_at(2, 800));
        assert_eq!((c.texture, c.sharp), (Some(&2), false));
        // The closest by ratio, not by difference: 300 is nearer to 450 than 1000 is.
        let c = choose_texture(&mut cache, doc, key_at(2, 450));
        assert_eq!((c.texture, c.sharp), (Some(&1), false));

        // The exact one.
        cache.insert_handle(doc, key_at(2, 800), 3, [10, 10]);
        let c = choose_texture(&mut cache, doc, key_at(2, 800));
        assert_eq!((c.texture, c.sharp), (Some(&3), true));

        // Another page has nothing yet: a white page, and a request.
        let c = choose_texture(&mut cache, doc, key_at(3, 800));
        assert_eq!((c.texture, c.sharp), (None, false));
    }

    // ---- on real documents, frame by frame ----

    const SCREEN: egui::Vec2 = egui::vec2(800.0, 600.0);

    /// A test app that draws the tab like a window does: the section path line, the view and its
    /// popup, on a screen with nothing else, with a clock the test sets.
    pub(crate) struct Harness {
        pub ctx: egui::Context,
        repaints: Repaints,
        pub state: AppState,
        pub time: f64,
        /// The texts the last frame drew, with where.
        pub drawn: Vec<(String, egui::Pos2)>,
    }

    fn collect_texts(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
        match shape {
            egui::Shape::Text(t) => out.push((t.galley.text().to_owned(), t.pos)),
            egui::Shape::Vec(v) => v.iter().for_each(|s| collect_texts(s, out)),
            _ => {}
        }
    }

    impl Harness {
        pub fn new() -> Harness {
            let ctx = egui::Context::default();
            let repaints = watch_repaints(&ctx);
            Harness {
                ctx,
                repaints,
                state: AppState::new(),
                time: 0.0,
                drawn: Vec::new(),
            }
        }

        pub fn open(&mut self, path: &std::path::Path) -> TabId {
            let target = OpenTarget {
                path: path.to_path_buf(),
                page: None,
            };
            dispatch(&mut self.state, Action::OpenFiles(vec![target]), &self.ctx);
            self.state.focus.expect("the new tab has the focus")
        }

        /// A frame at the time `at` (seconds) with these events.
        pub fn frame_at(&mut self, at: f64, events: Vec<egui::Event>) {
            self.time = at;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, SCREEN)),
                time: Some(at),
                events,
                ..Default::default()
            };
            collect_rendered(&self.ctx, &self.state.docs, &mut self.state.textures);
            preload::step(&self.ctx, &mut self.state);
            let state = &mut self.state;
            let mut queue = Vec::new();
            let output = self.ctx.run_ui(input, |ui| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| draw_active(ui, state, &mut queue));
            });
            self.drawn.clear();
            for clipped in &output.shapes {
                collect_texts(&clipped.shape, &mut self.drawn);
            }
            output.drop_without_applying_deltas();
            self.state.actions.append(&mut queue);
            dispatch_queued(&mut self.state, &self.ctx);
        }

        /// A frame a little after the last one.
        pub fn frame(&mut self, events: Vec<egui::Event>) {
            self.frame_at(self.time + 1.0 / 60.0, events);
        }

        /// Runs frames until `check` holds, waiting for a repaint request between them.
        pub fn frames_until(&mut self, what: &str, mut check: impl FnMut(&Harness) -> bool) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.frame(Vec::new());
                if check(self) {
                    return;
                }
                self.repaints.wait_one(what, deadline);
            }
        }

        pub fn tab(&self, tab: TabId) -> &PdfTab {
            self.state.tabs.pdf(tab).expect("pdf tab")
        }

        pub fn is_open(&self, tab: TabId) -> bool {
            self.tab(tab).ui.links.preview.is_open()
        }

        /// Whether the last frame drew a text that is exactly `text`.
        pub fn drew(&self, text: &str) -> bool {
            self.drawn.iter().any(|(t, _)| t == text)
        }

        pub fn view_rect(&self) -> egui::Rect {
            egui::Rect::from_min_max(Pos2::ZERO, pos2(SCREEN.x, SCREEN.y) - vec2(12.0, 12.0))
        }

        /// The screen position of the middle of link `index` of `page`'s first line, with the
        /// view at its top-left corner.
        pub fn link_pos(&self, tab: TabId, page: u32, rect: Rect) -> egui::Pos2 {
            let ui = &self.tab(tab).ui;
            let layout = ui.layout().expect("laid out");
            let r = links::screen_rects(layout, ui.scroll(), page, &[rect])[0];
            pos2((r.x0 + r.x1) / 2.0, (r.y0 + r.y1) / 2.0)
        }

        pub fn pixels(&self) -> PixelInfo {
            PixelInfo {
                pixels_per_point: 1.0,
                max_texture_side: self.ctx.input(|i| i.max_texture_side),
            }
        }

        pub fn popup(&self, tab: TabId) -> Option<(egui::Rect, RenderKey)> {
            let generation = self
                .state
                .docs
                .get(self.tab(tab).doc)
                .and_then(|d| d.info())
                .map_or(0, |i| i.generation);
            current_popup(
                &self.tab(tab).ui,
                self.view_rect(),
                self.pixels(),
                generation,
            )
        }
    }

    fn draw_active(ui: &mut Ui, state: &mut AppState, queue: &mut Vec<Action>) {
        let Some(active) = state.main_window().dock.focused_tab() else {
            return;
        };
        let show_breadcrumb = state.show_breadcrumb;
        let overlay_open = state.palette.open.is_some();
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
            let cx = crate::views::pdf_view::ViewContext {
                tab_id: active,
                focused: *focus == Some(active),
                textures,
                queue,
            };
            let history = refdesk_core::SearchHistory::default();
            crate::windows::draw_pdf_tab(ui, tab, doc, cx, show_breadcrumb, overlay_open, &history);
        }
    }

    /// The first link of page 1: to page 3, y = 300.
    pub(crate) const LINK: Rect = Rect {
        x0: 72.0,
        y0: 90.0,
        x1: 250.0,
        y1: 104.0,
    };
    /// The second: an address.
    const URL_LINK: Rect = Rect {
        x0: 72.0,
        y0: 150.0,
        x1: 250.0,
        y1: 164.0,
    };

    /// Four pages. The outline starts on page 2, so page 1 is before the first section.
    pub(crate) fn document(stem: &str) -> TempPdf {
        PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "see section 7.3.2 on page 3");
                p.link_to((72.0, 90.0, 250.0, 104.0), 3, Some((72.0, 300.0)));
                p.text(72.0, 160.0, 12.0, "https://example.com/");
                p.link_uri((72.0, 150.0, 250.0, 164.0), "https://example.com/");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 60.0, 16.0, "7 Functional specification");
                p.text(72.0, 210.0, 14.0, "7.3 State management");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 110.0, 12.0, "7.3.2 Transitions");
                p.text(72.0, 310.0, 12.0, "The destination of the link.");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "the end");
            })
            .outline("7 Functional specification", 2, 40.0, |o| {
                o.child_with("7.3 State management", 2, 200.0, |o| {
                    o.child("7.3.2 Transitions", 3, 100.0);
                });
            })
            .write_temp(stem)
    }

    /// A harness with the document open and laid out, the links of page 1 arrived and every page
    /// of it drawn once.
    pub(crate) fn opened_document(stem: &str) -> (Harness, TabId, TempPdf) {
        let pdf = document(stem);
        let mut h = Harness::new();
        let tab = h.open(pdf.path());
        h.frames_until("the links", |h| {
            let doc = h.state.docs.get(h.tab(tab).doc).expect("doc");
            h.tab(tab).ui.layout().is_some() && doc.page_links(0).is_some()
        });
        (h, tab, pdf)
    }

    /// Rests the pointer on `pos` from `at` on and runs the frames of a quarter second.
    fn hover_from(h: &mut Harness, at: f64, pos: egui::Pos2) {
        h.frame_at(at, vec![egui::Event::PointerMoved(pos)]);
        h.frame_at(at + 0.001, Vec::new());
    }

    #[test]
    fn resting_on_a_link_opens_the_popup_after_350_ms_with_the_title_line() {
        let (mut h, tab, _pdf) = opened_document("preview-open");
        let pos = h.link_pos(tab, 0, LINK);
        hover_from(&mut h, 100.0, pos);
        assert!(!h.is_open(tab));
        h.frame_at(100.2, Vec::new());
        assert!(!h.is_open(tab));
        h.frame_at(100.34, Vec::new());
        assert!(!h.is_open(tab));
        h.frame_at(100.36, Vec::new());
        assert!(h.is_open(tab));
        // (egui sizes a new area in its first frame without drawing it.)
        h.frame_at(100.37, Vec::new());

        // The same text as the tooltip: page, section.
        let shown = h.tab(tab).ui.links.preview.shown().expect("shown");
        let expected = crate::i18n::tr("link.page_title")
            .replace("{page}", "3")
            .replace("{title}", "7.3.2 Transitions");
        assert_eq!(shown.title(), expected);
        assert_eq!(shown.dest().page, 2);
        // It was drawn, with its title.
        assert!(h.drew(&expected), "{:?}", h.drawn);
        // Below the link, in the view.
        let (rect, _) = h.popup(tab).expect("popup");
        assert!(rect.min.y > pos.y && rect.max.y <= h.view_rect().max.y);
        assert!(rect.min.x >= 0.0 && rect.max.x <= h.view_rect().max.x);
    }

    #[test]
    fn the_popup_asks_for_a_sharp_picture_and_swaps_it_in_when_it_arrives() {
        let (mut h, tab, _pdf) = opened_document("preview-sharp");
        let pos = h.link_pos(tab, 0, LINK);
        hover_from(&mut h, 100.0, pos);
        h.frame_at(100.4, Vec::new());
        assert!(h.is_open(tab));
        let (_, key) = h.popup(tab).expect("popup");
        assert_eq!(key.page, 2);
        let doc = h.tab(tab).doc;
        h.frames_until("the sharp picture", |h| h.state.textures.contains(doc, key));
        // Drawn without a panic with the exact texture.
        h.frame(Vec::new());
        assert!(h.is_open(tab));
    }

    #[test]
    fn external_links_and_other_documents_have_no_popup() {
        let (mut h, tab, _pdf) = opened_document("preview-external");
        let pos = h.link_pos(tab, 0, URL_LINK);
        hover_from(&mut h, 100.0, pos);
        for t in [100.4, 101.0, 105.0] {
            h.frame_at(t, Vec::new());
            assert!(!h.is_open(tab));
        }
    }

    #[test]
    fn leaving_the_link_closes_it_after_200_ms_but_the_popup_can_be_reached() {
        let (mut h, tab, _pdf) = opened_document("preview-leave");
        let pos = h.link_pos(tab, 0, LINK);
        hover_from(&mut h, 100.0, pos);
        h.frame_at(100.4, Vec::new());
        assert!(h.is_open(tab));
        let (popup, _) = h.popup(tab).expect("popup");
        let away = pos2(popup.max.x - 5.0, popup.max.y + 40.0);
        assert!(!popup.contains(away));

        h.frame_at(101.0, vec![egui::Event::PointerMoved(away)]);
        h.frame_at(101.1, Vec::new());
        assert!(h.is_open(tab));
        h.frame_at(101.19, Vec::new());
        assert!(h.is_open(tab));
        h.frame_at(101.21, Vec::new());
        assert!(!h.is_open(tab));

        // Again, but the pointer goes to the popup within 200 ms: it stays.
        hover_from(&mut h, 110.0, pos);
        h.frame_at(110.4, Vec::new());
        assert!(h.is_open(tab));
        h.frame_at(111.0, vec![egui::Event::PointerMoved(away)]);
        h.frame_at(111.1, vec![egui::Event::PointerMoved(popup.center())]);
        for t in [111.2, 111.5, 112.0, 120.0] {
            h.frame_at(t, Vec::new());
            assert!(h.is_open(tab), "at {t}");
        }
    }

    #[test]
    fn scrolling_a_key_or_a_click_elsewhere_closes_it_at_once() {
        for (name, events) in [
            ("wheel", vec![wheel()]),
            ("key", vec![key_down(egui::Key::Escape)]),
            ("click", vec![press(pos2(700.0, 500.0))]),
        ] {
            let (mut h, tab, _pdf) = opened_document(&format!("preview-interrupt-{name}"));
            let pos = h.link_pos(tab, 0, LINK);
            hover_from(&mut h, 100.0, pos);
            h.frame_at(100.4, Vec::new());
            assert!(h.is_open(tab), "{name}");
            h.frame_at(100.41, events);
            assert!(!h.is_open(tab), "{name}");
            // It does not come back while the pointer rests on the same link.
            h.frame_at(101.0, Vec::new());
            h.frame_at(102.0, Vec::new());
            assert!(!h.is_open(tab), "{name}");
        }
    }

    #[test]
    fn nothing_opens_while_selecting() {
        let (mut h, tab, _pdf) = opened_document("preview-drag");
        let pos = h.link_pos(tab, 0, LINK);
        h.frame_at(100.0, vec![egui::Event::PointerMoved(pos)]);
        // The text of the page has to be there for a press to start a selection.
        let doc = h.state.docs.get(h.tab(tab).doc).cloned().expect("doc");
        h.frames_until("the text", |_| doc.page_text(0).is_some());
        h.frame_at(100.1, vec![press(pos)]);
        assert!(h.tab(tab).ui.selection.is_dragging());
        for t in [100.5, 101.0, 103.0] {
            h.frame_at(t, Vec::new());
            assert!(!h.is_open(tab), "at {t}");
        }
    }

    #[test]
    fn nothing_opens_while_the_search_bar_or_a_menu_is_open() {
        let (mut h, tab, _pdf) = opened_document("preview-blocked");
        let pos = h.link_pos(tab, 0, LINK);
        if let Some(Tab::Pdf(pdf)) = h.state.tabs.get_mut(tab) {
            pdf.ui.search.open = true;
        }
        hover_from(&mut h, 100.0, pos);
        h.frame_at(101.0, Vec::new());
        assert!(!h.is_open(tab));
        // The search bar adds a line above the view; the layout moved, find the link again.
        if let Some(Tab::Pdf(pdf)) = h.state.tabs.get_mut(tab) {
            pdf.ui.search.open = false;
        }
        h.frame_at(102.0, Vec::new());
        let pos = h.link_pos(tab, 0, LINK);
        hover_from(&mut h, 103.0, pos);
        h.frame_at(103.5, Vec::new());
        assert!(h.is_open(tab));
        // The palette opens: the popup closes at once.
        dispatch(
            &mut h.state,
            Action::ShowPalette(crate::overlays::palette::PaletteMode::Commands),
            &h.ctx,
        );
        h.frame_at(103.6, Vec::new());
        assert!(!h.is_open(tab));
    }

    #[test]
    fn clicking_the_popup_goes_to_the_destination_and_back_returns() {
        let (mut h, tab, _pdf) = opened_document("preview-click");
        let pos = h.link_pos(tab, 0, LINK);
        hover_from(&mut h, 100.0, pos);
        h.frame_at(100.4, Vec::new());
        h.frame_at(100.45, Vec::new());
        let (popup, _) = h.popup(tab).expect("popup");
        let inside = popup.center();
        let release = egui::Event::PointerButton {
            pos: inside,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::NONE,
        };
        h.frame_at(
            100.5,
            vec![egui::Event::PointerMoved(inside), press(inside)],
        );
        // The press on the popup does not close it.
        assert!(h.is_open(tab));
        assert_eq!(h.tab(tab).view.anchor.page, 0);
        h.frame_at(100.55, vec![release]);
        h.frame_at(100.6, Vec::new());
        assert_eq!(h.tab(tab).view.anchor.page, 2);
        assert!(!h.is_open(tab));
        assert!(h.tab(tab).view.history.can_back());

        dispatch(&mut h.state, Action::Back(tab), &h.ctx);
        h.frame_at(101.0, Vec::new());
        assert_eq!(h.tab(tab).view.anchor.page, 0);
    }

    #[test]
    fn the_tooltip_of_the_link_is_not_shown_over_the_popup() {
        // The hover text stays for the status bar; the popup replaces the tooltip only.
        let (mut h, tab, _pdf) = opened_document("preview-tooltip");
        let pos = h.link_pos(tab, 0, LINK);
        hover_from(&mut h, 100.0, pos);
        h.frame_at(100.4, Vec::new());
        assert!(h.is_open(tab));
        let title = h.tab(tab).ui.links.hover_text.clone().expect("hover text");
        // The title appears once, in the popup, and not a second time as a tooltip.
        h.frame_at(101.0, Vec::new());
        let count = h.drawn.iter().filter(|(t, _)| *t == title).count();
        assert_eq!(count, 1, "{:?}", h.drawn);
    }
}
