//! The file explorer (WS-2, WS-4, WS-5, WS-11): the folder tree of the workspace in the upper
//! section of the sidebar.
//!
//! The workspace (`AppState::workspace`) is one for the whole application and its index is
//! shared by every window; folding, filter and scroll are per window ([`ExplorerState`]).
//! Everything that decides something is a plain function or a method of [`ExplorerState`] that
//! does not touch egui, so it is tested without a window (rule R-10): which rows are visible,
//! what the filter matches, which folders open to show the current document, where to scroll,
//! how the folding is saved and restored. The drawing functions only show those results and turn
//! clicks into [`Action`]s.
//!
//! Opening a file goes through one place: a click on a file row becomes [`RowEvent::Open`] in
//! [`show_row`], and [`open_action`] turns it into [`Action::OpenFromExplorer`].

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use egui::{Key, Pos2, Rect, RichText, ScrollArea, Sense, TextEdit, TextStyle, Ui};
use refdesk_core::{EntryKind, ScanLimits, WorkspaceIndex, WorkspaceScan, WorkspaceWatcher};

use crate::action::{Action, notifier};
use crate::i18n::tr;
use crate::panels::toc::{Fold, match_ranges, scroll_target, title_job, triangle};
use crate::session;
use crate::state::{AppState, WatchState, WindowId, Workspace, normalize_path};

/// Each level of nesting moves a row this far to the right.
const INDENT: f32 = 12.0;
/// Rows nested deeper than this are not indented any further, so they stay readable.
const MAX_INDENT_DEPTH: u16 = 16;
/// Height of the explorer section's body when it shows one line instead of a tree.
pub const ONE_LINE_BODY_H: f32 = 28.0;
/// Height of the body when it shows an error and a button.
const TWO_LINE_BODY_H: f32 = 52.0;

// ---------------------------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------------------------

/// One visible line of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Index into `WorkspaceIndex::entries`.
    pub idx: u32,
    /// Nesting depth, top level 0.
    pub depth: u16,
    /// The fold triangle: folders are `Open` or `Closed`, files `Leaf`.
    pub fold: Fold,
    /// Byte ranges of the name the filter matched, for highlighting. Empty without a filter.
    pub matches: Vec<Range<usize>>,
}

/// The lines to show for a set of expanded folders, in the order of the index (folders before
/// files, natural name order). The contents of a folded folder are left out. Only visible rows
/// are visited, so a large folded tree costs little.
pub fn visible_rows(index: &WorkspaceIndex, expanded: &HashSet<u32>) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut stack: Vec<std::slice::Iter<'_, u32>> = vec![index.children_of(None).iter()];
    while let Some(level) = stack.last_mut() {
        let Some(&idx) = level.next() else {
            stack.pop();
            continue;
        };
        let Some(entry) = index.entry(idx) else {
            continue;
        };
        let fold = match entry.kind {
            EntryKind::Pdf => Fold::Leaf,
            EntryKind::Dir if expanded.contains(&idx) => Fold::Open,
            EntryKind::Dir => Fold::Closed,
        };
        rows.push(Row {
            idx,
            depth: entry.depth,
            fold,
            matches: Vec::new(),
        });
        // A parent always comes before its children in an index made by the scan; the bound
        // keeps a damaged hand-made one from looping.
        if fold == Fold::Open && stack.len() <= index.entries.len() {
            stack.push(index.children_of(Some(idx)).iter());
        }
    }
    rows
}

/// A character compared without regard to case (the same folding as the table of contents
/// filter uses, so [`match_ranges`] agrees with [`folded_names`]).
fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Every entry's name with each character folded by [`fold_char`], for a quick containment test
/// before the match ranges are worked out.
pub fn folded_names(index: &WorkspaceIndex) -> Vec<String> {
    index.entries.iter().map(|e| fold_name(&e.name)).collect()
}

/// `name` with every character folded by [`fold_char`]. For ASCII (most file names) that is
/// plain ASCII lowercasing, and the byte offsets stay those of `name`.
fn fold_name(name: &str) -> String {
    if name.is_ascii() {
        name.to_ascii_lowercase()
    } else {
        name.chars().map(fold_char).collect()
    }
}

/// What the filter box shows: the files whose name contains `filter` (ignoring case and
/// surrounding blanks) and every folder above them, all open, in index order, with the matched
/// parts of the file names. `folded` is [`folded_names`] of the index. A blank filter matches
/// nothing here; callers use [`visible_rows`] then.
pub fn filtered_rows(index: &WorkspaceIndex, folded: &[String], filter: &str) -> Vec<Row> {
    let needle: String = filter.trim().chars().map(fold_char).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let needle_chars: Vec<char> = needle.chars().collect();
    let n = index.entries.len();
    let mut keep = vec![false; n];
    let mut ranges: Vec<Vec<Range<usize>>> = vec![Vec::new(); n];
    for (i, entry) in index.entries.iter().enumerate() {
        let Some(name) = folded
            .get(i)
            .filter(|f| entry.kind == EntryKind::Pdf && f.contains(&needle))
        else {
            continue;
        };
        ranges[i] = if entry.name.is_ascii() {
            // Same bytes as the name, so the offsets carry over.
            name.match_indices(&needle)
                .map(|(at, m)| at..at + m.len())
                .collect()
        } else {
            match_ranges(&entry.name, &needle_chars)
        };
        // Keep the whole chain above. Once an ancestor is kept, everything above it is too.
        let mut cur = i;
        loop {
            if keep[cur] {
                break;
            }
            keep[cur] = true;
            match index.entries[cur].parent {
                // A parent always comes before its child; anything else is damaged data.
                Some(p) if (p as usize) < cur => cur = p as usize,
                _ => break,
            }
        }
    }
    index
        .entries
        .iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(i, entry)| Row {
            idx: i as u32,
            depth: entry.depth,
            fold: match entry.kind {
                EntryKind::Dir => Fold::Open,
                EntryKind::Pdf => Fold::Leaf,
            },
            matches: std::mem::take(&mut ranges[i]),
        })
        .collect()
}

/// Opens every folder above `idx`. Returns whether anything changed.
pub fn reveal(index: &WorkspaceIndex, expanded: &mut HashSet<u32>, idx: u32) -> bool {
    let mut changed = false;
    for ancestor in index.ancestors(idx) {
        changed |= expanded.insert(ancestor);
    }
    changed
}

/// The expanded folders as paths relative to the root, in index order, for saving
/// (`SessionWorkspace::expanded`). Paths, not indices, so a rescanned folder still finds them.
pub fn export_expanded(index: &WorkspaceIndex, expanded: &HashSet<u32>) -> Vec<PathBuf> {
    let mut list: Vec<u32> = expanded
        .iter()
        .copied()
        .filter(|i| index.entry(*i).is_some_and(|e| e.kind == EntryKind::Dir))
        .collect();
    list.sort_unstable();
    list.into_iter()
        .filter_map(|i| index.entry(i).map(|e| e.rel_path.clone()))
        .collect()
}

/// The folders of `index` named by saved relative paths. Paths that are not in the index (any
/// more), files and paths outside the root are ignored.
pub fn import_expanded(index: &WorkspaceIndex, saved: &[PathBuf]) -> HashSet<u32> {
    saved
        .iter()
        .filter_map(|p| index.find(p))
        .filter(|i| index.entry(*i).is_some_and(|e| e.kind == EntryKind::Dir))
        .collect()
}

/// What one frame of the explorer draws.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    /// The lines.
    pub rows: Arc<Vec<Row>>,
    /// The entry of the current document, when it is in the workspace.
    pub current: Option<u32>,
    /// The current document is not the one of the previous frame (and no filter is active):
    /// its folders were opened and the list should scroll to it.
    pub current_changed: bool,
    /// The position of the current document in `rows`, if it changed and is shown.
    pub current_row: Option<usize>,
    /// A filter is active: folding is ignored and folder rows do not fold.
    pub filtering: bool,
}

/// The rows of the last frame with what they were computed for.
#[derive(Clone, Debug)]
struct RowCache {
    index: Arc<WorkspaceIndex>,
    /// The filter, or empty for the folded tree.
    filter: String,
    /// `ExplorerState::version` of the folded tree; unused while filtering.
    version: u64,
    rows: Arc<Vec<Row>>,
}

/// The current document as of the last frame without a filter.
#[derive(Clone, Debug, PartialEq)]
struct LastCurrent {
    path: PathBuf,
    /// It was found in the index then.
    found: bool,
}

/// The explorer of one window. Not saved as such; the expanded folders are exported as relative
/// paths for the session ([`ExplorerState::expanded_paths`]).
#[derive(Clone, Debug, Default)]
pub struct ExplorerState {
    /// The text of the filter box.
    pub filter: String,
    /// Set by "Show Explorer": the filter box takes the keyboard when it is next drawn.
    pub focus_filter: bool,
    /// The index `expanded` refers to.
    index: Option<Arc<WorkspaceIndex>>,
    /// The expanded folders, as indices into `index`.
    expanded: HashSet<u32>,
    /// Saved expanded folders waiting for the first index.
    pending: Option<Vec<PathBuf>>,
    /// Bumped whenever `expanded` changes, so the rows are worked out again.
    version: u64,
    last_current: Option<LastCurrent>,
    rows: Option<RowCache>,
    /// [`folded_names`] of `index`, made when a filter first needs it.
    folded: Option<Arc<Vec<String>>>,
    /// Scroll offset and height of the list as of the last frame, for [`scroll_target`].
    scroll_offset: f32,
    view_height: f32,
}

impl ExplorerState {
    /// Moves the folding over to a new index (a rescan, or the first result): the expanded
    /// folders are carried by path, saved ones are applied to the first index.
    fn sync(&mut self, index: &Arc<WorkspaceIndex>) {
        if self.index.as_ref().is_some_and(|i| Arc::ptr_eq(i, index)) {
            return;
        }
        let paths = match self.pending.take() {
            Some(paths) => paths,
            None => self.expanded_paths(),
        };
        self.expanded = import_expanded(index, &paths);
        self.index = Some(Arc::clone(index));
        self.folded = None;
        self.version += 1;
    }

    /// Decides what to draw for `index` when the window shows the document `current` (a
    /// normalized path): see [`Prepared`]. When the current document changes, the folders above
    /// it open (not while a filter is active, so clearing the filter restores the folding from
    /// before).
    pub fn prepare(&mut self, index: &Arc<WorkspaceIndex>, current: Option<&Path>) -> Prepared {
        self.sync(index);
        let filtering = !self.filter.trim().is_empty();
        let current_idx = current
            .and_then(|p| index.find(p))
            .filter(|i| index.entry(*i).is_some_and(|e| e.kind == EntryKind::Pdf));
        let mut current_changed = false;
        if !filtering {
            let now = current.map(|p| LastCurrent {
                path: p.to_path_buf(),
                found: current_idx.is_some(),
            });
            // A document seen before it was in the index counts as new once it is found.
            let changed = match (&self.last_current, &now) {
                (Some(last), Some(now)) => last.path != now.path || (!last.found && now.found),
                (last, now) => last.is_some() != now.is_some(),
            };
            if changed {
                self.last_current = now;
                if let Some(c) = current_idx {
                    current_changed = true;
                    if reveal(index, &mut self.expanded, c) {
                        self.version += 1;
                    }
                }
            }
        }
        let rows = self.rows(index, filtering);
        let current_row = if current_changed {
            current_idx.and_then(|c| rows.iter().position(|r| r.idx == c))
        } else {
            None
        };
        Prepared {
            rows,
            current: current_idx,
            current_changed,
            current_row,
            filtering,
        }
    }

    /// The rows for this frame, worked out again only when the index, the folding or the
    /// filter changed.
    fn rows(&mut self, index: &Arc<WorkspaceIndex>, filtering: bool) -> Arc<Vec<Row>> {
        let filter = if filtering {
            self.filter.clone()
        } else {
            String::new()
        };
        if let Some(cache) = &self.rows
            && Arc::ptr_eq(&cache.index, index)
            && cache.filter == filter
            && (filtering || cache.version == self.version)
        {
            return Arc::clone(&cache.rows);
        }
        let rows = Arc::new(if filtering {
            let folded = Arc::clone(
                self.folded
                    .get_or_insert_with(|| Arc::new(folded_names(index))),
            );
            filtered_rows(index, &folded, &filter)
        } else {
            visible_rows(index, &self.expanded)
        });
        self.rows = Some(RowCache {
            index: Arc::clone(index),
            filter,
            version: self.version,
            rows: Arc::clone(&rows),
        });
        rows
    }

    /// Opens a folded folder or folds an open one. Files are left alone.
    pub fn toggle(&mut self, idx: u32) {
        let is_dir = self
            .index
            .as_ref()
            .and_then(|i| i.entry(idx))
            .is_some_and(|e| e.kind == EntryKind::Dir);
        if !is_dir {
            return;
        }
        if !self.expanded.remove(&idx) {
            self.expanded.insert(idx);
        }
        self.version += 1;
    }

    /// Folds every folder.
    pub fn collapse_all(&mut self) {
        self.expanded.clear();
        self.pending = None;
        self.version += 1;
    }

    /// The expanded folders as paths relative to the root, for saving. Before the first index
    /// is seen, what was handed to [`ExplorerState::set_expanded_paths`].
    pub fn expanded_paths(&self) -> Vec<PathBuf> {
        match (&self.pending, &self.index) {
            (Some(pending), _) => pending.clone(),
            (None, Some(index)) => export_expanded(index, &self.expanded),
            (None, None) => Vec::new(),
        }
    }

    /// Restores expanded folders saved with [`ExplorerState::expanded_paths`]. Paths the
    /// workspace does not have are ignored.
    pub fn set_expanded_paths(&mut self, paths: Vec<PathBuf>) {
        match &self.index {
            Some(index) => {
                self.expanded = import_expanded(index, &paths);
                self.version += 1;
            }
            None => self.pending = Some(paths),
        }
    }

    /// Whether folder `idx` of the current index is expanded.
    #[cfg(test)]
    pub fn is_expanded(&self, idx: u32) -> bool {
        self.expanded.contains(&idx)
    }
}

// ---------------------------------------------------------------------------------------------
// The workspace: opening, closing, refreshing (called from `dispatch`)
// ---------------------------------------------------------------------------------------------

/// What one frame shows of the workspace, read once from the scan.
#[derive(Clone, Debug)]
pub struct WorkspaceView {
    /// The folder.
    pub root: PathBuf,
    /// The latest finished index; while a rescan runs, the one before.
    pub index: Option<Arc<WorkspaceIndex>>,
    /// A scan is running.
    pub scanning: bool,
    /// Why the last scan failed.
    pub error: Option<String>,
    /// Why the folder is not watched, if it is not.
    pub watch_error: Option<String>,
}

impl WorkspaceView {
    /// The view of a workspace now.
    pub fn of(workspace: &Workspace) -> WorkspaceView {
        WorkspaceView {
            root: workspace.root.clone(),
            index: workspace.scan.index(),
            scanning: workspace.scan.scanning(),
            error: workspace.scan.error(),
            watch_error: workspace.watch_error(),
        }
    }

    /// The index, if it has a file to show.
    fn tree(&self) -> Option<&Arc<WorkspaceIndex>> {
        self.index.as_ref().filter(|i| i.file_count() > 0)
    }

    /// The name shown in the section header: the folder's name.
    pub fn name(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }
}

/// Starts working on `root` (already normalized) as the workspace: scans it and sets up the
/// watcher, both on their own threads (R-1). The previous workspace is dropped, which cancels its
/// scan and ends its watcher. Every window's explorer starts over.
pub fn start_workspace(state: &mut AppState, root: PathBuf, ctx: &egui::Context) {
    let scan = WorkspaceScan::start(root.clone(), ScanLimits::default(), notifier(ctx));
    let watch = Arc::new(Mutex::new(WatchState::Starting));
    let changed = Arc::new(AtomicBool::new(false));
    start_watcher(&root, &watch, &changed, ctx);
    state.workspace = Some(Workspace {
        root,
        scan,
        watch,
        changed,
    });
    for window in &mut state.windows {
        window.explorer = ExplorerState::default();
    }
}

/// Sets the watcher up on a thread of its own: watching a large tree reads every folder in it.
/// The result goes into `slot`; a change sets `changed` and asks for a frame. A watcher that
/// cannot be set up is logged and the explorer is refreshed by hand.
fn start_watcher(
    root: &Path,
    slot: &Arc<Mutex<WatchState>>,
    changed: &Arc<AtomicBool>,
    ctx: &egui::Context,
) {
    let (root, slot_for_thread, changed, ctx) = (
        root.to_path_buf(),
        Arc::clone(slot),
        Arc::clone(changed),
        ctx.clone(),
    );
    let spawned = std::thread::Builder::new()
        .name("refdesk-workspace-watch".into())
        .spawn(move || {
            let repaint = ctx.clone();
            let result = WorkspaceWatcher::start(&root, move || {
                changed.store(true, Ordering::Relaxed);
                repaint.request_repaint();
            });
            let watch = match result {
                Ok(watcher) => WatchState::Watching(watcher),
                Err(err) => {
                    log::warn!(
                        "cannot watch {} for changes; refresh by hand: {err}",
                        root.display()
                    );
                    WatchState::Failed(err.to_string())
                }
            };
            *slot_for_thread
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = watch;
            ctx.request_repaint();
        });
    if let Err(err) = spawned {
        log::warn!("cannot start the folder watcher thread: {err}");
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = WatchState::Failed(err.to_string());
    }
}

/// `Action::OpenFolder`: makes `path` the workspace, remembers it among the recent folders and
/// shows the explorer in the focused window. Open tabs stay. Opening the folder that already is
/// the workspace only scans it again.
pub fn open_folder(state: &mut AppState, path: &Path, ctx: &egui::Context) {
    let root = normalize_path(path);
    match &state.workspace {
        Some(ws) if ws.root == root => ws.scan.rescan(),
        _ => start_workspace(state, root.clone(), ctx),
    }
    session::record_folder(state, &root);
    let w = state.focused_window_index();
    if let Some(window) = state.windows.get_mut(w) {
        window.toc_open = true;
        window.explorer_open = true;
    }
    ctx.request_repaint();
}

/// `Action::CloseFolder`: no workspace any more. Tabs stay.
pub fn close_folder(state: &mut AppState) {
    state.workspace = None;
    for window in &mut state.windows {
        window.explorer = ExplorerState::default();
    }
}

/// `Action::RefreshWorkspace`: scans the workspace again.
pub fn refresh(state: &AppState) {
    if let Some(ws) = &state.workspace {
        ws.scan.rescan();
    }
}

/// Scans again when the watcher said the folder changed. Called once per frame by the app.
/// Returns whether a scan was started.
pub fn poll_workspace(state: &AppState) -> bool {
    match &state.workspace {
        Some(ws) if ws.changed.swap(false, Ordering::Relaxed) => {
            ws.scan.rescan();
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------------------------

/// The height the explorer's body needs when it shows a line or two instead of a tree; `None`
/// when it shows a tree and should get its share of the sidebar.
pub fn compact_body_height(view: Option<&WorkspaceView>) -> Option<f32> {
    match view {
        None => Some(ONE_LINE_BODY_H),
        Some(v) if v.tree().is_some() => None,
        Some(v) if v.index.is_none() && v.error.is_some() => Some(TWO_LINE_BODY_H),
        Some(_) => Some(ONE_LINE_BODY_H),
    }
}

/// The title of the explorer section and its tooltip: the folder's name and path, or
/// "Explorer" without a workspace.
pub fn header_title(view: Option<&WorkspaceView>) -> (String, Option<String>) {
    match view {
        Some(v) => (v.name(), Some(v.root.display().to_string())),
        None => (tr("explorer.title").to_owned(), None),
    }
}

/// The small buttons on the right of the section header (drawn right to left): collapse all,
/// refresh, and a spinner while a scan runs.
pub fn header_buttons(
    ui: &mut Ui,
    view: Option<&WorkspaceView>,
    explorer: &mut ExplorerState,
    queue: &mut Vec<Action>,
) {
    let Some(view) = view else {
        return;
    };
    if small_button(ui, "⊟", tr("explorer.collapse_all")) {
        explorer.collapse_all();
    }
    let refresh_tip = match &view.watch_error {
        Some(_) => tr("explorer.refresh_manual"),
        None => tr("explorer.refresh"),
    };
    if small_button(ui, "⟳", refresh_tip) {
        queue.push(Action::RefreshWorkspace);
    }
    if view.scanning {
        ui.add(egui::Spinner::new().size(12.0));
    }
}

fn small_button(ui: &mut Ui, text: &str, tip: &str) -> bool {
    ui.add(egui::Button::new(RichText::new(text).small()).frame(false))
        .on_hover_text(tip)
        .clicked()
}

fn hint(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).weak()).truncate());
}

/// What a row asked for, collected while drawing and applied afterwards.
enum RowEvent {
    Toggle(u32),
    /// The one place a file row asks to be opened.
    Open(u32),
}

/// The action that opens file `idx` of the index from the explorer of `window`. Every way the
/// explorer opens a file goes through here.
pub fn open_action(index: &WorkspaceIndex, idx: u32, window: WindowId) -> Action {
    Action::OpenFromExplorer {
        path: index.abs_path(idx),
        window,
    }
}

/// Draws the body of the explorer section of `window`: the filter box and the tree, or what to
/// do without them. `current` is the document of the window's focused tab.
pub fn show_body(
    ui: &mut Ui,
    view: Option<&WorkspaceView>,
    explorer: &mut ExplorerState,
    current: Option<&Path>,
    window: WindowId,
    queue: &mut Vec<Action>,
) {
    let Some(view) = view else {
        explorer.focus_filter = false;
        ui.horizontal(|ui| {
            if ui.button(tr("explorer.open_folder")).clicked() {
                queue.push(Action::OpenFolderDialog);
            }
            hint(ui, tr("explorer.no_folder"));
        });
        return;
    };
    let Some(index) = view.index.clone() else {
        match &view.error {
            Some(err) => {
                ui.add(
                    egui::Label::new(
                        RichText::new(tr("explorer.failed").replace("{}", err))
                            .color(ui.visuals().error_fg_color),
                    )
                    .truncate(),
                );
                if ui.button(tr("explorer.retry")).clicked() {
                    queue.push(Action::RefreshWorkspace);
                }
            }
            None => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(12.0));
                    hint(ui, tr("explorer.scanning"));
                });
            }
        }
        return;
    };
    if index.file_count() == 0 {
        match &view.error {
            Some(err) => hint(ui, &tr("explorer.failed").replace("{}", err)),
            None => hint(ui, tr("explorer.empty")),
        }
        return;
    }

    let edit = ui.add(
        TextEdit::singleline(&mut explorer.filter)
            .hint_text(tr("explorer.filter_hint"))
            .desired_width(f32::INFINITY),
    );
    if std::mem::take(&mut explorer.focus_filter) {
        edit.request_focus();
    }
    // The text field gives up the keyboard on Esc by itself; clearing is added to that.
    if (edit.has_focus() || edit.lost_focus()) && ui.input(|i| i.key_pressed(Key::Escape)) {
        explorer.filter.clear();
    }
    if let Some(err) = &view.error {
        hint(ui, &tr("explorer.failed").replace("{}", err));
    }
    if index.truncated {
        hint(ui, tr("explorer.truncated"));
    }
    ui.add_space(2.0);

    let prepared = explorer.prepare(&index, current);
    if prepared.rows.is_empty() {
        hint(ui, tr("explorer.no_match"));
        return;
    }
    show_tree(ui, &index, explorer, &prepared, window, queue);
}

/// The rows of the tree, only those in view.
fn show_tree(
    ui: &mut Ui,
    index: &Arc<WorkspaceIndex>,
    explorer: &mut ExplorerState,
    prepared: &Prepared,
    window: WindowId,
    queue: &mut Vec<Action>,
) {
    let row_h = ui.text_style_height(&TextStyle::Body) + 4.0;
    let stride = row_h + ui.spacing().item_spacing.y;
    let mut area = ScrollArea::vertical()
        .id_salt("explorer_tree")
        .auto_shrink([false, false]);
    // The list follows the reader only when the pointer is not over it: a row being clicked
    // must not move away.
    let in_panel = ui.rect_contains_pointer(ui.max_rect());
    if !in_panel
        && let Some(row) = prepared.current_row
        && let Some(offset) =
            scroll_target(row, stride, explorer.scroll_offset, explorer.view_height)
    {
        area = area.vertical_scroll_offset(offset);
    }

    let mut events = Vec::new();
    let output = area.show_rows(ui, row_h, prepared.rows.len(), |ui, range| {
        for row in &prepared.rows[range] {
            show_row(ui, row_h, row, index, prepared, &mut events);
        }
    });
    explorer.scroll_offset = output.state.clamped_offset().y;
    explorer.view_height = output.inner_rect.height();

    for event in events {
        match event {
            RowEvent::Toggle(idx) => explorer.toggle(idx),
            RowEvent::Open(idx) => queue.push(open_action(index, idx, window)),
        }
    }
}

/// Draws one row: the fold triangle of a folder, the name (one line, shortened with "…" when
/// too long; the relative path in a tooltip) and turns its click into an event. A click on a
/// folder folds it, a click on a file opens it (a single and a double click are told apart
/// here when files open in a preview tab).
fn show_row(
    ui: &mut Ui,
    row_h: f32,
    row: &Row,
    index: &WorkspaceIndex,
    prepared: &Prepared,
    events: &mut Vec<RowEvent>,
) {
    let Some(entry) = index.entry(row.idx) else {
        return;
    };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::hover());
    let indent = f32::from(row.depth.min(MAX_INDENT_DEPTH)) * INDENT;
    let fold_rect =
        Rect::from_min_size(rect.min + egui::vec2(indent, 0.0), egui::vec2(row_h, row_h));
    let name_rect = Rect::from_min_max(Pos2::new(fold_rect.right(), rect.top()), rect.max);
    let response = ui.interact(
        rect,
        ui.make_persistent_id(("explorer_row", row.idx)),
        Sense::click(),
    );

    let is_current = prepared.current == Some(row.idx);
    let visuals = ui.visuals().clone();
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
    if row.fold != Fold::Leaf {
        let color = if response.hovered() {
            visuals.strong_text_color()
        } else {
            visuals.weak_text_color()
        };
        painter.add(triangle(fold_rect, row.fold == Fold::Open, color));
    }
    let job = title_job(
        &entry.name,
        &row.matches,
        ui.style().text_styles[&TextStyle::Body].clone(),
        text_color,
        visuals.warn_fg_color.gamma_multiply(0.45),
        (name_rect.width() - 4.0).max(0.0),
    );
    let galley = ui.painter().layout_job(job);
    let pos = Pos2::new(
        name_rect.left() + 2.0,
        name_rect.center().y - galley.size().y / 2.0,
    );
    ui.painter().galley(pos, galley, text_color);

    let response = response.on_hover_ui(|ui| {
        ui.label(entry.rel_path.display().to_string());
    });
    if response.clicked() {
        match entry.kind {
            EntryKind::Dir if !prepared.filtering => events.push(RowEvent::Toggle(row.idx)),
            EntryKind::Dir => {}
            EntryKind::Pdf => events.push(RowEvent::Open(row.idx)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use refdesk_core::WorkspaceEntry;
    use std::time::{Duration, Instant};

    /// Builds an index from `(rel_path, is_dir)` in pre-order; parents and depths come from the
    /// paths.
    fn index_of(items: &[(&str, bool)]) -> Arc<WorkspaceIndex> {
        let mut entries: Vec<WorkspaceEntry> = Vec::new();
        for (path, is_dir) in items {
            let rel = PathBuf::from(path);
            let parent = rel.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| {
                entries
                    .iter()
                    .position(|e| e.rel_path == p)
                    .expect("parent listed first") as u32
            });
            entries.push(WorkspaceEntry {
                name: rel
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned(),
                depth: (rel.components().count() - 1) as u16,
                rel_path: rel,
                kind: if *is_dir {
                    EntryKind::Dir
                } else {
                    EntryKind::Pdf
                },
                parent,
            });
        }
        Arc::new(WorkspaceIndex::new(PathBuf::from("/ws"), entries, false))
    }

    /// specs/{net/{tcp.pdf, udp.pdf}, intro.pdf}, guides/{deep/{deeper/{x.pdf}}}, a.pdf, B.pdf
    fn sample() -> Arc<WorkspaceIndex> {
        index_of(&[
            ("guides", true),
            ("guides/deep", true),
            ("guides/deep/deeper", true),
            ("guides/deep/deeper/x.pdf", false),
            ("specs", true),
            ("specs/net", true),
            ("specs/net/tcp.pdf", false),
            ("specs/net/udp.pdf", false),
            ("specs/intro.pdf", false),
            ("a.pdf", false),
            ("B.pdf", false),
        ])
    }

    fn names(index: &WorkspaceIndex, rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| index.entry(r.idx).expect("entry").name.clone())
            .collect()
    }

    fn idx(index: &WorkspaceIndex, rel: &str) -> u32 {
        index.find(Path::new(rel)).expect("in index")
    }

    #[test]
    fn the_first_state_shows_only_the_top_level_folders_first() {
        let index = sample();
        let rows = visible_rows(&index, &HashSet::new());
        assert_eq!(names(&index, &rows), ["guides", "specs", "a.pdf", "B.pdf"]);
        assert_eq!(
            rows.iter().map(|r| r.fold).collect::<Vec<_>>(),
            [Fold::Closed, Fold::Closed, Fold::Leaf, Fold::Leaf]
        );
        let mut state = ExplorerState::default();
        let p = state.prepare(&index, None);
        assert_eq!(p.rows.len(), 4);
        assert_eq!(p.current, None);
    }

    #[test]
    fn expanding_a_folder_shows_its_children_in_index_order() {
        let index = sample();
        let mut state = ExplorerState::default();
        state.prepare(&index, None);
        state.toggle(idx(&index, "specs"));
        let p = state.prepare(&index, None);
        assert_eq!(
            names(&index, &p.rows),
            ["guides", "specs", "net", "intro.pdf", "a.pdf", "B.pdf"]
        );
        // Files do not fold.
        state.toggle(idx(&index, "a.pdf"));
        assert_eq!(state.prepare(&index, None).rows.len(), 6);
        // Folding again hides the children.
        state.toggle(idx(&index, "specs"));
        assert_eq!(state.prepare(&index, None).rows.len(), 4);
    }

    #[test]
    fn three_levels_unfold_one_at_a_time_and_collapse_all_folds_everything() {
        let index = sample();
        let mut state = ExplorerState::default();
        state.prepare(&index, None);
        state.toggle(idx(&index, "guides"));
        assert_eq!(state.prepare(&index, None).rows.len(), 5);
        state.toggle(idx(&index, "guides/deep"));
        assert_eq!(state.prepare(&index, None).rows.len(), 6);
        state.toggle(idx(&index, "guides/deep/deeper"));
        let p = state.prepare(&index, None);
        assert_eq!(
            names(&index, &p.rows)[..5],
            ["guides", "deep", "deeper", "x.pdf", "specs"]
        );
        assert_eq!(
            p.rows.iter().map(|r| r.depth).collect::<Vec<_>>()[..4],
            [0, 1, 2, 3]
        );
        state.collapse_all();
        assert_eq!(
            names(&index, &state.prepare(&index, None).rows),
            ["guides", "specs", "a.pdf", "B.pdf"]
        );
    }

    #[test]
    fn the_filter_shows_matching_files_and_their_folders_ignoring_case() {
        let index = sample();
        let folded = folded_names(&index);
        let rows = filtered_rows(&index, &folded, "  TCP ");
        assert_eq!(names(&index, &rows), ["specs", "net", "tcp.pdf"]);
        assert!(rows[..2].iter().all(|r| r.fold == Fold::Open));
        assert_eq!(rows[2].matches, vec![0..3]);
        // Folder names do not match by themselves.
        assert!(filtered_rows(&index, &folded, "specs").is_empty());
        // Several matches, in index order.
        let rows = filtered_rows(&index, &folded, "b");
        assert_eq!(names(&index, &rows), ["B.pdf"]);
        let rows = filtered_rows(&index, &folded, "p.pdf");
        assert_eq!(names(&index, &rows), ["specs", "net", "tcp.pdf", "udp.pdf"]);
        assert_eq!(rows[3].matches, vec![2..7]);
        // No match, and a blank filter matches nothing here.
        assert!(filtered_rows(&index, &folded, "nothing").is_empty());
        assert!(filtered_rows(&index, &folded, "  ").is_empty());
    }

    #[test]
    fn match_ranges_are_byte_ranges_of_the_name() {
        let index = index_of(&[("Überblick-über.pdf", false)]);
        let folded = folded_names(&index);
        let rows = filtered_rows(&index, &folded, "über");
        assert_eq!(rows.len(), 1);
        let name = &index.entries[0].name;
        let parts: Vec<&str> = rows[0].matches.iter().map(|r| &name[r.clone()]).collect();
        assert_eq!(parts, ["Über", "über"]);
    }

    #[test]
    fn clearing_the_filter_restores_the_folding_from_before() {
        let index = sample();
        let mut state = ExplorerState::default();
        state.prepare(&index, None);
        state.toggle(idx(&index, "guides"));
        let before = state.prepare(&index, None).rows;
        state.filter = "udp".into();
        let p = state.prepare(&index, None);
        assert!(p.filtering);
        assert_eq!(names(&index, &p.rows), ["specs", "net", "udp.pdf"]);
        // Folder rows do not fold while filtering.
        state.filter.clear();
        assert_eq!(state.prepare(&index, None).rows, before);
        assert!(!state.is_expanded(idx(&index, "specs")));
    }

    #[test]
    fn the_current_document_is_marked_and_its_folders_open() {
        let index = sample();
        let mut state = ExplorerState::default();
        let p = state.prepare(&index, Some(Path::new("/ws/specs/net/udp.pdf")));
        let udp = idx(&index, "specs/net/udp.pdf");
        assert_eq!(p.current, Some(udp));
        assert!(p.current_changed);
        assert_eq!(
            names(&index, &p.rows),
            [
                "guides",
                "specs",
                "net",
                "tcp.pdf",
                "udp.pdf",
                "intro.pdf",
                "a.pdf",
                "B.pdf"
            ]
        );
        assert_eq!(p.current_row, Some(4));
        // The next frame with the same document: no scroll target, the mark stays.
        let p = state.prepare(&index, Some(Path::new("/ws/specs/net/udp.pdf")));
        assert_eq!(p.current, Some(udp));
        assert!(!p.current_changed);
        assert_eq!(p.current_row, None);
    }

    #[test]
    fn a_folder_folded_by_hand_stays_folded_until_the_document_changes() {
        let index = sample();
        let mut state = ExplorerState::default();
        let tcp = Path::new("/ws/specs/net/tcp.pdf");
        state.prepare(&index, Some(tcp));
        state.toggle(idx(&index, "specs"));
        let p = state.prepare(&index, Some(tcp));
        assert_eq!(p.rows.len(), 4, "the hand-folded folder stays folded");
        assert_eq!(p.current_row, None);
        // Another document: its path opens and the list scrolls to it.
        let p = state.prepare(&index, Some(Path::new("/ws/guides/deep/deeper/x.pdf")));
        assert!(p.current_changed);
        assert_eq!(p.current_row, Some(3));
        // Back to the first: open again.
        let p = state.prepare(&index, Some(tcp));
        assert!(p.current_changed);
        assert!(state.is_expanded(idx(&index, "specs")));
        assert!(p.current_row.is_some());
    }

    #[test]
    fn a_document_outside_the_workspace_is_not_marked() {
        let index = sample();
        let mut state = ExplorerState::default();
        let p = state.prepare(&index, Some(Path::new("/elsewhere/specs/net/tcp.pdf")));
        assert_eq!(p.current, None);
        assert!(!p.current_changed);
        assert_eq!(p.current_row, None);
        assert_eq!(p.rows.len(), 4);
        // A folder of the workspace is not a document either.
        let p = state.prepare(&index, Some(Path::new("/ws/specs")));
        assert_eq!(p.current, None);
    }

    #[test]
    fn a_document_found_only_by_a_later_scan_is_revealed_then() {
        let empty = index_of(&[("a.pdf", false)]);
        let full = sample();
        let mut state = ExplorerState::default();
        let tcp = Path::new("/ws/specs/net/tcp.pdf");
        assert!(!state.prepare(&empty, Some(tcp)).current_changed);
        let p = state.prepare(&full, Some(tcp));
        assert!(p.current_changed);
        assert!(p.current_row.is_some());
        // A rescan that finds it again (a new index) does not count as a change.
        let again = sample();
        assert!(!state.prepare(&again, Some(tcp)).current_changed);
    }

    #[test]
    fn a_document_changed_during_filtering_is_revealed_afterwards() {
        let index = sample();
        let mut state = ExplorerState::default();
        state.prepare(&index, None);
        state.filter = "intro".into();
        let p = state.prepare(&index, Some(Path::new("/ws/specs/net/tcp.pdf")));
        assert!(!p.current_changed);
        state.filter.clear();
        let p = state.prepare(&index, Some(Path::new("/ws/specs/net/tcp.pdf")));
        assert!(p.current_changed);
        assert!(state.is_expanded(idx(&index, "specs/net")));
    }

    #[test]
    fn scroll_target_moves_only_rows_out_of_view() {
        assert_eq!(scroll_target(2, 20.0, 0.0, 200.0), None);
        assert_eq!(scroll_target(50, 20.0, 0.0, 300.0), Some(900.0));
    }

    #[test]
    fn the_folding_survives_a_round_trip_and_unknown_paths_are_ignored() {
        let index = sample();
        let mut state = ExplorerState::default();
        state.prepare(&index, None);
        state.toggle(idx(&index, "specs/net"));
        state.toggle(idx(&index, "guides"));
        let saved = state.expanded_paths();
        assert_eq!(saved, [PathBuf::from("guides"), PathBuf::from("specs/net")]);

        let mut restored = ExplorerState::default();
        let mut with_noise = saved.clone();
        with_noise.extend([
            PathBuf::from("gone"),
            PathBuf::from("a.pdf"),
            PathBuf::from("../outside"),
            PathBuf::from("/abs/elsewhere"),
        ]);
        restored.set_expanded_paths(with_noise);
        // Before the index is known, the saved paths are kept as they were given.
        assert_eq!(restored.expanded_paths().len(), 6);
        restored.prepare(&index, None);
        assert_eq!(restored.expanded_paths(), saved);
        assert_eq!(
            restored.prepare(&index, None).rows,
            state.prepare(&index, None).rows
        );
        // Setting paths after the index is known applies them at once.
        restored.set_expanded_paths(vec![PathBuf::from("specs")]);
        assert_eq!(restored.expanded_paths(), [PathBuf::from("specs")]);
    }

    #[test]
    fn the_folding_follows_a_rescan_by_path() {
        let index = sample();
        let mut state = ExplorerState::default();
        state.prepare(&index, None);
        state.toggle(idx(&index, "specs"));
        // A new folder before "specs" shifts every index.
        let rescanned = index_of(&[
            ("aaa", true),
            ("aaa/new.pdf", false),
            ("guides", true),
            ("guides/deep", true),
            ("guides/deep/deeper", true),
            ("guides/deep/deeper/x.pdf", false),
            ("specs", true),
            ("specs/intro.pdf", false),
            ("a.pdf", false),
        ]);
        let p = state.prepare(&rescanned, None);
        assert_eq!(
            names(&rescanned, &p.rows),
            ["aaa", "guides", "specs", "intro.pdf", "a.pdf"]
        );
    }

    #[test]
    fn rows_are_worked_out_again_only_when_something_changed() {
        let index = sample();
        let mut state = ExplorerState::default();
        let a = state.prepare(&index, None).rows;
        let b = state.prepare(&index, None).rows;
        assert!(Arc::ptr_eq(&a, &b), "same index, folding and filter");
        state.toggle(idx(&index, "specs"));
        let c = state.prepare(&index, None).rows;
        assert!(!Arc::ptr_eq(&b, &c));
        state.filter = "tcp".into();
        let d = state.prepare(&index, None).rows;
        let e = state.prepare(&index, None).rows;
        assert!(Arc::ptr_eq(&d, &e));
        let again = sample();
        let f = state.prepare(&again, None).rows;
        assert!(!Arc::ptr_eq(&e, &f), "a new index");
    }

    /// 50 000 entries: 50 top folders × 10 sub-folders × 99 files.
    fn big_index() -> Arc<WorkspaceIndex> {
        let mut entries = Vec::new();
        for t in 0..50u32 {
            let top = entries.len() as u32;
            entries.push(WorkspaceEntry {
                name: format!("area{t}"),
                rel_path: PathBuf::from(format!("area{t}")),
                kind: EntryKind::Dir,
                parent: None,
                depth: 0,
            });
            for s in 0..10u32 {
                let sub = entries.len() as u32;
                entries.push(WorkspaceEntry {
                    name: format!("part{s}"),
                    rel_path: PathBuf::from(format!("area{t}/part{s}")),
                    kind: EntryKind::Dir,
                    parent: Some(top),
                    depth: 1,
                });
                for f in 0..98u32 {
                    entries.push(WorkspaceEntry {
                        name: format!("Specification {t}-{s}-{f} Revision.pdf"),
                        rel_path: PathBuf::from(format!(
                            "area{t}/part{s}/Specification {t}-{s}-{f} Revision.pdf"
                        )),
                        kind: EntryKind::Pdf,
                        parent: Some(sub),
                        depth: 2,
                    });
                }
            }
        }
        Arc::new(WorkspaceIndex::new(PathBuf::from("/ws"), entries, false))
    }

    #[test]
    fn fifty_thousand_entries_are_cheap_to_lay_out_and_filter() {
        let index = big_index();
        assert!(index.entries.len() >= 49_000);
        let all_dirs: HashSet<u32> = index
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == EntryKind::Dir)
            .map(|(i, _)| i as u32)
            .collect();

        let t = Instant::now();
        let folded_rows = visible_rows(&index, &HashSet::new());
        let per_folded = t.elapsed();
        assert_eq!(folded_rows.len(), 50);

        let t = Instant::now();
        let open_rows = visible_rows(&index, &all_dirs);
        let per_open = t.elapsed();
        assert_eq!(open_rows.len(), index.entries.len());

        let mut state = ExplorerState::default();
        state.prepare(&index, None);
        state.filter = "7-3-4".into();
        let t = Instant::now();
        let first_filter = state.prepare(&index, None);
        let per_first_filter = t.elapsed();
        assert!(!first_filter.rows.is_empty());

        state.filter = "revision".into();
        let t = Instant::now();
        let every = state.prepare(&index, None);
        let per_filter_all = t.elapsed();
        assert_eq!(every.rows.len(), index.entries.len());

        let t = Instant::now();
        state.prepare(&index, None);
        let per_cached = t.elapsed();

        eprintln!(
            "WP-71 timing for {} entries: visible_rows folded {per_folded:?}, all open \
             {per_open:?}; first filter (with folding the names) {per_first_filter:?}, filter \
             matching everything {per_filter_all:?}, cached frame {per_cached:?}",
            index.entries.len()
        );
        // Generous: the point is that nothing is quadratic or per-entry expensive.
        let limit = Duration::from_millis(1000);
        assert!(per_folded < limit && per_open < limit);
        assert!(per_first_filter < limit && per_filter_all < limit);
        assert!(per_cached < Duration::from_millis(50));
    }

    // --- the workspace through `dispatch`, and drawing, without a window -----------------

    use crate::action::{OpenTarget, dispatch};
    use crate::state::{Tab, TabId};
    use crate::views::test_support::{Repaints, watch_repaints};
    use crate::windows::show_window;
    use std::sync::atomic::AtomicU32;

    /// A folder under the system's temporary directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "refdesk-wp71-{}-{}-{name}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir(normalize_path(&path))
        }

        /// Creates an empty file (and its folders) at `rel` and returns its path.
        fn file(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create folders");
            }
            std::fs::write(&path, b"").expect("write file");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn index_now(state: &AppState) -> Option<Arc<WorkspaceIndex>> {
        state.workspace.as_ref().and_then(|w| w.scan.index())
    }

    /// Waits for `check`, re-checking after every repaint request. egui calls the repaint
    /// callback only once until a frame has run, so a frame without content runs before each
    /// check: the scan, the watcher and the documents each ask for a repaint.
    fn wait_for(
        ctx: &egui::Context,
        repaints: &Repaints,
        what: &str,
        mut check: impl FnMut() -> bool,
    ) {
        repaints.wait_until(what, || {
            ctx.run_ui(egui::RawInput::default(), |_| {})
                .drop_without_applying_deltas();
            check()
        });
    }

    /// Waits until the workspace has a finished scan with `files` PDF files.
    fn wait_for_files(ctx: &egui::Context, repaints: &Repaints, state: &AppState, files: usize) {
        wait_for(ctx, repaints, "the folder scan", || {
            index_now(state).is_some_and(|i| i.file_count() == files)
        });
    }

    fn open_folder_and_wait(
        ctx: &egui::Context,
        repaints: &Repaints,
        state: &mut AppState,
        dir: &Path,
        files: usize,
    ) {
        dispatch(state, Action::OpenFolder(dir.to_path_buf()), ctx);
        wait_for_files(ctx, repaints, state, files);
    }

    fn pdf_tabs(state: &AppState) -> Vec<(TabId, PathBuf)> {
        let mut tabs: Vec<(TabId, PathBuf)> = state
            .windows
            .iter()
            .flat_map(|w| w.dock.tabs())
            .filter_map(|t| {
                let doc = state.tabs.pdf(t)?.doc;
                Some((t, state.docs.get(doc)?.path().to_path_buf()))
            })
            .collect();
        tabs.sort();
        tabs
    }

    #[test]
    fn opening_a_folder_scans_it_remembers_it_and_shows_the_explorer() {
        let dir = TempDir::new("open");
        dir.file("a.pdf");
        dir.file("sub/b.PDF");
        dir.file("notes.txt");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/wp71-kept.pdf")]),
            &ctx,
        );
        let tabs_before = pdf_tabs(&state);
        let focus_before = state.focus;
        state.windows[0].explorer_open = false;

        open_folder_and_wait(&ctx, &repaints, &mut state, &dir.0, 2);
        let ws = state.workspace.as_ref().expect("workspace");
        assert_eq!(ws.root, dir.0);
        let index = index_now(&state).expect("index");
        assert_eq!(index.dir_count(), 1);
        assert_eq!(
            state.doc_db.recent_folders(5)[0].path,
            dir.0,
            "remembered among the recent folders"
        );
        let window = state.main_window();
        assert!(window.toc_open && window.explorer_open);
        assert_eq!(pdf_tabs(&state), tabs_before, "open tabs stay");
        assert_eq!(state.focus, focus_before);
    }

    #[test]
    fn opening_another_folder_replaces_the_workspace_and_closing_keeps_the_tabs() {
        let first = TempDir::new("first");
        first.file("one.pdf");
        let second = TempDir::new("second");
        second.file("x/two.pdf");
        second.file("x/three.pdf");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        open_folder_and_wait(&ctx, &repaints, &mut state, &first.0, 1);
        state.windows[0].explorer.filter = "one".into();
        open_folder_and_wait(&ctx, &repaints, &mut state, &second.0, 2);
        assert_eq!(state.workspace.as_ref().expect("ws").root, second.0);
        assert_eq!(index_now(&state).expect("index").root, second.0);
        assert!(
            state.windows[0].explorer.filter.is_empty(),
            "the explorer starts over"
        );
        let recent: Vec<PathBuf> = state
            .doc_db
            .recent_folders(5)
            .into_iter()
            .map(|f| f.path.clone())
            .collect();
        assert_eq!(recent[..2], [second.0.clone(), first.0.clone()]);

        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/wp71-stay.pdf")]),
            &ctx,
        );
        let tabs = pdf_tabs(&state);
        dispatch(&mut state, Action::CloseFolder, &ctx);
        assert!(state.workspace.is_none());
        assert_eq!(pdf_tabs(&state), tabs);
    }

    #[test]
    fn refreshing_after_adding_a_file_shows_one_more_row() {
        let dir = TempDir::new("refresh");
        dir.file("a.pdf");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        open_folder_and_wait(&ctx, &repaints, &mut state, &dir.0, 1);
        let index = index_now(&state).expect("index");
        let before = state.windows[0].explorer.prepare(&index, None).rows.len();
        dir.file("b.pdf");
        dispatch(&mut state, Action::RefreshWorkspace, &ctx);
        wait_for_files(&ctx, &repaints, &state, 2);
        let index = index_now(&state).expect("index");
        let after = state.windows[0].explorer.prepare(&index, None).rows.len();
        assert_eq!(after, before + 1);
    }

    #[test]
    fn a_pdf_added_to_the_folder_appears_by_itself() {
        let dir = TempDir::new("watch");
        dir.file("a.pdf");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        open_folder_and_wait(&ctx, &repaints, &mut state, &dir.0, 1);
        wait_for(&ctx, &repaints, "the watcher", || {
            state.workspace.as_ref().is_some_and(|w| w.watching())
        });
        dir.file("new/added.pdf");
        // What the app does every frame: scan again when the watcher saw a change.
        wait_for(&ctx, &repaints, "the added file in the index", || {
            poll_workspace(&state);
            index_now(&state).is_some_and(|i| i.find(Path::new("new/added.pdf")).is_some())
        });
    }

    #[test]
    fn a_folder_that_cannot_be_read_shows_the_error_and_can_be_tried_again() {
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        dispatch(
            &mut state,
            Action::OpenFolder(PathBuf::from("/refdesk-test/wp71-no-such-folder")),
            &ctx,
        );
        wait_for(&ctx, &repaints, "the failed scan", || {
            state
                .workspace
                .as_ref()
                .is_some_and(|w| w.scan.error().is_some() && !w.scan.scanning())
        });
        let view = WorkspaceView::of(state.workspace.as_ref().expect("ws"));
        assert!(view.index.is_none());
        assert!(compact_body_height(Some(&view)).is_some());
        for _ in 0..3 {
            assert!(frame(&ctx, &mut state, Vec::new()).is_empty());
        }
    }

    // --- drawing -------------------------------------------------------------------------

    fn frame(ctx: &egui::Context, state: &mut AppState, events: Vec<egui::Event>) -> Vec<Action> {
        let mut queue = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1000.0, 700.0))),
            events,
            ..Default::default()
        };
        ctx.run_ui(input, |ui| show_window(ui, state, 0, &mut queue))
            .drop_without_applying_deltas();
        queue
    }

    fn click_at(ctx: &egui::Context, state: &mut AppState, pos: Pos2) -> Vec<Action> {
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut queue = frame(ctx, state, vec![egui::Event::PointerMoved(pos)]);
        queue.extend(frame(ctx, state, vec![button(true)]));
        queue.extend(frame(ctx, state, vec![button(false)]));
        // Move away so that the list may follow the current document again.
        queue.extend(frame(
            ctx,
            state,
            vec![egui::Event::PointerMoved(Pos2::new(900.0, 650.0))],
        ));
        queue
    }

    fn key(key: Key) -> Vec<egui::Event> {
        [true, false]
            .into_iter()
            .map(|pressed| egui::Event::Key {
                key,
                physical_key: Some(key),
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            })
            .collect()
    }

    #[test]
    fn the_sidebar_draws_without_a_folder_while_scanning_after_and_while_filtering() {
        let dir = TempDir::new("draw");
        dir.file("guides/intro.pdf");
        dir.file("specs/net/tcp.pdf");
        dir.file("a.pdf");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        dispatch(&mut state, Action::ToggleToc, &ctx);
        // No folder.
        for _ in 0..3 {
            assert!(frame(&ctx, &mut state, Vec::new()).is_empty());
        }
        // Scanning (or already done: either must draw), then done.
        dispatch(&mut state, Action::OpenFolder(dir.0.clone()), &ctx);
        assert!(frame(&ctx, &mut state, Vec::new()).is_empty());
        wait_for_files(&ctx, &repaints, &state, 3);
        for _ in 0..3 {
            assert!(frame(&ctx, &mut state, Vec::new()).is_empty());
        }
        // The first state: top level only.
        let index = index_now(&state).expect("index");
        let p = state.windows[0].explorer.prepare(&index, None);
        assert_eq!(p.rows.len(), 3);
        // Filtering.
        state.windows[0].explorer.filter = "TCP".into();
        for _ in 0..3 {
            assert!(frame(&ctx, &mut state, Vec::new()).is_empty());
        }
        let p = state.windows[0].explorer.prepare(&index, None);
        assert_eq!(names(&index, &p.rows), ["specs", "net", "tcp.pdf"]);
        // Both sections folded: two headers only.
        state.windows[0].explorer_open = false;
        state.windows[0].outline_open = false;
        for _ in 0..2 {
            assert!(frame(&ctx, &mut state, Vec::new()).is_empty());
        }
    }

    #[test]
    fn every_state_of_the_body_draws() {
        let index = sample();
        let empty = index_of(&[]);
        let mut truncated = (*sample()).clone();
        truncated.truncated = true;
        let view = |index: Option<Arc<WorkspaceIndex>>, scanning: bool, error: Option<&str>| {
            WorkspaceView {
                root: PathBuf::from("/ws"),
                index,
                scanning,
                error: error.map(str::to_owned),
                watch_error: Some("no watches left".into()),
            }
        };
        let views = [
            None,
            Some(view(None, true, None)),
            Some(view(None, false, Some("permission denied"))),
            Some(view(Some(empty), false, None)),
            Some(view(Some(Arc::clone(&index)), true, None)),
            Some(view(Some(Arc::clone(&index)), false, Some("gone"))),
            Some(view(Some(Arc::new(truncated)), false, None)),
        ];
        let ctx = egui::Context::default();
        for v in &views {
            let mut explorer = ExplorerState {
                focus_filter: true,
                ..Default::default()
            };
            for filter in ["", "tcp", "nothing"] {
                explorer.filter = filter.into();
                let mut queue = Vec::new();
                ctx.run_ui(egui::RawInput::default(), |ui| {
                    ui.horizontal(|ui| header_buttons(ui, v.as_ref(), &mut explorer, &mut queue));
                    show_body(
                        ui,
                        v.as_ref(),
                        &mut explorer,
                        Some(Path::new("/ws/specs/net/tcp.pdf")),
                        WindowId::MAIN,
                        &mut queue,
                    );
                })
                .drop_without_applying_deltas();
                assert!(queue.is_empty(), "nothing was clicked: {queue:?}");
            }
        }
        // Only a tree gets its share of the sidebar.
        assert_eq!(
            compact_body_height(views[0].as_ref()),
            Some(ONE_LINE_BODY_H)
        );
        assert_eq!(
            compact_body_height(views[2].as_ref()),
            Some(TWO_LINE_BODY_H)
        );
        assert!(compact_body_height(views[3].as_ref()).is_some());
        assert_eq!(compact_body_height(views[4].as_ref()), None);
        assert_eq!(header_title(views[4].as_ref()).0, "ws");
        assert_eq!(header_title(None).0, tr("explorer.title"));
    }

    /// Clicks down the explorer until a click opens a file; returns the click position and the
    /// action.
    fn find_file_row(ctx: &egui::Context, state: &mut AppState) -> (Pos2, PathBuf, WindowId) {
        for y in (28..300).step_by(4) {
            let pos = Pos2::new(100.0, y as f32);
            for action in click_at(ctx, state, pos) {
                if let Action::OpenFromExplorer { path, window } = action {
                    return (pos, path, window);
                }
            }
        }
        panic!("no row of the explorer opened a file");
    }

    #[test]
    fn clicking_a_file_opens_and_focuses_its_tab_once() {
        let dir = TempDir::new("click");
        let a = dir.file("a.pdf");
        dir.file("b.pdf");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        open_folder_and_wait(&ctx, &repaints, &mut state, &dir.0, 2);
        for _ in 0..2 {
            frame(&ctx, &mut state, Vec::new());
        }
        let (pos, path, window) = find_file_row(&ctx, &mut state);
        assert_eq!(path, a, "the first file row");
        assert_eq!(window, WindowId::MAIN);
        dispatch(
            &mut state,
            Action::OpenFromExplorer {
                path: path.clone(),
                window,
            },
            &ctx,
        );
        let tabs = pdf_tabs(&state);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].1, a);
        assert_eq!(state.focus, Some(tabs[0].0));
        assert!(matches!(state.tabs.get(tabs[0].0), Some(Tab::Pdf(_))));

        // The row is now the current document; clicking it again adds no tab.
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/wp71-other.pdf")]),
            &ctx,
        );
        let opened = click_at(&ctx, &mut state, pos);
        let again: Vec<Action> = opened
            .into_iter()
            .filter(|a| matches!(a, Action::OpenFromExplorer { .. }))
            .collect();
        assert_eq!(again.len(), 1, "{again:?}");
        for action in again {
            dispatch(&mut state, action, &ctx);
        }
        assert_eq!(pdf_tabs(&state).len(), 2, "no new tab for an open document");
        assert_eq!(state.focus, Some(tabs[0].0));
    }

    #[test]
    fn the_current_document_opens_its_folders_in_the_drawn_explorer() {
        let dir = TempDir::new("current");
        let deep = dir.file("specs/net/tcp.pdf");
        dir.file("other/x.pdf");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        open_folder_and_wait(&ctx, &repaints, &mut state, &dir.0, 2);
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget {
                path: deep.clone(),
                page: None,
            }]),
            &ctx,
        );
        for _ in 0..2 {
            frame(&ctx, &mut state, Vec::new());
        }
        let index = index_now(&state).expect("index");
        let explorer = &state.windows[0].explorer;
        assert!(explorer.is_expanded(idx(&index, "specs")));
        assert!(explorer.is_expanded(idx(&index, "specs/net")));
        assert!(!explorer.is_expanded(idx(&index, "other")));
        assert_eq!(
            explorer.expanded_paths(),
            [PathBuf::from("specs"), PathBuf::from("specs/net")]
        );
    }

    #[test]
    fn show_explorer_puts_the_keyboard_in_the_filter_and_escape_clears_it() {
        let dir = TempDir::new("focus");
        dir.file("alpha.pdf");
        dir.file("beta.pdf");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        open_folder_and_wait(&ctx, &repaints, &mut state, &dir.0, 2);
        state.windows[0].toc_open = false;
        dispatch(&mut state, Action::ShowExplorer, &ctx);
        assert!(state.main_window().toc_open && state.main_window().explorer_open);
        frame(&ctx, &mut state, Vec::new());
        assert!(
            !state.main_window().explorer.focus_filter,
            "taken by the box"
        );
        frame(&ctx, &mut state, vec![egui::Event::Text("bet".into())]);
        assert_eq!(state.main_window().explorer.filter, "bet");
        let index = index_now(&state).expect("index");
        let p = state.windows[0].explorer.prepare(&index, None);
        assert_eq!(names(&index, &p.rows), ["beta.pdf"]);
        frame(&ctx, &mut state, key(Key::Escape));
        assert_eq!(state.main_window().explorer.filter, "");
    }
}
