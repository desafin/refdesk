//! The table of contents panel (TOC-1 ~ TOC-7): a tree beside the tabs of one window that shows
//! the outline of the focused document.
//!
//! Everything that decides something is a plain function or a method of [`TocState`] that does
//! not touch egui, so it is tested without a window (rule R-10): which rows are visible, which
//! section is the current one, what the filter matches, how the collapsed state is saved and
//! restored. [`show`] only draws those results and turns clicks into [`Action`]s.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Key, Pos2, Rect, RichText, ScrollArea, Sense, Shape, Stroke, TextEdit, TextStyle, Ui};
use refdesk_core::{Location, Outline, PageLayout, Point};

use crate::action::{Action, OpenHow};
use crate::i18n::tr;
use crate::panels::status_bar;
use crate::state::{DocStore, PdfTab, Tab, TabId, TabStore};
use crate::views::pdf_view::anchor_at;

/// The width of a panel that has not been resized.
pub const DEFAULT_TOC_WIDTH: f32 = 260.0;
/// The narrowest and the widest the panel can be dragged to.
pub const TOC_WIDTH_RANGE: (f32, f32) = (140.0, 700.0);
/// Each level of nesting moves a row this far to the right.
const INDENT: f32 = 14.0;
/// Rows nested deeper than this are not indented any further, so they stay readable.
const MAX_INDENT_DEPTH: u16 = 12;
/// The section the reader is in is judged at this fraction of the viewport height below its top,
/// so that a heading just scrolled off the top still counts as the current section.
const READING_LINE: f32 = 0.25;

// ---------------------------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------------------------

/// Whether a row has a fold triangle and which way it points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fold {
    /// No children to show: no triangle.
    Leaf,
    /// Its children are shown.
    Open,
    /// It has children that are hidden.
    Closed,
}

/// One visible line of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Index into `Outline::items`.
    pub idx: u32,
    /// Nesting depth, top level 0.
    pub depth: u16,
    /// The fold triangle.
    pub fold: Fold,
    /// Byte ranges of the title the filter matched, for highlighting. Empty without a filter.
    pub matches: Vec<Range<usize>>,
}

/// The entries that start out folded: every entry that has children.
pub fn initial_collapsed(outline: &Outline) -> BTreeSet<u32> {
    (0..outline.items.len() as u32)
        .filter(|&i| outline.has_children(i))
        .collect()
}

/// The lines to show for a set of folded entries, in document order. The children of a folded
/// entry are left out.
pub fn visible_rows(outline: &Outline, collapsed: &BTreeSet<u32>) -> Vec<Row> {
    let mut rows = Vec::new();
    // While set, items deeper than this are inside a folded entry.
    let mut hidden_below: Option<u16> = None;
    for (i, item) in outline.items.iter().enumerate() {
        if let Some(depth) = hidden_below {
            if item.depth > depth {
                continue;
            }
            hidden_below = None;
        }
        let idx = i as u32;
        let fold = if !outline.has_children(idx) {
            Fold::Leaf
        } else if collapsed.contains(&idx) {
            hidden_below = Some(item.depth);
            Fold::Closed
        } else {
            Fold::Open
        };
        rows.push(Row {
            idx,
            depth: item.depth,
            fold,
            matches: Vec::new(),
        });
    }
    rows
}

/// A character compared without regard to case.
fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// The byte ranges of `title` that equal `needle` ignoring case, left to right without overlap.
/// `needle` is already folded with [`fold_char`]. Empty for an empty needle.
pub fn match_ranges(title: &str, needle: &[char]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let hay: Vec<(usize, char)> = title
        .char_indices()
        .map(|(at, c)| (at, fold_char(c)))
        .collect();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()]
            .iter()
            .zip(needle)
            .all(|((_, h), n)| h == n)
        {
            let start = hay[i].0;
            let end = hay.get(i + needle.len()).map_or(title.len(), |(at, _)| *at);
            out.push(start..end);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// What the filter box shows: the entries whose title contains `filter` (ignoring case) and
/// all their ancestors, with the matched parts of the titles. Folding does not apply; every
/// ancestor of a match is open. A blank filter matches nothing here; callers use
/// [`visible_rows`] then.
pub fn filtered_rows(outline: &Outline, filter: &str) -> Vec<Row> {
    let needle: Vec<char> = filter.trim().chars().map(fold_char).collect();
    let n = outline.items.len();
    let mut keep = vec![false; n];
    let mut ranges: Vec<Vec<Range<usize>>> = vec![Vec::new(); n];
    for (i, item) in outline.items.iter().enumerate() {
        let found = match_ranges(&item.title, &needle);
        if found.is_empty() {
            continue;
        }
        ranges[i] = found;
        // Keep the whole chain above. Once an ancestor is kept, everything above it is too.
        let mut cur = i;
        loop {
            if keep[cur] {
                break;
            }
            keep[cur] = true;
            match outline.items[cur].parent {
                // A parent always comes before its child; anything else is damaged data.
                Some(p) if (p as usize) < cur => cur = p as usize,
                _ => break,
            }
        }
    }
    let mut has_shown_child = vec![false; n];
    for (i, item) in outline.items.iter().enumerate() {
        if keep[i]
            && let Some(p) = item.parent
            && let Some(flag) = has_shown_child.get_mut(p as usize)
        {
            *flag = true;
        }
    }
    outline
        .items
        .iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(i, item)| Row {
            idx: i as u32,
            depth: item.depth,
            fold: if has_shown_child[i] {
                Fold::Open
            } else {
                Fold::Leaf
            },
            matches: std::mem::take(&mut ranges[i]),
        })
        .collect()
}

/// Unfolds every ancestor of `idx`. Returns whether anything changed.
pub fn reveal(outline: &Outline, collapsed: &mut BTreeSet<u32>, idx: u32) -> bool {
    let mut changed = false;
    for ancestor in outline.ancestors(idx) {
        changed |= collapsed.remove(&ancestor);
    }
    changed
}

/// The location the current section is judged at: the quarter-height line of the viewport, in
/// the page space of the page under it. Without a layout (nothing drawn yet) the anchor itself.
pub fn reading_location(
    anchor: Location,
    layout: Option<&PageLayout>,
    viewport_h: f32,
) -> Location {
    let Some(layout) = layout.filter(|l| anchor.page < l.page_count()) else {
        return anchor;
    };
    let top = layout.content_pos(anchor);
    anchor_at(layout, Point::new(top.x, top.y + viewport_h * READING_LINE))
}

/// The scroll offset that brings row `row` into view in a list whose rows are `stride` apart,
/// shown through a viewport `view_h` high that is scrolled to `offset`. `None` when the row is
/// fully visible already. A row that has to move is put a third of the way down the viewport.
pub fn scroll_target(row: usize, stride: f32, offset: f32, view_h: f32) -> Option<f32> {
    let top = row as f32 * stride;
    let bottom = top + stride;
    if view_h > 0.0 && top >= offset && bottom <= offset + view_h {
        return None;
    }
    Some((top - view_h / 3.0).max(0.0))
}

/// The title of each item as it appears in a saved path. A title that an earlier sibling already
/// uses gets one NUL character per earlier sibling, so equal siblings have different paths.
fn path_segments(outline: &Outline) -> Vec<String> {
    let mut seen: HashMap<(Option<u32>, &str), usize> = HashMap::new();
    outline
        .items
        .iter()
        .map(|item| {
            let count = seen.entry((item.parent, item.title.as_str())).or_insert(0);
            let segment = format!("{}{}", item.title, "\0".repeat(*count));
            *count += 1;
            segment
        })
        .collect()
}

/// The path of segments from the top level down to each item.
fn item_paths(outline: &Outline) -> Vec<Vec<String>> {
    let segments = path_segments(outline);
    let mut paths: Vec<Vec<String>> = Vec::with_capacity(segments.len());
    for (i, segment) in segments.into_iter().enumerate() {
        let mut path = match outline.items[i].parent {
            Some(p) if (p as usize) < i => paths[p as usize].clone(),
            _ => Vec::new(),
        };
        path.push(segment);
        paths.push(path);
    }
    paths
}

/// The folded entries as title paths, in document order, for saving (`DocRecord.toc_collapsed`).
/// Paths, not indices, so a changed document still finds its entries.
#[allow(dead_code)] // used by session saving (a later work package)
pub fn export_collapsed(outline: &Outline, collapsed: &BTreeSet<u32>) -> Vec<Vec<String>> {
    if collapsed.is_empty() {
        return Vec::new();
    }
    let paths = item_paths(outline);
    collapsed
        .iter()
        .filter_map(|&i| paths.get(i as usize).cloned())
        .collect()
}

/// The entries of `outline` named by saved title paths. Paths that no entry has any more (the
/// document changed) are ignored; so are entries that cannot be folded.
pub fn import_collapsed(outline: &Outline, saved: &[Vec<String>]) -> BTreeSet<u32> {
    if saved.is_empty() {
        return BTreeSet::new();
    }
    let by_path: HashMap<Vec<String>, u32> = item_paths(outline)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| outline.has_children(*i as u32))
        .map(|(i, path)| (path, i as u32))
        .collect();
    saved
        .iter()
        .filter_map(|path| by_path.get(path).copied())
        .collect()
}

/// What one frame of the panel draws.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    /// The lines.
    pub rows: Arc<Vec<Row>>,
    /// The section the reader is in.
    pub current: Option<u32>,
    /// Its ancestors, nearest first.
    pub ancestors: Vec<u32>,
    /// The current section is not the one of the previous frame (and no filter is active).
    pub current_changed: bool,
    /// The position of the current section in `rows`, if it is shown.
    pub current_row: Option<usize>,
    /// A filter is active: folding is ignored and the triangles do nothing.
    pub filtering: bool,
}

/// The result of [`filtered_rows`] with what it was computed for.
#[derive(Clone, Debug)]
struct FilterCache {
    filter: String,
    generation: u32,
    rows: Arc<Vec<Row>>,
}

/// The state of the panel for one view. Not saved; the folded entries can be exported as title
/// paths ([`TocState::collapsed_paths`]) for the session.
#[derive(Clone, Debug, Default)]
pub struct TocState {
    /// The text of the filter box.
    pub filter: String,
    /// The folded entries; `None` until the outline is known (then every entry with children).
    collapsed: Option<BTreeSet<u32>>,
    /// Saved folded entries waiting for the outline to be known.
    pending_paths: Option<Vec<Vec<String>>>,
    /// The document generation `collapsed` belongs to.
    generation: Option<u32>,
    /// The current section as of the last frame without a filter.
    last_current: Option<u32>,
    /// The rows of the last filter, so that frames in which nothing changed do not search again.
    filter_cache: Option<FilterCache>,
    /// The reading position of the last frame drawn.
    last_reading: Option<Location>,
    /// Scroll offset and height of the list as of the last frame, for [`scroll_target`].
    scroll_offset: f32,
    view_height: f32,
}

impl TocState {
    /// Makes the folded set match `outline`: a fresh outline starts with everything folded (or
    /// with the saved paths), a new generation of the document starts over.
    fn sync(&mut self, outline: &Outline, generation: u32) {
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            self.collapsed = None;
            self.last_current = None;
        }
        if self.collapsed.is_none() {
            self.collapsed = Some(match self.pending_paths.take() {
                Some(paths) => import_collapsed(outline, &paths),
                None => initial_collapsed(outline),
            });
        }
    }

    /// Decides what to draw for `outline` when the reader is at `reading`: see [`Prepared`].
    /// A section that became current is unfolded (not while a filter is active, so clearing
    /// the filter restores the folding from before).
    pub fn prepare(&mut self, outline: &Outline, generation: u32, reading: Location) -> Prepared {
        self.sync(outline, generation);
        let current = outline.current(reading);
        let ancestors = current.map(|c| outline.ancestors(c)).unwrap_or_default();
        let filtering = !self.filter.trim().is_empty();
        let mut current_changed = false;
        if !filtering && current != self.last_current {
            self.last_current = current;
            current_changed = current.is_some();
            if let (Some(c), Some(collapsed)) = (current, self.collapsed.as_mut()) {
                reveal(outline, collapsed, c);
            }
        }
        let rows = if filtering {
            self.filtered(outline, generation)
        } else {
            Arc::new(visible_rows(
                outline,
                self.collapsed.as_ref().unwrap_or(&BTreeSet::new()),
            ))
        };
        let current_row = if current_changed {
            current.and_then(|c| rows.iter().position(|r| r.idx == c))
        } else {
            None
        };
        Prepared {
            rows,
            current,
            ancestors,
            current_changed,
            current_row,
            filtering,
        }
    }

    /// The rows for the current filter, searched again only when the filter or the outline
    /// changed.
    fn filtered(&mut self, outline: &Outline, generation: u32) -> Arc<Vec<Row>> {
        if let Some(cache) = &self.filter_cache
            && cache.filter == self.filter
            && cache.generation == generation
        {
            return Arc::clone(&cache.rows);
        }
        let rows = Arc::new(filtered_rows(outline, &self.filter));
        self.filter_cache = Some(FilterCache {
            filter: self.filter.clone(),
            generation,
            rows: Arc::clone(&rows),
        });
        rows
    }

    /// The folded entries, if the outline has been seen.
    #[cfg(test)]
    pub fn collapsed(&self) -> Option<&BTreeSet<u32>> {
        self.collapsed.as_ref()
    }

    /// Folds an open entry or opens a folded one. Entries without children are left alone.
    pub fn toggle(&mut self, outline: &Outline, idx: u32) {
        if !outline.has_children(idx) {
            return;
        }
        if let Some(collapsed) = self.collapsed.as_mut()
            && !collapsed.remove(&idx)
        {
            collapsed.insert(idx);
        }
    }

    /// Opens every entry.
    pub fn expand_all(&mut self) {
        if let Some(collapsed) = self.collapsed.as_mut() {
            collapsed.clear();
        }
    }

    /// Folds every entry that has children.
    pub fn collapse_all(&mut self, outline: &Outline) {
        self.collapsed = Some(initial_collapsed(outline));
    }

    /// The folded entries as title paths, for saving. Before the outline is seen, what was
    /// handed to [`TocState::set_collapsed_paths`] (nothing if that never happened).
    #[allow(dead_code)] // used by session saving (a later work package)
    pub fn collapsed_paths(&self, outline: &Outline) -> Vec<Vec<String>> {
        match (&self.collapsed, &self.pending_paths) {
            (Some(collapsed), _) => export_collapsed(outline, collapsed),
            (None, Some(pending)) => pending.clone(),
            (None, None) => export_collapsed(outline, &initial_collapsed(outline)),
        }
    }

    /// Restores folded entries saved with [`TocState::collapsed_paths`]. They are matched to the
    /// outline when it is first drawn; paths the document no longer has are ignored.
    #[allow(dead_code)] // used by session saving (a later work package)
    pub fn set_collapsed_paths(&mut self, paths: Vec<Vec<String>>) {
        self.pending_paths = Some(paths);
        self.collapsed = None;
    }
}

// ---------------------------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------------------------

/// What a row asked for, collected while drawing and applied afterwards.
enum RowEvent {
    Toggle(u32),
    Go(u32, OpenHow),
    CopyTitle(u32),
    ExpandAll,
    CollapseAll,
}

/// Draws the table of contents section of the sidebar for the focused tab `tab` of a window.
/// Anything but a PDF tab gets a hint.
pub fn show(
    ui: &mut Ui,
    tabs: &mut TabStore,
    docs: &DocStore,
    tab: Option<TabId>,
    queue: &mut Vec<Action>,
) {
    // The section header with the title is drawn by the sidebar (`panels/sidebar.rs`).
    let Some((tab_id, Tab::Pdf(pdf))) = tab.and_then(|id| tabs.get_mut(id).map(|t| (id, t))) else {
        hint(ui, tr("toc.no_document"));
        return;
    };
    let Some(doc) = docs.get(pdf.doc) else {
        hint(ui, tr("toc.no_document"));
        return;
    };
    let Some(info) = doc.info() else {
        let text =
            status_bar::status_text(&doc.status()).unwrap_or_else(|| tr("doc.loading").into());
        hint(ui, &text);
        return;
    };
    let outline = &info.outline;
    if outline.items.is_empty() {
        hint(ui, tr("toc.empty"));
        return;
    }
    show_tree(ui, tab_id, pdf, outline, info.generation, queue);
}

fn hint(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).weak());
}

/// The filter box and the tree of a PDF tab with an outline.
fn show_tree(
    ui: &mut Ui,
    tab: TabId,
    pdf: &mut PdfTab,
    outline: &Outline,
    generation: u32,
    queue: &mut Vec<Action>,
) {
    let reading = reading_location(pdf.view.anchor, pdf.ui.layout(), pdf.ui.viewport().h);
    let toc = &mut pdf.toc;
    // The panel is drawn before the view, so it sees the position of the previous frame. One
    // more frame after every change lets it catch up when the view stops moving.
    if toc.last_reading != Some(reading) {
        toc.last_reading = Some(reading);
        ui.ctx().request_repaint();
    }

    let edit = ui.add(
        TextEdit::singleline(&mut toc.filter)
            .hint_text(tr("toc.filter_hint"))
            .desired_width(f32::INFINITY),
    );
    // The text field gives up the keyboard on Esc by itself; clearing is added to that.
    if (edit.has_focus() || edit.lost_focus()) && ui.input(|i| i.key_pressed(Key::Escape)) {
        toc.filter.clear();
    }
    ui.add_space(2.0);

    let prepared = toc.prepare(outline, generation, reading);
    if prepared.rows.is_empty() {
        hint(ui, tr("toc.no_match"));
        return;
    }

    let row_h = ui.text_style_height(&TextStyle::Body) + 4.0;
    let stride = row_h + ui.spacing().item_spacing.y;
    let mut area = ScrollArea::vertical().auto_shrink([false, false]);
    // The list follows the reader only when the reader is not working in the panel.
    let in_panel = ui.rect_contains_pointer(ui.max_rect());
    if !in_panel
        && let Some(row) = prepared.current_row
        && let Some(offset) = scroll_target(row, stride, toc.scroll_offset, toc.view_height)
    {
        area = area.vertical_scroll_offset(offset);
    }

    let mut events = Vec::new();
    let output = area.show_rows(ui, row_h, prepared.rows.len(), |ui, range| {
        for row in &prepared.rows[range] {
            show_row(ui, row_h, row, outline, &prepared, &mut events);
        }
    });
    toc.scroll_offset = output.state.clamped_offset().y;
    toc.view_height = output.inner_rect.height();

    for event in events {
        match event {
            RowEvent::Toggle(idx) => toc.toggle(outline, idx),
            RowEvent::Go(idx, how) => {
                if let Some(target) = outline.items.get(idx as usize).and_then(|i| i.target) {
                    // The horizontal position of a heading is its margin; it is dropped so that
                    // a zoomed-in view does not scroll sideways.
                    queue.push(Action::Navigate {
                        tab,
                        to: Location::new(target.page, 0.0, target.y),
                        how,
                    });
                }
            }
            RowEvent::CopyTitle(idx) => {
                if let Some(item) = outline.items.get(idx as usize) {
                    ui.ctx().copy_text(item.title.clone());
                }
            }
            RowEvent::ExpandAll => toc.expand_all(),
            RowEvent::CollapseAll => toc.collapse_all(outline),
        }
    }
}

/// Draws one row: the fold triangle, the title (one line, shortened with "…" when too long,
/// whole in a tooltip) and handles its clicks and its right-click menu.
fn show_row(
    ui: &mut Ui,
    row_h: f32,
    row: &Row,
    outline: &Outline,
    prepared: &Prepared,
    events: &mut Vec<RowEvent>,
) {
    let Some(item) = outline.items.get(row.idx as usize) else {
        return;
    };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::hover());
    let indent = f32::from(row.depth.min(MAX_INDENT_DEPTH)) * INDENT;
    let fold_rect =
        Rect::from_min_size(rect.min + egui::vec2(indent, 0.0), egui::vec2(row_h, row_h));
    let title_rect = Rect::from_min_max(Pos2::new(fold_rect.right(), rect.top()), rect.max);

    let id = ui.make_persistent_id(("toc_row", row.idx));
    let title = ui.interact(title_rect, id.with("title"), Sense::click());
    // The row as a button with its title (and, for a branch, whether it is open) in the
    // accessibility tree, so screen checks can read and press it.
    title.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &item.title));
    if row.fold != Fold::Leaf {
        ui.ctx().accesskit_node_builder(title.id, |node| {
            node.set_expanded(row.fold == Fold::Open);
        });
    }
    let foldable = row.fold != Fold::Leaf && !prepared.filtering;
    let fold = foldable.then(|| ui.interact(fold_rect, id.with("fold"), Sense::click()));
    let hovered = title.hovered() || fold.as_ref().is_some_and(|f| f.hovered());

    let is_current = prepared.current == Some(row.idx);
    let is_ancestor = prepared.ancestors.contains(&row.idx);
    let visuals = ui.visuals().clone();
    let painter = ui.painter();
    if is_current {
        painter.rect_filled(rect, 2.0, visuals.selection.bg_fill);
    } else if is_ancestor {
        painter.rect_filled(rect, 2.0, visuals.selection.bg_fill.gamma_multiply(0.3));
    } else if hovered {
        painter.rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let text_color = if is_current {
        visuals.selection.stroke.color
    } else {
        visuals.text_color()
    };

    if row.fold != Fold::Leaf {
        let color = if hovered {
            visuals.strong_text_color()
        } else {
            visuals.weak_text_color()
        };
        painter.add(triangle(fold_rect, row.fold == Fold::Open, color));
    }

    let job = title_job(
        &item.title,
        &row.matches,
        ui.style().text_styles[&TextStyle::Body].clone(),
        text_color,
        visuals.warn_fg_color.gamma_multiply(0.45),
        (title_rect.width() - 4.0).max(0.0),
    );
    let galley = ui.painter().layout_job(job);
    let pos = Pos2::new(
        title_rect.left() + 2.0,
        title_rect.center().y - galley.size().y / 2.0,
    );
    let elided = galley.elided;
    ui.painter().galley(pos, galley, text_color);

    let title = if elided {
        title.on_hover_text(&item.title)
    } else {
        title
    };

    if fold.as_ref().is_some_and(|f| f.clicked()) {
        events.push(RowEvent::Toggle(row.idx));
    }
    let how = if title.middle_clicked() || (title.clicked() && ui.input(|i| i.modifiers.ctrl)) {
        Some(OpenHow::NewTab)
    } else if title.clicked() {
        Some(OpenHow::Here)
    } else {
        None
    };
    if let Some(how) = how {
        if item.target.is_some() {
            events.push(RowEvent::Go(row.idx, how));
        } else if foldable {
            // An entry that goes nowhere only folds.
            events.push(RowEvent::Toggle(row.idx));
        }
    }

    title.context_menu(|ui| {
        let has_target = item.target.is_some();
        if ui
            .add_enabled(has_target, egui::Button::new(tr("toc.open_new_tab")))
            .clicked()
        {
            events.push(RowEvent::Go(row.idx, OpenHow::NewTab));
            ui.close();
        }
        if ui
            .add_enabled(has_target, egui::Button::new(tr("toc.open_split")))
            .clicked()
        {
            events.push(RowEvent::Go(row.idx, OpenHow::SplitSide));
            ui.close();
        }
        if ui.button(tr("toc.copy_title")).clicked() {
            events.push(RowEvent::CopyTitle(row.idx));
            ui.close();
        }
        ui.separator();
        if ui.button(tr("toc.expand_all")).clicked() {
            events.push(RowEvent::ExpandAll);
            ui.close();
        }
        if ui.button(tr("toc.collapse_all")).clicked() {
            events.push(RowEvent::CollapseAll);
            ui.close();
        }
    });
}

/// The fold triangle inside `rect`: pointing down when open, right when folded.
pub(crate) fn triangle(rect: Rect, open: bool, color: egui::Color32) -> Shape {
    let c = rect.center();
    let r = 4.0;
    let points = if open {
        vec![
            c + egui::vec2(-r, -r * 0.5),
            c + egui::vec2(r, -r * 0.5),
            c + egui::vec2(0.0, r * 0.7),
        ]
    } else {
        vec![
            c + egui::vec2(-r * 0.5, -r),
            c + egui::vec2(r * 0.7, 0.0),
            c + egui::vec2(-r * 0.5, r),
        ]
    };
    Shape::convex_polygon(points, color, Stroke::NONE)
}

/// A one-line text with the `matches` ranges highlighted, shortened with "…" past `max_width`.
pub(crate) fn title_job(
    title: &str,
    matches: &[Range<usize>],
    font: egui::FontId,
    color: egui::Color32,
    highlight: egui::Color32,
    max_width: f32,
) -> LayoutJob {
    // Control characters would break the line. Replacing ASCII ones keeps the byte offsets.
    let text = title.replace(|c: char| c.is_ascii_control(), " ");
    let plain = TextFormat::simple(font.clone(), color);
    let marked = TextFormat {
        background: highlight,
        ..TextFormat::simple(font, color)
    };
    let mut job = LayoutJob {
        wrap: TextWrapping {
            max_width,
            max_rows: 1,
            break_anywhere: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut at = 0;
    for range in matches {
        if range.start > at {
            job.append(&text[at..range.start], 0.0, plain.clone());
        }
        job.append(&text[range.clone()], 0.0, marked.clone());
        at = range.end;
    }
    if at < text.len() {
        job.append(&text[at..], 0.0, plain);
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use refdesk_core::OutlineNode;
    use refdesk_core::ZoomMode;
    use refdesk_core::geom::Size;
    use refdesk_core::layout::LayoutParams;

    fn node(title: &str, at: Option<(u32, f32)>, children: Vec<OutlineNode>) -> OutlineNode {
        OutlineNode {
            title: title.to_string(),
            target: at.map(|(p, y)| Location::new(p, 0.0, y)),
            uri: None,
            children,
        }
    }

    /// 1 Intro (p0 y100)
    ///   1.1 Scope (p0 y400)
    ///     1.1.1 Terms (p1 y100)
    ///   1.2 Goals (p1 y500)
    /// 2 Design (p2 y100)
    ///   2.1 Parts (p2 y400)
    /// 3 Appendix (p3 y100)
    fn sample() -> Outline {
        Outline::from_tree(vec![
            node(
                "1 Intro",
                Some((0, 100.0)),
                vec![
                    node(
                        "1.1 Scope",
                        Some((0, 400.0)),
                        vec![node("1.1.1 Terms", Some((1, 100.0)), vec![])],
                    ),
                    node("1.2 Goals", Some((1, 500.0)), vec![]),
                ],
            ),
            node(
                "2 Design",
                Some((2, 100.0)),
                vec![node("2.1 Parts", Some((2, 400.0)), vec![])],
            ),
            node("3 Appendix", Some((3, 100.0)), vec![]),
        ])
    }

    fn titles<'a>(outline: &'a Outline, rows: &[Row]) -> Vec<&'a str> {
        rows.iter()
            .map(|r| outline.items[r.idx as usize].title.as_str())
            .collect()
    }

    fn at(page: u32, y: f32) -> Location {
        Location::new(page, 0.0, y)
    }

    // --- visible rows -------------------------------------------------------------------

    #[test]
    fn the_first_state_shows_only_the_top_level() {
        let outline = sample();
        let rows = visible_rows(&outline, &initial_collapsed(&outline));
        assert_eq!(
            titles(&outline, &rows),
            vec!["1 Intro", "2 Design", "3 Appendix"]
        );
        assert_eq!(rows[0].fold, Fold::Closed);
        assert_eq!(rows[2].fold, Fold::Leaf);
    }

    #[test]
    fn opening_one_entry_shows_its_children_but_not_their_children() {
        let outline = sample();
        let mut collapsed = initial_collapsed(&outline);
        collapsed.remove(&0);
        let rows = visible_rows(&outline, &collapsed);
        assert_eq!(
            titles(&outline, &rows),
            vec![
                "1 Intro",
                "1.1 Scope",
                "1.2 Goals",
                "2 Design",
                "3 Appendix"
            ]
        );
        assert_eq!(rows[0].fold, Fold::Open);
        assert_eq!(rows[1].fold, Fold::Closed);
        assert_eq!(rows[1].depth, 1);
    }

    #[test]
    fn three_levels_of_nesting_unfold_one_level_at_a_time() {
        let outline = sample();
        let mut state = TocState::default();
        let before = state.prepare(&outline, 0, at(3, 500.0));
        assert_eq!(before.rows.len(), 3);
        state.toggle(&outline, 0);
        state.toggle(&outline, 1);
        let rows = state.prepare(&outline, 0, at(3, 500.0)).rows;
        assert_eq!(
            titles(&outline, &rows),
            vec![
                "1 Intro",
                "1.1 Scope",
                "1.1.1 Terms",
                "1.2 Goals",
                "2 Design",
                "3 Appendix"
            ]
        );
        assert_eq!(rows[2].depth, 2);
        // Folding the top entry hides everything below it again, however it was left.
        state.toggle(&outline, 0);
        assert_eq!(state.prepare(&outline, 0, at(3, 500.0)).rows.len(), 3);
    }

    #[test]
    fn expand_all_and_collapse_all() {
        let outline = sample();
        let mut state = TocState::default();
        // Far away from every entry with children: the reading position is in the appendix.
        let reading = at(3, 500.0);
        state.prepare(&outline, 0, reading);
        state.expand_all();
        let all = state.prepare(&outline, 0, reading).rows;
        assert_eq!(all.len(), outline.items.len());
        assert!(all.iter().all(|r| r.fold != Fold::Closed));
        state.collapse_all(&outline);
        let top = state.prepare(&outline, 0, reading).rows;
        assert_eq!(top.len(), 3);
    }

    #[test]
    fn entries_without_children_have_no_triangle_and_do_not_fold() {
        let outline = sample();
        let mut state = TocState::default();
        let rows = state.prepare(&outline, 0, at(3, 500.0)).rows;
        assert_eq!(rows[2].fold, Fold::Leaf); // 3 Appendix
        let before = state.collapsed().cloned();
        state.toggle(&outline, 2);
        assert_eq!(state.collapsed().cloned(), before);
        state.expand_all();
        let rows = state.prepare(&outline, 0, at(3, 500.0)).rows;
        for r in rows.iter() {
            assert_eq!(
                r.fold == Fold::Leaf,
                !outline.has_children(r.idx),
                "{}",
                outline.items[r.idx as usize].title
            );
        }
    }

    // --- current section ----------------------------------------------------------------

    #[test]
    fn the_path_to_the_current_section_is_opened() {
        let outline = sample();
        let mut state = TocState::default();
        // On page 1 at y 200: after 1.1.1 Terms (page 1 y 100), before 1.2 Goals.
        let p = state.prepare(&outline, 0, at(1, 200.0));
        assert_eq!(p.current, Some(2));
        assert_eq!(p.ancestors, vec![1, 0]);
        assert!(p.current_changed);
        assert_eq!(
            titles(&outline, &p.rows),
            vec![
                "1 Intro",
                "1.1 Scope",
                "1.1.1 Terms",
                "1.2 Goals",
                "2 Design",
                "3 Appendix"
            ]
        );
        assert_eq!(p.current_row, Some(2));
    }

    #[test]
    fn before_the_first_entry_there_is_no_current_section() {
        let outline = Outline::from_tree(vec![node("Chapter", Some((1, 300.0)), vec![])]);
        let mut state = TocState::default();
        let p = state.prepare(&outline, 0, at(0, 700.0));
        assert_eq!(p.current, None);
        assert!(p.ancestors.is_empty());
        assert!(!p.current_changed);
        assert_eq!(p.current_row, None);
        let p = state.prepare(&outline, 0, at(1, 299.0));
        assert_eq!(p.current, None);
        assert_eq!(state.prepare(&outline, 0, at(1, 300.0)).current, Some(0));
    }

    #[test]
    fn an_entry_without_a_target_is_never_the_current_section() {
        let outline = Outline::from_tree(vec![
            node(
                "Part I",
                None,
                vec![node("Chapter 1", Some((0, 100.0)), vec![])],
            ),
            node("Part II", None, vec![]),
            node("Chapter 2", Some((2, 100.0)), vec![]),
        ]);
        let mut state = TocState::default();
        let p = state.prepare(&outline, 0, at(1, 0.0));
        assert_eq!(p.current, Some(1)); // Chapter 1, not Part II
        assert_eq!(p.ancestors, vec![0]); // but its parent has no target and is still an ancestor
        assert_eq!(state.prepare(&outline, 0, at(5, 0.0)).current, Some(3));
    }

    #[test]
    fn the_list_scrolls_only_when_the_current_section_changes() {
        let outline = sample();
        let mut state = TocState::default();
        assert!(state.prepare(&outline, 0, at(2, 150.0)).current_changed);
        // Reading on within the same section: nothing to follow.
        assert!(!state.prepare(&outline, 0, at(2, 300.0)).current_changed);
        assert!(!state.prepare(&outline, 0, at(2, 300.0)).current_changed);
        let p = state.prepare(&outline, 0, at(2, 450.0));
        assert!(p.current_changed);
        assert_eq!(p.current, Some(5));
        // Back to no current section: a change, but nothing to scroll to.
        let none = Outline::from_tree(vec![node("X", Some((5, 0.0)), vec![])]);
        let mut state = TocState::default();
        state.prepare(&none, 0, at(5, 10.0));
        let p = state.prepare(&none, 0, at(0, 10.0));
        assert_eq!(p.current, None);
        assert!(!p.current_changed);
    }

    #[test]
    fn folding_the_current_branch_by_hand_sticks_until_the_section_changes() {
        let outline = sample();
        let mut state = TocState::default();
        state.prepare(&outline, 0, at(1, 200.0)); // opens 1 and 1.1
        state.toggle(&outline, 0); // the reader folds "1 Intro"
        let p = state.prepare(&outline, 0, at(1, 210.0));
        assert_eq!(
            p.rows.len(),
            3,
            "the fold is kept while the section is the same"
        );
        let p = state.prepare(&outline, 0, at(1, 600.0)); // now in 1.2 Goals
        assert!(
            p.rows.iter().any(|r| r.idx == 3),
            "the new section is shown"
        );
    }

    #[test]
    fn a_new_document_generation_starts_over() {
        let outline = sample();
        let mut state = TocState::default();
        state.prepare(&outline, 0, at(3, 500.0));
        state.expand_all();
        assert_eq!(state.prepare(&outline, 0, at(3, 500.0)).rows.len(), 7);
        assert_eq!(state.prepare(&outline, 1, at(3, 500.0)).rows.len(), 3);
    }

    #[test]
    fn the_reading_line_is_a_quarter_of_the_viewport_below_the_top() {
        let sizes = vec![Size::new(600.0, 800.0); 3];
        let layout = PageLayout::new(
            &sizes,
            LayoutParams {
                viewport: Size::new(600.0, 400.0),
                zoom: ZoomMode::Custom(1.0),
                gap: 10.0,
                margin: 0.0,
                fit_page: 0,
            },
        );
        let s = layout.scale();
        // The viewport top is at page 1, y 0. A quarter of 400px down is 100px / scale pt.
        let l = reading_location(at(1, 0.0), Some(&layout), 400.0);
        assert_eq!(l.page, 1);
        assert!((l.y - 100.0 / s).abs() < 0.01, "{l:?}");
        // Without a layout (nothing drawn yet) it is the anchor.
        assert_eq!(reading_location(at(1, 5.0), None, 400.0), at(1, 5.0));
        // A view whose viewport is not known yet is judged at its top.
        let l = reading_location(at(1, 40.0), Some(&layout), 0.0);
        assert!((l.y - 40.0).abs() < 0.01 && l.page == 1);
        // An anchor beyond the last page does not panic.
        assert_eq!(
            reading_location(at(9, 0.0), Some(&layout), 400.0),
            at(9, 0.0)
        );
    }

    #[test]
    fn a_heading_in_the_top_quarter_of_the_viewport_is_already_the_current_section() {
        let sizes = vec![Size::new(600.0, 800.0)];
        let layout = PageLayout::new(
            &sizes,
            LayoutParams {
                viewport: Size::new(600.0, 400.0),
                zoom: ZoomMode::Custom(1.0),
                gap: 10.0,
                margin: 0.0,
                fit_page: 0,
            },
        );
        // The reading line is `quarter` pt below the viewport top.
        let quarter = 100.0 / layout.scale();
        let top = 200.0;
        let outline = Outline::from_tree(vec![
            node("A", Some((0, 50.0)), vec![]),
            node("B", Some((0, top + quarter * 0.5)), vec![]),
            node("C", Some((0, top + quarter * 2.0)), vec![]),
        ]);
        let line = reading_location(at(0, top), Some(&layout), 400.0);
        assert_eq!(
            outline.current(line),
            Some(1),
            "B is below the top but above the line"
        );
        // The anchor itself would still say A.
        assert_eq!(outline.current(at(0, top)), Some(0));
    }

    #[test]
    fn scroll_target_leaves_visible_rows_alone() {
        // 20 px per row, viewport 100 px high showing rows 5..10.
        assert_eq!(scroll_target(5, 20.0, 100.0, 100.0), None);
        assert_eq!(scroll_target(9, 20.0, 100.0, 100.0), None);
        // Above: moves up; below: moves down; both put the row a third from the top.
        let up = scroll_target(2, 20.0, 100.0, 100.0).expect("scrolls");
        let down = scroll_target(30, 20.0, 100.0, 100.0).expect("scrolls");
        assert!((up - (40.0 - 100.0 / 3.0)).abs() < 0.01);
        assert!((down - (600.0 - 100.0 / 3.0)).abs() < 0.01);
        // A row partly hidden counts as hidden.
        assert!(scroll_target(10, 20.0, 110.0, 100.0).is_some());
        // The top row never asks for a negative offset; an unknown viewport scrolls to the row.
        assert_eq!(scroll_target(0, 20.0, 300.0, 100.0), Some(0.0));
        assert_eq!(scroll_target(4, 20.0, 0.0, 0.0), Some(80.0));
    }

    // --- filter -------------------------------------------------------------------------

    #[test]
    fn the_filter_shows_matches_and_their_ancestors_only() {
        let outline = sample();
        let rows = filtered_rows(&outline, "terms");
        assert_eq!(
            titles(&outline, &rows),
            vec!["1 Intro", "1.1 Scope", "1.1.1 Terms"]
        );
        // Ancestors are open; the match is a leaf of the shown tree.
        assert_eq!(rows[0].fold, Fold::Open);
        assert_eq!(rows[2].fold, Fold::Leaf);
        // Only the matching row carries highlight ranges.
        assert!(rows[0].matches.is_empty() && rows[1].matches.is_empty());
        assert_eq!(rows[2].matches, vec![6..11]);
        // Several matches in different branches.
        let rows = filtered_rows(&outline, "s");
        assert_eq!(
            titles(&outline, &rows),
            vec![
                "1 Intro",
                "1.1 Scope",
                "1.1.1 Terms",
                "1.2 Goals",
                "2 Design",
                "2.1 Parts",
            ]
        );
    }

    #[test]
    fn the_filter_ignores_case_and_surrounding_blanks() {
        let outline = sample();
        let a = filtered_rows(&outline, "SCOPE");
        let b = filtered_rows(&outline, "  scope ");
        assert_eq!(a, b);
        assert_eq!(titles(&outline, &a), vec!["1 Intro", "1.1 Scope"]);
        let outline = Outline::from_tree(vec![node("Ünïcode ÄÖ", None, vec![])]);
        let rows = filtered_rows(&outline, "äö");
        assert_eq!(rows.len(), 1);
        let range = rows[0].matches[0].clone();
        assert_eq!(&outline.items[0].title[range], "ÄÖ");
    }

    #[test]
    fn a_filter_without_matches_shows_nothing() {
        let outline = sample();
        assert!(filtered_rows(&outline, "zzz").is_empty());
        let mut state = TocState {
            filter: "zzz".into(),
            ..TocState::default()
        };
        let p = state.prepare(&outline, 0, at(0, 0.0));
        assert!(p.filtering && p.rows.is_empty());
    }

    #[test]
    fn match_ranges_are_byte_ranges_without_overlap() {
        let needle: Vec<char> = "aa".chars().collect();
        assert_eq!(match_ranges("aaaaa", &needle), vec![0..2, 2..4]);
        let needle: Vec<char> = "é".chars().collect();
        assert_eq!(match_ranges("café É", &needle), vec![3..5, 6..8]);
        assert!(match_ranges("abc", &[]).is_empty());
        assert!(match_ranges("", &['a']).is_empty());
    }

    #[test]
    fn clearing_the_filter_restores_the_folding_from_before() {
        let outline = sample();
        let mut state = TocState::default();
        let reading = at(3, 500.0);
        state.prepare(&outline, 0, reading);
        state.toggle(&outline, 2 - 2); // open "1 Intro"
        let before = state.prepare(&outline, 0, reading).rows;
        let folding = state.collapsed().cloned();

        state.filter = "terms".into();
        let filtered = state.prepare(&outline, 0, reading);
        assert!(filtered.filtering);
        assert_eq!(filtered.rows.len(), 3);
        assert_eq!(
            state.collapsed().cloned(),
            folding,
            "the filter leaves the folding alone"
        );

        state.filter.clear();
        let after = state.prepare(&outline, 0, reading).rows;
        assert_eq!(after, before);
        assert_eq!(state.collapsed().cloned(), folding);
    }

    #[test]
    fn a_section_that_changed_during_filtering_is_revealed_afterwards() {
        let outline = sample();
        let mut state = TocState::default();
        state.prepare(&outline, 0, at(3, 500.0));
        state.filter = "x".into();
        let p = state.prepare(&outline, 0, at(1, 200.0));
        assert!(!p.current_changed, "no scrolling while filtering");
        state.filter.clear();
        let p = state.prepare(&outline, 0, at(1, 200.0));
        assert!(p.current_changed);
        assert!(p.rows.iter().any(|r| r.idx == 2), "1.1.1 is shown now");
    }

    // --- saving the folding -------------------------------------------------------------

    #[test]
    fn the_folding_survives_a_round_trip() {
        let outline = sample();
        let mut state = TocState::default();
        state.prepare(&outline, 0, at(3, 500.0));
        state.toggle(&outline, 0); // open "1 Intro"; "1.1 Scope" and "2 Design" stay folded
        let saved = state.collapsed_paths(&outline);
        assert_eq!(
            saved,
            vec![
                vec![
                    "1 Intro\0".trim_end_matches('\0').to_string(),
                    "1.1 Scope".into()
                ],
                vec!["2 Design".to_string()],
            ]
        );

        let mut restored = TocState::default();
        restored.set_collapsed_paths(saved);
        let rows = restored.prepare(&outline, 0, at(3, 500.0)).rows;
        assert_eq!(
            rows,
            state.prepare(&outline, 0, at(3, 500.0)).rows,
            "same rows after restoring"
        );
        assert_eq!(restored.collapsed(), state.collapsed());
    }

    #[test]
    fn a_fully_open_folding_round_trips_as_an_empty_list() {
        let outline = sample();
        assert!(export_collapsed(&outline, &BTreeSet::new()).is_empty());
        let mut state = TocState::default();
        state.set_collapsed_paths(Vec::new());
        state.prepare(&outline, 0, at(3, 500.0));
        assert_eq!(state.prepare(&outline, 0, at(3, 500.0)).rows.len(), 7);
    }

    #[test]
    fn siblings_with_the_same_title_keep_their_own_folding() {
        let outline = Outline::from_tree(vec![
            node("Notes", Some((0, 0.0)), vec![node("a", None, vec![])]),
            node("Notes", Some((1, 0.0)), vec![node("b", None, vec![])]),
            node("Notes", Some((2, 0.0)), vec![node("c", None, vec![])]),
        ]);
        // The three "Notes" are items 0, 2 and 4.
        for folded in [
            vec![0u32],
            vec![2],
            vec![4],
            vec![0, 4],
            vec![2, 4],
            vec![0, 2, 4],
            vec![],
        ] {
            let set: BTreeSet<u32> = folded.iter().copied().collect();
            let paths = export_collapsed(&outline, &set);
            assert_eq!(paths.len(), set.len(), "{folded:?}");
            assert_eq!(import_collapsed(&outline, &paths), set, "{folded:?}");
        }
    }

    #[test]
    fn paths_of_a_changed_document_are_ignored_and_the_rest_applies() {
        let outline = sample();
        let saved = vec![
            vec!["1 Intro".to_string(), "1.1 Scope".to_string()],
            vec!["Removed chapter".to_string()],
            vec!["1 Intro".to_string(), "Moved".to_string()],
            vec!["3 Appendix".to_string()], // exists, but has no children
            vec![],
        ];
        let set = import_collapsed(&outline, &saved);
        assert_eq!(set, BTreeSet::from([1]));
        let mut state = TocState::default();
        state.set_collapsed_paths(saved);
        let p = state.prepare(&outline, 0, at(3, 500.0));
        // Everything is open except 1.1 Scope.
        assert_eq!(p.rows.len(), 6);
        assert!(p.rows.iter().any(|r| r.idx == 1 && r.fold == Fold::Closed));
    }

    #[test]
    fn collapsed_paths_before_the_outline_is_seen_return_what_was_set() {
        let outline = sample();
        let mut state = TocState::default();
        let saved = vec![vec!["2 Design".to_string()]];
        state.set_collapsed_paths(saved.clone());
        assert_eq!(state.collapsed_paths(&outline), saved);
    }

    // --- speed --------------------------------------------------------------------------

    fn big_outline() -> Outline {
        // 25 parts x 20 chapters x 10 sections = 5525 entries.
        let parts = (0..25)
            .map(|p| {
                let chapters = (0..20)
                    .map(|c| {
                        let sections = (0..10)
                            .map(|s| {
                                node(
                                    &format!("Section {p}.{c}.{s} Handling"),
                                    Some((p, c as f32 * 30.0 + s as f32)),
                                    vec![],
                                )
                            })
                            .collect();
                        node(
                            &format!("Chapter {p}.{c}"),
                            Some((p, c as f32 * 30.0)),
                            sections,
                        )
                    })
                    .collect();
                node(&format!("Part {p}"), Some((p, 0.0)), chapters)
            })
            .collect();
        Outline::from_tree(parts)
    }

    #[test]
    fn five_thousand_entries_are_cheap_to_lay_out_and_filter() {
        use std::time::Instant;
        let outline = big_outline();
        assert!(outline.items.len() >= 5000);
        let collapsed = initial_collapsed(&outline);

        let started = Instant::now();
        let mut rows = Vec::new();
        for _ in 0..20 {
            rows = visible_rows(&outline, &collapsed);
        }
        let per_rows = started.elapsed() / 20;
        assert_eq!(rows.len(), 25);

        let started = Instant::now();
        let mut matched = Vec::new();
        for _ in 0..20 {
            matched = filtered_rows(&outline, "7.1.2 handling");
        }
        let per_filter = started.elapsed() / 20;
        assert!(!matched.is_empty());

        let mut state = TocState::default();
        let started = Instant::now();
        let p = state.prepare(&outline, 0, at(12, 100.0));
        let per_prepare = started.elapsed();
        assert!(p.current.is_some());

        // A frame in which only the filter text stayed the same does not search again.
        state.filter = "7.1.2 handling".into();
        state.prepare(&outline, 0, at(12, 100.0));
        let started = Instant::now();
        let again = state.prepare(&outline, 0, at(12, 100.0));
        let per_cached = started.elapsed();
        assert!(!again.rows.is_empty());

        eprintln!(
            "WP-31 timing for {} entries: visible_rows {per_rows:?}, filtered_rows {per_filter:?}, \
             first prepare {per_prepare:?}, \
             prepare with a cached filter {per_cached:?}",
            outline.items.len()
        );
        // Generous: the point is that nothing is quadratic or per-entry expensive.
        let limit = std::time::Duration::from_millis(100);
        assert!(
            per_rows < limit && per_filter < limit && per_prepare < limit && per_cached < limit
        );
    }

    // --- drawing, without a window ------------------------------------------------------

    use crate::action::{OpenTarget, dispatch};
    use crate::state::AppState;
    use crate::views::test_support::watch_repaints;
    use crate::windows::show_window;
    use refdesk_core::testutil::PdfBuilder;

    fn frame(ctx: &egui::Context, state: &mut AppState, events: Vec<egui::Event>) -> Vec<Action> {
        frame_with(ctx, state, events, egui::Modifiers::NONE)
    }

    fn frame_with(
        ctx: &egui::Context,
        state: &mut AppState,
        events: Vec<egui::Event>,
        modifiers: egui::Modifiers,
    ) -> Vec<Action> {
        let mut queue = Vec::new();
        let mut all = vec![egui::Event::ModifiersChanged(modifiers)];
        all.extend(events);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1000.0, 700.0))),
            events: all,
            ..Default::default()
        };
        ctx.run_ui(input, |ui| show_window(ui, state, 0, &mut queue))
            .drop_without_applying_deltas();
        queue
    }

    fn open_ready(
        ctx: &egui::Context,
        state: &mut AppState,
        pdf: &refdesk_core::testutil::TempPdf,
    ) -> TabId {
        let repaints = watch_repaints(ctx);
        dispatch(
            state,
            Action::OpenFiles(vec![OpenTarget::parse(
                pdf.path().to_str().expect("utf-8 temp path"),
            )]),
            ctx,
        );
        let tab = state.focus.expect("focused");
        let doc = state
            .docs
            .get(state.tabs.pdf(tab).expect("pdf").doc)
            .expect("doc")
            .clone();
        repaints.wait_until("the document info", || doc.info().is_some());
        tab
    }

    #[test]
    fn the_panel_draws_for_a_document_with_an_outline() {
        let pdf = PdfBuilder::new()
            .page(595.0, 842.0, |p| {
                p.text(72.0, 100.0, 18.0, "1 Introduction");
            })
            .page(595.0, 842.0, |p| {
                p.text(72.0, 100.0, 18.0, "2 Design");
            })
            .outline("1 Introduction", 1, 100.0, |o| {
                o.child("1.1 Scope", 1, 300.0);
            })
            .outline_leaf("2 Design", 2, 100.0)
            .write_temp("wp31-panel");
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let tab = open_ready(&ctx, &mut state, &pdf);
        dispatch(&mut state, Action::ToggleToc, &ctx);
        assert!(state.main_window().toc_open);
        for _ in 0..4 {
            let queue = frame(&ctx, &mut state, Vec::new());
            assert!(queue.is_empty(), "nothing was clicked: {queue:?}");
        }
        // The view was laid out beside the panel, and the panel shows the top-level entries.
        let toc = &state.tabs.pdf(tab).expect("pdf").toc;
        assert_eq!(toc.collapsed().map(|c| c.len()), Some(1));
        assert!(state.main_window().toc_width >= TOC_WIDTH_RANGE.0);
        // Closing the panel draws without it.
        dispatch(&mut state, Action::ToggleToc, &ctx);
        frame(&ctx, &mut state, Vec::new());
        assert!(!state.main_window().toc_open);
    }

    #[test]
    fn the_panel_draws_for_the_start_page() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(&mut state, Action::ToggleToc, &ctx);
        for _ in 0..3 {
            assert!(frame(&ctx, &mut state, Vec::new()).is_empty());
        }
    }

    #[test]
    fn the_panel_draws_for_a_document_without_an_outline() {
        let pdf = PdfBuilder::new()
            .page(595.0, 842.0, |p| {
                p.text(72.0, 100.0, 18.0, "No outline here");
            })
            .write_temp("wp31-no-outline");
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        // The document is opened before any frame: see `watch_repaints` in `open_ready`.
        let tab = open_ready(&ctx, &mut state, &pdf);
        dispatch(&mut state, Action::ToggleToc, &ctx);
        for _ in 0..3 {
            assert!(frame(&ctx, &mut state, Vec::new()).is_empty());
        }
        let toc = &state.tabs.pdf(tab).expect("pdf").toc;
        assert!(
            toc.collapsed().is_none(),
            "no outline: nothing was prepared"
        );
    }

    #[test]
    fn the_panel_draws_while_the_document_is_still_opening_or_missing() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(&mut state, Action::ToggleToc, &ctx);
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/missing-toc.pdf")]),
            &ctx,
        );
        for _ in 0..3 {
            assert!(frame(&ctx, &mut state, Vec::new()).is_empty());
        }
    }

    #[test]
    fn clicking_a_title_in_the_panel_navigates_here_and_the_triangle_only_folds() {
        let pdf = PdfBuilder::new()
            .page(595.0, 842.0, |p| {
                p.text(72.0, 100.0, 18.0, "1 Introduction");
            })
            .page(595.0, 842.0, |p| {
                p.text(72.0, 100.0, 18.0, "2 Design");
            })
            .outline("1 Introduction", 1, 100.0, |o| {
                o.child("1.1 Scope", 1, 300.0);
            })
            .outline_leaf("2 Design", 2, 120.0)
            .write_temp("wp31-click");
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let tab = open_ready(&ctx, &mut state, &pdf);
        dispatch(&mut state, Action::ToggleToc, &ctx);
        for _ in 0..3 {
            frame(&ctx, &mut state, Vec::new());
        }
        // Rows start below the heading, the separator and the filter box; find the row by
        // probing the panel from the top: the second row is "2 Design".
        let click_at = |ctx: &egui::Context,
                        state: &mut AppState,
                        pos: Pos2,
                        modifiers: egui::Modifiers,
                        button: egui::PointerButton| {
            let ev = |pressed| egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers,
            };
            let mut queue = Vec::new();
            queue.extend(frame_with(
                ctx,
                state,
                vec![egui::Event::PointerMoved(pos)],
                modifiers,
            ));
            queue.extend(frame_with(ctx, state, vec![ev(true)], modifiers));
            queue.extend(frame_with(ctx, state, vec![ev(false)], modifiers));
            queue
        };
        let mut found = None;
        for y in (40..200).step_by(4) {
            let pos = Pos2::new(100.0, y as f32);
            let queue = click_at(
                &ctx,
                &mut state,
                pos,
                egui::Modifiers::NONE,
                egui::PointerButton::Primary,
            );
            if let Some(Action::Navigate { to, how, .. }) = queue
                .into_iter()
                .find(|a| matches!(a, Action::Navigate { .. }))
                && to.page == 1
            {
                found = Some((to, how));
                break;
            }
        }
        let (to, how) = found.expect("a click on a title queued a Navigate to page 2");
        assert_eq!((to.page, to.y), (1, 120.0));
        assert_eq!(how, OpenHow::Here);

        // Ctrl+click opens a new tab.
        let mut queue = Vec::new();
        for y in (40..200).step_by(4) {
            let pos = Pos2::new(100.0, y as f32);
            queue = click_at(
                &ctx,
                &mut state,
                pos,
                egui::Modifiers::CTRL,
                egui::PointerButton::Primary,
            );
            if queue
                .iter()
                .any(|a| matches!(a, Action::Navigate { to, .. } if to.page == 1))
            {
                break;
            }
        }
        assert!(
            queue.iter().any(
                |a| matches!(a, Action::Navigate { tab: t, how: OpenHow::NewTab, .. } if *t == tab)
            ),
            "{queue:?}"
        );
        // Middle click too.
        let mut queue = Vec::new();
        for y in (40..200).step_by(4) {
            let pos = Pos2::new(100.0, y as f32);
            queue = click_at(
                &ctx,
                &mut state,
                pos,
                egui::Modifiers::NONE,
                egui::PointerButton::Middle,
            );
            if queue
                .iter()
                .any(|a| matches!(a, Action::Navigate { to, .. } if to.page == 1))
            {
                break;
            }
        }
        assert!(
            queue.iter().any(|a| matches!(
                a,
                Action::Navigate {
                    how: OpenHow::NewTab,
                    ..
                }
            )),
            "{queue:?}"
        );
    }

    #[test]
    fn escape_in_the_filter_box_clears_it() {
        let pdf = PdfBuilder::new()
            .page(595.0, 842.0, |p| {
                p.text(72.0, 100.0, 18.0, "1 Introduction");
            })
            .outline_leaf("1 Introduction", 1, 100.0)
            .outline_leaf("2 Design", 1, 200.0)
            .write_temp("wp31-esc");
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let tab = open_ready(&ctx, &mut state, &pdf);
        dispatch(&mut state, Action::ToggleToc, &ctx);
        for _ in 0..3 {
            frame(&ctx, &mut state, Vec::new());
        }
        // Find the filter box by clicking down the top of the panel until typing reaches it.
        let mut typed = false;
        for y in (4..90).step_by(3) {
            let pos = Pos2::new(100.0, y as f32);
            let click = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            frame(&ctx, &mut state, vec![egui::Event::PointerMoved(pos)]);
            frame(&ctx, &mut state, vec![click(true)]);
            frame(&ctx, &mut state, vec![click(false)]);
            frame(&ctx, &mut state, vec![egui::Event::Text("des".into())]);
            if !state.tabs.pdf(tab).expect("pdf").toc.filter.is_empty() {
                typed = true;
                break;
            }
        }
        assert!(typed, "the filter box takes typed text");
        assert_eq!(state.tabs.pdf(tab).expect("pdf").toc.filter, "des");
        let esc = |pressed| egui::Event::Key {
            key: Key::Escape,
            physical_key: Some(Key::Escape),
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&ctx, &mut state, vec![esc(true)]);
        frame(&ctx, &mut state, vec![esc(false)]);
        assert_eq!(state.tabs.pdf(tab).expect("pdf").toc.filter, "");
    }

    #[test]
    fn titles_are_laid_out_on_one_line_with_the_matches_marked() {
        let ctx = egui::Context::default();
        let font = egui::FontId::proportional(14.0);
        let job = title_job(
            "A very long title that cannot possibly fit into forty pixels",
            &std::iter::once(2..6).collect::<Vec<_>>(),
            font,
            egui::Color32::WHITE,
            egui::Color32::YELLOW,
            40.0,
        );
        assert_eq!(job.sections.len(), 3);
        assert_eq!(
            job.text,
            "A very long title that cannot possibly fit into forty pixels"
        );
        let mut elided = false;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            elided = ui.painter().layout_job(job.clone()).elided;
        })
        .drop_without_applying_deltas();
        assert!(elided, "a long title is shortened");
    }
}
