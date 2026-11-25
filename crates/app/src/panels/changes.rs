//! The change list of a comparison tab and the position strips beside its scroll bars (DIFF-5,
//! DIFF-7; `docs/ARCHITECTURE.md` §4.9).
//!
//! Everything that decides something is a plain function or a method of [`ChangesState`] that
//! does not touch egui, so it is tested without a window (rule R-10): how the changes are grouped
//! by section, which rows are visible, what a row says, which row the current change is on, where
//! a change is marked on a strip, how marks that share a pixel merge, and which document place a
//! click on a strip means. [`show`] and [`show_strip`] only draw those results and turn clicks
//! into [`Action`]s or fractions.
//!
//! The grouping and the strip positions are worked out when a result arrives, not every frame.

use std::collections::BTreeSet;
use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Color32, Pos2, Rect, RichText, ScrollArea, Sense, Ui, pos2, vec2};
use refdesk_core::{Change, ChangeKind, DiffResult, Location, Outline, Side, Size};

use crate::action::Action;
use crate::i18n::tr;
use crate::panels::toc::scroll_target;
use crate::state::TabId;
use crate::views::diff_view::counts_text;

/// The width of a panel that has not been resized.
pub const DEFAULT_PANEL_WIDTH: f32 = 300.0;
/// The narrowest and the widest the panel can be dragged to.
pub const PANEL_WIDTH_RANGE: (f32, f32) = (180.0, 700.0);
/// The width of a position strip (logical pixels).
pub const STRIP_WIDTH: f32 = 8.0;
/// A tick on a strip is drawn at least this thick, though it stands for one pixel row.
const TICK_THICKNESS: f32 = 2.0;
/// Characters of a change's text shown on its row before the row shortens it itself.
pub const SNIPPET_CHARS: usize = 160;
/// Characters of each side's text in the tooltip of a row.
pub const TOOLTIP_CHARS: usize = 200;
/// Each level of nesting moves a row this far to the right.
const INDENT: f32 = 14.0;

// ------------------------------------------------------------------------------------------------
// Kinds
// ------------------------------------------------------------------------------------------------

/// Which kinds of change the list shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindFilter {
    /// Show additions.
    pub added: bool,
    /// Show removals.
    pub removed: bool,
    /// Show modifications.
    pub modified: bool,
}

impl Default for KindFilter {
    /// Everything shown.
    fn default() -> Self {
        KindFilter {
            added: true,
            removed: true,
            modified: true,
        }
    }
}

impl KindFilter {
    /// Whether changes of `kind` are shown.
    pub fn shows(&self, kind: ChangeKind) -> bool {
        match kind {
            ChangeKind::Added => self.added,
            ChangeKind::Removed => self.removed,
            ChangeKind::Modified => self.modified,
        }
    }

    /// Switches one kind.
    pub fn toggle(&mut self, kind: ChangeKind) {
        let flag = match kind {
            ChangeKind::Added => &mut self.added,
            ChangeKind::Removed => &mut self.removed,
            ChangeKind::Modified => &mut self.modified,
        };
        *flag = !*flag;
    }
}

/// The kinds in the order the filter buttons are shown.
pub const KINDS: [ChangeKind; 3] = [ChangeKind::Added, ChangeKind::Removed, ChangeKind::Modified];

/// The letter-like symbol of a kind, so that the kind does not rest on color alone.
pub fn kind_symbol(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "＋",
        ChangeKind::Removed => "－",
        ChangeKind::Modified => "～",
    }
}

/// The i18n key of the name of a kind.
pub fn kind_name_key(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "changes.kind_added",
        ChangeKind::Removed => "changes.kind_removed",
        ChangeKind::Modified => "changes.kind_modified",
    }
}

/// The color of a kind as RGB: green, red and orange, a little lighter on a dark theme.
pub fn kind_color(kind: ChangeKind, dark: bool) -> [u8; 3] {
    match (kind, dark) {
        (ChangeKind::Added, false) => [20, 140, 40],
        (ChangeKind::Removed, false) => [200, 20, 20],
        (ChangeKind::Modified, false) => [200, 110, 0],
        (ChangeKind::Added, true) => [90, 210, 110],
        (ChangeKind::Removed, true) => [245, 100, 100],
        (ChangeKind::Modified, true) => [240, 170, 60],
    }
}

fn color32(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// Which of two kinds wins the color of a strip pixel they share: removals, then additions, then
/// modifications.
pub fn stronger(a: ChangeKind, b: ChangeKind) -> ChangeKind {
    let rank = |k: ChangeKind| match k {
        ChangeKind::Removed => 2,
        ChangeKind::Added => 1,
        ChangeKind::Modified => 0,
    };
    if rank(b) > rank(a) { b } else { a }
}

// ------------------------------------------------------------------------------------------------
// Grouping
// ------------------------------------------------------------------------------------------------

/// What a run of changes belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupKey {
    /// Before the first section of a document that has a table of contents.
    Start,
    /// A section: the titles from the top level down to it.
    Section(Vec<String>),
    /// A page (zero-based), for a document without a table of contents.
    Page(u32),
}

/// A run of neighboring changes of one section (or page).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// What the changes belong to.
    pub key: GroupKey,
    /// Indices into `DiffResult::changes`, in document order.
    pub changes: Vec<u32>,
}

/// The changes of a result gathered by section.
#[derive(Clone, Debug, Default)]
pub struct Grouping {
    /// The groups in document order.
    pub groups: Vec<Group>,
    /// For each change, the index of its group.
    group_of: Vec<u32>,
}

impl Grouping {
    /// The group of change `change`.
    pub fn group_of(&self, change: u32) -> Option<u32> {
        self.group_of.get(change as usize).copied()
    }
}

/// The group a change belongs to. The section is the one of the table of contents that is current
/// at the change's place: for additions and modifications that is the newer (right) document, for
/// removals the older (left) one. A document without a table of contents gives pages.
pub fn group_key(change: &Change, left: &Outline, right: &Outline) -> GroupKey {
    let (outline, at) = match change.kind {
        ChangeKind::Removed => (left, change.anchor.0),
        ChangeKind::Added | ChangeKind::Modified => (right, change.anchor.1),
    };
    if outline.items.is_empty() {
        return GroupKey::Page(at.page);
    }
    match outline.current(at) {
        Some(idx) => GroupKey::Section(
            outline
                .path_titles(idx)
                .into_iter()
                .map(str::to_owned)
                .collect(),
        ),
        None => GroupKey::Start,
    }
}

/// Gathers the changes into groups: neighbors in the list with the same section share a group.
pub fn group_changes(changes: &[Change], left: &Outline, right: &Outline) -> Grouping {
    let mut grouping = Grouping {
        groups: Vec::new(),
        group_of: Vec::with_capacity(changes.len()),
    };
    for (i, change) in changes.iter().enumerate() {
        let key = group_key(change, left, right);
        if grouping.groups.last().is_none_or(|g| g.key != key) {
            grouping.groups.push(Group {
                key,
                changes: Vec::new(),
            });
        }
        let index = grouping.groups.len() as u32 - 1;
        if let Some(group) = grouping.groups.last_mut() {
            group.changes.push(i as u32);
        }
        grouping.group_of.push(index);
    }
    grouping
}

/// The last two titles of a section path: "7.3 State › 7.3.2 Transitions".
pub fn section_label(path: &[String]) -> String {
    let from = path.len().saturating_sub(2);
    path[from..].join(" › ")
}

/// The text of a group's header (without the count).
pub fn group_label(key: &GroupKey) -> String {
    match key {
        GroupKey::Start => tr("changes.start").to_owned(),
        GroupKey::Section(path) => section_label(path),
        GroupKey::Page(page) => page_label(*page),
    }
}

/// "12쪽" for a zero-based page.
pub fn page_label(page: u32) -> String {
    tr("changes.page").replace("{n}", &(page + 1).to_string())
}

// ------------------------------------------------------------------------------------------------
// Rows
// ------------------------------------------------------------------------------------------------

/// One visible line of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// The header of a section.
    Header {
        /// The group.
        group: u32,
        /// How many of its changes the filter lets through.
        shown: u32,
        /// Its changes are hidden.
        folded: bool,
    },
    /// One change.
    Change {
        /// The group it is in.
        group: u32,
        /// Index into `DiffResult::changes`.
        change: u32,
    },
}

/// The lines of the list: for each group with at least one change the filter lets through its
/// header, then (unless the group is folded) those changes.
pub fn visible_rows(
    grouping: &Grouping,
    changes: &[Change],
    filter: KindFilter,
    folded: &BTreeSet<u32>,
) -> Vec<Row> {
    let mut rows = Vec::with_capacity(changes.len() + grouping.groups.len());
    for (g, group) in grouping.groups.iter().enumerate() {
        let group_index = g as u32;
        let passes = |c: &&u32| {
            changes
                .get(**c as usize)
                .is_some_and(|ch| filter.shows(ch.kind))
        };
        let shown = group.changes.iter().filter(passes).count() as u32;
        if shown == 0 {
            continue;
        }
        let is_folded = folded.contains(&group_index);
        rows.push(Row::Header {
            group: group_index,
            shown,
            folded: is_folded,
        });
        if !is_folded {
            rows.extend(group.changes.iter().filter(passes).map(|c| Row::Change {
                group: group_index,
                change: *c,
            }));
        }
    }
    rows
}

/// The line that shows change `change`, if it is visible.
pub fn row_of_change(rows: &[Row], change: u32) -> Option<usize> {
    rows.iter()
        .position(|r| matches!(r, Row::Change { change: c, .. } if *c == change))
}

/// The first line of `text`, with whitespace runs made single spaces, cut to `max` characters
/// with "…" when longer.
pub fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let squeezed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&squeezed, max)
}

/// `text` cut to `max` characters, with "…" in place of the rest.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The text a change's row shows: where it starts (the newer document's text, the older
/// document's for a removal).
pub fn change_snippet(change: &Change) -> String {
    let text = match change.kind {
        ChangeKind::Removed => &change.left_text,
        ChangeKind::Added | ChangeKind::Modified => &change.right_text,
    };
    first_line(text, SNIPPET_CHARS)
}

/// "12쪽" when both sides are on the same page, else "이전 12쪽 → 이후 13쪽".
pub fn change_pages(change: &Change) -> String {
    let (left, right) = (change.anchor.0.page, change.anchor.1.page);
    if left == right {
        return page_label(left);
    }
    tr("changes.page_both")
        .replace("{left}", &(left + 1).to_string())
        .replace("{right}", &(right + 1).to_string())
}

/// The tooltip of a row: the text of both sides, each cut to [`TOOLTIP_CHARS`]. A side without
/// text (the older one of an addition) is left out.
pub fn change_tooltip(change: &Change) -> String {
    let mut lines = Vec::new();
    for (key, text) in [
        ("changes.tip_left", &change.left_text),
        ("changes.tip_right", &change.right_text),
    ] {
        if !text.trim().is_empty() {
            lines.push(format!("{}: {}", tr(key), clip(text.trim(), TOOLTIP_CHARS)));
        }
    }
    lines.join("\n")
}

// ------------------------------------------------------------------------------------------------
// Position strips
// ------------------------------------------------------------------------------------------------

/// Where each page of a document starts when the pages are stacked without gaps, in pt: the basis
/// of "how far into the document" for the strips.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Extent {
    /// The top of every page, then the total height as the last entry.
    tops: Vec<f64>,
}

impl Extent {
    /// The extent of a document with these page sizes.
    pub fn new(sizes: &[Size]) -> Extent {
        let mut tops = Vec::with_capacity(sizes.len() + 1);
        let mut at = 0.0;
        tops.push(at);
        for size in sizes {
            at += f64::from(size.h.max(0.0));
            tops.push(at);
        }
        Extent { tops }
    }

    /// The number of pages.
    pub fn pages(&self) -> usize {
        self.tops.len().saturating_sub(1)
    }

    /// The total height in pt.
    pub fn total(&self) -> f64 {
        self.tops.last().copied().unwrap_or(0.0)
    }

    /// How far into the document `loc` is, from 0.0 (start) to 1.0 (end). A page past the end
    /// counts as the last; `y` is limited to the page.
    pub fn fraction(&self, loc: Location) -> f32 {
        let total = self.total();
        if self.pages() == 0 || total <= 0.0 {
            return 0.0;
        }
        let page = (loc.page as usize).min(self.pages() - 1);
        let height = self.tops[page + 1] - self.tops[page];
        let y = f64::from(loc.y).clamp(0.0, height);
        ((self.tops[page] + y) / total).clamp(0.0, 1.0) as f32
    }

    /// The place `fraction` of the way into the document (clamped to 0.0 ..= 1.0), in page space.
    pub fn location_at(&self, fraction: f32) -> Location {
        let total = self.total();
        if self.pages() == 0 || total <= 0.0 {
            return Location::default();
        }
        let at = f64::from(fraction.clamp(0.0, 1.0)) * total;
        // The last page whose top is not after `at`.
        let page = self.tops[..self.pages()]
            .partition_point(|top| *top <= at)
            .saturating_sub(1);
        Location::new(page as u32, 0.0, (at - self.tops[page]) as f32)
    }
}

/// A change to mark on a strip: how far into the document, and what kind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StripPoint {
    /// 0.0 ..= 1.0.
    pub fraction: f32,
    /// The kind, which gives the color.
    pub kind: ChangeKind,
}

/// The changes that are in the document of `side` (removals and modifications on the left,
/// additions and modifications on the right) with where they are.
pub fn strip_points(changes: &[Change], side: Side, extent: &Extent) -> Vec<StripPoint> {
    changes
        .iter()
        .filter(|c| {
            matches!(
                (side, c.kind),
                (_, ChangeKind::Modified)
                    | (Side::Left, ChangeKind::Removed)
                    | (Side::Right, ChangeKind::Added)
            )
        })
        .map(|c| StripPoint {
            fraction: extent.fraction(match side {
                Side::Left => c.anchor.0,
                Side::Right => c.anchor.1,
            }),
            kind: c.kind,
        })
        .collect()
}

/// One tick of a strip: a pixel row and the color of the strongest change in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tick {
    /// The pixel row from the top of the strip.
    pub row: u32,
    /// The kind that colors it.
    pub kind: ChangeKind,
}

/// The ticks of a strip `height` pixels high, in order from the top. Changes that fall on the same
/// pixel row make one tick, so there are never more ticks than rows however many changes there
/// are. A row with different kinds takes the color of the [`stronger`] one.
pub fn strip_ticks(points: &[StripPoint], height: u32) -> Vec<Tick> {
    if height == 0 {
        return Vec::new();
    }
    let mut rows: Vec<Option<ChangeKind>> = vec![None; height as usize];
    for point in points {
        let row = ((point.fraction.clamp(0.0, 1.0) * height as f32) as usize).min(rows.len() - 1);
        rows[row] = Some(match rows[row] {
            Some(kind) => stronger(kind, point.kind),
            None => point.kind,
        });
    }
    rows.iter()
        .enumerate()
        .filter_map(|(row, kind)| {
            kind.map(|kind| Tick {
                row: row as u32,
                kind,
            })
        })
        .collect()
}

/// How far down a strip a pointer at vertical position `y` is, for a strip that starts at `top`
/// and is `height` high (limited to 0.0 ..= 1.0).
pub fn fraction_at(y: f32, top: f32, height: f32) -> f32 {
    if height <= 0.0 {
        return 0.0;
    }
    ((y - top) / height).clamp(0.0, 1.0)
}

/// The part of a strip the view shows now, as fractions: from the place at the top of the
/// viewport to the one at its bottom.
pub fn visible_span(extent: &Extent, top: Location, bottom: Location) -> (f32, f32) {
    let (a, b) = (extent.fraction(top), extent.fraction(bottom));
    (a.min(b), a.max(b))
}

/// What the strips of a comparison need, worked out once per result.
#[derive(Clone, Debug)]
pub struct Overview {
    left_extent: Extent,
    right_extent: Extent,
    left: Vec<StripPoint>,
    right: Vec<StripPoint>,
}

impl Overview {
    /// The strips of a result.
    pub fn new(result: &DiffResult) -> Overview {
        let left_extent = Extent::new(&result.left_page_sizes);
        let right_extent = Extent::new(&result.right_page_sizes);
        Overview {
            left: strip_points(&result.changes, Side::Left, &left_extent),
            right: strip_points(&result.changes, Side::Right, &right_extent),
            left_extent,
            right_extent,
        }
    }

    /// The page extent of one side.
    pub fn extent(&self, side: Side) -> &Extent {
        match side {
            Side::Left => &self.left_extent,
            Side::Right => &self.right_extent,
        }
    }

    /// The marks of one side.
    pub fn points(&self, side: Side) -> &[StripPoint] {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }
}

// ------------------------------------------------------------------------------------------------
// State
// ------------------------------------------------------------------------------------------------

/// What is worked out from one result.
#[derive(Clone, Debug)]
struct Prepared {
    /// Kept so that a different result is noticed (and so its address is not reused).
    result: Arc<DiffResult>,
    grouping: Grouping,
    overview: Arc<Overview>,
}

/// The unsaved state of the change list of one comparison tab.
#[derive(Clone, Debug)]
pub struct ChangesState {
    /// Whether the panel is shown.
    pub open: bool,
    filter: KindFilter,
    folded: BTreeSet<u32>,
    prepared: Option<Prepared>,
    rows: Arc<Vec<Row>>,
    rows_stale: bool,
    /// The current change as of the last frame, to notice when it moved.
    seen_current: Option<usize>,
    /// The change whose row is to be brought into view.
    scroll_to: Option<u32>,
    /// The scroll offset and viewport height of the list as of the last frame.
    scroll_offset: f32,
    view_height: f32,
}

impl Default for ChangesState {
    /// The panel is shown, with every kind.
    fn default() -> Self {
        ChangesState {
            open: true,
            filter: KindFilter::default(),
            folded: BTreeSet::new(),
            prepared: None,
            rows: Arc::new(Vec::new()),
            rows_stale: true,
            seen_current: None,
            scroll_to: None,
            scroll_offset: 0.0,
            view_height: 0.0,
        }
    }
}

impl ChangesState {
    /// The kinds shown.
    pub fn filter(&self) -> KindFilter {
        self.filter
    }

    /// Shows or hides one kind in the list.
    pub fn toggle_kind(&mut self, kind: ChangeKind) {
        self.filter.toggle(kind);
        self.rows_stale = true;
    }

    /// Whether a group is folded.
    #[cfg(test)]
    pub fn is_folded(&self, group: u32) -> bool {
        self.folded.contains(&group)
    }

    /// Folds or unfolds a group.
    pub fn toggle_fold(&mut self, group: u32) {
        if !self.folded.remove(&group) {
            self.folded.insert(group);
        }
        self.rows_stale = true;
    }

    /// Takes in a result. Nothing happens when it is the one already prepared; a different one
    /// is grouped again, with nothing folded and no current change.
    pub fn prepare(&mut self, result: &Arc<DiffResult>, left: &Outline, right: &Outline) {
        if self
            .prepared
            .as_ref()
            .is_some_and(|p| Arc::ptr_eq(&p.result, result))
        {
            return;
        }
        self.prepared = Some(Prepared {
            grouping: group_changes(&result.changes, left, right),
            overview: Arc::new(Overview::new(result)),
            result: Arc::clone(result),
        });
        self.folded.clear();
        self.rows_stale = true;
        self.seen_current = None;
        self.scroll_to = None;
    }

    /// The grouping of the prepared result.
    pub fn grouping(&self) -> Option<&Grouping> {
        self.prepared.as_ref().map(|p| &p.grouping)
    }

    /// The strips of the prepared result.
    pub fn overview(&self) -> Option<Arc<Overview>> {
        self.prepared.as_ref().map(|p| Arc::clone(&p.overview))
    }

    /// The visible lines, recomputed only after something changed.
    pub fn rows(&mut self) -> Arc<Vec<Row>> {
        if self.rows_stale {
            self.rows = Arc::new(match &self.prepared {
                Some(p) => visible_rows(&p.grouping, &p.result.changes, self.filter, &self.folded),
                None => Vec::new(),
            });
            self.rows_stale = false;
        }
        Arc::clone(&self.rows)
    }

    /// Tells the list which change is the current one. When it moved since the last call, its
    /// group is unfolded and its row is to be scrolled into view.
    pub fn note_current(&mut self, current: Option<usize>) {
        if current == self.seen_current {
            return;
        }
        self.seen_current = current;
        let Some(change) = current.map(|c| c as u32) else {
            return;
        };
        if let Some(group) = self.grouping().and_then(|g| g.group_of(change))
            && self.folded.remove(&group)
        {
            self.rows_stale = true;
        }
        self.scroll_to = Some(change);
    }

    /// The change whose row is waiting to be scrolled into view.
    #[cfg(test)]
    pub fn pending_scroll(&self) -> Option<u32> {
        self.scroll_to
    }

    /// The scroll offset of the list that shows the row waiting to be scrolled to, if it has to
    /// move: `None` when nothing waits, the row is visible already, or the pointer is over the
    /// list (the user is working in it). The request is used up either way.
    pub fn take_scroll(&mut self, rows: &[Row], stride: f32, pointer_in_list: bool) -> Option<f32> {
        let change = self.scroll_to.take()?;
        if pointer_in_list {
            return None;
        }
        let row = row_of_change(rows, change)?;
        scroll_target(row, stride, self.scroll_offset, self.view_height)
    }
}

// ------------------------------------------------------------------------------------------------
// Drawing
// ------------------------------------------------------------------------------------------------

/// What the panel shows for a comparison tab.
pub enum PanelContent<'a> {
    /// The comparison is running; the progress from 0.0 to 1.0.
    Running(f32),
    /// There is no result (cancelled, failed).
    Unavailable,
    /// A result, with the outlines of the documents it compares.
    Result {
        /// The result.
        result: &'a Arc<DiffResult>,
        /// The table of contents of the older document.
        left: &'a Outline,
        /// The table of contents of the newer document.
        right: &'a Outline,
        /// A document changed since: the list is dimmed and cannot be used to move.
        stale: bool,
    },
}

enum RowEvent {
    Fold(u32),
    Go(u32),
}

fn hint(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).weak());
}

/// Draws the change list of the comparison tab `tab`, whose current change is `current`.
pub fn show(
    ui: &mut Ui,
    state: &mut ChangesState,
    content: PanelContent<'_>,
    current: Option<usize>,
    tab: TabId,
    queue: &mut Vec<Action>,
) {
    ui.label(RichText::new(tr("changes.title")).strong());
    ui.separator();
    let (result, left, right, stale) = match content {
        PanelContent::Running(progress) => {
            ui.add(egui::ProgressBar::new(progress).show_percentage());
            hint(ui, tr("diff.running"));
            return;
        }
        PanelContent::Unavailable => {
            hint(ui, tr("changes.no_result"));
            return;
        }
        PanelContent::Result {
            result,
            left,
            right,
            stale,
        } => (result, left, right, stale),
    };
    if stale {
        ui.set_opacity(0.45);
    }
    state.prepare(result, left, right);
    ui.label(counts_text(result));
    show_filter(ui, state);
    if result.changes.is_empty() {
        ui.add_space(4.0);
        ui.strong(tr("diff.no_changes"));
        return;
    }
    ui.separator();

    state.note_current(current);
    let rows = state.rows();
    if rows.is_empty() {
        hint(ui, tr("changes.all_hidden"));
        return;
    }
    let Some(grouping) = state.grouping().cloned() else {
        return;
    };
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
    let stride = row_h + ui.spacing().item_spacing.y;
    let in_list = ui.rect_contains_pointer(ui.max_rect());
    let mut area = ScrollArea::vertical().auto_shrink([false, false]);
    if let Some(offset) = state.take_scroll(&rows, stride, in_list) {
        area = area.vertical_scroll_offset(offset);
    }
    let mut events = Vec::new();
    let output = area.show_rows(ui, row_h, rows.len(), |ui, range| {
        for row in &rows[range] {
            match *row {
                Row::Header {
                    group,
                    shown,
                    folded,
                } => {
                    if let Some(g) = grouping.groups.get(group as usize) {
                        show_header(ui, row_h, group, g, shown, folded, &mut events);
                    }
                }
                Row::Change { change, .. } => {
                    if let Some(c) = result.changes.get(change as usize) {
                        let is_current = current == Some(change as usize);
                        show_change(ui, row_h, change, c, is_current, &mut events);
                    }
                }
            }
        }
    });
    state.scroll_offset = output.state.clamped_offset().y;
    state.view_height = output.inner_rect.height();

    for event in events {
        match event {
            RowEvent::Fold(group) => state.toggle_fold(group),
            // A result that no longer describes the documents cannot be moved to.
            RowEvent::Go(change) if !stale => queue.push(Action::GoToChange {
                tab,
                index: change as usize,
            }),
            RowEvent::Go(_) => {}
        }
    }
}

/// The buttons that switch the kinds shown.
fn show_filter(ui: &mut Ui, state: &mut ChangesState) {
    let dark = ui.visuals().dark_mode;
    ui.horizontal_wrapped(|ui| {
        for kind in KINDS {
            let on = state.filter().shows(kind);
            let color = if on {
                color32(kind_color(kind, dark))
            } else {
                ui.visuals().weak_text_color()
            };
            let label = RichText::new(format!("{} {}", kind_symbol(kind), tr(kind_name_key(kind))))
                .color(color);
            if ui
                .selectable_label(on, label)
                .on_hover_text(tr("changes.filter_hint"))
                .clicked()
            {
                state.toggle_kind(kind);
            }
        }
    });
}

fn row_rect(ui: &mut Ui, row_h: f32) -> Rect {
    ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::hover())
        .0
}

fn show_header(
    ui: &mut Ui,
    row_h: f32,
    index: u32,
    group: &Group,
    shown: u32,
    folded: bool,
    events: &mut Vec<RowEvent>,
) {
    let rect = row_rect(ui, row_h);
    let id = ui.make_persistent_id(("changes_header", index));
    let response = ui.interact(rect, id, Sense::click());
    let visuals = ui.visuals().clone();
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let font = egui::TextStyle::Body.resolve(ui.style());
    let arrow = if folded { "▸" } else { "▾" };
    let text = format!("{arrow} {}  ({shown})", group_label(&group.key));
    let job = single_line(&text, font, visuals.strong_text_color(), rect.width() - 8.0);
    let galley = painter.layout_job(job);
    let pos = pos2(rect.left() + 2.0, rect.center().y - galley.size().y / 2.0);
    painter.galley(pos, galley, visuals.strong_text_color());
    if response.clicked() {
        events.push(RowEvent::Fold(index));
    }
}

fn show_change(
    ui: &mut Ui,
    row_h: f32,
    index: u32,
    change: &Change,
    is_current: bool,
    events: &mut Vec<RowEvent>,
) {
    let rect = row_rect(ui, row_h);
    let id = ui.make_persistent_id(("changes_row", index));
    let response = ui.interact(rect, id, Sense::click());
    let visuals = ui.visuals().clone();
    let font = egui::TextStyle::Body.resolve(ui.style());
    let painter = ui.painter();
    if is_current {
        painter.rect_filled(rect, 2.0, visuals.selection.bg_fill);
    } else if response.hovered() {
        painter.rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let text_color = if is_current {
        visuals.selection.stroke.color
    } else {
        visuals.text_color()
    };
    let weak = if is_current {
        text_color
    } else {
        visuals.weak_text_color()
    };

    // The symbol, then the text, with the page on the right.
    let mut x = rect.left() + INDENT;
    let kind_color = color32(kind_color(change.kind, visuals.dark_mode));
    let symbol = painter.layout_no_wrap(
        kind_symbol(change.kind).to_owned(),
        font.clone(),
        kind_color,
    );
    let center_y = rect.center().y;
    let symbol_size = symbol.size();
    painter.galley(pos2(x, center_y - symbol_size.y / 2.0), symbol, kind_color);
    x += symbol_size.x + 4.0;

    let page = painter.layout_no_wrap(change_pages(change), font.clone(), weak);
    let page_x = (rect.right() - page.size().x - 4.0).max(x);
    let page_y = center_y - page.size().y / 2.0;
    painter.galley(pos2(page_x, page_y), page, weak);

    let snippet_width = (page_x - x - 6.0).max(0.0);
    if snippet_width > 8.0 {
        let job = single_line(&change_snippet(change), font, text_color, snippet_width);
        let galley = painter.layout_job(job);
        painter.galley(
            Pos2::new(x, center_y - galley.size().y / 2.0),
            galley,
            text_color,
        );
    }

    let tooltip = change_tooltip(change);
    let response = if tooltip.is_empty() {
        response
    } else {
        response.on_hover_text(tooltip)
    };
    if response.clicked() {
        events.push(RowEvent::Go(index));
    }
}

/// One line of text, shortened with "…" at `max_width`.
fn single_line(text: &str, font: egui::FontId, color: Color32, max_width: f32) -> LayoutJob {
    let clean = text.replace(char::is_control, " ");
    let mut job = LayoutJob {
        wrap: TextWrapping {
            max_width: max_width.max(0.0),
            max_rows: 1,
            break_anywhere: true,
            ..Default::default()
        },
        ..Default::default()
    };
    job.append(&clean, 0.0, TextFormat::simple(font, color));
    job
}

/// Draws a position strip in `rect` (`STRIP_WIDTH` wide, as high as the scroll bar beside it) and
/// returns where the user pressed or dragged on it, as a fraction of its height.
pub fn show_strip(
    ui: &mut Ui,
    id: egui::Id,
    rect: Rect,
    ticks: &[Tick],
    visible: Option<(f32, f32)>,
) -> Option<f32> {
    let response = ui.interact(rect, id, Sense::click_and_drag());
    let visuals = ui.visuals().clone();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, visuals.extreme_bg_color);
    if let Some((from, to)) = visible {
        let (y0, y1) = (
            rect.top() + from * rect.height(),
            rect.top() + to * rect.height(),
        );
        let band = Rect::from_min_max(pos2(rect.left(), y0), pos2(rect.right(), y1.max(y0 + 2.0)));
        painter.rect_filled(band, 0.0, visuals.selection.bg_fill.gamma_multiply(0.5));
    }
    for tick in ticks {
        let y = rect.top() + tick.row as f32;
        let color = color32(kind_color(tick.kind, visuals.dark_mode));
        painter.rect_filled(
            Rect::from_min_max(
                pos2(rect.left() + 1.0, y),
                pos2(rect.right() - 1.0, (y + TICK_THICKNESS).min(rect.bottom())),
            ),
            0.0,
            color,
        );
    }
    if response.clicked() || response.dragged() {
        let pointer = response.interact_pointer_pos()?;
        return Some(fraction_at(pointer.y, rect.top(), rect.height()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use refdesk_core::OutlineNode;
    use std::time::Instant;

    fn loc(page: u32, y: f32) -> Location {
        Location::new(page, 0.0, y)
    }

    fn change(kind: ChangeKind, left: Location, right: Location, text: &str) -> Change {
        let (left_text, right_text) = match kind {
            ChangeKind::Added => (String::new(), text.to_owned()),
            ChangeKind::Removed => (text.to_owned(), String::new()),
            ChangeKind::Modified => (format!("old {text}"), text.to_owned()),
        };
        Change {
            kind,
            left: None,
            right: None,
            left_text,
            right_text,
            anchor: (left, right),
        }
    }

    /// "1 Intro" at page 0 y 100, "1.1 Scope" below it at page 1 y 50, "2 Design" at page 3.
    fn outline() -> Outline {
        let node = |title: &str, page: u32, y: f32, children: Vec<OutlineNode>| OutlineNode {
            title: title.to_owned(),
            target: Some(loc(page, y)),
            uri: None,
            children,
        };
        Outline::from_tree(vec![
            node(
                "1 Intro",
                0,
                100.0,
                vec![node("1.1 Scope", 1, 50.0, vec![])],
            ),
            node("2 Design", 3, 0.0, vec![]),
        ])
    }

    /// A different outline for the older document: "2 Design" starts a page later.
    fn old_outline() -> Outline {
        let node = |title: &str, page: u32, y: f32| OutlineNode {
            title: title.to_owned(),
            target: Some(loc(page, y)),
            uri: None,
            children: vec![],
        };
        Outline::from_tree(vec![node("1 Intro", 0, 100.0), node("2 Design", 4, 0.0)])
    }

    fn sample() -> Vec<Change> {
        vec![
            // Before the first section.
            change(
                ChangeKind::Added,
                loc(0, 10.0),
                loc(0, 10.0),
                "preface added",
            ),
            // In 1 Intro.
            change(
                ChangeKind::Modified,
                loc(0, 300.0),
                loc(0, 320.0),
                "intro changed",
            ),
            // In 1 › 1.1 Scope.
            change(
                ChangeKind::Added,
                loc(1, 200.0),
                loc(1, 220.0),
                "scope added",
            ),
            change(
                ChangeKind::Removed,
                loc(1, 400.0),
                loc(1, 420.0),
                "scope removed",
            ),
            // Left page 3 is still in 1.1 of the old outline; right page 3 is "2 Design".
            change(
                ChangeKind::Removed,
                loc(3, 100.0),
                loc(3, 100.0),
                "old design text",
            ),
            change(
                ChangeKind::Added,
                loc(3, 100.0),
                loc(3, 100.0),
                "new design text",
            ),
        ]
    }

    fn labels(grouping: &Grouping) -> Vec<String> {
        grouping
            .groups
            .iter()
            .map(|g| group_label(&g.key))
            .collect()
    }

    // --------------------------------------------------------------------------------------------
    // Grouping
    // --------------------------------------------------------------------------------------------

    #[test]
    fn changes_are_grouped_by_section_in_document_order() {
        let changes = sample();
        let grouping = group_changes(&changes, &old_outline(), &outline());
        let keys: Vec<&GroupKey> = grouping.groups.iter().map(|g| &g.key).collect();
        let path = |p: &[&str]| GroupKey::Section(p.iter().map(|s| (*s).to_owned()).collect());
        assert_eq!(
            keys,
            vec![
                &GroupKey::Start,
                &path(&["1 Intro"]),
                &path(&["1 Intro", "1.1 Scope"]),
                // The removal is judged by the old outline (page 3 is still in 1 Intro there)...
                &path(&["1 Intro"]),
                // ...the addition by the new one.
                &path(&["2 Design"]),
            ]
        );
        // Document order is kept and every change is in exactly one group.
        let flat: Vec<u32> = grouping
            .groups
            .iter()
            .flat_map(|g| g.changes.iter().copied())
            .collect();
        assert_eq!(flat, (0..changes.len() as u32).collect::<Vec<_>>());
        for (i, _) in changes.iter().enumerate() {
            let g = grouping.group_of(i as u32).expect("group");
            assert!(grouping.groups[g as usize].changes.contains(&(i as u32)));
        }
        assert_eq!(grouping.group_of(99), None);
    }

    #[test]
    fn additions_and_modifications_use_the_newer_outline_and_removals_the_older() {
        // The same place (page 3, y 100): "2 Design" in the new outline, "1 Intro" in the old.
        let (new, old) = (outline(), old_outline());
        let at = loc(3, 100.0);
        let key = |kind| group_key(&change(kind, at, at, "x"), &old, &new);
        let design = GroupKey::Section(vec!["2 Design".to_owned()]);
        let intro = GroupKey::Section(vec!["1 Intro".to_owned()]);
        assert_eq!(key(ChangeKind::Added), design);
        assert_eq!(key(ChangeKind::Modified), design);
        assert_eq!(key(ChangeKind::Removed), intro);
        // A modification on different pages is judged at its right place.
        let m = change(ChangeKind::Modified, loc(0, 10.0), loc(3, 100.0), "x");
        assert_eq!(group_key(&m, &old, &new), design);
    }

    #[test]
    fn a_document_without_contents_is_grouped_by_page() {
        let none = Outline::new();
        let changes = vec![
            change(ChangeKind::Added, loc(0, 10.0), loc(0, 10.0), "a"),
            change(ChangeKind::Added, loc(0, 20.0), loc(0, 30.0), "b"),
            change(ChangeKind::Removed, loc(2, 10.0), loc(3, 10.0), "c"),
            change(ChangeKind::Modified, loc(5, 10.0), loc(6, 10.0), "d"),
        ];
        let grouping = group_changes(&changes, &none, &none);
        let keys: Vec<GroupKey> = grouping.groups.iter().map(|g| g.key.clone()).collect();
        // Added and modified: the right page; removed: the left page.
        assert_eq!(
            keys,
            vec![GroupKey::Page(0), GroupKey::Page(2), GroupKey::Page(6)]
        );
        assert_eq!(grouping.groups[0].changes, vec![0, 1]);
        assert_eq!(labels(&grouping)[0], page_label(0));
        // Only one side with contents: the other side's changes go by page.
        let g = group_changes(&changes, &none, &outline());
        assert_eq!(
            g.groups[1].key,
            GroupKey::Page(2),
            "the removal: old document has no contents"
        );
        assert!(matches!(
            g.groups[0].key,
            GroupKey::Start | GroupKey::Section(_)
        ));
    }

    #[test]
    fn a_change_before_the_first_section_is_in_the_start_group() {
        let changes = vec![change(ChangeKind::Added, loc(0, 5.0), loc(0, 5.0), "a")];
        let g = group_changes(&changes, &outline(), &outline());
        assert_eq!(g.groups.len(), 1);
        assert_eq!(g.groups[0].key, GroupKey::Start);
        assert_eq!(group_label(&GroupKey::Start), tr("changes.start"));
    }

    #[test]
    fn a_section_label_is_its_last_two_titles() {
        let p = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            section_label(&p(&["7 A", "7.3 State", "7.3.2 Transitions"])),
            "7.3 State › 7.3.2 Transitions"
        );
        assert_eq!(section_label(&p(&["7 A", "7.3 State"])), "7 A › 7.3 State");
        assert_eq!(section_label(&p(&["7 A"])), "7 A");
        assert_eq!(section_label(&[]), "");
    }

    // --------------------------------------------------------------------------------------------
    // Rows
    // --------------------------------------------------------------------------------------------

    fn rows_of(changes: &[Change], filter: KindFilter, folded: &[u32]) -> (Grouping, Vec<Row>) {
        let grouping = group_changes(changes, &old_outline(), &outline());
        let folded: BTreeSet<u32> = folded.iter().copied().collect();
        let rows = visible_rows(&grouping, changes, filter, &folded);
        (grouping, rows)
    }

    #[test]
    fn rows_are_headers_followed_by_their_changes() {
        let changes = sample();
        let (_, rows) = rows_of(&changes, KindFilter::default(), &[]);
        assert_eq!(rows.len(), 5 + 6);
        assert_eq!(
            rows[0],
            Row::Header {
                group: 0,
                shown: 1,
                folded: false
            }
        );
        assert_eq!(
            rows[1],
            Row::Change {
                group: 0,
                change: 0
            }
        );
        assert_eq!(row_of_change(&rows, 3), Some(7));
        assert_eq!(row_of_change(&rows, 42), None);
    }

    #[test]
    fn folding_a_group_removes_its_change_rows() {
        let changes = sample();
        let (_, rows) = rows_of(&changes, KindFilter::default(), &[3]);
        // Group 3 (1 Intro again, by the old outline) holds the removals 3 and 4.
        assert_eq!(row_of_change(&rows, 3), None);
        assert_eq!(row_of_change(&rows, 4), None);
        assert!(row_of_change(&rows, 1).is_some() && row_of_change(&rows, 5).is_some());
        assert!(rows.contains(&Row::Header {
            group: 3,
            shown: 2,
            folded: true
        }));
        assert_eq!(rows.len(), 11 - 2);
    }

    #[test]
    fn switching_a_kind_off_removes_it_and_empty_groups() {
        let changes = sample();
        let filter = KindFilter {
            added: false,
            ..KindFilter::default()
        };
        let (grouping, rows) = rows_of(&changes, filter, &[]);
        for change in [0u32, 2, 5] {
            assert_eq!(row_of_change(&rows, change), None, "added change {change}");
        }
        // Groups 0 (the start), 2 (1.1 Scope) and 4 (2 Design) only held additions: gone with
        // their header.
        let header_groups: Vec<u32> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Header { group, .. } => Some(*group),
                _ => None,
            })
            .collect();
        assert_eq!(header_groups, vec![1, 3]);
        // The count of a header is what is shown of it.
        assert!(rows.contains(&Row::Header {
            group: 3,
            shown: 2,
            folded: false
        }));
        assert_eq!(grouping.groups[3].changes.len(), 2);

        let none = KindFilter {
            added: false,
            removed: false,
            modified: false,
        };
        assert!(rows_of(&changes, none, &[]).1.is_empty());
    }

    #[test]
    fn the_summary_counts_ignore_the_filter() {
        let result = DiffResult {
            changes: sample(),
            added: 3,
            removed: 2,
            modified: 1,
            ..DiffResult::default()
        };
        let before = counts_text(&result);
        let mut state = ChangesState::default();
        state.toggle_kind(ChangeKind::Added);
        state.toggle_kind(ChangeKind::Modified);
        assert!(!state.filter().added && !state.filter().modified && state.filter().removed);
        assert_eq!(counts_text(&result), before);
        assert!(before.contains('3') && before.contains('2') && before.contains('1'));
    }

    #[test]
    fn symbols_differ_and_texts_of_a_row() {
        let symbols: Vec<&str> = KINDS.iter().map(|k| kind_symbol(*k)).collect();
        assert_eq!(symbols, vec!["＋", "－", "～"]);
        for dark in [false, true] {
            let colors: Vec<[u8; 3]> = KINDS.iter().map(|k| kind_color(*k, dark)).collect();
            assert!(colors[0] != colors[1] && colors[1] != colors[2] && colors[0] != colors[2]);
        }

        let added = change(
            ChangeKind::Added,
            loc(11, 0.0),
            loc(11, 0.0),
            "new text here",
        );
        assert_eq!(change_snippet(&added), "new text here");
        let removed = change(ChangeKind::Removed, loc(11, 0.0), loc(11, 0.0), "gone text");
        assert_eq!(change_snippet(&removed), "gone text");
        let modified = change(ChangeKind::Modified, loc(11, 0.0), loc(12, 0.0), "after");
        assert_eq!(change_snippet(&modified), "after", "the newer text");

        // Same page / different pages.
        assert_eq!(change_pages(&added), page_label(11));
        assert!(page_label(11).contains("12"));
        let both = change_pages(&modified);
        assert!(both.contains("12") && both.contains("13"), "{both}");
        assert_ne!(both, page_label(11));

        // Tooltip: both sides, and none for the side without text.
        let tip = change_tooltip(&modified);
        assert!(tip.contains("old after") && tip.contains("after"));
        assert_eq!(tip.lines().count(), 2);
        assert_eq!(change_tooltip(&added).lines().count(), 1);
        let long = change(
            ChangeKind::Modified,
            loc(0, 0.0),
            loc(0, 0.0),
            &"x".repeat(500),
        );
        for line in change_tooltip(&long).lines() {
            let body = line.split_once(": ").map_or(line, |(_, b)| b);
            assert!(
                body.chars().count() <= TOOLTIP_CHARS,
                "{}",
                body.chars().count()
            );
        }
    }

    #[test]
    fn a_long_text_is_cut_to_one_line() {
        assert_eq!(first_line("one line", 20), "one line");
        assert_eq!(first_line("  first\nsecond line", 20), "first");
        assert_eq!(first_line("\n\n  after blanks \n x", 20), "after blanks");
        assert_eq!(first_line("a   b\tc", 20), "a b c");
        assert_eq!(first_line("", 20), "");
        let cut = first_line(&"word ".repeat(100), 30);
        assert_eq!(cut.chars().count(), 30);
        assert!(cut.ends_with('…'));
        // Characters, not bytes.
        let korean = first_line(&"가".repeat(50), 10);
        assert_eq!(korean.chars().count(), 10);
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "ab…");
    }

    // --------------------------------------------------------------------------------------------
    // State
    // --------------------------------------------------------------------------------------------

    fn prepared_state(changes: Vec<Change>) -> (ChangesState, Arc<DiffResult>) {
        let result = Arc::new(DiffResult {
            changes,
            ..DiffResult::default()
        });
        let mut state = ChangesState::default();
        state.prepare(&result, &old_outline(), &outline());
        (state, result)
    }

    #[test]
    fn the_grouping_is_made_again_only_for_a_different_result() {
        let (mut state, result) = prepared_state(sample());
        assert!(state.open, "the panel starts open");
        state.toggle_fold(1);
        let rows = state.rows();
        // The same result again: nothing is recomputed and the fold stays.
        state.prepare(&result, &old_outline(), &outline());
        assert!(state.is_folded(1));
        assert!(
            Arc::ptr_eq(&rows, &state.rows()),
            "the rows are kept between frames"
        );
        // A different result starts over.
        let other = Arc::new(DiffResult {
            changes: sample()[..2].to_vec(),
            ..DiffResult::default()
        });
        state.prepare(&other, &old_outline(), &outline());
        assert!(!state.is_folded(1));
        assert_eq!(state.grouping().map(|g| g.groups.len()), Some(2));
        assert!(!Arc::ptr_eq(&rows, &state.rows()));
    }

    #[test]
    fn the_current_change_unfolds_its_group_and_becomes_the_scroll_target() {
        let (mut state, _) = prepared_state(sample());
        state.note_current(None);
        assert_eq!(state.pending_scroll(), None);
        // Change 3 is in group 3; fold it.
        let group = state.grouping().and_then(|g| g.group_of(3)).expect("group");
        state.toggle_fold(group);
        assert_eq!(row_of_change(&state.rows(), 3), None);

        state.note_current(Some(3));
        assert!(!state.is_folded(group), "unfolded");
        let rows = state.rows();
        let row = row_of_change(&rows, 3).expect("visible now");
        assert_eq!(state.pending_scroll(), Some(3));
        // The list is 3 rows high and scrolled to the top: the row must be scrolled to.
        state.view_height = 60.0;
        let target = state.take_scroll(&rows, 20.0, false).expect("scrolls");
        assert_eq!(target, (row as f32 * 20.0 - 20.0).max(0.0));
        assert_eq!(state.pending_scroll(), None, "used up");

        // The same current change again asks for nothing.
        state.note_current(Some(3));
        assert_eq!(state.pending_scroll(), None);
        // Moving on to another one asks again, but the pointer over the list wins.
        state.note_current(Some(4));
        assert_eq!(state.pending_scroll(), Some(4));
        assert_eq!(state.take_scroll(&rows, 20.0, true), None);
        assert_eq!(state.pending_scroll(), None);
        // A row that is in view needs no scrolling.
        state.view_height = 10_000.0;
        state.note_current(Some(1));
        assert_eq!(state.take_scroll(&rows, 20.0, false), None);
    }

    // --------------------------------------------------------------------------------------------
    // Strips
    // --------------------------------------------------------------------------------------------

    fn pages(n: usize) -> Vec<Size> {
        vec![Size::new(600.0, 800.0); n]
    }

    #[test]
    fn the_extent_maps_places_to_fractions_and_back() {
        let e = Extent::new(&pages(4));
        assert_eq!(e.pages(), 4);
        assert_eq!(e.total(), 3200.0);
        assert_eq!(e.fraction(loc(0, 0.0)), 0.0);
        assert_eq!(e.fraction(loc(2, 0.0)), 0.5);
        assert!((e.fraction(loc(1, 400.0)) - 0.375).abs() < 1e-6);
        // Past the end and beyond a page are limited.
        assert_eq!(e.fraction(loc(9, 100.0)), (2400.0 + 100.0) / 3200.0);
        assert_eq!(e.fraction(loc(0, -50.0)), 0.0);
        assert_eq!(e.fraction(loc(0, 5000.0)), 0.25);
        assert_eq!(Extent::new(&[]).fraction(loc(0, 10.0)), 0.0);

        let back = e.location_at(0.375);
        assert_eq!(back.page, 1);
        assert!((back.y - 400.0).abs() < 0.01);
        assert_eq!(e.location_at(0.0), loc(0, 0.0));
        let end = e.location_at(1.0);
        assert_eq!(end.page, 3);
        assert!((end.y - 800.0).abs() < 0.01);
        assert_eq!(e.location_at(2.0), end, "limited");
        assert_eq!(
            e.location_at(0.5).page,
            2,
            "a page boundary belongs to the page below"
        );
        assert_eq!(Extent::new(&[]).location_at(0.5), Location::default());
    }

    #[test]
    fn changes_at_the_start_middle_and_end_are_marked_in_proportion() {
        let e = Extent::new(&pages(10));
        let changes = vec![
            change(ChangeKind::Modified, loc(0, 0.0), loc(0, 0.0), "start"),
            change(ChangeKind::Modified, loc(5, 0.0), loc(5, 0.0), "middle"),
            change(ChangeKind::Modified, loc(9, 800.0), loc(9, 800.0), "end"),
        ];
        let points = strip_points(&changes, Side::Right, &e);
        let ticks = strip_ticks(&points, 100);
        let rows: Vec<u32> = ticks.iter().map(|t| t.row).collect();
        assert_eq!(rows, vec![0, 50, 99]);
        assert!(ticks.iter().all(|t| t.kind == ChangeKind::Modified));
        // A taller strip scales.
        let rows: Vec<u32> = strip_ticks(&points, 400).iter().map(|t| t.row).collect();
        assert_eq!(rows, vec![0, 200, 399]);
        assert!(strip_ticks(&points, 0).is_empty());
    }

    #[test]
    fn each_strip_has_the_changes_that_are_in_its_document() {
        let e = Extent::new(&pages(4));
        let changes = vec![
            change(ChangeKind::Added, loc(0, 0.0), loc(1, 0.0), "a"),
            change(ChangeKind::Removed, loc(2, 0.0), loc(3, 0.0), "r"),
            change(ChangeKind::Modified, loc(1, 0.0), loc(2, 0.0), "m"),
        ];
        let left = strip_points(&changes, Side::Left, &e);
        let right = strip_points(&changes, Side::Right, &e);
        assert_eq!(
            left.iter()
                .map(|p| (p.kind, p.fraction))
                .collect::<Vec<_>>(),
            vec![(ChangeKind::Removed, 0.5), (ChangeKind::Modified, 0.25)]
        );
        assert_eq!(
            right
                .iter()
                .map(|p| (p.kind, p.fraction))
                .collect::<Vec<_>>(),
            vec![(ChangeKind::Added, 0.25), (ChangeKind::Modified, 0.5)]
        );
    }

    #[test]
    fn changes_on_one_pixel_make_one_tick_with_the_stronger_color() {
        let point = |fraction, kind| StripPoint { fraction, kind };
        let ticks = strip_ticks(
            &[
                point(0.301, ChangeKind::Modified),
                point(0.302, ChangeKind::Added),
                point(0.303, ChangeKind::Modified),
                point(0.7, ChangeKind::Modified),
                point(0.701, ChangeKind::Modified),
                point(0.9, ChangeKind::Added),
                point(0.9, ChangeKind::Removed),
            ],
            100,
        );
        assert_eq!(
            ticks,
            vec![
                Tick {
                    row: 30,
                    kind: ChangeKind::Added
                },
                Tick {
                    row: 70,
                    kind: ChangeKind::Modified
                },
                Tick {
                    row: 90,
                    kind: ChangeKind::Removed
                },
            ]
        );
        assert_eq!(
            stronger(ChangeKind::Modified, ChangeKind::Added),
            ChangeKind::Added
        );
        assert_eq!(
            stronger(ChangeKind::Added, ChangeKind::Removed),
            ChangeKind::Removed
        );
        assert_eq!(
            stronger(ChangeKind::Removed, ChangeKind::Modified),
            ChangeKind::Removed
        );
        assert_eq!(
            stronger(ChangeKind::Modified, ChangeKind::Modified),
            ChangeKind::Modified
        );
    }

    #[test]
    fn thousands_of_changes_give_no_more_ticks_than_pixels() {
        let e = Extent::new(&pages(500));
        let changes: Vec<Change> = (0..5000u32)
            .map(|i| {
                let at = loc(i / 10, (i % 10) as f32 * 70.0);
                let kind = match i % 3 {
                    0 => ChangeKind::Added,
                    1 => ChangeKind::Removed,
                    _ => ChangeKind::Modified,
                };
                change(kind, at, at, "x")
            })
            .collect();
        for height in [1u32, 37, 400, 900] {
            for side in [Side::Left, Side::Right] {
                let ticks = strip_ticks(&strip_points(&changes, side, &e), height);
                assert!(
                    ticks.len() as u32 <= height,
                    "{} ticks for {height}",
                    ticks.len()
                );
                assert!(!ticks.is_empty());
                assert!(ticks.windows(2).all(|w| w[0].row < w[1].row));
            }
        }
    }

    #[test]
    fn a_click_on_the_strip_is_a_place_in_the_document() {
        assert_eq!(fraction_at(150.0, 100.0, 200.0), 0.25);
        assert_eq!(fraction_at(50.0, 100.0, 200.0), 0.0);
        assert_eq!(fraction_at(900.0, 100.0, 200.0), 1.0);
        assert_eq!(fraction_at(10.0, 0.0, 0.0), 0.0);
        let e = Extent::new(&pages(4));
        let at = e.location_at(fraction_at(250.0, 100.0, 300.0));
        // Halfway down the strip is the top of the third page.
        assert_eq!(at.page, 2);
        assert!(at.y.abs() < 0.01);
        let at = e.location_at(fraction_at(100.0 + 206.25, 100.0, 300.0));
        assert_eq!(at.page, 2);
        assert!((at.y - 600.0).abs() < 0.01, "{at:?}");
    }

    #[test]
    fn the_visible_span_runs_from_the_top_to_the_bottom_of_the_viewport() {
        let e = Extent::new(&pages(4));
        let (a, b) = visible_span(&e, loc(1, 0.0), loc(2, 400.0));
        assert_eq!((a, b), (0.25, 0.5 + 0.125));
        let (a, b) = visible_span(&e, loc(0, -30.0), loc(9, 0.0));
        assert_eq!((a, b), (0.0, 0.75));
    }

    #[test]
    fn the_overview_holds_both_strips() {
        let result = DiffResult {
            changes: vec![
                change(ChangeKind::Added, loc(0, 0.0), loc(1, 0.0), "a"),
                change(ChangeKind::Removed, loc(1, 0.0), loc(2, 0.0), "r"),
            ],
            left_page_sizes: pages(2),
            right_page_sizes: pages(4),
            ..DiffResult::default()
        };
        let o = Overview::new(&result);
        assert_eq!(o.extent(Side::Left).pages(), 2);
        assert_eq!(o.extent(Side::Right).pages(), 4);
        assert_eq!(o.points(Side::Left).len(), 1);
        assert_eq!(o.points(Side::Right).len(), 1);
        assert_eq!(o.points(Side::Right)[0].fraction, 0.25);
    }

    // --------------------------------------------------------------------------------------------
    // Speed
    // --------------------------------------------------------------------------------------------

    #[test]
    fn grouping_and_rows_of_five_thousand_changes_are_fast() {
        let new = Outline::from_tree(
            (0..300u32)
                .map(|s| OutlineNode {
                    title: format!("{s} Section"),
                    target: Some(loc(s, 0.0)),
                    uri: None,
                    children: (0..3u32)
                        .map(|k| OutlineNode {
                            title: format!("{s}.{k} Part"),
                            target: Some(loc(s, 100.0 + 200.0 * k as f32)),
                            uri: None,
                            children: vec![],
                        })
                        .collect(),
                })
                .collect(),
        );
        let changes: Vec<Change> = (0..5000u32)
            .map(|i| {
                let at = loc(i / 17, (i % 17) as f32 * 40.0);
                let kind = [ChangeKind::Added, ChangeKind::Removed, ChangeKind::Modified]
                    [(i % 3) as usize];
                change(kind, at, at, &format!("change number {i} with some text"))
            })
            .collect();
        let started = Instant::now();
        let grouping = group_changes(&changes, &new, &new);
        let grouped = started.elapsed();
        let started = Instant::now();
        let rows = visible_rows(&grouping, &changes, KindFilter::default(), &BTreeSet::new());
        let listed = started.elapsed();
        let started = Instant::now();
        let filter = KindFilter {
            removed: false,
            ..KindFilter::default()
        };
        let filtered = visible_rows(&grouping, &changes, filter, &BTreeSet::new());
        let filtered_in = started.elapsed();
        eprintln!(
            "5000 changes: grouping {grouped:?}, rows {listed:?}, filtered rows {filtered_in:?} ({} groups, {} rows)",
            grouping.groups.len(),
            rows.len()
        );
        assert_eq!(rows.len(), 5000 + grouping.groups.len());
        assert!(filtered.len() < rows.len());
        // A generous bound: these take a few milliseconds.
        assert!(grouped.as_millis() < 2000 && listed.as_millis() < 2000);
        let started = Instant::now();
        let e = Extent::new(&pages(300));
        let ticks = strip_ticks(&strip_points(&changes, Side::Right, &e), 800);
        eprintln!(
            "5000 changes: strip {:?} ({} ticks)",
            started.elapsed(),
            ticks.len()
        );
        assert!(started.elapsed().as_millis() < 2000);
    }
}
