//! The command palette (Ctrl+Shift+P, CMD-1) and quick open (Ctrl+P, CMD-2).
//!
//! One overlay with two lists. Which list, what is typed and which row is selected live in
//! [`PaletteState`] on the `AppState`. Everything that decides something is a plain function over
//! the state and tested without a window: matching and ranking ([`command_items`],
//! [`quick_items`]), moving the selection ([`move_selection`]), opening and closing
//! ([`toggle`]) and choosing a row ([`choose`]). [`show`] only reads the keys, draws and
//! calls those.
//!
//! The palette runs nothing itself: choosing a command queues what the command table says
//! ([`Command::run`]), choosing a tab queues `FocusTab`, choosing a file of the workspace or a
//! recent document queues `OpenFiles`. Structure changes therefore still happen only in
//! `dispatch` (rule R-6).
//!
//! Quick open lists the open tabs, then the files of the workspace folder (WS-6), then the
//! recent documents. The workspace may hold tens of thousands of files, so its part is not
//! worked out per frame: the files are prepared once per index and matched once per query
//! ([`sync_folder`]), and a query that only grew looks at the files that matched before.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align2, Color32, Context, CornerRadius, Id, Key, KeyboardShortcut, Modifiers, Response,
    ScrollArea, Sense, Stroke, TextEdit, TextStyle, Ui, vec2,
};

use crate::action::{Action, OpenTarget};
use crate::commands::{self, COMMANDS, Command};
use crate::i18n::{Lang, current_lang, lookup, tr};
use crate::overlays::fuzzy::{fuzzy_match, highlight_ranges};
use crate::state::{AppState, TabId, WindowId};
use crate::windows::tab_title;
use refdesk_core::{EntryKind, WorkspaceIndex};

/// Identifier of the command that opens the command palette.
pub const COMMANDS_COMMAND_ID: &str = "palette.commands";
/// Identifier of the command that opens quick open.
pub const QUICK_OPEN_COMMAND_ID: &str = "palette.quick_open";

/// How many executed commands are remembered for the empty-query order.
pub const MAX_RECENT_COMMANDS: usize = 10;
/// How many recent documents quick open lists at most.
pub const MAX_RECENT_DOCS: usize = 100;
/// How many files of the workspace quick open lists for an empty query; typing finds the rest.
pub const FOLDER_FIRST_FILES: usize = 50;
/// How many files of the workspace quick open lists for a query, best first.
pub const FOLDER_MATCHES: usize = 200;
/// How many rows PgUp / PgDn move.
pub const PAGE_ROWS: usize = 8;
/// How many focused tabs are remembered.
const MAX_FOCUS_HISTORY: usize = 200;
/// A match on the id of a command counts for less than one on its name.
const ID_PENALTY: i32 = 10;
/// A match on the file name counts for more than one on the rest of the path.
const NAME_BONUS: i32 = 30;

/// Which list the palette shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteMode {
    /// All commands of the command table.
    Commands,
    /// Open tabs, files of the workspace folder and recent documents.
    QuickOpen,
}

/// The palette while it is shown.
#[derive(Clone, Debug)]
pub struct OpenPalette {
    /// Which list it shows.
    pub mode: PaletteMode,
    /// The window it is drawn in.
    pub window: WindowId,
    /// What the user typed.
    pub query: String,
    /// Index of the selected row in the filtered list.
    pub selected: usize,
    /// Set when the selection moved by key: the list scrolls it into view once.
    scroll_to_selected: bool,
}

/// The palette state kept in `AppState`.
#[derive(Debug, Default)]
pub struct PaletteState {
    /// `Some` while the palette is shown.
    pub open: Option<OpenPalette>,
    /// Identifiers of the commands run from the palette, the latest first. Not saved.
    recent_commands: Vec<&'static str>,
    /// The tabs that had the focus, the latest first. Not saved.
    focus_history: Vec<TabId>,
    /// The files of the workspace and how they match the query (see [`sync_folder`]).
    folder: FolderCache,
}

impl PaletteState {
    /// Notes which tab has the focus now (for the order of quick open).
    pub fn note_focus(&mut self, focus: Option<TabId>) {
        let Some(tab) = focus else {
            return;
        };
        if self.focus_history.first() == Some(&tab) {
            return;
        }
        self.focus_history.retain(|t| *t != tab);
        self.focus_history.insert(0, tab);
        self.focus_history.truncate(MAX_FOCUS_HISTORY);
    }

    /// Notes that a command was run from the palette.
    fn note_command(&mut self, id: &'static str) {
        self.recent_commands.retain(|c| *c != id);
        self.recent_commands.insert(0, id);
        self.recent_commands.truncate(MAX_RECENT_COMMANDS);
    }

    /// Commands run from the palette, the latest first.
    pub fn recent_commands(&self) -> &[&'static str] {
        &self.recent_commands
    }
}

/// Where quick open takes the recent documents from. Called with the state each time the list is
/// built, so it should be cheap (no file access).
pub type RecentDocuments<'a> = &'a dyn Fn(&AppState) -> Vec<PathBuf>;

/// The recent-documents source until the session work provides one: there are none.
///
/// To attach the real list, pass a function that returns the paths of the recently opened
/// documents, most recent first, in `windows::show_window` where this one is passed.
#[cfg(test)]
pub fn no_recent_documents(_: &AppState) -> Vec<PathBuf> {
    Vec::new()
}

// ------------------------------------------------------------------------------------------------
// Opening and closing
// ------------------------------------------------------------------------------------------------

/// What the commands that open the palette do (`Action::ShowPalette`): open it on the focused
/// window, or close it when it already shows this list, or switch it to this list.
pub fn toggle(state: &mut AppState, mode: PaletteMode, ctx: &Context) {
    match &mut state.palette.open {
        Some(open) if open.mode == mode => close(state, ctx),
        Some(open) => {
            open.mode = mode;
            open.query.clear();
            open.selected = 0;
            open.scroll_to_selected = true;
        }
        None => {
            let window = state.focused_window().id;
            state.palette.open = Some(OpenPalette {
                mode,
                window,
                query: String::new(),
                selected: 0,
                scroll_to_selected: true,
            });
        }
    }
}

/// Closes the palette and lets go of the keyboard: the text field of the palette is not drawn any
/// more, but egui would keep it focused for one more frame and the shortcuts would not work.
fn close(state: &mut AppState, ctx: &Context) {
    state.palette.open = None;
    ctx.memory_mut(|m| m.stop_text_input());
}

/// Keeps the palette state in step with the rest of the state; `dispatch` calls it after every
/// action. Remembers the focus, and closes a palette whose window is gone.
pub fn after_dispatch(state: &mut AppState) {
    state.palette.note_focus(state.focus);
    let gone = state
        .palette
        .open
        .as_ref()
        .is_some_and(|open| !state.windows.iter().any(|w| w.id == open.window));
    if gone {
        state.palette.open = None;
    }
}

// ------------------------------------------------------------------------------------------------
// Selection
// ------------------------------------------------------------------------------------------------

/// A movement of the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// One row up.
    Up,
    /// One row down.
    Down,
    /// [`PAGE_ROWS`] rows up.
    PageUp,
    /// [`PAGE_ROWS`] rows down.
    PageDown,
    /// The first row.
    Home,
    /// The last row.
    End,
}

/// The selection after a step in a list of `len` rows. It stops at both ends; it does not wrap.
pub fn move_selection(selected: usize, len: usize, step: Step) -> usize {
    if len == 0 {
        return 0;
    }
    let last = len - 1;
    let current = selected.min(last);
    match step {
        Step::Up => current.saturating_sub(1),
        Step::Down => (current + 1).min(last),
        Step::PageUp => current.saturating_sub(PAGE_ROWS),
        Step::PageDown => (current + PAGE_ROWS).min(last),
        Step::Home => 0,
        Step::End => last,
    }
}

/// The selection brought inside a list of `len` rows (after the list got shorter).
pub fn clamp_selection(selected: usize, len: usize) -> usize {
    selected.min(len.saturating_sub(1))
}

// ------------------------------------------------------------------------------------------------
// The lists
// ------------------------------------------------------------------------------------------------

/// One row of the command list.
pub struct CommandItem {
    /// The row of the command table.
    pub command: &'static Command,
    /// The name in the language of the list.
    pub name: String,
    /// The name of the menu the command is in, in the same language.
    pub menu: String,
    /// Whether the command can run now; disabled ones are shown dimmed.
    pub enabled: bool,
    /// Characters of `name` that matched the query (indices into `name.chars()`).
    pub name_positions: Vec<usize>,
    /// How well the row matches; the list is sorted by it.
    pub score: i32,
}

fn text(lang: Lang, key: &'static str) -> &'static str {
    lookup(lang, key).unwrap_or(key)
}

/// Every command of the table that matches `query`, best first. The query is matched against the
/// name in `lang`, the English name and the identifier, so `zoom` finds "확대" in the Korean UI.
/// With an empty query the commands run most recently (`recent`, latest first) come first and the
/// others follow in table order; otherwise equal scores keep table order.
pub fn command_items(
    state: &AppState,
    lang: Lang,
    query: &str,
    recent: &[&'static str],
) -> Vec<CommandItem> {
    let mut items: Vec<CommandItem> = COMMANDS
        .iter()
        .filter_map(|command| {
            let name = text(lang, command.name_key);
            let english = text(Lang::En, command.name_key);
            let on_name = fuzzy_match(query, name);
            let scores = [
                on_name.as_ref().map(|m| m.score),
                fuzzy_match(query, english).map(|m| m.score),
                fuzzy_match(query, command.id).map(|m| m.score - ID_PENALTY),
            ];
            let score = scores.into_iter().flatten().max()?;
            Some(CommandItem {
                command,
                name: name.to_owned(),
                menu: text(lang, command.menu.name_key()).to_owned(),
                enabled: (command.enabled)(state),
                name_positions: on_name.map(|m| m.positions).unwrap_or_default(),
                score,
            })
        })
        .collect();
    if query.trim().is_empty() {
        // Stable: commands that were not run recently stay in table order.
        items.sort_by_key(|item| {
            recent
                .iter()
                .position(|id| *id == item.command.id)
                .unwrap_or(usize::MAX)
        });
    } else {
        items.sort_by_key(|item| Reverse(item.score));
    }
    items
}

/// What a row of quick open stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickTarget {
    /// A tab that is open.
    Tab(TabId),
    /// A file of the workspace folder that is not open; choosing it opens it (WS-6).
    Folder(PathBuf),
    /// "N more files in the folder": a line that only says typing finds them.
    FolderMore(usize),
    /// A document that is not open; choosing it opens it.
    Recent(PathBuf),
}

/// One row of quick open.
#[derive(Clone, Debug)]
pub struct QuickItem {
    /// What choosing the row does.
    pub target: QuickTarget,
    /// The file name (the tab title for a tab).
    pub name: String,
    /// The full path; the path relative to the workspace folder for a file of the folder.
    pub path: String,
    /// Index in `AppState::windows` of the window holding the tab; `None` for the others.
    pub window: Option<usize>,
    /// Characters of `name` that matched (indices into `name.chars()`).
    pub name_positions: Vec<usize>,
    /// Characters of `path` that matched.
    pub path_positions: Vec<usize>,
    /// How well the row matches; the list is sorted by it.
    pub score: i32,
}

/// The PDF tabs in the order of an empty query: those that had the focus most recently first, then
/// the ones that never had it (window order), and the tab with the focus now last. With the
/// focus on tab B and A before it, the top row is A: Ctrl+P, Enter switches back and forth.
fn tabs_by_recent_focus(state: &AppState) -> Vec<(usize, TabId)> {
    let all: Vec<(usize, TabId)> = state
        .windows
        .iter()
        .enumerate()
        .flat_map(|(w, window)| window.dock.tabs().into_iter().map(move |t| (w, t)))
        .filter(|(_, t)| state.tabs.pdf(*t).is_some())
        .collect();
    let rank = |tab: TabId| state.palette.focus_history.iter().position(|t| *t == tab);
    let mut ordered: Vec<(usize, TabId)> = all
        .iter()
        .copied()
        .filter(|(_, t)| Some(*t) != state.focus)
        .collect();
    // Stable: tabs without a place in the history keep window order, after the others.
    ordered.sort_by_key(|(_, t)| rank(*t).unwrap_or(usize::MAX));
    ordered.extend(all.into_iter().filter(|(_, t)| Some(*t) == state.focus));
    ordered
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// How well `query` matches a row with this file name and path: the file name counts for more
/// than the rest of the path. The score and the matched characters of each.
fn match_name_and_path(
    query: &str,
    name: &str,
    path: &str,
) -> Option<(i32, Vec<usize>, Vec<usize>)> {
    let on_name = fuzzy_match(query, name);
    let on_path = fuzzy_match(query, path);
    let score = [
        on_name.as_ref().map(|m| m.score + NAME_BONUS),
        on_path.as_ref().map(|m| m.score),
    ]
    .into_iter()
    .flatten()
    .max()?;
    Some((
        score,
        on_name.map(|m| m.positions).unwrap_or_default(),
        on_path.map(|m| m.positions).unwrap_or_default(),
    ))
}

/// Scores one row.
fn quick_item(
    target: QuickTarget,
    window: Option<usize>,
    name: String,
    path: String,
    query: &str,
) -> Option<QuickItem> {
    let (score, name_positions, path_positions) = match_name_and_path(query, &name, &path)?;
    Some(QuickItem {
        target,
        name,
        path,
        window,
        name_positions,
        path_positions,
        score,
    })
}

// --- the files of the workspace (WS-6) -------------------------------------------------------

/// One PDF file of the workspace, ready for matching.
#[derive(Debug)]
struct FolderFile {
    /// Its entry in the index.
    entry: u32,
    abs: PathBuf,
    name: String,
    /// The path relative to the workspace folder.
    rel: String,
    /// `rel` folded like [`search_key`], for the quick containment test.
    folded: String,
}

/// The files of one workspace index, in index order (the order of the tree).
#[derive(Debug)]
struct FolderFiles {
    index: Arc<WorkspaceIndex>,
    files: Vec<FolderFile>,
}

impl FolderFiles {
    fn new(index: &Arc<WorkspaceIndex>) -> FolderFiles {
        let files = index
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == EntryKind::Pdf)
            .map(|(i, e)| {
                let rel = e.rel_path.display().to_string();
                FolderFile {
                    entry: i as u32,
                    abs: index.abs_path(i as u32),
                    name: e.name.clone(),
                    folded: rel.chars().map(fold_char).collect(),
                    rel,
                }
            })
            .collect();
        FolderFiles {
            index: Arc::clone(index),
            files,
        }
    }
}

/// A file of the workspace that matched a query.
#[derive(Clone, Debug)]
struct FolderHit {
    /// Position in `FolderFiles::files`.
    file: u32,
    score: i32,
    name_positions: Vec<usize>,
    path_positions: Vec<usize>,
}

/// The files that matched one query, best first.
#[derive(Debug)]
struct FolderSearch {
    /// The query as [`search_key`] gives it.
    key: String,
    hits: Arc<Vec<FolderHit>>,
}

/// The workspace part of quick open, kept between frames.
#[derive(Debug, Default)]
struct FolderCache {
    files: Option<Arc<FolderFiles>>,
    search: Option<FolderSearch>,
}

/// A character compared without regard to case, as the fuzzy matcher compares it.
fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// The query as the matcher reads it: folded, without white space. Two queries with the same
/// key match the same files with the same scores.
fn search_key(query: &str) -> String {
    query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(fold_char)
        .collect()
}

/// Whether the characters of `needle` occur in `hay` in order.
fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut rest = hay.chars();
    needle.chars().all(|n| rest.any(|h| h == n))
}

/// Matches `query` (whose [`search_key`] is `key`) against the files at `candidates`, best
/// first; equal scores keep index order. An empty query matches every candidate with score 0.
fn search_files(
    files: &FolderFiles,
    query: &str,
    key: &str,
    candidates: impl Iterator<Item = u32>,
) -> Vec<FolderHit> {
    let mut hits: Vec<FolderHit> = candidates
        .filter_map(|i| {
            let file = files.files.get(i as usize)?;
            if key.is_empty() {
                return Some(FolderHit {
                    file: i,
                    score: 0,
                    name_positions: Vec::new(),
                    path_positions: Vec::new(),
                });
            }
            // The cheap test first: most files do not contain the query at all.
            if !is_subsequence(key, &file.folded) {
                return None;
            }
            let (score, name_positions, path_positions) =
                match_name_and_path(query, &file.name, &file.rel)?;
            Some(FolderHit {
                file: i,
                score,
                name_positions,
                path_positions,
            })
        })
        .collect();
    hits.sort_by_key(|h| (Reverse(h.score), h.file));
    hits
}

/// Brings the workspace part of quick open up to date with the workspace `index` and `query`.
/// The files are prepared again only for another index, and matched again only when the query
/// changed; a query that extends the previous one only looks at the files that matched it.
/// Without a workspace the part is empty.
pub fn sync_folder(palette: &mut PaletteState, index: Option<&Arc<WorkspaceIndex>>, query: &str) {
    let cache = &mut palette.folder;
    let Some(index) = index else {
        *cache = FolderCache::default();
        return;
    };
    let files = match &cache.files {
        Some(files) if Arc::ptr_eq(&files.index, index) => Arc::clone(files),
        _ => {
            let files = Arc::new(FolderFiles::new(index));
            cache.files = Some(Arc::clone(&files));
            cache.search = None;
            files
        }
    };
    let key = search_key(query);
    if cache.search.as_ref().is_some_and(|s| s.key == key) {
        return;
    }
    let hits = match &cache.search {
        // Whatever matches the longer query matched the shorter one.
        Some(prev) if !prev.key.is_empty() && key.starts_with(&prev.key) => {
            let mut before: Vec<u32> = prev.hits.iter().map(|h| h.file).collect();
            before.sort_unstable();
            search_files(&files, query, &key, before.into_iter())
        }
        _ => search_files(&files, query, &key, 0..files.files.len() as u32),
    };
    cache.search = Some(FolderSearch {
        key,
        hits: Arc::new(hits),
    });
}

/// The finished index of the workspace, if a folder is open.
fn workspace_index(state: &AppState) -> Option<Arc<WorkspaceIndex>> {
    state.workspace.as_ref().and_then(|w| w.scan.index())
}

/// The rows of the workspace part for `query`, without the documents `open` in tabs: the first
/// [`FOLDER_FIRST_FILES`] files in tree order and a line for the rest for an empty query, else
/// the best [`FOLDER_MATCHES`]. Uses what [`sync_folder`] prepared.
fn folder_items(state: &AppState, query: &str, open: &[&Path]) -> Vec<QuickItem> {
    let cache = &state.palette.folder;
    let Some(files) = &cache.files else {
        return Vec::new();
    };
    let key = search_key(query);
    let computed;
    let hits: &[FolderHit] = match &cache.search {
        Some(search) if search.key == key => &search.hits,
        // Not prepared for this query (only when called out of turn): match now.
        _ => {
            computed = search_files(files, query, &key, 0..files.files.len() as u32);
            &computed
        }
    };
    let root = &files.index.root;
    let open_entries: HashSet<u32> = open
        .iter()
        .filter_map(|p| files.index.find(p.strip_prefix(root).ok()?))
        .collect();
    let limit = if key.is_empty() {
        FOLDER_FIRST_FILES
    } else {
        FOLDER_MATCHES
    };
    let mut items = Vec::new();
    let mut more = 0;
    for hit in hits {
        let Some(file) = files.files.get(hit.file as usize) else {
            continue;
        };
        if open_entries.contains(&file.entry) {
            continue;
        }
        if items.len() == limit {
            more += 1;
            if !key.is_empty() {
                break;
            }
            continue;
        }
        items.push(QuickItem {
            target: QuickTarget::Folder(file.abs.clone()),
            name: file.name.clone(),
            path: file.rel.clone(),
            window: None,
            name_positions: hit.name_positions.clone(),
            path_positions: hit.path_positions.clone(),
            score: hit.score,
        });
    }
    if key.is_empty() && more > 0 {
        items.push(QuickItem {
            target: QuickTarget::FolderMore(more),
            name: tr("palette.folder_more").replace("{n}", &more.to_string()),
            path: String::new(),
            window: None,
            name_positions: Vec::new(),
            path_positions: Vec::new(),
            score: 0,
        });
    }
    items
}

/// Whether `path` is a file of the workspace that [`sync_folder`] prepared.
fn in_folder(state: &AppState, path: &Path) -> bool {
    state.palette.folder.files.as_ref().is_some_and(|files| {
        path.strip_prefix(&files.index.root)
            .ok()
            .and_then(|rel| files.index.find(rel))
            .is_some()
    })
}

/// The rows of quick open that match `query`, in three parts: the open tabs, the files of the
/// workspace folder that are not open ([`folder_items`]), the recent documents that are neither
/// open nor in the folder. With an empty query the tabs are ordered "latest focus first, current
/// tab last" (see [`tabs_by_recent_focus`]) and `recent` as given; otherwise each part is best
/// score first with equal scores in that same order. At most [`MAX_RECENT_DOCS`] recent
/// documents are listed.
pub fn quick_items(state: &AppState, query: &str, recent: &[PathBuf]) -> Vec<QuickItem> {
    let searching = !query.trim().is_empty();
    let by_score = |items: &mut Vec<QuickItem>| {
        if searching {
            items.sort_by_key(|item| Reverse(item.score));
        }
    };
    let mut items: Vec<QuickItem> = Vec::new();
    for (window, tab) in tabs_by_recent_focus(state) {
        let path = state
            .tabs
            .pdf(tab)
            .and_then(|pdf| state.docs.get(pdf.doc))
            .map(|doc| doc.path().display().to_string())
            .unwrap_or_default();
        items.extend(quick_item(
            QuickTarget::Tab(tab),
            Some(window),
            tab_title(state, tab),
            path,
            query,
        ));
    }
    by_score(&mut items);
    let open_paths: Vec<&Path> = state
        .tabs
        .iter()
        .filter_map(|(_, tab)| match tab {
            crate::state::Tab::Pdf(pdf) => state.docs.get(pdf.doc).map(|d| d.path()),
            // A document that is open only inside a comparison stays in the other parts, so it
            // can still be opened as a tab of its own.
            crate::state::Tab::Diff(_) | crate::state::Tab::Start => None,
        })
        .collect();
    items.extend(folder_items(state, query, &open_paths));
    let mut recents: Vec<QuickItem> = recent
        .iter()
        .filter(|p| !open_paths.contains(&p.as_path()) && !in_folder(state, p))
        .take(MAX_RECENT_DOCS)
        .filter_map(|path| {
            quick_item(
                QuickTarget::Recent(path.clone()),
                None,
                file_name_of(path),
                path.display().to_string(),
                query,
            )
        })
        .collect();
    by_score(&mut recents);
    items.extend(recents);
    items
}

/// The rows of whichever list is shown.
enum Items {
    Commands(Vec<CommandItem>),
    Quick(Vec<QuickItem>),
}

impl Items {
    fn len(&self) -> usize {
        match self {
            Items::Commands(items) => items.len(),
            Items::Quick(items) => items.len(),
        }
    }
}

/// The rows of the list in `mode` for `query`. For quick open the workspace part is brought up
/// to date first ([`sync_folder`]).
fn build_items(
    state: &mut AppState,
    lang: Lang,
    mode: PaletteMode,
    query: &str,
    recent: RecentDocuments<'_>,
) -> Items {
    if mode == PaletteMode::QuickOpen {
        let index = workspace_index(state);
        sync_folder(&mut state.palette, index.as_ref(), query);
    }
    match mode {
        PaletteMode::Commands => Items::Commands(command_items(
            state,
            lang,
            query,
            state.palette.recent_commands(),
        )),
        PaletteMode::QuickOpen => Items::Quick(quick_items(state, query, &recent(state))),
    }
}

// ------------------------------------------------------------------------------------------------
// Choosing
// ------------------------------------------------------------------------------------------------

/// Runs row `index` of the list the palette shows: closes the palette and queues the row's action.
/// Returns false, leaving the palette open, when there is no such row or the command cannot run
/// now.
pub fn choose(
    state: &mut AppState,
    ctx: &Context,
    lang: Lang,
    recent: RecentDocuments<'_>,
    index: usize,
    queue: &mut Vec<Action>,
) -> bool {
    let Some((mode, query)) = state
        .palette
        .open
        .as_ref()
        .map(|o| (o.mode, o.query.clone()))
    else {
        return false;
    };
    let items = build_items(state, lang, mode, &query, recent);
    let action = match items {
        Items::Commands(items) => {
            let Some(item) = items.get(index) else {
                return false;
            };
            if !item.enabled {
                return false;
            }
            state.palette.note_command(item.command.id);
            item.command.run(state)
        }
        Items::Quick(items) => match items.get(index).map(|item| &item.target) {
            None | Some(QuickTarget::FolderMore(_)) => return false,
            Some(QuickTarget::Tab(tab)) => Some(Action::FocusTab(*tab)),
            // An ordinary tab, not a preview one: quick open is not the explorer.
            Some(QuickTarget::Folder(path) | QuickTarget::Recent(path)) => {
                Some(Action::OpenFiles(vec![OpenTarget {
                    path: path.clone(),
                    page: None,
                }]))
            }
        },
    };
    close(state, ctx);
    queue.extend(action);
    true
}

// ------------------------------------------------------------------------------------------------
// Keys and drawing
// ------------------------------------------------------------------------------------------------

/// The keys of one frame that the palette reacts to.
#[derive(Default)]
struct Keys {
    /// A shortcut that opens a palette: the same one closes it, the other switches to its list.
    toggle: Option<PaletteMode>,
    close: bool,
    run: bool,
    steps: Vec<Step>,
}

/// Takes the palette's keys out of this frame's input, so the text field and the views behind do
/// not see them. Home and End move the selection here, not the text cursor.
fn read_keys(ctx: &Context) -> Keys {
    let mut bindings: Vec<(KeyboardShortcut, PaletteMode)> = [
        (COMMANDS_COMMAND_ID, PaletteMode::Commands),
        (QUICK_OPEN_COMMAND_ID, PaletteMode::QuickOpen),
    ]
    .into_iter()
    .filter_map(|(id, mode)| commands::command(id).map(|c| (c, mode)))
    .flat_map(|(c, mode)| c.all_shortcuts().map(move |s| (s, mode)))
    .collect();
    // `consume_shortcut` ignores an extra Shift: Ctrl+Shift+P must be asked before Ctrl+P.
    bindings.sort_by_key(|(s, _)| Reverse(u8::from(s.modifiers.shift) + u8::from(s.modifiers.alt)));

    ctx.input_mut(|input| {
        let mut keys = Keys::default();
        for (shortcut, mode) in &bindings {
            if input.consume_shortcut(shortcut) {
                keys.toggle = Some(*mode);
                break;
            }
        }
        keys.close = input.consume_key(Modifiers::NONE, Key::Escape);
        keys.run = input.consume_key(Modifiers::NONE, Key::Enter);
        for (key, step) in [
            (Key::ArrowUp, Step::Up),
            (Key::ArrowDown, Step::Down),
            (Key::PageUp, Step::PageUp),
            (Key::PageDown, Step::PageDown),
            (Key::Home, Step::Home),
            (Key::End, Step::End),
        ] {
            if input.consume_key(Modifiers::NONE, key) {
                keys.steps.push(step);
            }
        }
        keys
    })
}

const ROW_HEIGHT: f32 = 26.0;
const ROW_PADDING: f32 = 8.0;
const PALETTE_WIDTH: f32 = 600.0;
const LIST_MAX_HEIGHT: f32 = 360.0;
const TOP_OFFSET: f32 = 70.0;

/// A row ready to draw.
struct Row {
    name: String,
    name_positions: Vec<usize>,
    /// Dimmed text after the name (the path in quick open) and its matched characters.
    detail: Option<(String, Vec<usize>)>,
    /// Dimmed text at the right edge.
    right: String,
    enabled: bool,
}

fn rows(ctx: &Context, state: &AppState, items: &Items) -> Vec<Row> {
    match items {
        Items::Commands(items) => items
            .iter()
            .map(|item| {
                let right = match item.command.shortcut {
                    Some(shortcut) => format!("{}  {}", item.menu, ctx.format_shortcut(&shortcut)),
                    None => item.menu.clone(),
                };
                Row {
                    name: item.name.clone(),
                    name_positions: item.name_positions.clone(),
                    detail: None,
                    right,
                    enabled: item.enabled,
                }
            })
            .collect(),
        Items::Quick(items) => items
            .iter()
            .map(|item| {
                let right = match (&item.target, item.window) {
                    (QuickTarget::Recent(_), _) => tr("palette.recent").to_owned(),
                    (QuickTarget::Folder(_), _) => tr("palette.folder").to_owned(),
                    (QuickTarget::Tab(_), Some(w)) if state.windows.len() > 1 => {
                        tr("palette.window_n").replace("{n}", &(w + 1).to_string())
                    }
                    _ => String::new(),
                };
                let more = matches!(item.target, QuickTarget::FolderMore(_));
                Row {
                    name: item.name.clone(),
                    name_positions: item.name_positions.clone(),
                    detail: (!more).then(|| (item.path.clone(), item.path_positions.clone())),
                    right,
                    enabled: !more,
                }
            })
            .collect(),
    }
}

fn append_highlighted(
    job: &mut LayoutJob,
    text: &str,
    positions: &[usize],
    plain: &TextFormat,
    lit: &TextFormat,
) {
    for (range, highlighted) in highlight_ranges(text, positions) {
        let part = text.get(range).unwrap_or_default();
        job.append(part, 0.0, if highlighted { lit } else { plain }.clone());
    }
}

/// Draws one row and returns its click response.
fn draw_row(ui: &mut Ui, row: &Row, selected: bool, scroll_to_it: bool) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if scroll_to_it {
        response.scroll_to_me(None);
    }
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let visuals = ui.visuals();
    let weak = visuals.weak_text_color();
    let plain_color = if row.enabled {
        visuals.text_color()
    } else {
        weak
    };
    let lit_color = if row.enabled {
        visuals.strong_text_color()
    } else {
        weak
    };
    let underline = Stroke::new(1.0, visuals.hyperlink_color);
    let background = if selected {
        Some(visuals.selection.bg_fill)
    } else if response.hovered() {
        Some(visuals.widgets.hovered.weak_bg_fill)
    } else {
        None
    };
    let font = TextStyle::Body.resolve(ui.style());

    let painter = ui.painter();
    if let Some(fill) = background {
        painter.rect_filled(rect, CornerRadius::same(4), fill);
    }
    let right = painter.layout_no_wrap(row.right.clone(), font.clone(), weak);
    let name_width = (rect.width() - right.size().x - 3.0 * ROW_PADDING).max(40.0);

    let format = |color: Color32, underline: Stroke| TextFormat {
        font_id: font.clone(),
        color,
        underline,
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = name_width;
    job.wrap.max_rows = 1;
    append_highlighted(
        &mut job,
        &row.name,
        &row.name_positions,
        &format(plain_color, Stroke::NONE),
        &format(lit_color, underline),
    );
    if let Some((detail, positions)) = &row.detail {
        job.append("  ", 0.0, format(weak, Stroke::NONE));
        append_highlighted(
            &mut job,
            detail,
            positions,
            &format(weak, Stroke::NONE),
            &format(lit_color, underline),
        );
    }
    let galley = painter.layout_job(job);
    let left_top = rect.left_center() + vec2(ROW_PADDING, -galley.size().y / 2.0);
    painter.galley(left_top, galley, plain_color);
    let right_top =
        rect.right_center() + vec2(-ROW_PADDING - right.size().x, -right.size().y / 2.0);
    painter.galley(right_top, right, weak);
    response
}

/// Draws the palette if it is open on window `window_index`, and handles its keys. The window
/// calls this before it draws its own content, so the palette takes its keys first.
pub fn show(
    ctx: &Context,
    state: &mut AppState,
    window_index: usize,
    queue: &mut Vec<Action>,
    recent: RecentDocuments<'_>,
) {
    let Some(window_id) = state.windows.get(window_index).map(|w| w.id) else {
        return;
    };
    let Some(open) = state
        .palette
        .open
        .as_ref()
        .filter(|o| o.window == window_id)
    else {
        return;
    };
    let lang = current_lang();
    let mode = open.mode;
    let mut query = open.query.clone();
    let mut selected = open.selected;
    let mut scroll_to_selected = open.scroll_to_selected;

    let keys = read_keys(ctx);
    if let Some(mode) = keys.toggle
        && let Some(action) = palette_command_action(state, mode)
    {
        queue.push(action);
    }
    if keys.close {
        close(state, ctx);
        return;
    }
    let mut items = build_items(state, lang, mode, &query, recent);
    for step in &keys.steps {
        selected = move_selection(selected, items.len(), *step);
        scroll_to_selected = true;
    }
    if keys.run {
        let index = clamp_selection(selected, items.len());
        if choose(state, ctx, lang, recent, index, queue) {
            return;
        }
    }

    let id = Id::unique(("palette", window_id));
    let width = PALETTE_WIDTH
        .min(ctx.content_rect().width() - 32.0)
        .max(200.0);
    let modal = egui::Modal::new(id)
        .area(egui::Modal::default_area(id).anchor(Align2::CENTER_TOP, vec2(0.0, TOP_OFFSET)));
    let response = modal.show(ctx, |ui| {
        ui.set_width(width);
        let hint = match mode {
            PaletteMode::Commands => tr("palette.hint_commands"),
            PaletteMode::QuickOpen => tr("palette.hint_quick_open"),
        };
        let edit = ui.add(
            TextEdit::singleline(&mut query)
                .hint_text(hint)
                .desired_width(f32::INFINITY),
        );
        // The text field keeps the keyboard for as long as the palette is open.
        edit.request_focus();
        if edit.changed() {
            selected = 0;
            scroll_to_selected = true;
            items = build_items(state, lang, mode, &query, recent);
        }
        let selected_now = clamp_selection(selected, items.len());
        let rows = rows(ctx, state, &items);
        ui.add_space(4.0);
        let mut clicked = None;
        if rows.is_empty() {
            ui.weak(tr("palette.no_match"));
        }
        ScrollArea::vertical()
            .max_height(LIST_MAX_HEIGHT)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (index, row) in rows.iter().enumerate() {
                    let is_selected = index == selected_now;
                    let response =
                        draw_row(ui, row, is_selected, is_selected && scroll_to_selected);
                    if response.clicked() {
                        clicked = Some(index);
                    }
                }
            });
        clicked
    });

    let clicked = response.inner;
    let outside = response.backdrop_response.clicked();
    // Write the typed text and the selection back before anything reads them.
    if let Some(open) = &mut state.palette.open {
        open.query = query;
        open.selected = clicked.unwrap_or(selected);
        open.scroll_to_selected = false;
    }
    if outside {
        close(state, ctx);
    } else if let Some(index) = clicked {
        choose(state, ctx, lang, recent, index, queue);
    }
}

/// The action of the command that opens the palette in `mode`.
fn palette_command_action(state: &AppState, mode: PaletteMode) -> Option<Action> {
    let id = match mode {
        PaletteMode::Commands => COMMANDS_COMMAND_ID,
        PaletteMode::QuickOpen => QUICK_OPEN_COMMAND_ID,
    };
    commands::command(id)?.run(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{dispatch, dispatch_queued};
    use crate::state::WindowState;

    fn open_pdf(state: &mut AppState, ctx: &Context, path: &str) -> TabId {
        dispatch(state, Action::OpenFiles(vec![OpenTarget::parse(path)]), ctx);
        state.focus.expect("focused tab")
    }

    fn state_with(ctx: &Context, names: &[&str]) -> (AppState, Vec<TabId>) {
        let mut state = AppState::new();
        let tabs = names
            .iter()
            .map(|n| open_pdf(&mut state, ctx, &format!("/refdesk-test/{n}.pdf")))
            .collect();
        (state, tabs)
    }

    fn ids(items: &[CommandItem]) -> Vec<&'static str> {
        items.iter().map(|i| i.command.id).collect()
    }

    fn names(items: &[QuickItem]) -> Vec<&str> {
        items.iter().map(|i| i.name.as_str()).collect()
    }

    fn open_palette(state: &mut AppState, mode: PaletteMode) {
        toggle(state, mode, &Context::default());
        assert!(state.palette.open.is_some());
    }

    fn set_query(state: &mut AppState, query: &str) {
        if let Some(open) = &mut state.palette.open {
            open.query = query.to_owned();
        }
    }

    // ---- matching and ranking of commands ----

    #[test]
    fn korean_english_and_identifier_queries_find_the_same_command() {
        let state = AppState::new();
        for (lang, query) in [
            (Lang::Ko, "분할"),
            (Lang::En, "split"),
            (Lang::Ko, "split"),
            (Lang::En, "tab.split"),
        ] {
            let found = ids(&command_items(&state, lang, query, &[]));
            assert!(found.contains(&"tab.split_right"), "{lang:?} {query}");
            assert!(found.contains(&"tab.split_down"), "{lang:?} {query}");
            assert!(!found.contains(&"app.quit"), "{lang:?} {query}");
        }
        // The identifier works in both languages.
        for lang in [Lang::Ko, Lang::En] {
            let found = ids(&command_items(&state, lang, "view.zoom", &[]));
            for id in ["view.zoom_in", "view.zoom_out", "view.zoom_reset"] {
                assert!(found.contains(&id), "{lang:?} {id}");
            }
        }
    }

    #[test]
    fn an_empty_query_lists_every_command_in_table_order() {
        let state = AppState::new();
        let items = command_items(&state, Lang::En, "", &[]);
        let table: Vec<&str> = COMMANDS.iter().map(|c| c.id).collect();
        assert_eq!(ids(&items), table);
        let items = command_items(&state, Lang::Ko, "  ", &[]);
        assert_eq!(ids(&items), table);
    }

    #[test]
    fn rows_carry_name_menu_and_the_matched_characters() {
        let state = AppState::new();
        let items = command_items(&state, Lang::Ko, "분할", &[]);
        let right = items
            .iter()
            .find(|i| i.command.id == "tab.split_right")
            .expect("row");
        assert_eq!(right.name, "오른쪽으로 분할");
        assert_eq!(right.menu, "탭");
        assert_eq!(right.name_positions, vec![6, 7]);
        let chars: Vec<char> = right.name.chars().collect();
        let lit: String = right.name_positions.iter().map(|i| chars[*i]).collect();
        assert_eq!(lit, "분할");
        // A match through the English name or the id highlights nothing in the Korean name.
        let items = command_items(&state, Lang::Ko, "zoom", &[]);
        assert!(items.iter().all(|i| i.name_positions.is_empty()));
        assert!(!items.is_empty());
        let items = command_items(&state, Lang::En, "zoom", &[]);
        let zoom_in = items
            .iter()
            .find(|i| i.command.id == "view.zoom_in")
            .expect("row");
        assert_eq!(zoom_in.name_positions, vec![0, 1, 2, 3]);
        assert_eq!(zoom_in.menu, "View");
    }

    #[test]
    fn a_word_start_match_ranks_above_a_middle_match() {
        let state = AppState::new();
        let items = command_items(&state, Lang::En, "ct", &[]);
        let found = ids(&items);
        assert_eq!(found.first(), Some(&"tab.close"), "{found:?}");
    }

    #[test]
    fn contiguous_matches_rank_above_scattered_ones_and_ties_keep_table_order() {
        let state = AppState::new();
        let items = command_items(&state, Lang::En, "zoom", &[]);
        // "Zoom In" and "Zoom Out" match identically: table order. "Actual Size" has no zoom.
        let found = ids(&items);
        let pos = |id: &str| found.iter().position(|i| *i == id);
        assert!(pos("view.zoom_in") < pos("view.zoom_out"));
        assert!(pos("view.zoom_in").is_some());
        // Scattered "ptn" is a weaker match than contiguous "pane".
        let scattered = command_items(&state, Lang::En, "fnp", &[]);
        assert!(ids(&scattered).contains(&"tab.focus_next_pane"));
    }

    #[test]
    fn disabled_commands_are_listed_but_have_no_action() {
        let ctx = Context::default();
        let state = AppState::new();
        let items = command_items(&state, Lang::En, "close tab", &[]);
        let close = items
            .iter()
            .find(|i| i.command.id == "tab.close")
            .expect("listed");
        assert!(!close.enabled);
        assert!(close.command.run(&state).is_none());

        let (state, _) = state_with(&ctx, &["a"]);
        let items = command_items(&state, Lang::En, "close tab", &[]);
        let close = items
            .iter()
            .find(|i| i.command.id == "tab.close")
            .expect("listed");
        assert!(close.enabled);
        assert!(close.command.run(&state).is_some());
    }

    #[test]
    fn recently_run_commands_come_first_for_an_empty_query_only() {
        let state = AppState::new();
        let recent = ["app.quit", "file.open"];
        let items = command_items(&state, Lang::En, "", &recent);
        assert_eq!(&ids(&items)[..2], &["app.quit", "file.open"]);
        // The rest keeps table order.
        let rest: Vec<&str> = ids(&items)[2..].to_vec();
        let table: Vec<&str> = COMMANDS
            .iter()
            .map(|c| c.id)
            .filter(|id| !recent.contains(id))
            .collect();
        assert_eq!(rest, table);
        // With a query the score decides.
        let items = command_items(&state, Lang::En, "open", &recent);
        assert_eq!(ids(&items).first(), Some(&"file.open"));
    }

    #[test]
    fn the_commands_run_from_the_palette_are_remembered_latest_first() {
        let mut state = AppState::new();
        for id in ["file.open", "app.quit", "file.open"] {
            state
                .palette
                .note_command(commands::command(id).expect("command").id);
        }
        assert_eq!(state.palette.recent_commands(), &["file.open", "app.quit"]);
        for id in COMMANDS.iter().map(|c| c.id).cycle().take(30) {
            state.palette.note_command(id);
        }
        assert_eq!(state.palette.recent_commands().len(), MAX_RECENT_COMMANDS);
    }

    // ---- quick open ----

    #[test]
    fn quick_open_lists_every_open_tab_with_its_window() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b", "c"]);
        // Move tab `c` into a second window.
        let second = state.new_window_id();
        state.windows.push(WindowState::new(second));
        state.windows[0].dock.remove_tab(tabs[2]);
        state.windows[1].dock.add_tab(tabs[2], None);

        let items = quick_items(&state, "", &[]);
        assert_eq!(items.len(), 3);
        for (tab, window) in [(tabs[0], 0), (tabs[1], 0), (tabs[2], 1)] {
            let item = items
                .iter()
                .find(|i| i.target == QuickTarget::Tab(tab))
                .expect("row");
            assert_eq!(item.window, Some(window));
        }
        let c = items
            .iter()
            .find(|i| i.target == QuickTarget::Tab(tabs[2]))
            .expect("row");
        assert_eq!(c.name, "c.pdf");
        assert_eq!(c.path, "/refdesk-test/c.pdf");
    }

    #[test]
    fn the_start_page_is_not_listed() {
        let state = AppState::new();
        assert!(quick_items(&state, "", &[]).is_empty());
    }

    #[test]
    fn quick_open_orders_by_recent_focus_with_the_current_tab_last() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b", "c"]);
        // Focus history: c (now), b, a.
        assert_eq!(
            names(&quick_items(&state, "", &[])),
            ["b.pdf", "a.pdf", "c.pdf"]
        );

        dispatch(&mut state, Action::FocusTab(tabs[0]), &ctx);
        assert_eq!(
            names(&quick_items(&state, "", &[])),
            ["c.pdf", "b.pdf", "a.pdf"]
        );

        // Ctrl+P, Enter: the top row is the tab we came from.
        dispatch(&mut state, Action::FocusTab(tabs[2]), &ctx);
        assert_eq!(names(&quick_items(&state, "", &[]))[0], "a.pdf");
    }

    #[test]
    fn tabs_that_never_had_the_focus_follow_the_others_in_window_order() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b", "c"]);
        state.palette = PaletteState::default();
        state.palette.note_focus(Some(tabs[1]));
        state.focus = Some(tabs[2]);
        // b was focused; a never was; c is current.
        assert_eq!(
            names(&quick_items(&state, "", &[])),
            ["b.pdf", "a.pdf", "c.pdf"]
        );
    }

    #[test]
    fn recent_documents_come_after_the_open_tabs_and_skip_open_ones() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a", "b"]);
        let recent = vec![
            PathBuf::from("/docs/r1.pdf"),
            PathBuf::from("/refdesk-test/a.pdf"),
            PathBuf::from("/docs/r2.pdf"),
        ];
        let items = quick_items(&state, "", &recent);
        assert_eq!(names(&items), ["a.pdf", "b.pdf", "r1.pdf", "r2.pdf"]);
        assert_eq!(
            items[2].target,
            QuickTarget::Recent(PathBuf::from("/docs/r1.pdf"))
        );
        assert_eq!(items[2].window, None);
        // The source is asked with the state; it decides what is recent.
        let source = |_: &AppState| vec![PathBuf::from("/x/y.pdf")];
        let provider: RecentDocuments<'_> = &source;
        let built = build_items(&mut state, Lang::En, PaletteMode::QuickOpen, "", provider);
        assert_eq!(built.len(), 3);
    }

    #[test]
    fn quick_open_matches_file_names_and_paths_and_prefers_names() {
        let ctx = Context::default();
        let (state, _) = state_with(&ctx, &["alpha", "beta"]);
        let recent = vec![PathBuf::from("/notes/alpha-notes/readme.pdf")];

        let items = quick_items(&state, "alpha", &recent);
        // The tab named alpha beats the document that only has alpha in its path.
        assert_eq!(names(&items), ["alpha.pdf", "readme.pdf"]);
        assert_eq!(items[0].name_positions, vec![0, 1, 2, 3, 4]);
        assert_eq!(items[1].name_positions, Vec::<usize>::new());
        assert!(!items[1].path_positions.is_empty());

        // Both live under /refdesk-test: the path finds both tabs.
        let items = quick_items(&state, "refdesk-test", &[]);
        assert_eq!(items.len(), 2);
        assert!(quick_items(&state, "zzz", &recent).is_empty());
    }

    // ---- selection ----

    #[test]
    fn the_selection_stops_at_both_ends() {
        assert_eq!(move_selection(0, 5, Step::Up), 0);
        assert_eq!(move_selection(4, 5, Step::Down), 4);
        assert_eq!(move_selection(1, 5, Step::Down), 2);
        assert_eq!(move_selection(3, 5, Step::Up), 2);
        assert_eq!(move_selection(2, 5, Step::PageDown), 4);
        assert_eq!(move_selection(2, 5, Step::PageUp), 0);
        assert_eq!(move_selection(2, 50, Step::PageDown), 2 + PAGE_ROWS);
        assert_eq!(move_selection(30, 50, Step::PageUp), 30 - PAGE_ROWS);
        assert_eq!(move_selection(2, 5, Step::Home), 0);
        assert_eq!(move_selection(2, 5, Step::End), 4);
        assert_eq!(move_selection(3, 0, Step::Down), 0);
        // A selection beyond the list counts as the last row.
        assert_eq!(move_selection(9, 5, Step::Up), 3);
    }

    #[test]
    fn a_shorter_list_brings_the_selection_back_inside() {
        assert_eq!(clamp_selection(7, 3), 2);
        assert_eq!(clamp_selection(1, 3), 1);
        assert_eq!(clamp_selection(5, 0), 0);
    }

    // ---- opening, closing, choosing ----

    #[test]
    fn the_shortcut_opens_closes_and_switches_the_list() {
        let mut state = AppState::new();
        assert!(state.palette.open.is_none());
        toggle(&mut state, PaletteMode::Commands, &Context::default());
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::Commands)
        );
        // Typing something, then the other shortcut: the other list, nothing typed.
        set_query(&mut state, "abc");
        toggle(&mut state, PaletteMode::QuickOpen, &Context::default());
        let open = state.palette.open.as_ref().expect("still open");
        assert_eq!(open.mode, PaletteMode::QuickOpen);
        assert_eq!(open.query, "");
        // The same shortcut again closes.
        toggle(&mut state, PaletteMode::QuickOpen, &Context::default());
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn the_palette_opens_on_the_window_of_the_focused_tab() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b"]);
        let second = state.new_window_id();
        state.windows.push(WindowState::new(second));
        state.windows[0].dock.remove_tab(tabs[1]);
        state.windows[1].dock.add_tab(tabs[1], None);
        dispatch(&mut state, Action::FocusTab(tabs[1]), &ctx);
        toggle(&mut state, PaletteMode::Commands, &Context::default());
        assert_eq!(state.palette.open.as_ref().map(|o| o.window), Some(second));
    }

    #[test]
    fn the_show_palette_action_toggles_through_dispatch() {
        let ctx = Context::default();
        let mut state = AppState::new();
        dispatch(&mut state, Action::ShowPalette(PaletteMode::Commands), &ctx);
        assert!(state.palette.open.is_some());
        dispatch(&mut state, Action::ShowPalette(PaletteMode::Commands), &ctx);
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn a_palette_whose_window_is_gone_is_closed() {
        let ctx = Context::default();
        let mut state = AppState::new();
        toggle(&mut state, PaletteMode::Commands, &Context::default());
        let missing = state.new_window_id();
        if let Some(open) = &mut state.palette.open {
            open.window = missing;
        }
        dispatch(&mut state, Action::OpenFileDialog, &ctx);
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn running_a_command_closes_the_palette_and_queues_its_action() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a"]);
        open_palette(&mut state, PaletteMode::Commands);
        set_query(&mut state, "duplicate");
        let mut queue = Vec::new();
        assert!(choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &no_recent_documents,
            0,
            &mut queue
        ));
        assert_eq!(queue, vec![Action::DuplicateTab(tabs[0])]);
        assert!(state.palette.open.is_none());
        assert_eq!(state.palette.recent_commands(), &["tab.duplicate"]);
        // The queued action is the ordinary one.
        state.actions.append(&mut queue);
        dispatch_queued(&mut state, &ctx);
        assert_eq!(state.main_window().dock.tabs().len(), 2);
    }

    #[test]
    fn running_a_disabled_command_does_nothing_and_keeps_the_palette_open() {
        let mut state = AppState::new();
        open_palette(&mut state, PaletteMode::Commands);
        set_query(&mut state, "close tab");
        let mut queue = Vec::new();
        assert!(!choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &no_recent_documents,
            0,
            &mut queue
        ));
        assert!(queue.is_empty());
        assert!(state.palette.open.is_some());
        assert!(state.palette.recent_commands().is_empty());
        // A row that does not exist is not a command either.
        assert!(!choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &no_recent_documents,
            99,
            &mut queue
        ));
    }

    #[test]
    fn choosing_the_command_that_opens_the_palette_reopens_it() {
        let ctx = Context::default();
        let mut state = AppState::new();
        open_palette(&mut state, PaletteMode::Commands);
        set_query(&mut state, "quick open");
        let mut queue = Vec::new();
        assert!(choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &no_recent_documents,
            0,
            &mut queue
        ));
        assert_eq!(queue, vec![Action::ShowPalette(PaletteMode::QuickOpen)]);
        state.actions.append(&mut queue);
        dispatch_queued(&mut state, &ctx);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::QuickOpen)
        );
    }

    #[test]
    fn choosing_a_tab_of_another_window_focuses_it() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b"]);
        let second = state.new_window_id();
        state.windows.push(WindowState::new(second));
        state.windows[0].dock.remove_tab(tabs[0]);
        state.windows[1].dock.add_tab(tabs[0], None);
        assert_eq!(state.focus, Some(tabs[1]));

        open_palette(&mut state, PaletteMode::QuickOpen);
        set_query(&mut state, "a.pdf");
        let mut queue = Vec::new();
        assert!(choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &no_recent_documents,
            0,
            &mut queue
        ));
        assert_eq!(queue, vec![Action::FocusTab(tabs[0])]);
        state.actions.append(&mut queue);
        dispatch_queued(&mut state, &ctx);
        assert_eq!(state.focus, Some(tabs[0]));
        assert_eq!(state.windows[1].dock.focused_tab(), Some(tabs[0]));
    }

    #[test]
    fn choosing_a_recent_document_opens_it() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        let source = |_: &AppState| vec![PathBuf::from("/docs/old.pdf")];
        open_palette(&mut state, PaletteMode::QuickOpen);
        let mut queue = Vec::new();
        // Row 0 is the open tab, row 1 the recent document.
        assert!(choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &source,
            1,
            &mut queue
        ));
        assert_eq!(
            queue,
            vec![Action::OpenFiles(vec![OpenTarget {
                path: PathBuf::from("/docs/old.pdf"),
                page: None
            }])]
        );
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn the_palette_commands_are_in_the_view_menu_with_their_shortcuts() {
        let view: Vec<&str> = commands::in_menu(commands::MenuGroup::View)
            .map(|c| c.id)
            .collect();
        for (id, shortcut) in [
            (
                COMMANDS_COMMAND_ID,
                KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::P),
            ),
            (
                QUICK_OPEN_COMMAND_ID,
                KeyboardShortcut::new(Modifiers::CTRL, Key::P),
            ),
        ] {
            assert!(view.contains(&id), "{id}");
            let command = commands::command(id).expect("command");
            assert_eq!(command.shortcut, Some(shortcut));
            assert!(command.run(&AppState::new()).is_some(), "{id} always runs");
        }
    }

    // ---- frames without a window ----

    const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);

    fn input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            events,
            ..Default::default()
        }
    }

    fn key(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// One frame the way `RefdeskApp::ui` runs it: shortcuts first, then the window. Dispatches
    /// the queue at the end, like the app does.
    fn frame(ctx: &Context, state: &mut AppState, events: Vec<egui::Event>) {
        let mut queue = Vec::new();
        ctx.run_ui(input(events), |ui| {
            queue.extend(commands::handle_shortcuts(ui.ctx(), state));
            crate::windows::show_window(ui, state, 0, &mut queue);
        })
        .drop_without_applying_deltas();
        state.actions.append(&mut queue);
        dispatch_queued(state, ctx);
    }

    fn settle(ctx: &Context, state: &mut AppState) {
        for _ in 0..3 {
            frame(ctx, state, vec![]);
        }
    }

    #[test]
    fn typing_and_enter_run_the_command_without_panicking() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        open_palette(&mut state, PaletteMode::Commands);
        settle(&ctx, &mut state);

        frame(&ctx, &mut state, vec![egui::Event::Text("duplic".into())]);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.query.as_str()),
            Some("duplic")
        );
        assert_eq!(state.main_window().dock.tabs().len(), 1);

        frame(&ctx, &mut state, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(state.palette.open.is_none());
        assert_eq!(state.main_window().dock.tabs().len(), 2, "the command ran");
        settle(&ctx, &mut state);
    }

    #[test]
    fn arrow_keys_move_the_selection_and_escape_closes() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a"]);
        open_palette(&mut state, PaletteMode::Commands);
        settle(&ctx, &mut state);
        let selected = |s: &AppState| s.palette.open.as_ref().map(|o| o.selected);
        assert_eq!(selected(&state), Some(0));
        frame(&ctx, &mut state, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        frame(&ctx, &mut state, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        assert_eq!(selected(&state), Some(2));
        frame(&ctx, &mut state, vec![key(Key::ArrowUp, Modifiers::NONE)]);
        assert_eq!(selected(&state), Some(1));
        frame(&ctx, &mut state, vec![key(Key::End, Modifiers::NONE)]);
        assert_eq!(selected(&state), Some(COMMANDS.len() - 1));
        frame(&ctx, &mut state, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        assert_eq!(selected(&state), Some(COMMANDS.len() - 1), "no wrap");
        frame(&ctx, &mut state, vec![key(Key::Home, Modifiers::NONE)]);
        assert_eq!(selected(&state), Some(0));

        frame(&ctx, &mut state, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(state.palette.open.is_none());
        assert_eq!(state.focus, Some(tabs[0]));
    }

    #[test]
    fn other_shortcuts_do_nothing_while_the_palette_is_open() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        let ctrl_w = key(Key::W, Modifiers::CTRL);
        // Without the palette Ctrl+W closes the tab.
        let mut probe = Vec::new();
        ctx.run_ui(input(vec![ctrl_w.clone()]), |ui| {
            probe.extend(commands::handle_shortcuts(ui.ctx(), &state));
        })
        .drop_without_applying_deltas();
        assert!(matches!(probe.as_slice(), [Action::CloseTab(_)]));

        open_palette(&mut state, PaletteMode::Commands);
        settle(&ctx, &mut state);
        frame(&ctx, &mut state, vec![ctrl_w.clone()]);
        let mut probe = Vec::new();
        ctx.run_ui(input(vec![ctrl_w]), |ui| {
            probe.extend(commands::handle_shortcuts(ui.ctx(), &state));
        })
        .drop_without_applying_deltas();
        assert!(probe.is_empty());
        assert_eq!(
            state.main_window().dock.tabs().len(),
            1,
            "the tab is still open"
        );
        assert!(state.palette.open.is_some());
    }

    #[test]
    fn shortcuts_are_blocked_from_the_very_first_frame_of_the_palette() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        open_palette(&mut state, PaletteMode::Commands);
        // No frame has run, so the text field has not taken the focus yet.
        assert!(!ctx.text_edit_focused());
        let mut probe = Vec::new();
        ctx.run_ui(input(vec![key(Key::W, Modifiers::CTRL)]), |ui| {
            probe.extend(commands::handle_shortcuts(ui.ctx(), &state));
        })
        .drop_without_applying_deltas();
        assert!(probe.is_empty());
    }

    #[test]
    fn the_shortcuts_close_and_switch_while_the_palette_is_open() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        let ctrl_shift_p = key(Key::P, Modifiers::CTRL.plus(Modifiers::SHIFT));
        let ctrl_p = key(Key::P, Modifiers::CTRL);

        // Closed: Ctrl+Shift+P opens the command palette.
        frame(&ctx, &mut state, vec![ctrl_shift_p.clone()]);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::Commands)
        );
        settle(&ctx, &mut state);
        // Ctrl+P switches to quick open.
        frame(&ctx, &mut state, vec![ctrl_p.clone()]);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::QuickOpen)
        );
        settle(&ctx, &mut state);
        // Ctrl+Shift+P switches back, Ctrl+Shift+P again closes.
        frame(&ctx, &mut state, vec![ctrl_shift_p.clone()]);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::Commands)
        );
        frame(&ctx, &mut state, vec![ctrl_shift_p]);
        assert!(state.palette.open.is_none());
        // Ctrl+P opens quick open and, pressed again, closes it.
        frame(&ctx, &mut state, vec![ctrl_p.clone()]);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::QuickOpen)
        );
        frame(&ctx, &mut state, vec![ctrl_p]);
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn quick_open_with_enter_switches_between_two_tabs() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b"]);
        assert_eq!(state.focus, Some(tabs[1]));
        for expected in [tabs[0], tabs[1], tabs[0]] {
            frame(&ctx, &mut state, vec![key(Key::P, Modifiers::CTRL)]);
            settle(&ctx, &mut state);
            frame(&ctx, &mut state, vec![key(Key::Enter, Modifiers::NONE)]);
            assert!(state.palette.open.is_none());
            assert_eq!(state.focus, Some(expected));
        }
    }

    #[test]
    fn a_click_outside_closes_the_palette() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        open_palette(&mut state, PaletteMode::Commands);
        settle(&ctx, &mut state);
        // The bottom-left corner is far from the palette at the top centre.
        let at = egui::pos2(10.0, SCREEN.y - 10.0);
        let click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame(&ctx, &mut state, vec![egui::Event::PointerMoved(at)]);
        frame(&ctx, &mut state, vec![click(true)]);
        assert!(state.palette.open.is_some(), "closes on release");
        frame(&ctx, &mut state, vec![click(false)]);
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn both_lists_draw_without_panicking_for_odd_input() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a", "b"]);
        for mode in [PaletteMode::Commands, PaletteMode::QuickOpen] {
            state.palette.open = None;
            open_palette(&mut state, mode);
            settle(&ctx, &mut state);
            for text in ["분할", "zzzz-no-match", "", "  ", "a.pdf"] {
                set_query(&mut state, text);
                frame(&ctx, &mut state, vec![]);
                frame(&ctx, &mut state, vec![key(Key::ArrowDown, Modifiers::NONE)]);
                frame(&ctx, &mut state, vec![key(Key::PageDown, Modifiers::NONE)]);
            }
        }
    }

    // ---- the files of the workspace (WS-6) ----

    /// An index of the workspace "/ws" with these files (folders are made from the paths), in
    /// the order given, which must list a folder's files together.
    fn folder_index(files: &[String]) -> Arc<WorkspaceIndex> {
        use refdesk_core::WorkspaceEntry;
        let mut entries: Vec<WorkspaceEntry> = Vec::new();
        for file in files {
            let rel = PathBuf::from(file);
            let mut parent = None;
            let mut so_far = PathBuf::new();
            let components: Vec<_> = rel.components().collect();
            for (depth, part) in components.iter().enumerate() {
                so_far.push(part);
                let is_file = depth + 1 == components.len();
                let known = entries.iter().position(|e| e.rel_path == so_far);
                let at = match known {
                    Some(at) => at,
                    None => {
                        entries.push(WorkspaceEntry {
                            name: part.as_os_str().to_string_lossy().into_owned(),
                            rel_path: so_far.clone(),
                            kind: if is_file {
                                EntryKind::Pdf
                            } else {
                                EntryKind::Dir
                            },
                            parent,
                            depth: depth as u16,
                        });
                        entries.len() - 1
                    }
                };
                parent = Some(at as u32);
            }
        }
        Arc::new(WorkspaceIndex::new(PathBuf::from("/ws"), entries, false))
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn quick(
        state: &mut AppState,
        index: &Arc<WorkspaceIndex>,
        query: &str,
        recent: &[PathBuf],
    ) -> Vec<QuickItem> {
        sync_folder(&mut state.palette, Some(index), query);
        quick_items(state, query, recent)
    }

    #[test]
    fn quick_open_lists_tabs_then_folder_files_then_recent_documents_without_repeats() {
        let ctx = Context::default();
        let mut state = AppState::new();
        open_pdf(&mut state, &ctx, "/ws/specs/a.pdf");
        let index = folder_index(&strings(&["specs/a.pdf", "specs/b.pdf", "c.pdf"]));
        let recent = vec![
            PathBuf::from("/ws/c.pdf"),
            PathBuf::from("/docs/r.pdf"),
            PathBuf::from("/ws/specs/a.pdf"),
        ];
        let items = quick(&mut state, &index, "", &recent);
        assert_eq!(names(&items), ["a.pdf", "b.pdf", "c.pdf", "r.pdf"]);
        assert!(matches!(items[0].target, QuickTarget::Tab(_)));
        assert_eq!(
            items[1].target,
            QuickTarget::Folder(PathBuf::from("/ws/specs/b.pdf"))
        );
        assert_eq!(
            items[1].path, "specs/b.pdf",
            "the path relative to the folder"
        );
        assert_eq!(
            items[2].target,
            QuickTarget::Folder(PathBuf::from("/ws/c.pdf"))
        );
        assert_eq!(
            items[3].target,
            QuickTarget::Recent(PathBuf::from("/docs/r.pdf"))
        );
        // With a query the parts keep their order.
        let items = quick(&mut state, &index, "pdf", &recent);
        let kinds: Vec<&str> = items
            .iter()
            .map(|i| match i.target {
                QuickTarget::Tab(_) => "tab",
                QuickTarget::Folder(_) => "folder",
                QuickTarget::FolderMore(_) => "more",
                QuickTarget::Recent(_) => "recent",
            })
            .collect();
        assert_eq!(kinds, ["tab", "folder", "folder", "recent"]);
        // Without a workspace there is no folder part, and nothing of the folder is held.
        sync_folder(&mut state.palette, None, "");
        assert_eq!(
            names(&quick_items(&state, "", &recent)),
            ["a.pdf", "c.pdf", "r.pdf"]
        );
    }

    #[test]
    fn an_empty_query_lists_the_first_fifty_files_and_a_line_for_the_rest() {
        let ctx = Context::default();
        let mut state = AppState::new();
        let files: Vec<String> = (0..120).map(|n| format!("doc{n:03}.pdf")).collect();
        let index = folder_index(&files);
        open_pdf(&mut state, &ctx, "/ws/doc000.pdf");
        let items = quick(&mut state, &index, "", &[]);
        let folder: Vec<&QuickItem> = items
            .iter()
            .filter(|i| matches!(i.target, QuickTarget::Folder(_)))
            .collect();
        assert_eq!(folder.len(), FOLDER_FIRST_FILES);
        assert_eq!(folder[0].name, "doc001.pdf", "the open one is left out");
        let last = items.last().expect("rows");
        assert_eq!(
            last.target,
            QuickTarget::FolderMore(120 - 1 - FOLDER_FIRST_FILES)
        );
        assert!(last.name.contains("69"), "{}", last.name);
        // The line does nothing when chosen.
        open_palette(&mut state, PaletteMode::QuickOpen);
        let mut queue = Vec::new();
        let source = |_: &AppState| Vec::new();
        assert!(!choose(
            &mut state,
            &ctx,
            Lang::En,
            &source,
            items.len() - 1,
            &mut queue
        ));
        assert!(queue.is_empty());
        assert!(state.palette.open.is_some());
    }

    #[test]
    fn a_query_lists_the_best_matches_first_and_at_most_two_hundred() {
        let mut state = AppState::new();
        let mut files: Vec<String> = (0..300).map(|n| format!("misc/file{n:03}.pdf")).collect();
        files.push("target/notes.pdf".into());
        files.push("zeta-target.pdf".into());
        let index = folder_index(&files);
        let items = quick(&mut state, &index, "target", &[]);
        assert_eq!(
            names(&items),
            ["zeta-target.pdf", "notes.pdf"],
            "a name match first"
        );
        let items = quick(&mut state, &index, "file", &[]);
        assert_eq!(items.len(), FOLDER_MATCHES);
        assert!(items.windows(2).all(|w| w[0].score >= w[1].score));
        assert!(
            !items
                .iter()
                .any(|i| matches!(i.target, QuickTarget::FolderMore(_)))
        );
        assert!(quick(&mut state, &index, "zzz", &[]).is_empty());
    }

    #[test]
    fn a_growing_query_matches_the_same_as_a_fresh_one() {
        let files: Vec<String> = (0..500)
            .map(|n| format!("area{}/Spec {n} Rev{}.pdf", n % 7, n % 3))
            .collect();
        let index = folder_index(&files);
        let mut typed = AppState::new();
        for query in ["s", "sp", "spe", "spec 1", "spec 1 r"] {
            sync_folder(&mut typed.palette, Some(&index), query);
        }
        let mut fresh = AppState::new();
        let a = quick_items(&typed, "spec 1 r", &[]);
        let b = quick(&mut fresh, &index, "spec 1 r", &[]);
        assert!(!a.is_empty());
        assert_eq!(names(&a), names(&b));
        // Going back to a shorter query matches again from all files.
        sync_folder(&mut typed.palette, Some(&index), "s");
        assert_eq!(quick_items(&typed, "s", &[]).len(), FOLDER_MATCHES);
    }

    #[test]
    fn the_folder_is_matched_again_only_when_the_query_or_the_index_changes() {
        let index = folder_index(&strings(&["a.pdf", "b.pdf"]));
        let mut state = AppState::new();
        sync_folder(&mut state.palette, Some(&index), "a");
        let hits = |state: &AppState| {
            Arc::clone(&state.palette.folder.search.as_ref().expect("searched").hits)
        };
        let first = hits(&state);
        sync_folder(&mut state.palette, Some(&index), "a");
        sync_folder(&mut state.palette, Some(&index), " A ");
        assert!(Arc::ptr_eq(&first, &hits(&state)), "the same key");
        sync_folder(&mut state.palette, Some(&index), "b");
        assert!(!Arc::ptr_eq(&first, &hits(&state)));
        let other = folder_index(&strings(&["a.pdf"]));
        sync_folder(&mut state.palette, Some(&other), "b");
        assert!(hits(&state).is_empty(), "a new index is read again");
    }

    #[test]
    fn choosing_a_file_of_the_folder_opens_an_ordinary_tab() {
        let dir = std::env::temp_dir().join(format!("refdesk-wp72-palette-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).expect("folders");
        std::fs::write(dir.join("sub/pick.pdf"), b"").expect("file");
        let dir = crate::state::normalize_path(&dir);
        let ctx = Context::default();
        let repaints = crate::views::test_support::watch_repaints(&ctx);
        let mut state = AppState::new();
        dispatch(&mut state, Action::OpenFolder(dir.clone()), &ctx);
        repaints.wait_until("the scan", || {
            ctx.run_ui(egui::RawInput::default(), |_| {})
                .drop_without_applying_deltas();
            workspace_index(&state).is_some_and(|i| i.file_count() == 1)
        });
        open_palette(&mut state, PaletteMode::QuickOpen);
        set_query(&mut state, "pick");
        let source = |_: &AppState| Vec::new();
        let mut queue = Vec::new();
        assert!(choose(&mut state, &ctx, Lang::En, &source, 0, &mut queue));
        let path = dir.join("sub/pick.pdf");
        assert_eq!(
            queue,
            vec![Action::OpenFiles(vec![OpenTarget {
                path: path.clone(),
                page: None
            }])]
        );
        state.actions.append(&mut queue);
        dispatch_queued(&mut state, &ctx);
        let (_, pdf) = state.focused_pdf().expect("opened");
        assert!(!pdf.preview);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fifty_thousand_files_are_matched_quickly_per_typed_character() {
        use std::time::{Duration, Instant};
        let files: Vec<String> = (0..50)
            .flat_map(|t| {
                (0..10).flat_map(move |s| {
                    (0..100).map(move |f| {
                        format!("area{t}/part{s}/Specification {t}-{s}-{f} Revision.pdf")
                    })
                })
            })
            .collect();
        let index = {
            // Built directly: the generic builder above looks folders up linearly.
            use refdesk_core::WorkspaceEntry;
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
                    for f in 0..100u32 {
                        let name = format!("Specification {t}-{s}-{f} Revision.pdf");
                        entries.push(WorkspaceEntry {
                            rel_path: PathBuf::from(format!("area{t}/part{s}/{name}")),
                            name,
                            kind: EntryKind::Pdf,
                            parent: Some(sub),
                            depth: 2,
                        });
                    }
                }
            }
            Arc::new(WorkspaceIndex::new(PathBuf::from("/ws"), entries, false))
        };
        assert_eq!(index.file_count(), files.len());
        let mut state = AppState::new();
        let t = Instant::now();
        sync_folder(&mut state.palette, Some(&index), "");
        let prepare = t.elapsed();
        let mut timings = Vec::new();
        for query in ["s", "sp", "spe", "spec 4", "spec 42", "spec 42-"] {
            let t = Instant::now();
            sync_folder(&mut state.palette, Some(&index), query);
            let items = quick_items(&state, query, &[]);
            timings.push((query, t.elapsed(), items.len()));
        }
        let t = Instant::now();
        let again = quick_items(&state, "spec 42-", &[]);
        let frame = t.elapsed();
        eprintln!(
            "WP-72 quick open timing for {} files: preparing the files {prepare:?}; per typed \
             query (match + list): {timings:?}; a frame with the same query {frame:?}",
            files.len()
        );
        assert!(!again.is_empty());
        // Generous (debug build): nothing per frame, nothing quadratic.
        assert!(prepare < Duration::from_secs(3));
        for (query, took, _) in &timings {
            assert!(*took < Duration::from_secs(3), "{query}: {took:?}");
        }
        assert!(frame < Duration::from_millis(100), "{frame:?}");
    }
}
