//! The "Pages" view of the sidebar: the pages of the focused document as a grid of thumbnails
//! (VIEW-13, `docs/ARCHITECTURE.md` §5.9).
//!
//! A thumbnail is not rendered for this view. It is the page's texture in the cache (§5.6): the
//! preview layer holds every page of an open document, so [`show_view`] only asks the cache for
//! the texture nearest in scale and draws it small. A page that has none yet is drawn as an
//! empty sheet and filled in when its texture arrives (the preview layer fills the cache by
//! itself, nearest to where the reader is first).
//!
//! What decides something is a plain function or a method that does not touch egui (rule R-10):
//! how many columns fit, how high a row is, which row a page is in, which cells are on screen,
//! where the list has to scroll to show a page, which pages a search found something on and how
//! many times. [`show_view`] only draws and turns clicks into actions (R-6).
//!
//! Only the cells on screen are laid out and drawn. The rows' heights are summed once into a
//! [`Grid`] (kept between frames while nothing changes), so finding the visible cells is a
//! bisection and a frame costs what its visible cells cost, whatever the page count.

use std::ops::Range;
use std::sync::Arc;

use egui::{
    Align2, Button, Checkbox, Color32, CursorIcon, Id, Pos2, Rect, RichText, ScrollArea, Sense,
    Stroke, StrokeKind, TextStyle, Ui, WidgetInfo, vec2,
};
use refdesk_core::{Location, SearchMatch, Size};

use crate::action::{Action, OpenHow};
use crate::i18n::tr;
use crate::panels::search::line_job;
use crate::state::{DocId, DocStore, Tab, TabId, TabStore, WindowId};
use crate::views::links::{ClickButton, ClickMods, open_how, page_label};
use crate::views::textures::TextureCache;

/// Width of a thumbnail in a window that was never adjusted, in logical pixels.
pub const DEFAULT_WIDTH: f32 = 120.0;
/// The narrowest and the widest thumbnail.
pub const WIDTH_RANGE: (f32, f32) = (80.0, 320.0);
/// How much one press of − or + changes the width.
pub const WIDTH_STEP: f32 = 20.0;
/// Space between cells and under a row.
pub const GAP: f32 = 6.0;
/// Space kept free at the left and right edge of the list (room for the outline of a cell).
pub const SIDE_PAD: f32 = 3.0;
/// Space between a thumbnail and its page label.
pub const CAPTION_GAP: f32 = 2.0;
/// The tallest a page is drawn, as a multiple of its width; a very long page is cut short here.
const MAX_ASPECT: f32 = 4.0;
/// The lowest a page is drawn, as a multiple of its width.
const MIN_ASPECT: f32 = 0.25;
/// The height over the width of a page that has no usable size (A4).
const FALLBACK_ASPECT: f32 = 1.414;

// ---------------------------------------------------------------------------------------------
// Pure logic: the width
// ---------------------------------------------------------------------------------------------

/// A thumbnail width kept within [`WIDTH_RANGE`]; a value that is not a number becomes the
/// default.
pub fn clamp_width(width: f32) -> f32 {
    if width.is_nan() {
        DEFAULT_WIDTH
    } else {
        width.clamp(WIDTH_RANGE.0, WIDTH_RANGE.1)
    }
}

/// The width after `steps` presses of + (negative: −), kept within the range.
pub fn stepped_width(width: f32, steps: i32) -> f32 {
    clamp_width(clamp_width(width) + steps as f32 * WIDTH_STEP)
}

// ---------------------------------------------------------------------------------------------
// Pure logic: the grid
// ---------------------------------------------------------------------------------------------

/// How the cells sit across a list `avail` wide.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// Cells per row, at least 1.
    pub cols: usize,
    /// Width of a cell (and of the thumbnail in it).
    pub cell_w: f32,
    /// Distance from the left edge of the list to the first cell: the cells are centered.
    pub left: f32,
}

/// The columns for a list `avail` wide and thumbnails `thumb_w` wide: as many as fit, one at
/// least. A list narrower than a thumbnail gets a single narrower column.
pub fn metrics(avail: f32, thumb_w: f32) -> Metrics {
    let thumb_w = clamp_width(thumb_w);
    let inner = (avail - 2.0 * SIDE_PAD).max(1.0);
    let cols = (((inner + GAP) / (thumb_w + GAP)).floor() as usize).max(1);
    let cell_w = thumb_w.min(inner);
    let used = cols as f32 * cell_w + (cols - 1) as f32 * GAP;
    Metrics {
        cols,
        cell_w,
        left: SIDE_PAD + ((inner - used) / 2.0).max(0.0),
    }
}

/// The height of a page of `size` drawn `cell_w` wide. A page without a usable size is drawn
/// like an A4 sheet; a very narrow or very flat one is limited so that no row becomes absurd.
pub fn thumb_height(size: Size, cell_w: f32) -> f32 {
    let aspect = if size.w.is_finite() && size.h.is_finite() && size.w > 0.0 && size.h > 0.0 {
        size.h / size.w
    } else {
        FALLBACK_ASPECT
    };
    cell_w * aspect.clamp(MIN_ASPECT, MAX_ASPECT)
}

/// The rows of a grid of cells with their heights summed: the n-th cell is found by bisection.
/// A row is as high as the highest thumbnail in it plus the label and the gap.
#[derive(Clone, Debug, PartialEq)]
pub struct Grid {
    cols: usize,
    /// The height of the thumbnail of every cell.
    heights: Vec<f32>,
    /// The top of every row, and at the end the total: one more entry than there are rows.
    tops: Vec<f32>,
}

impl Grid {
    /// The grid of cells whose thumbnails are `heights` high, `cols` to a row, with labels
    /// `caption_h` high.
    pub fn build(heights: Vec<f32>, cols: usize, caption_h: f32) -> Grid {
        let cols = cols.max(1);
        let mut tops = Vec::with_capacity(heights.len().div_ceil(cols) + 1);
        let mut top = 0.0f32;
        tops.push(top);
        for row in heights.chunks(cols) {
            let tallest = row.iter().copied().fold(0.0f32, f32::max);
            top += tallest + CAPTION_GAP + caption_h + GAP;
            tops.push(top);
        }
        Grid {
            cols,
            heights,
            tops,
        }
    }

    /// The number of cells.
    pub fn len(&self) -> usize {
        self.heights.len()
    }

    /// There is no cell.
    pub fn is_empty(&self) -> bool {
        self.heights.is_empty()
    }

    /// The number of rows.
    pub fn rows(&self) -> usize {
        self.tops.len() - 1
    }

    /// The height of everything.
    pub fn total_height(&self) -> f32 {
        self.tops.last().copied().unwrap_or(0.0)
    }

    /// The row and the column of cell `n`.
    pub fn slot(&self, n: usize) -> (usize, usize) {
        (n / self.cols, n % self.cols)
    }

    /// The top of row `row`.
    pub fn row_top(&self, row: usize) -> f32 {
        self.tops
            .get(row)
            .copied()
            .unwrap_or_else(|| self.total_height())
    }

    /// The bottom of row `row`, with the gap under it.
    pub fn row_bottom(&self, row: usize) -> f32 {
        self.row_top(row + 1)
    }

    /// The height of the thumbnail of cell `n`.
    pub fn thumb_h(&self, n: usize) -> f32 {
        self.heights.get(n).copied().unwrap_or(0.0)
    }

    /// The cells of the rows that overlap the part of the list from `top` to `top + height`.
    pub fn visible(&self, top: f32, height: f32) -> Range<usize> {
        let rows = self.rows();
        if rows == 0 {
            return 0..0;
        }
        let bottom = top + height.max(0.0);
        // The last row that starts at or above `top` is the first that is seen.
        let first = self
            .tops
            .partition_point(|t| *t <= top)
            .saturating_sub(1)
            .min(rows);
        // Rows that start below the bottom are not seen.
        let end = self.tops[..rows]
            .partition_point(|t| *t < bottom)
            .max(first);
        (first * self.cols).min(self.len())..(end * self.cols).min(self.len())
    }

    /// The scroll offset that shows cell `n` ([`reveal_offset`] for its row); `None` when it is
    /// shown already or when the grid has no such cell.
    pub fn scroll_to(&self, n: usize, offset: f32, view_h: f32) -> Option<f32> {
        if n >= self.len() {
            return None;
        }
        let row = self.slot(n).0;
        reveal_offset(self.row_top(row), self.row_bottom(row), offset, view_h)
    }
}

/// The scroll offset that shows the band from `top` to `bottom` in a viewport `view_h` high that
/// is scrolled to `offset`: `None` when the whole band is visible already; else the nearest
/// offset (the band at the top edge when it has to move up or is higher than the viewport, at
/// the bottom edge when it has to move down). A viewport of unknown height (0) puts the band at
/// the top.
pub fn reveal_offset(top: f32, bottom: f32, offset: f32, view_h: f32) -> Option<f32> {
    if view_h > 0.0 && top >= offset && bottom <= offset + view_h {
        return None;
    }
    if view_h <= 0.0 || top < offset || bottom - top > view_h {
        return Some(top.max(0.0));
    }
    Some((bottom - view_h).max(0.0))
}

/// Whether the list scrolls to the current page now, and where to. It follows the reader when
/// the current page (or what is listed) changed since it last did and the pointer is not on the
/// list (the user is browsing it by hand). `None` when it stays where it is.
pub fn follow_offset(
    grid: &Grid,
    current: Option<usize>,
    offset: f32,
    view_h: f32,
    changed: bool,
    pointer_on_list: bool,
) -> Option<f32> {
    if !changed || pointer_on_list {
        return None;
    }
    grid.scroll_to(current?, offset, view_h)
}

// ---------------------------------------------------------------------------------------------
// Pure logic: which pages are listed
// ---------------------------------------------------------------------------------------------

/// Whose search the pages with results come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The search of the tab's own document (the search bar).
    InDocument,
    /// The search in all open documents.
    AllDocuments,
}

/// The pages a search found something on, with how many matches start on each, in page order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hits {
    /// The search the hits come from.
    pub source: Source,
    /// (page, matches), each page once, ascending.
    pub pages: Vec<(u32, u32)>,
}

/// The pages of `matches` with their numbers of matches (a match belongs to the page where it
/// starts), ascending. Pages from `page_count` on are dropped. Matches arrive in document order,
/// so this is one pass; a list that is not in order is sorted first.
pub fn page_counts(matches: &[SearchMatch], page_count: u32) -> Vec<(u32, u32)> {
    let mut pages: Vec<u32> = matches
        .iter()
        .map(|m| m.page)
        .filter(|p| *p < page_count)
        .collect();
    if !pages.is_sorted() {
        pages.sort_unstable();
    }
    let mut out: Vec<(u32, u32)> = Vec::new();
    for page in pages {
        match out.last_mut() {
            Some((last, n)) if *last == page => *n += 1,
            _ => out.push((page, 1)),
        }
    }
    out
}

/// The pages to keep when only pages with results are shown: those of the tab's own search when
/// it has results, else those of the search in all documents for this document. `None` when
/// neither has a result on a page of the document.
pub fn hits(
    in_document: &[SearchMatch],
    all_documents: Option<&[SearchMatch]>,
    page_count: u32,
) -> Option<Hits> {
    let own = page_counts(in_document, page_count);
    if !own.is_empty() {
        return Some(Hits {
            source: Source::InDocument,
            pages: own,
        });
    }
    let all = page_counts(all_documents?, page_count);
    (!all.is_empty()).then_some(Hits {
        source: Source::AllDocuments,
        pages: all,
    })
}

/// The cells of the list: every page of the document, or only the pages with hits.
#[derive(Clone, Copy, Debug)]
pub struct Listing<'a> {
    total: u32,
    hits: Option<&'a [(u32, u32)]>,
}

impl<'a> Listing<'a> {
    /// Every page of a document of `total` pages.
    pub fn all(total: u32) -> Listing<'a> {
        Listing { total, hits: None }
    }

    /// Only these pages (see [`Hits::pages`]).
    pub fn only(hits: &'a [(u32, u32)]) -> Listing<'a> {
        Listing {
            total: 0,
            hits: Some(hits),
        }
    }

    /// The number of cells.
    pub fn len(&self) -> usize {
        self.hits.map_or(self.total as usize, <[_]>::len)
    }

    /// There is no cell.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The page in cell `n`.
    pub fn page(&self, n: usize) -> Option<u32> {
        match self.hits {
            Some(h) => h.get(n).map(|(p, _)| *p),
            None => (n < self.total as usize).then_some(n as u32),
        }
    }

    /// The number of matches on the page in cell `n`; `None` when every page is listed.
    pub fn count(&self, n: usize) -> Option<u32> {
        self.hits?.get(n).map(|(_, c)| *c)
    }

    /// The cell of `page` and whether it is exactly that page: with only some pages listed, a
    /// page that is not among them gives the first listed page after it (else the last one).
    pub fn position(&self, page: u32) -> Option<(usize, bool)> {
        if self.is_empty() {
            return None;
        }
        match self.hits {
            None => {
                let n = (page as usize).min(self.len() - 1);
                Some((n, n == page as usize))
            }
            Some(h) => {
                let at = h.partition_point(|(p, _)| *p < page);
                let n = at.min(h.len() - 1);
                Some((n, h.get(n).is_some_and(|(p, _)| *p == page)))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The state
// ---------------------------------------------------------------------------------------------

/// What the list followed last: it scrolls again when this changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FollowKey {
    tab: TabId,
    page: u32,
    /// Only pages with results were listed.
    filtered: bool,
}

/// What a cached grid was built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridKey {
    doc: DocId,
    generation: u32,
    pages: u32,
    cols: usize,
    cell_w: u32,
    caption_h: u32,
}

/// The Pages view of one window. Not saved.
#[derive(Clone, Debug)]
pub struct ThumbState {
    /// Width of a thumbnail ([`WIDTH_RANGE`]).
    pub width: f32,
    /// Only pages with search results are listed (when a search has any).
    pub only_matches: bool,
    /// The scroll offset of the list as of the last frame.
    offset: f32,
    /// The height of the list as of the last frame.
    view_h: f32,
    followed: Option<FollowKey>,
    /// The grid of all pages, kept while nothing it depends on changes.
    grid: Option<(GridKey, Arc<Grid>)>,
}

impl Default for ThumbState {
    fn default() -> ThumbState {
        ThumbState {
            width: DEFAULT_WIDTH,
            only_matches: false,
            offset: 0.0,
            view_h: 0.0,
            followed: None,
            grid: None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------------------------

/// The accessible name of a cell: "Page 12", or "Page 12, 3 results" when the cell has a count.
pub fn cell_label(label: &str, count: Option<u32>) -> String {
    let page = tr("link.page").replace("{page}", label);
    match count {
        None => page,
        Some(n) => {
            let key = if n == 1 {
                "pages.item_count_one"
            } else {
                "pages.item_count"
            };
            tr(key)
                .replace("{page}", &page)
                .replace("{n}", &n.to_string())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------------------------

/// What [`show_view`] reads besides its own state.
pub struct Env<'a> {
    /// The tabs.
    pub tabs: &'a TabStore,
    /// The open documents.
    pub docs: &'a DocStore,
    /// The texture cache the thumbnails are taken from.
    pub textures: &'a mut TextureCache,
    /// The search in all documents.
    pub global_search: &'a crate::panels::global_search::GlobalSearch,
    /// The focused tab of the window.
    pub focus_tab: Option<TabId>,
    /// The window.
    pub window: WindowId,
}

fn hint(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).weak());
}

/// Draws the Pages view into `ui` (the sidebar below its view switch): the controls and the
/// grid of the document of the focused tab.
pub fn show_view(ui: &mut Ui, thumbs: &mut ThumbState, env: Env<'_>, queue: &mut Vec<Action>) {
    let Env {
        tabs,
        docs,
        textures,
        global_search,
        focus_tab,
        window,
    } = env;
    let Some((tab, Tab::Pdf(pdf))) = focus_tab.and_then(|t| Some((t, tabs.get(t)?))) else {
        hint(ui, tr("pages.no_doc"));
        return;
    };
    let Some(info) = docs.get(pdf.doc).and_then(|d| d.info()) else {
        hint(ui, tr("pages.loading"));
        return;
    };
    let page_count = info.page_sizes.len() as u32;
    let found = hits(
        &pdf.ui.search.snapshot().matches,
        global_search.matches(pdf.doc),
        page_count,
    );
    let filtered = thumbs.only_matches && found.is_some();

    controls(ui, thumbs, found.as_ref());

    // What is listed and how it is laid out.
    let list_rect = ui.available_rect_before_wrap();
    let avail = (list_rect.width() - ui.spacing().scroll.allocated_width()).max(0.0);
    let m = metrics(avail, thumbs.width);
    let caption_h = ui.text_style_height(&TextStyle::Small) + 2.0;
    let listing = match (&found, filtered) {
        (Some(h), true) => Listing::only(&h.pages),
        _ => Listing::all(page_count),
    };
    let size_of = |page: u32| {
        info.page_sizes
            .get(page as usize)
            .copied()
            .unwrap_or(Size::new(0.0, 0.0))
    };
    let grid = match (filtered, thumbs.grid.as_ref()) {
        (false, Some((key, grid))) if *key == grid_key(pdf.doc, &info, m, caption_h) => {
            Arc::clone(grid)
        }
        _ => {
            let heights = (0..listing.len())
                .map(|n| thumb_height(size_of(listing.page(n).unwrap_or(0)), m.cell_w))
                .collect();
            let grid = Arc::new(Grid::build(heights, m.cols, caption_h));
            if !filtered {
                thumbs.grid = Some((grid_key(pdf.doc, &info, m, caption_h), Arc::clone(&grid)));
            }
            grid
        }
    };

    if grid.is_empty() {
        return;
    }

    // The page the view is at, and where the list has to go to show it.
    let current_page = pdf.view.anchor.page.min(page_count.saturating_sub(1));
    let position = listing.position(current_page);
    let key = FollowKey {
        tab,
        page: current_page,
        filtered,
    };
    let changed = thumbs.followed != Some(key);
    thumbs.followed = Some(key);
    let follow = follow_offset(
        &grid,
        position.map(|(n, _)| n),
        thumbs.offset,
        thumbs.view_h,
        changed,
        ui.rect_contains_pointer(list_rect),
    );

    let mut area = ScrollArea::vertical()
        .id_salt(("thumbnails", window))
        .content_margin(egui::Margin::ZERO)
        .auto_shrink([false, false]);
    if let Some(offset) = follow {
        area = area.vertical_scroll_offset(offset);
    }
    let ppp = ui.ctx().pixels_per_point();
    let output = area.show_viewport(ui, |ui, viewport| {
        let (_, whole) = ui.allocate_space(vec2(avail, grid.total_height()));
        for n in grid.visible(viewport.min.y, viewport.height()) {
            let Some(page) = listing.page(n) else {
                continue;
            };
            let (row, col) = grid.slot(n);
            let at = Pos2::new(
                whole.left() + m.left + col as f32 * (m.cell_w + GAP),
                whole.top() + grid.row_top(row),
            );
            let thumb = Rect::from_min_size(at, vec2(m.cell_w, grid.thumb_h(n)));
            let size = size_of(page);
            let scale_milli = if size.w > 0.0 && size.w.is_finite() {
                ((m.cell_w * ppp / size.w * 1000.0).round() as u32).max(1)
            } else {
                1000
            };
            let texture = textures
                .nearest(pdf.doc, page, info.generation, scale_milli)
                .map(|t| t.id());
            let label = page_label(page, &info.page_labels);
            let cell = Cell {
                id: ui.make_persistent_id(("thumbnail", page)),
                thumb,
                caption_h,
                label: &label,
                count: listing.count(n),
                current: position == Some((n, true)),
                texture,
            };
            if let Some(how) = draw_cell(ui, &cell) {
                queue.push(Action::Navigate {
                    tab,
                    to: Location::new(page, 0.0, 0.0),
                    how,
                });
            }
        }
    });
    thumbs.offset = output.state.clamped_offset().y;
    thumbs.view_h = output.inner_rect.height();
}

fn grid_key(doc: DocId, info: &refdesk_core::DocumentInfo, m: Metrics, caption_h: f32) -> GridKey {
    GridKey {
        doc,
        generation: info.generation,
        pages: info.page_sizes.len() as u32,
        cols: m.cols,
        cell_w: m.cell_w.to_bits(),
        caption_h: caption_h.to_bits(),
    }
}

/// The row of controls above the grid: smaller and larger thumbnails, and the toggle that keeps
/// only the pages with search results.
fn controls(ui: &mut Ui, thumbs: &mut ThumbState, found: Option<&Hits>) {
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        let buttons = [
            ("−", -1, thumbs.width > WIDTH_RANGE.0, "pages.smaller"),
            ("+", 1, thumbs.width < WIDTH_RANGE.1, "pages.larger"),
        ];
        for (text, steps, enabled, name) in buttons {
            let response = ui.add_enabled(
                enabled,
                Button::new(text).min_size(vec2(ui.spacing().interact_size.y, 0.0)),
            );
            response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, enabled, tr(name)));
            if response.on_hover_text(tr(name)).clicked() {
                thumbs.width = stepped_width(thumbs.width, steps);
            }
        }
        let available = found.is_some();
        let mut on = thumbs.only_matches && available;
        let tip = match found.map(|h| h.source) {
            Some(Source::InDocument) => tr("pages.only_matches_doc"),
            Some(Source::AllDocuments) => tr("pages.only_matches_all"),
            None => tr("pages.only_matches_none"),
        };
        let response = ui
            .add_enabled(available, Checkbox::new(&mut on, tr("pages.only_matches")))
            .on_hover_text(tip)
            .on_disabled_hover_text(tip);
        if response.changed() {
            thumbs.only_matches = on;
        }
    });
}

/// What [`draw_cell`] draws.
struct Cell<'a> {
    id: Id,
    /// Where the thumbnail goes; the label goes under it.
    thumb: Rect,
    caption_h: f32,
    label: &'a str,
    /// Matches on the page, when the list shows them.
    count: Option<u32>,
    /// The view is at this page.
    current: bool,
    texture: Option<egui::TextureId>,
}

/// Draws one cell and returns how the user asked to open its page, if they did (a click: here; a
/// middle click or Ctrl: a new tab; Ctrl+Shift: beside, as for a link).
fn draw_cell(ui: &mut Ui, cell: &Cell<'_>) -> Option<OpenHow> {
    let hit = Rect::from_min_size(
        cell.thumb.min,
        vec2(
            cell.thumb.width(),
            cell.thumb.height() + CAPTION_GAP + cell.caption_h,
        ),
    );
    let response = ui
        .interact(hit, cell.id, Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    response.widget_info(|| {
        WidgetInfo::labeled(egui::Role::Button, true, cell_label(cell.label, cell.count))
    });
    let visuals = ui.visuals().clone();
    let painter = ui.painter();
    match cell.texture {
        Some(texture) => {
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            painter.image(texture, cell.thumb, uv, Color32::WHITE);
        }
        // Not rendered yet: an empty sheet of the page's shape.
        None => {
            painter.rect_filled(cell.thumb, 0.0, Color32::WHITE);
        }
    }
    let outline = if cell.current {
        Stroke::new(2.0, visuals.selection.stroke.color)
    } else if response.hovered() {
        visuals.widgets.hovered.fg_stroke
    } else {
        Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color)
    };
    painter.rect_stroke(cell.thumb, 0.0, outline, StrokeKind::Outside);

    if let Some(n) = cell.count {
        let font = ui.style().text_styles[&TextStyle::Small].clone();
        let galley = painter.layout_no_wrap(n.to_string(), font, visuals.selection.stroke.color);
        let pad = vec2(4.0, 1.0);
        let badge = Rect::from_min_size(
            Pos2::new(
                cell.thumb.right() - galley.size().x - 2.0 * pad.x - 2.0,
                cell.thumb.top() + 2.0,
            ),
            galley.size() + 2.0 * pad,
        );
        painter.rect_filled(badge, 3.0, visuals.selection.bg_fill);
        painter.galley(badge.min + pad, galley, visuals.selection.stroke.color);
    }

    let caption = Rect::from_min_size(
        Pos2::new(cell.thumb.left(), cell.thumb.bottom() + CAPTION_GAP),
        vec2(cell.thumb.width(), cell.caption_h),
    );
    let font = ui.style().text_styles[&TextStyle::Small].clone();
    let color = if cell.current {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    let galley = ui
        .painter()
        .layout_job(line_job(cell.label, font, color, caption.width()));
    let center = Align2::CENTER_TOP.align_size_within_rect(galley.size(), caption);
    ui.painter().galley(center.min, galley, color);

    let button = if response.middle_clicked() {
        ClickButton::Middle
    } else if response.clicked() {
        ClickButton::Primary
    } else {
        return None;
    };
    let mods = ui.input(|i| ClickMods {
        ctrl: i.modifiers.ctrl || i.modifiers.command,
        shift: i.modifiers.shift,
    });
    Some(open_how(button, mods))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the width ----------------------------------------------------------------------------

    #[test]
    fn the_width_steps_within_its_range() {
        assert_eq!(clamp_width(f32::NAN), DEFAULT_WIDTH);
        assert_eq!(clamp_width(10.0), WIDTH_RANGE.0);
        assert_eq!(clamp_width(1000.0), WIDTH_RANGE.1);
        assert_eq!(stepped_width(120.0, 1), 140.0);
        assert_eq!(stepped_width(120.0, -1), 100.0);
        assert_eq!(stepped_width(WIDTH_RANGE.0, -1), WIDTH_RANGE.0);
        assert_eq!(stepped_width(WIDTH_RANGE.1, 1), WIDTH_RANGE.1);
        // From the default the range is reached after a few presses and never left.
        let mut w = DEFAULT_WIDTH;
        for _ in 0..50 {
            w = stepped_width(w, 1);
        }
        assert_eq!(w, WIDTH_RANGE.1);
    }

    // --- columns and rows ---------------------------------------------------------------------

    #[test]
    fn columns_follow_the_width_of_the_list_and_of_the_thumbnails() {
        // 240 wide, 120 thumbnails: two do not fit with the pad and the gap.
        assert_eq!(metrics(240.0, 120.0).cols, 1);
        // 2 * 120 + 6 + 2 * 3 = 252.
        assert_eq!(metrics(252.0, 120.0).cols, 2);
        assert_eq!(metrics(251.0, 120.0).cols, 1);
        assert_eq!(metrics(400.0, 80.0).cols, 4);
        assert_eq!(metrics(400.0, 320.0).cols, 1);
        // Never zero columns, even for a list of no width.
        assert_eq!(metrics(0.0, 120.0).cols, 1);
        assert_eq!(metrics(-5.0, 120.0).cols, 1);
        // A list narrower than a thumbnail shrinks the one column to fit.
        let narrow = metrics(60.0, 120.0);
        assert_eq!(narrow.cols, 1);
        assert!((narrow.cell_w - (60.0 - 2.0 * SIDE_PAD)).abs() < 1e-4);
        // A thumbnail width out of range counts as the nearest in range.
        assert_eq!(metrics(1000.0, 5.0), metrics(1000.0, WIDTH_RANGE.0));
    }

    #[test]
    fn the_cells_are_centered_and_always_inside_the_list() {
        for avail in [0.0, 50.0, 200.0, 252.0, 300.0, 777.0] {
            for width in [80.0, 120.0, 200.0, 320.0] {
                let m = metrics(avail, width);
                let right = m.left + m.cols as f32 * m.cell_w + (m.cols - 1) as f32 * GAP;
                assert!(m.left >= SIDE_PAD - 1e-4, "{avail} {width}: {m:?}");
                if avail > 2.0 * SIDE_PAD + 1.0 {
                    assert!(right <= avail - SIDE_PAD + 1e-3, "{avail} {width}: {m:?}");
                    // Centered: the room left and right of the cells is the same.
                    assert!(((avail - right) - m.left).abs() < 1e-3, "{avail} {width}");
                }
            }
        }
    }

    #[test]
    fn a_page_is_as_high_as_its_shape_says_within_limits() {
        let a4 = Size::new(595.0, 842.0);
        assert!((thumb_height(a4, 120.0) - 120.0 * 842.0 / 595.0).abs() < 1e-3);
        assert!(
            (thumb_height(Size::new(842.0, 595.0), 120.0) - 120.0 * 595.0 / 842.0).abs() < 1e-3
        );
        // No usable size: A4-like.
        for bad in [
            Size::new(0.0, 0.0),
            Size::new(f32::NAN, 10.0),
            Size::new(10.0, f32::INFINITY),
            Size::new(-1.0, 5.0),
        ] {
            let h = thumb_height(bad, 100.0);
            assert!((h - 141.4).abs() < 1e-3, "{bad:?}: {h}");
        }
        // A very long and a very flat page are limited.
        assert_eq!(thumb_height(Size::new(10.0, 10_000.0), 100.0), 400.0);
        assert_eq!(thumb_height(Size::new(10_000.0, 10.0), 100.0), 25.0);
    }

    #[test]
    fn a_row_is_as_high_as_its_highest_thumbnail() {
        // Portrait, landscape, portrait | landscape, landscape | tall.
        let heights = vec![140.0, 80.0, 140.0, 80.0, 80.0, 200.0];
        let grid = Grid::build(heights, 3, 12.0);
        let extra = CAPTION_GAP + 12.0 + GAP;
        assert_eq!(grid.rows(), 2);
        assert_eq!(grid.row_top(0), 0.0);
        assert!((grid.row_top(1) - (140.0 + extra)).abs() < 1e-4);
        assert!((grid.total_height() - (140.0 + extra + 200.0 + extra)).abs() < 1e-4);
        // The second row has two landscapes and a tall page: the tall one decides.
        assert!((grid.row_bottom(1) - grid.total_height()).abs() < 1e-4);
        assert_eq!(grid.slot(0), (0, 0));
        assert_eq!(grid.slot(4), (1, 1));
        assert_eq!(grid.slot(5), (1, 2));
        assert_eq!(grid.thumb_h(1), 80.0);
        assert_eq!(grid.thumb_h(99), 0.0);
    }

    #[test]
    fn a_last_row_that_is_not_full_counts_only_its_own_pages() {
        let grid = Grid::build(vec![100.0, 100.0, 100.0, 100.0, 50.0], 2, 10.0);
        let extra = CAPTION_GAP + 10.0 + GAP;
        assert_eq!(grid.rows(), 3);
        assert!((grid.row_top(2) - 2.0 * (100.0 + extra)).abs() < 1e-4);
        assert!((grid.total_height() - (200.0 + 3.0 * extra + 50.0 - 0.0)).abs() < 1e-3);
        assert_eq!(grid.len(), 5);
    }

    #[test]
    fn an_empty_grid_has_no_height_and_nothing_visible() {
        let grid = Grid::build(Vec::new(), 3, 10.0);
        assert!(grid.is_empty());
        assert_eq!(grid.rows(), 0);
        assert_eq!(grid.total_height(), 0.0);
        assert_eq!(grid.visible(0.0, 500.0), 0..0);
        assert_eq!(grid.scroll_to(0, 0.0, 100.0), None);
    }

    // --- the visible cells --------------------------------------------------------------------

    /// 10 rows of 3 cells, each row 100 high (thumbnail 80 + 2 + 12 + 6).
    fn ten_rows() -> Grid {
        Grid::build(vec![80.0; 30], 3, 12.0)
    }

    #[test]
    fn the_visible_cells_are_those_of_the_rows_that_overlap_the_view() {
        let grid = ten_rows();
        assert_eq!(grid.total_height(), 1000.0);
        // From the top: rows 0 and 1 fill 200; row 2 starts at 200 (the bottom edge).
        assert_eq!(grid.visible(0.0, 200.0), 0..6);
        assert_eq!(grid.visible(0.0, 200.5), 0..9);
        // Scrolled into the middle of a row: that row and the ones below.
        assert_eq!(grid.visible(150.0, 100.0), 3..9);
        // A row that ends exactly at the top edge is not seen.
        assert_eq!(grid.visible(100.0, 100.0), 3..6);
        // At the very end.
        assert_eq!(grid.visible(900.0, 300.0), 27..30);
        // Past the end (an offset that was not clamped yet): nothing.
        assert_eq!(grid.visible(5000.0, 300.0), 30..30);
        // A view of no height still shows the row at its top.
        assert_eq!(grid.visible(50.0, 0.0), 0..3);
    }

    #[test]
    fn a_last_row_that_is_not_full_has_no_cells_past_the_end() {
        let grid = Grid::build(vec![80.0; 7], 3, 12.0);
        assert_eq!(grid.visible(0.0, 10_000.0), 0..7);
        assert_eq!(grid.visible(250.0, 100.0), 6..7);
    }

    #[test]
    fn thousands_of_pages_are_looked_up_in_a_bisection() {
        // Mixed sizes: portrait and landscape alternate; 6000 pages, 4 columns.
        let heights: Vec<f32> = (0..6000)
            .map(|n| if n % 7 == 3 { 90.0 } else { 160.0 })
            .collect();
        let started = std::time::Instant::now();
        let grid = Grid::build(heights, 4, 12.0);
        let mut seen = 0usize;
        for step in 0..2000 {
            let top = step as f32 * 3.7;
            let range = grid.visible(top, 700.0);
            // Only what the view needs: a few rows, whatever the size of the document.
            assert!(range.len() <= 4 * 8, "{top}: {range:?}");
            seen += range.len();
        }
        assert!(seen > 0);
        // Generous: it takes microseconds; a scan of all rows per lookup would take far more.
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "took {:?}",
            started.elapsed()
        );
        // The last page is in the last row.
        let last = grid.rows() - 1;
        assert_eq!(grid.slot(5999).0, last);
        assert_eq!(grid.visible(grid.total_height() - 1.0, 10.0).end, 6000);
    }

    #[test]
    fn the_grid_of_a_big_document_is_not_walked_cell_by_cell_per_frame() {
        // Where the n-th page is comes from the summed rows: rows are contiguous.
        let grid = Grid::build(vec![100.0; 5000], 5, 10.0);
        for n in [0usize, 1, 4, 5, 999, 4999] {
            let (row, _) = grid.slot(n);
            assert!(grid.row_top(row) <= grid.row_bottom(row));
            let seen = grid.visible(grid.row_top(row) + 1.0, 1.0);
            assert!(seen.contains(&n), "{n} in {seen:?}");
        }
    }

    // --- scrolling to a page ------------------------------------------------------------------

    #[test]
    fn a_band_that_is_visible_stays_and_one_that_is_not_scrolls_the_least() {
        // Viewport 300 high at offset 200: shows 200..500.
        assert_eq!(reveal_offset(250.0, 350.0, 200.0, 300.0), None);
        assert_eq!(
            reveal_offset(200.0, 500.0, 200.0, 300.0),
            None,
            "exactly fits"
        );
        // Above: its top goes to the top edge.
        assert_eq!(reveal_offset(100.0, 200.0, 200.0, 300.0), Some(100.0));
        // Partly above.
        assert_eq!(reveal_offset(150.0, 250.0, 200.0, 300.0), Some(150.0));
        // Below: its bottom goes to the bottom edge.
        assert_eq!(reveal_offset(600.0, 700.0, 200.0, 300.0), Some(400.0));
        assert_eq!(reveal_offset(450.0, 550.0, 200.0, 300.0), Some(250.0));
        // Higher than the view: the top.
        assert_eq!(reveal_offset(600.0, 1000.0, 200.0, 300.0), Some(600.0));
        // Unknown view height: the top of the band; never negative.
        assert_eq!(reveal_offset(80.0, 180.0, 0.0, 0.0), Some(80.0));
        assert_eq!(reveal_offset(-5.0, 50.0, 100.0, 300.0), Some(0.0));
    }

    #[test]
    fn the_list_follows_the_current_page_by_its_row() {
        let grid = ten_rows();
        // Page 14 is in row 4, 400..500; the view shows 0..300.
        assert_eq!(grid.scroll_to(14, 0.0, 300.0), Some(200.0));
        // The view shows 300..600: it is in view.
        assert_eq!(grid.scroll_to(14, 300.0, 300.0), None);
        // Page 2 shares row 0 with page 0.
        assert_eq!(grid.scroll_to(2, 300.0, 300.0), Some(0.0));
        assert_eq!(grid.scroll_to(30, 0.0, 300.0), None, "no such cell");
    }

    #[test]
    fn it_follows_only_when_something_changed_and_the_pointer_is_not_on_the_list() {
        let grid = ten_rows();
        let go = |changed, pointer| follow_offset(&grid, Some(14), 0.0, 300.0, changed, pointer);
        assert_eq!(go(true, false), Some(200.0));
        assert_eq!(
            go(false, false),
            None,
            "nothing changed: the user may have scrolled away"
        );
        assert_eq!(go(true, true), None, "the user is browsing the list");
        assert_eq!(
            follow_offset(&grid, None, 0.0, 300.0, true, false),
            None,
            "no current page in the list"
        );
        // A current page that is in view stays.
        assert_eq!(
            follow_offset(&grid, Some(14), 300.0, 300.0, true, false),
            None
        );
    }

    // --- the pages with results ---------------------------------------------------------------

    fn hit(page: u32) -> SearchMatch {
        SearchMatch {
            page,
            chars: 0..1,
            rects: Vec::new(),
            cont: None,
            before: String::new(),
            matched: "x".into(),
            after: String::new(),
        }
    }

    fn hits_on(pages: &[u32]) -> Vec<SearchMatch> {
        pages.iter().map(|p| hit(*p)).collect()
    }

    #[test]
    fn matches_are_counted_per_page_in_page_order() {
        assert_eq!(page_counts(&hits_on(&[]), 10), vec![]);
        assert_eq!(
            page_counts(&hits_on(&[1, 1, 1, 4, 7, 7]), 10),
            vec![(1, 3), (4, 1), (7, 2)]
        );
        // Not in order: sorted, and still counted once per page.
        assert_eq!(
            page_counts(&hits_on(&[7, 1, 7, 1, 4]), 10),
            vec![(1, 2), (4, 1), (7, 2)]
        );
        // Pages the document does not have are dropped.
        assert_eq!(
            page_counts(&hits_on(&[2, 9, 10, 50]), 10),
            vec![(2, 1), (9, 1)]
        );
        assert_eq!(page_counts(&hits_on(&[0]), 0), vec![]);
    }

    #[test]
    fn the_search_of_the_tab_comes_before_the_search_in_all_documents() {
        let own = hits_on(&[1, 1, 3]);
        let all = hits_on(&[0, 2, 2, 2]);
        let both = hits(&own, Some(&all), 10).expect("hits");
        assert_eq!(both.source, Source::InDocument);
        assert_eq!(both.pages, vec![(1, 2), (3, 1)]);
        // Without a result of its own the search in all documents counts.
        let fallback = hits(&[], Some(&all), 10).expect("hits");
        assert_eq!(fallback.source, Source::AllDocuments);
        assert_eq!(fallback.pages, vec![(0, 1), (2, 3)]);
        // Its own search with results only past the end of the document does not count either.
        let outside = hits(&hits_on(&[40]), Some(&all), 10).expect("hits");
        assert_eq!(outside.source, Source::AllDocuments);
    }

    #[test]
    fn without_any_result_there_is_nothing_to_filter_by() {
        assert_eq!(hits(&[], None, 10), None);
        assert_eq!(hits(&[], Some(&[]), 10), None);
        assert_eq!(hits(&hits_on(&[12]), Some(&hits_on(&[13])), 10), None);
    }

    #[test]
    fn a_listing_of_every_page_or_of_the_pages_with_hits() {
        let all = Listing::all(5);
        assert_eq!(all.len(), 5);
        assert_eq!(
            (all.page(0), all.page(4), all.page(5)),
            (Some(0), Some(4), None)
        );
        assert_eq!(all.count(2), None, "no counts when every page is listed");
        assert_eq!(all.position(3), Some((3, true)));
        assert_eq!(
            all.position(99),
            Some((4, false)),
            "past the end: the last page"
        );
        assert_eq!(Listing::all(0).position(0), None);

        let pages = [(2, 5), (4, 1), (9, 3)];
        let some = Listing::only(&pages);
        assert_eq!(some.len(), 3);
        assert_eq!(some.page(1), Some(4));
        assert_eq!(some.count(1), Some(1));
        assert_eq!(some.count(3), None);
        assert_eq!(some.position(4), Some((1, true)));
        // A page without hits: the next listed page, not exactly it.
        assert_eq!(some.position(3), Some((1, false)));
        assert_eq!(some.position(0), Some((0, false)));
        assert_eq!(some.position(7), Some((2, false)));
        // Past the last listed page: the last one.
        assert_eq!(some.position(20), Some((2, false)));
        assert_eq!(Listing::only(&[]).position(0), None);
    }

    // --- names --------------------------------------------------------------------------------

    #[test]
    fn a_cell_is_named_by_its_page_label_and_its_results() {
        assert_eq!(
            cell_label("12", None),
            tr("link.page").replace("{page}", "12")
        );
        let page = tr("link.page").replace("{page}", "iv");
        let three = cell_label("iv", Some(3));
        assert_eq!(
            three,
            tr("pages.item_count")
                .replace("{page}", &page)
                .replace("{n}", "3")
        );
        assert!(three.contains('3') && three.starts_with(&page));
        let one = cell_label("iv", Some(1));
        assert_eq!(
            one,
            tr("pages.item_count_one")
                .replace("{page}", &page)
                .replace("{n}", "1")
        );
    }

    // --- with documents, frame by frame ---------------------------------------------------------

    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use refdesk_core::testutil::PdfBuilder;

    use crate::action::{OpenTarget, dispatch, dispatch_queued};
    use crate::commands::{self, MenuGroup};
    use crate::panels::sidebar::SidebarView;
    use crate::state::AppState;
    use crate::views::test_support::{Repaints, TIMEOUT, watch_repaints};
    use crate::views::{preload, textures};
    use crate::windows::show_window;

    /// A folder for the test's documents, removed at the end.
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("refdesk-wp73-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Dir(path)
        }

        /// A document of `pages` pages; "say hello there" is on the pages in `hits`, as many
        /// times as the entry says.
        fn doc(&self, name: &str, pages: usize, hits: &[(usize, usize)]) -> PathBuf {
            let mut b = PdfBuilder::new();
            for page in 0..pages {
                b = b.page(595.0, 842.0, |p| {
                    p.text(72.0, 60.0, 12.0, &format!("heading {page}"));
                    for (at, times) in hits {
                        if *at == page {
                            for k in 0..*times {
                                p.text(72.0, 100.0 + 60.0 * k as f32, 12.0, "say hello there");
                            }
                        }
                    }
                });
            }
            let path = self.0.join(name);
            std::fs::write(&path, b.build()).expect("write pdf");
            path
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Rig {
        ctx: egui::Context,
        repaints: Repaints,
        state: AppState,
        time: f64,
    }

    impl Rig {
        fn new() -> Rig {
            let ctx = egui::Context::default();
            let repaints = watch_repaints(&ctx);
            Rig {
                ctx,
                repaints,
                state: AppState::new(),
                time: 0.0,
            }
        }

        fn open(&mut self, path: &Path) -> TabId {
            let target = OpenTarget {
                path: path.to_path_buf(),
                page: None,
            };
            dispatch(&mut self.state, Action::OpenFiles(vec![target]), &self.ctx);
            let tab = self.state.focus.expect("focused");
            self.frames_until("the layout", |rig| {
                rig.state
                    .tabs
                    .pdf(tab)
                    .is_some_and(|p| p.ui.layout().is_some())
            });
            tab
        }

        /// One frame like the app runs it: the logic steps, the window, the queued actions.
        fn frame(&mut self, events: Vec<egui::Event>) -> Vec<String> {
            self.time += 1.0 / 60.0;
            textures::collect_rendered(&self.ctx, &self.state.docs, &mut self.state.textures);
            preload::step(&self.ctx, &mut self.state);
            crate::panels::global_search::tick(&mut self.state, &self.ctx);
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 700.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let state = &mut self.state;
            let mut queue = Vec::new();
            let output = self
                .ctx
                .run_ui(input, |ui| show_window(ui, state, 0, &mut queue));
            let update = output.platform_output.accesskit_update.clone();
            output.drop_without_applying_deltas();
            self.state.actions.append(&mut queue);
            dispatch_queued(&mut self.state, &self.ctx);
            update
                .map(|u| u.nodes)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(_, n)| {
                    let b = n.bounds()?;
                    let label = n.label().or(n.value())?;
                    Some(format!(
                        "{label}@{},{}",
                        (b.x0 + b.x1) / 2.0,
                        (b.y0 + b.y1) / 2.0
                    ))
                })
                .collect()
        }

        fn frames_until(&mut self, what: &str, mut check: impl FnMut(&Rig) -> bool) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.frame(Vec::new());
                if check(self) {
                    return;
                }
                self.repaints.wait_one(what, deadline);
            }
        }

        /// The names of the nodes of one frame, with the center of each (`name@x,y`).
        fn nodes(&mut self) -> Vec<String> {
            self.ctx.enable_accesskit();
            self.frame(Vec::new())
        }

        /// The center of the node named exactly `name`.
        fn center(&mut self, name: &str) -> Pos2 {
            let nodes = self.nodes();
            nodes
                .iter()
                .find_map(|n| {
                    let (label, at) = n.rsplit_once('@')?;
                    let (x, y) = at.split_once(',')?;
                    (label == name).then_some(())?;
                    Some(Pos2::new(x.parse().ok()?, y.parse().ok()?))
                })
                .unwrap_or_else(|| panic!("no node {name} in {nodes:?}"))
        }

        fn click(&mut self, pos: Pos2) {
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(pos)]);
            self.frame(vec![button(true)]);
            self.frame(vec![button(false)]);
            // The pointer leaves again: the list may follow the view.
            self.frame(vec![egui::Event::PointerGone]);
        }

        fn show_pages(&mut self) {
            dispatch(&mut self.state, Action::ShowPages, &self.ctx);
            self.frame(Vec::new());
        }

        /// The names of the nodes in the sidebar's list, below its controls, top to bottom.
        fn cells(&mut self) -> Vec<String> {
            let mut found: Vec<((f32, f32), String)> = Vec::new();
            for n in self.nodes() {
                let Some((label, at)) = n.rsplit_once('@') else {
                    continue;
                };
                let Some((x, y)) = at.split_once(',') else {
                    continue;
                };
                let (x, y): (f32, f32) = (x.parse().unwrap_or(0.0), y.parse().unwrap_or(0.0));
                let prefix = tr("link.page").replace("{page}", "");
                if x < 250.0
                    && y > 60.0
                    && y < 660.0
                    && label.starts_with(prefix.trim())
                    && !label.contains(" of ")
                {
                    found.push(((y.round(), x), label.to_owned()));
                }
            }
            found.sort_by(|a, b| a.0.0.total_cmp(&b.0.0).then(a.0.1.total_cmp(&b.0.1)));
            found.into_iter().map(|(_, l)| l).collect()
        }
    }

    fn page_name(n: u32) -> String {
        cell_label(&n.to_string(), None)
    }

    #[test]
    fn show_pages_opens_the_sidebar_on_the_pages_view_and_the_cells_are_named_by_page() {
        let dir = Dir::new("names");
        let mut rig = Rig::new();
        rig.open(&dir.doc("a.pdf", 4, &[]));
        rig.state.windows[0].thumbnails.width = WIDTH_RANGE.0;
        assert!(!rig.state.main_window().toc_open);
        rig.show_pages();
        assert!(rig.state.main_window().toc_open);
        assert_eq!(rig.state.main_window().sidebar_view, SidebarView::Pages);
        let cells = rig.cells();
        assert_eq!(cells, (1..=4).map(page_name).collect::<Vec<_>>());
        // The switch has its three buttons, the third named "Pages" (in English).
        let nodes = rig.nodes();
        for key in ["sidebar.files", "sidebar.search", "sidebar.pages"] {
            assert!(
                nodes
                    .iter()
                    .any(|n| n.starts_with(&format!("{}@", tr(key)))),
                "{key} in {nodes:?}"
            );
        }
        // The other views are one click away, and back.
        dispatch(&mut rig.state, Action::ShowExplorer, &rig.ctx);
        assert_eq!(rig.state.main_window().sidebar_view, SidebarView::Files);
        dispatch(&mut rig.state, Action::ShowGlobalSearch, &rig.ctx);
        assert_eq!(rig.state.main_window().sidebar_view, SidebarView::Search);
        let to_pages = rig.center(tr("sidebar.pages"));
        rig.click(to_pages);
        assert_eq!(rig.state.main_window().sidebar_view, SidebarView::Pages);
    }

    #[test]
    fn the_cells_use_the_textures_of_the_cache_and_arrive_as_previews_fill_in() {
        let dir = Dir::new("textures");
        let mut rig = Rig::new();
        let tab = rig.open(&dir.doc("a.pdf", 3, &[]));
        let doc = rig.state.tabs.pdf(tab).expect("pdf").doc;
        rig.show_pages();
        // The preview layer fills itself; the view only reads it.
        rig.frames_until("all previews", |r| r.state.textures.preview_count(doc) == 3);
        rig.frame(Vec::new());
        let generation = rig
            .state
            .docs
            .get(doc)
            .and_then(|d| d.info())
            .expect("info")
            .generation;
        for page in 0..3 {
            assert!(
                rig.state
                    .textures
                    .nearest(doc, page, generation, 200)
                    .is_some(),
                "page {page}"
            );
        }
    }

    #[test]
    fn clicking_a_cell_goes_to_the_top_of_that_page_and_back_returns() {
        let dir = Dir::new("click");
        let mut rig = Rig::new();
        let tab = rig.open(&dir.doc("a.pdf", 5, &[]));
        rig.show_pages();
        assert_eq!(rig.state.tabs.pdf(tab).expect("pdf").view.anchor.page, 0);
        let at = rig.center(&page_name(3));
        rig.click(at);
        let anchor = rig.state.tabs.pdf(tab).expect("pdf").view.anchor;
        assert_eq!((anchor.page, anchor.y), (2, 0.0));
        dispatch(&mut rig.state, Action::Back(tab), &rig.ctx);
        assert_eq!(rig.state.tabs.pdf(tab).expect("pdf").view.anchor.page, 0);
    }

    #[test]
    fn a_ctrl_click_and_a_middle_click_open_the_page_in_a_new_tab() {
        let dir = Dir::new("newtab");
        let mut rig = Rig::new();
        let tab = rig.open(&dir.doc("a.pdf", 5, &[]));
        rig.show_pages();
        let at = rig.center(&page_name(2));
        let before = rig.state.tabs.len();
        let press = |button, modifiers| {
            [true, false].map(|pressed| egui::Event::PointerButton {
                pos: at,
                button,
                pressed,
                modifiers,
            })
        };
        rig.frame(vec![egui::Event::PointerMoved(at)]);
        let [down, up] = press(egui::PointerButton::Middle, egui::Modifiers::NONE);
        rig.frame(vec![down]);
        rig.frame(vec![up]);
        assert_eq!(rig.state.tabs.len(), before + 1, "a new tab");
        // The asking view stays where it was; the new tab is at page 2 (index 1).
        assert_eq!(rig.state.tabs.pdf(tab).expect("pdf").view.anchor.page, 0);
        let new = rig.state.focus.expect("focus");
        assert_ne!(new, tab);
        assert_eq!(rig.state.tabs.pdf(new).expect("pdf").view.anchor.page, 1);

        // Ctrl+click on a cell opens another.
        rig.show_pages();
        let at = rig.center(&page_name(3));
        rig.frame(vec![egui::Event::PointerMoved(at)]);
        let [down, up] = press(egui::PointerButton::Primary, egui::Modifiers::CTRL);
        rig.frame(vec![egui::Event::ModifiersChanged(egui::Modifiers::CTRL)]);
        rig.frame(vec![down]);
        rig.frame(vec![up]);
        assert_eq!(rig.state.tabs.len(), before + 2);
    }

    #[test]
    fn the_filter_keeps_the_pages_with_results_and_names_their_counts() {
        let dir = Dir::new("filter");
        let mut rig = Rig::new();
        rig.open(&dir.doc("a.pdf", 6, &[(1, 2), (4, 1)]));
        rig.state.windows[0].thumbnails.width = WIDTH_RANGE.0;
        rig.show_pages();
        assert_eq!(rig.cells().len(), 6);

        // Without a search the toggle does nothing and every page stays.
        rig.state.windows[0].thumbnails.only_matches = true;
        assert_eq!(rig.cells().len(), 6, "nothing to filter by");

        // The search in all documents has results: they are the filter.
        dispatch(
            &mut rig.state,
            Action::GlobalSearchFor("hello".into()),
            &rig.ctx,
        );
        rig.show_pages();
        rig.frames_until("the search", |r| r.state.global_search.finished());
        let cells = rig.cells();
        assert_eq!(
            cells,
            vec![cell_label("2", Some(2)), cell_label("5", Some(1))]
        );
        assert!(cells[1].contains('1'));

        // Off: all pages again, without counts.
        rig.state.windows[0].thumbnails.only_matches = false;
        assert_eq!(rig.cells().len(), 6);
        // On again, then the search goes: the list follows.
        rig.state.windows[0].thumbnails.only_matches = true;
        assert_eq!(rig.cells().len(), 2);
        dispatch(&mut rig.state, Action::ClearGlobalSearch, &rig.ctx);
        rig.frames_until("the results gone", |r| !r.state.global_search.searched());
        assert_eq!(rig.cells().len(), 6);
    }

    #[test]
    fn the_toggle_can_be_clicked() {
        let dir = Dir::new("toggle");
        let mut rig = Rig::new();
        rig.open(&dir.doc("a.pdf", 6, &[(2, 1)]));
        rig.show_pages();
        dispatch(
            &mut rig.state,
            Action::GlobalSearchFor("hello".into()),
            &rig.ctx,
        );
        rig.show_pages();
        rig.frames_until("the search", |r| r.state.global_search.finished());
        let at = rig.center(tr("pages.only_matches"));
        rig.click(at);
        assert!(rig.state.windows[0].thumbnails.only_matches);
        assert_eq!(rig.cells(), vec![cell_label("3", Some(1))]);
    }

    #[test]
    fn a_comparison_tab_or_the_start_page_shows_a_note_instead_of_pages() {
        let dir = Dir::new("diff");
        let mut rig = Rig::new();
        // The start page is focused.
        rig.show_pages();
        let nodes = rig.nodes();
        assert_eq!(rig.cells(), Vec::<String>::new(), "{nodes:?}");
        assert!(
            nodes.iter().any(|n| n.starts_with(tr("pages.no_doc"))),
            "{nodes:?}"
        );
        // A comparison.
        let (old, new) = (dir.doc("old.pdf", 2, &[]), dir.doc("new.pdf", 2, &[]));
        dispatch(
            &mut rig.state,
            Action::StartDiff {
                left: old,
                right: new,
            },
            &rig.ctx,
        );
        rig.show_pages();
        let nodes = rig.nodes();
        assert!(rig.cells().is_empty());
        assert!(nodes.iter().any(|n| n.starts_with(tr("pages.no_doc"))));
    }

    #[test]
    fn the_list_follows_the_view_but_not_while_the_pointer_is_on_it() {
        let dir = Dir::new("follow");
        let mut rig = Rig::new();
        let tab = rig.open(&dir.doc("a.pdf", 80, &[]));
        rig.show_pages();
        rig.frame(Vec::new());
        let offset = |rig: &Rig| rig.state.windows[0].thumbnails.offset;
        assert_eq!(offset(&rig), 0.0);

        // The view goes far away: the list scrolls to show that page.
        dispatch(&mut rig.state, Action::GoToPage { tab, page: 70 }, &rig.ctx);
        rig.frame(Vec::new());
        rig.frame(Vec::new());
        let followed = offset(&rig);
        assert!(followed > 100.0, "followed to {followed}");
        // The cell of that page is in the list now.
        assert!(rig.cells().contains(&page_name(71)));

        // With the pointer on the list it stays where the user has it.
        let on_list = Pos2::new(60.0, 400.0);
        rig.frame(vec![egui::Event::PointerMoved(on_list)]);
        dispatch(&mut rig.state, Action::GoToPage { tab, page: 2 }, &rig.ctx);
        rig.frame(Vec::new());
        rig.frame(Vec::new());
        assert_eq!(offset(&rig), followed, "the list was left alone");
    }

    #[test]
    fn the_thumbnail_width_is_per_window_and_changes_the_columns() {
        let dir = Dir::new("width");
        let mut rig = Rig::new();
        rig.open(&dir.doc("a.pdf", 3, &[]));
        rig.show_pages();
        let smaller = rig.center(tr("pages.smaller"));
        rig.click(smaller);
        assert_eq!(
            rig.state.windows[0].thumbnails.width,
            DEFAULT_WIDTH - WIDTH_STEP
        );
        let larger = rig.center(tr("pages.larger"));
        rig.click(larger);
        rig.click(larger);
        assert_eq!(
            rig.state.windows[0].thumbnails.width,
            DEFAULT_WIDTH + WIDTH_STEP
        );
    }

    #[test]
    fn the_new_command_is_in_the_view_menu_without_a_shortcut() {
        let command = commands::command("view.show_pages").expect("the command");
        assert_eq!(command.menu, MenuGroup::View);
        assert_eq!(command.shortcut, None);
        let state = AppState::new();
        assert_eq!(command.run(&state), Some(Action::ShowPages));
        assert!(commands::in_menu(MenuGroup::View).any(|c| c.id == "view.show_pages"));
        // Its name exists in both languages.
        assert!(
            crate::i18n::TABLE
                .iter()
                .any(|(k, ko, en)| { *k == command.name_key && !ko.is_empty() && !en.is_empty() })
        );
    }
}
