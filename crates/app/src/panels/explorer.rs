//! The file explorer (WS-2 – WS-5, WS-9 – WS-11): the folder tree of the workspace in the upper
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
//! Opening a file goes through one place: [`open_action`] makes the [`Action::OpenFromExplorer`]
//! for a click (whose kind [`crate::action::explorer_click`] decides), a key ([`tree_key`]) or
//! an entry of the right-click menu ([`EXPLORER_MENU`]). Copying a path and showing a folder in
//! the file manager go through [`ExplorerServices`], which tests replace.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use egui::{
    EventFilter, FocusDirection, Id, Key, Modifiers, Pos2, Rect, RichText, ScrollArea, Sense,
    StrokeKind, TextEdit, TextStyle, Ui, WidgetInfo,
};
use refdesk_core::{EntryKind, ScanLimits, WorkspaceIndex, WorkspaceScan, WorkspaceWatcher};

use crate::action::{Action, ExplorerOpen, explorer_click, notifier};
use crate::i18n::tr;
use crate::panels::project::{self, ProjectAction};
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
/// The height of the body of a project without documents: a line, a button and a hint.
const EMPTY_PROJECT_BODY_H: f32 = 96.0;

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

/// The folders inside folder `idx` at any depth, in index order. The index lists a folder's
/// contents right after it (pre-order), so they are the entries that follow it while deeper.
pub fn descendant_dirs(index: &WorkspaceIndex, idx: u32) -> Vec<u32> {
    let Some(depth) = index.entry(idx).map(|e| e.depth) else {
        return Vec::new();
    };
    index
        .entries
        .iter()
        .enumerate()
        .skip(idx as usize + 1)
        .take_while(|(_, e)| e.depth > depth)
        .filter(|(_, e)| e.kind == EntryKind::Dir)
        .map(|(i, _)| i as u32)
        .collect()
}

// --- keyboard (WS-10) ------------------------------------------------------------------------

/// A key the tree reacts to while it has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeKey {
    /// ↑
    Up,
    /// ↓
    Down,
    /// ←
    Left,
    /// →
    Right,
    /// Home
    Home,
    /// End
    End,
    /// Page Up
    PageUp,
    /// Page Down
    PageDown,
    /// Enter: open a file in an ordinary tab, fold or unfold a folder.
    Enter,
    /// Space: open a file in the preview tab, fold or unfold a folder.
    Space,
}

impl TreeKey {
    /// The keys and the egui keys they come from. Taking them from the input also keeps them
    /// from the view, which would scroll with them.
    const ALL: [(Key, TreeKey); 10] = [
        (Key::ArrowUp, TreeKey::Up),
        (Key::ArrowDown, TreeKey::Down),
        (Key::ArrowLeft, TreeKey::Left),
        (Key::ArrowRight, TreeKey::Right),
        (Key::Home, TreeKey::Home),
        (Key::End, TreeKey::End),
        (Key::PageUp, TreeKey::PageUp),
        (Key::PageDown, TreeKey::PageDown),
        (Key::Enter, TreeKey::Enter),
        (Key::Space, TreeKey::Space),
    ];
}

/// What a key in the tree does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeMove {
    /// The cursor goes to this entry.
    Cursor(u32),
    /// Folder `idx` opens or folds; the cursor stays on it.
    Fold {
        /// The folder.
        idx: u32,
        /// Open it (else fold it).
        open: bool,
    },
    /// File `idx` opens.
    Open {
        /// The file.
        idx: u32,
        /// In which kind of tab.
        how: ExplorerOpen,
    },
    /// The keyboard goes back to the filter box (↑ on the first row).
    ToFilter,
    /// Nothing happens.
    Nothing,
}

/// What `key` does in the tree showing `rows` with the cursor on entry `cursor` (`None`, or an
/// entry that is not shown, counts as no cursor: the first key puts it on the first row, End on
/// the last). `page` is how many rows Page Up and Page Down move. While `filtering`, every
/// folder is open and does not fold, so → and ← only move.
pub fn tree_key(
    index: &WorkspaceIndex,
    rows: &[Row],
    cursor: Option<u32>,
    key: TreeKey,
    page: usize,
    filtering: bool,
) -> TreeMove {
    let Some(last) = rows.len().checked_sub(1) else {
        return TreeMove::Nothing;
    };
    let at = |pos: usize| TreeMove::Cursor(rows[pos.min(last)].idx);
    let Some(pos) = cursor.and_then(|c| rows.iter().position(|r| r.idx == c)) else {
        return match key {
            TreeKey::End => at(last),
            _ => at(0),
        };
    };
    let row = &rows[pos];
    let foldable = row.fold != Fold::Leaf && !filtering;
    match key {
        TreeKey::Up if pos == 0 => TreeMove::ToFilter,
        TreeKey::Up => at(pos - 1),
        TreeKey::Down => at(pos + 1),
        TreeKey::Home => at(0),
        TreeKey::End => at(last),
        TreeKey::PageUp => at(pos.saturating_sub(page.max(1))),
        TreeKey::PageDown => at(pos + page.max(1)),
        TreeKey::Right => match row.fold {
            Fold::Closed if foldable => TreeMove::Fold {
                idx: row.idx,
                open: true,
            },
            Fold::Leaf => TreeMove::Nothing,
            // Open: to the first child, which is the next row.
            _ => match rows.get(pos + 1) {
                Some(next) if next.depth > row.depth => TreeMove::Cursor(next.idx),
                _ => TreeMove::Nothing,
            },
        },
        TreeKey::Left => {
            if foldable && row.fold == Fold::Open {
                return TreeMove::Fold {
                    idx: row.idx,
                    open: false,
                };
            }
            index
                .entry(row.idx)
                .and_then(|e| e.parent)
                .filter(|p| rows.iter().any(|r| r.idx == *p))
                .map_or(TreeMove::Nothing, TreeMove::Cursor)
        }
        TreeKey::Enter | TreeKey::Space => match row.fold {
            Fold::Leaf => TreeMove::Open {
                idx: row.idx,
                how: if key == TreeKey::Enter {
                    ExplorerOpen::Pinned
                } else {
                    ExplorerOpen::Preview
                },
            },
            fold if foldable => TreeMove::Fold {
                idx: row.idx,
                open: fold == Fold::Closed,
            },
            _ => TreeMove::Nothing,
        },
    }
}

// --- the right-click menu (WS-9) --------------------------------------------------------------

/// What a right click in the explorer was on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuOn {
    /// A file row.
    File,
    /// A folder row.
    Folder,
    /// The empty part of the list.
    Empty,
}

/// Everything the entries of the right-click menu decide from.
#[derive(Clone, Debug)]
pub struct MenuContext<'a> {
    /// What was clicked.
    pub on: MenuOn,
    /// The entry of the index, for a row.
    pub idx: Option<u32>,
    /// The absolute path of the entry; the workspace folder for [`MenuOn::Empty`].
    pub path: PathBuf,
    /// The path relative to the workspace folder (empty for [`MenuOn::Empty`]).
    pub rel: PathBuf,
    /// A folder row is expanded.
    pub expanded: bool,
    /// A filter is active: folders do not fold.
    pub filtering: bool,
    /// The file picked with "Select for Compare", if any.
    pub pick: Option<&'a Path>,
    /// The document of the window's focused tab, if any.
    pub current: Option<&'a Path>,
    /// The window whose explorer this is.
    pub window: WindowId,
}

impl MenuContext<'_> {
    fn open(&self, how: ExplorerOpen) -> Option<MenuEffect> {
        Some(MenuEffect::Queue(Action::OpenFromExplorer {
            path: self.path.clone(),
            window: self.window,
            how,
        }))
    }
}

/// What choosing an entry of the right-click menu does. Opening and comparing are actions for
/// `dispatch`; the rest only touches this window's explorer or the outside world.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuEffect {
    /// Queue an action.
    Queue(Action),
    /// Open or fold a folder.
    Fold {
        /// The folder.
        idx: u32,
        /// Open it (else fold it).
        open: bool,
    },
    /// Open a folder and every folder inside it.
    ExpandAll(u32),
    /// Fold every folder.
    CollapseAll,
    /// Remember a file as the older side of the next comparison.
    Pick(PathBuf),
    /// Put a text on the clipboard.
    Copy(String),
    /// Show a folder in the file manager.
    Reveal(PathBuf),
    /// Start typing a name in the explorer (new folder, rename).
    Edit(InlineEdit),
}

/// One entry of the explorer's right-click menu. Like `commands::TAB_MENU` it is a table that
/// drawing reads; whether an entry is possible and what it does are plain functions.
pub struct ExplorerMenuEntry {
    /// What the click has to be on for the entry to be listed.
    pub on: MenuOn,
    /// A separator line goes above the entry.
    pub separator_before: bool,
    /// The i18n key of the displayed name.
    pub name: fn(&MenuContext<'_>) -> &'static str,
    /// Whether the entry can be chosen now.
    pub enabled: fn(&MenuContext<'_>) -> bool,
    /// What choosing it does.
    pub effect: fn(&MenuContext<'_>) -> Option<MenuEffect>,
}

fn yes(_: &MenuContext<'_>) -> bool {
    true
}

fn not_filtering(m: &MenuContext<'_>) -> bool {
    !m.filtering
}

fn copy_path(m: &MenuContext<'_>) -> Option<MenuEffect> {
    Some(MenuEffect::Copy(m.path.display().to_string()))
}

/// The menu, in display order; each kind of click lists its own rows.
pub static EXPLORER_MENU: &[ExplorerMenuEntry] = &[
    // A file.
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: false,
        name: |_| "explorer.menu.open",
        enabled: yes,
        effect: |m| m.open(ExplorerOpen::Pinned),
    },
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: false,
        name: |_| "explorer.menu.open_background",
        enabled: yes,
        effect: |m| m.open(ExplorerOpen::Background),
    },
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: false,
        name: |_| "explorer.menu.open_split",
        enabled: yes,
        effect: |m| m.open(ExplorerOpen::SplitSide),
    },
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: false,
        name: |_| "explorer.menu.open_window",
        enabled: yes,
        effect: |m| m.open(ExplorerOpen::NewWindow),
    },
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: true,
        name: |_| "explorer.menu.pick_compare",
        enabled: yes,
        effect: |m| Some(MenuEffect::Pick(m.path.clone())),
    },
    // The picked file is the older (left) side, the clicked one the newer.
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: false,
        name: |_| "explorer.menu.compare_picked",
        enabled: |m| m.pick.is_some_and(|p| p != m.path),
        effect: |m| {
            Some(MenuEffect::Queue(Action::StartDiff {
                left: m.pick?.to_path_buf(),
                right: m.path.clone(),
            }))
        },
    },
    // The document being read is the older side.
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: false,
        name: |_| "explorer.menu.compare_current",
        enabled: |m| m.current.is_some_and(|c| c != m.path),
        effect: |m| {
            Some(MenuEffect::Queue(Action::StartDiff {
                left: m.current?.to_path_buf(),
                right: m.path.clone(),
            }))
        },
    },
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: true,
        name: |_| "explorer.menu.copy_path",
        enabled: yes,
        effect: copy_path,
    },
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: false,
        name: |_| "explorer.menu.copy_relative",
        enabled: yes,
        effect: |m| Some(MenuEffect::Copy(m.rel.display().to_string())),
    },
    // The folder that holds the file.
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: false,
        name: |_| "explorer.menu.reveal",
        enabled: yes,
        effect: |m| Some(MenuEffect::Reveal(m.path.parent()?.to_path_buf())),
    },
    // A folder.
    ExplorerMenuEntry {
        on: MenuOn::Folder,
        separator_before: false,
        name: |m| {
            if m.expanded {
                "explorer.menu.collapse"
            } else {
                "explorer.menu.expand"
            }
        },
        enabled: not_filtering,
        effect: |m| {
            Some(MenuEffect::Fold {
                idx: m.idx?,
                open: !m.expanded,
            })
        },
    },
    ExplorerMenuEntry {
        on: MenuOn::Folder,
        separator_before: false,
        name: |_| "explorer.menu.expand_all",
        enabled: not_filtering,
        effect: |m| Some(MenuEffect::ExpandAll(m.idx?)),
    },
    ExplorerMenuEntry {
        on: MenuOn::Folder,
        separator_before: true,
        name: |_| "explorer.menu.copy_path",
        enabled: yes,
        effect: copy_path,
    },
    // The folder itself.
    ExplorerMenuEntry {
        on: MenuOn::Folder,
        separator_before: false,
        name: |_| "explorer.menu.reveal",
        enabled: yes,
        effect: |m| Some(MenuEffect::Reveal(m.path.clone())),
    },
    // The empty part of the list.
    ExplorerMenuEntry {
        on: MenuOn::Empty,
        separator_before: false,
        name: |_| "explorer.refresh",
        enabled: yes,
        effect: |_| Some(MenuEffect::Queue(Action::RefreshWorkspace)),
    },
    ExplorerMenuEntry {
        on: MenuOn::Empty,
        separator_before: false,
        name: |_| "explorer.collapse_all",
        enabled: yes,
        effect: |_| Some(MenuEffect::CollapseAll),
    },
    ExplorerMenuEntry {
        on: MenuOn::Empty,
        separator_before: false,
        name: |_| "cmd.file.close_folder",
        enabled: yes,
        effect: |_| Some(MenuEffect::Queue(Action::CloseFolder)),
    },
];

/// The rows of the menu for a click on `on`, in display order.
pub fn menu_entries(on: MenuOn) -> impl Iterator<Item = &'static ExplorerMenuEntry> {
    EXPLORER_MENU.iter().filter(move |e| e.on == on)
}

/// The folder a menu entry imports into: the clicked folder, or the root for the empty space.
fn import_folder_of(m: &MenuContext<'_>) -> PathBuf {
    m.rel.clone()
}

/// The entries a project adds to the menu (PROJ-2, PROJ-5), after [`EXPLORER_MENU`]'s. They are
/// listed only while the workspace is a project.
pub static PROJECT_MENU: &[ExplorerMenuEntry] = &[
    ExplorerMenuEntry {
        on: MenuOn::Folder,
        separator_before: true,
        name: |_| "explorer.menu.import_here",
        enabled: yes,
        effect: |m| {
            Some(MenuEffect::Queue(Action::Project(
                ProjectAction::ImportDialog {
                    into: import_folder_of(m),
                },
            )))
        },
    },
    ExplorerMenuEntry {
        on: MenuOn::Folder,
        separator_before: false,
        name: |_| "explorer.menu.import_folder",
        enabled: yes,
        effect: |m| {
            Some(MenuEffect::Queue(Action::Project(
                ProjectAction::ImportFolderDialog {
                    into: import_folder_of(m),
                },
            )))
        },
    },
    ExplorerMenuEntry {
        on: MenuOn::Folder,
        separator_before: false,
        name: |_| "explorer.menu.new_folder",
        enabled: yes,
        effect: |m| Some(MenuEffect::Edit(InlineEdit::new_folder(m.rel.clone()))),
    },
    ExplorerMenuEntry {
        on: MenuOn::Folder,
        separator_before: false,
        name: |_| "explorer.menu.rename",
        enabled: yes,
        effect: |m| Some(MenuEffect::Edit(InlineEdit::rename(m.rel.clone(), true))),
    },
    ExplorerMenuEntry {
        on: MenuOn::Folder,
        separator_before: false,
        name: |_| "explorer.menu.trash",
        enabled: yes,
        effect: |m| {
            Some(MenuEffect::Queue(Action::Project(
                ProjectAction::AskTrash { rel: m.rel.clone() },
            )))
        },
    },
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: true,
        name: |_| "explorer.menu.rename",
        enabled: yes,
        effect: |m| Some(MenuEffect::Edit(InlineEdit::rename(m.rel.clone(), false))),
    },
    ExplorerMenuEntry {
        on: MenuOn::File,
        separator_before: false,
        name: |_| "explorer.menu.trash",
        enabled: yes,
        effect: |m| {
            Some(MenuEffect::Queue(Action::Project(
                ProjectAction::AskTrash { rel: m.rel.clone() },
            )))
        },
    },
    ExplorerMenuEntry {
        on: MenuOn::Empty,
        separator_before: true,
        name: |_| "explorer.menu.import_here",
        enabled: yes,
        effect: |m| {
            Some(MenuEffect::Queue(Action::Project(
                ProjectAction::ImportDialog {
                    into: import_folder_of(m),
                },
            )))
        },
    },
    ExplorerMenuEntry {
        on: MenuOn::Empty,
        separator_before: false,
        name: |_| "explorer.menu.import_folder",
        enabled: yes,
        effect: |m| {
            Some(MenuEffect::Queue(Action::Project(
                ProjectAction::ImportFolderDialog {
                    into: import_folder_of(m),
                },
            )))
        },
    },
    ExplorerMenuEntry {
        on: MenuOn::Empty,
        separator_before: false,
        name: |_| "explorer.menu.new_folder",
        enabled: yes,
        effect: |_| Some(MenuEffect::Edit(InlineEdit::new_folder(PathBuf::new()))),
    },
    ExplorerMenuEntry {
        on: MenuOn::Empty,
        separator_before: false,
        name: |_| "cmd.file.project_trash",
        enabled: yes,
        effect: |_| Some(MenuEffect::Queue(Action::Project(ProjectAction::ShowTrash))),
    },
];

/// The rows of the menu for a click on `on`: [`menu_entries`], then the project's when
/// `in_project`.
pub fn menu_entries_for(
    on: MenuOn,
    in_project: bool,
) -> impl Iterator<Item = &'static ExplorerMenuEntry> {
    let extra: &'static [ExplorerMenuEntry] = if in_project { PROJECT_MENU } else { &[] };
    menu_entries(on).chain(extra.iter().filter(move |e| e.on == on))
}

/// What is being typed in the explorer's field (PROJ-5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditKind {
    /// The name of a new folder inside `parent` (below the root).
    NewFolder {
        /// The folder it goes into.
        parent: PathBuf,
    },
    /// A new name for `rel`.
    Rename {
        /// The file or folder below the root.
        rel: PathBuf,
        /// It is a folder (a file keeps its extension).
        is_dir: bool,
    },
}

/// A name being typed in the explorer: a new folder or a rename.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineEdit {
    /// What it is for.
    pub kind: EditKind,
    /// The text of the field.
    pub text: String,
    /// Why the text cannot be used, after Enter.
    pub error: Option<String>,
    /// The field takes the keyboard when it is next drawn.
    pub focus: bool,
}

impl InlineEdit {
    /// Typing the name of a new folder inside `parent`.
    pub fn new_folder(parent: PathBuf) -> InlineEdit {
        InlineEdit {
            kind: EditKind::NewFolder { parent },
            text: String::new(),
            error: None,
            focus: true,
        }
    }

    /// Typing a new name for `rel`, starting from its name (without a file's extension).
    pub fn rename(rel: PathBuf, is_dir: bool) -> InlineEdit {
        InlineEdit {
            text: project::rename_start_text(&rel, is_dir),
            kind: EditKind::Rename { rel, is_dir },
            error: None,
            focus: true,
        }
    }
}

// --- the outside world ---------------------------------------------------------------------

/// Puts text on the clipboard. Tests use a fake that records instead.
pub trait TextClipboard: Send + Sync {
    /// Copies `text`.
    fn copy_text(&self, ctx: &egui::Context, text: String);
}

/// Called with the outcome of [`FolderOpener::open_folder`], possibly on another thread.
pub type OpenDone = Box<dyn FnOnce(Result<(), String>) + Send>;

/// Shows a folder in the file manager. Tests use a fake that records instead.
pub trait FolderOpener: Send + Sync {
    /// Opens `dir` without blocking the caller; `done` gets the outcome.
    fn open_folder(&self, dir: &Path, done: OpenDone);
}

/// The clipboard of the windowing system, through egui.
struct EguiClipboard;

impl TextClipboard for EguiClipboard {
    fn copy_text(&self, ctx: &egui::Context, text: String) {
        ctx.copy_text(text);
    }
}

/// `xdg-open DIR` on a thread of its own: it may take a moment to start the file manager.
struct XdgOpen;

impl FolderOpener for XdgOpen {
    fn open_folder(&self, dir: &Path, done: OpenDone) {
        // Shared so that `done` is still there to report a thread that could not start.
        let done = Arc::new(Mutex::new(Some(done)));
        let finish = |slot: &Mutex<Option<OpenDone>>, result| {
            if let Some(done) = slot.lock().unwrap_or_else(PoisonError::into_inner).take() {
                done(result);
            }
        };
        let (dir, slot) = (dir.to_path_buf(), Arc::clone(&done));
        let spawned = std::thread::Builder::new()
            .name("refdesk-xdg-open".into())
            .spawn(move || {
                let result = std::process::Command::new("xdg-open")
                    .arg(&dir)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map_err(|e| format!("xdg-open: {e}"))
                    .and_then(|status| {
                        if status.success() {
                            Ok(())
                        } else {
                            Err(format!("xdg-open: {status}"))
                        }
                    });
                finish(&slot, result);
            });
        if let Err(err) = spawned {
            finish(&done, Err(err.to_string()));
        }
    }
}

/// How the explorer reaches the clipboard and the file manager.
#[derive(Clone)]
pub struct ExplorerServices {
    /// Where "Copy Path" puts the text.
    pub clipboard: Arc<dyn TextClipboard>,
    /// What "Show in File Manager" runs.
    pub opener: Arc<dyn FolderOpener>,
}

impl Default for ExplorerServices {
    fn default() -> Self {
        ExplorerServices {
            clipboard: Arc::new(EguiClipboard),
            opener: Arc::new(XdgOpen),
        }
    }
}

impl std::fmt::Debug for ExplorerServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExplorerServices")
    }
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
    /// The row chosen with the keyboard (or the last clicked one), as an entry of `index`.
    cursor: Option<u32>,
    /// The cursor moved by key: the list scrolls it into view on the next frame.
    scroll_to_cursor: bool,
    /// The file chosen with "Select for Compare" (absolute path).
    compare_pick: Option<PathBuf>,
    /// The clipboard and the file manager.
    pub services: ExplorerServices,
    /// Errors from the file manager, reported from its thread, shown on the next frame.
    reveal_errors: Arc<Mutex<Vec<String>>>,
    /// A name being typed: a new folder or a rename (PROJ-5).
    pub edit: Option<InlineEdit>,
}

impl ExplorerState {
    /// Starts over for another workspace: no folding, filter, cursor or pick. The services stay.
    pub fn reset(&mut self) {
        *self = ExplorerState {
            services: self.services.clone(),
            ..ExplorerState::default()
        };
    }

    /// The file chosen with "Select for Compare", if any.
    #[cfg(test)]
    pub fn compare_pick(&self) -> Option<&Path> {
        self.compare_pick.as_deref()
    }

    /// Forgets the file chosen with "Select for Compare" (a comparison started).
    pub fn clear_compare_pick(&mut self) {
        self.compare_pick = None;
    }

    /// The folder (below the root) of the row the cursor is on: the folder itself, or the
    /// folder of a file. `None` without a cursor.
    pub fn cursor_folder(&self) -> Option<PathBuf> {
        let entry = self.index.as_ref()?.entry(self.cursor?)?;
        Some(match entry.kind {
            EntryKind::Dir => entry.rel_path.clone(),
            EntryKind::Pdf => entry
                .rel_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
        })
    }

    /// Puts the cursor on the entry at `rel` (below the root) of `index`, opening the folders
    /// above it and scrolling to it. Returns whether the entry was found.
    pub fn select_path(&mut self, index: &Arc<WorkspaceIndex>, rel: &Path) -> bool {
        self.sync(index);
        let Some(idx) = index.find(rel) else {
            return false;
        };
        if reveal(index, &mut self.expanded, idx) {
            self.version += 1;
        }
        self.cursor = Some(idx);
        self.scroll_to_cursor = true;
        true
    }

    /// The entry the keyboard cursor is on.
    #[cfg(test)]
    pub fn cursor(&self) -> Option<u32> {
        self.cursor
    }

    /// Opens or folds folder `idx`. Files are left alone.
    pub fn set_expanded(&mut self, idx: u32, open: bool) {
        if self.is_dir(idx) && self.expanded.contains(&idx) != open {
            self.toggle(idx);
        }
    }

    /// Opens folder `idx` and every folder inside it.
    pub fn expand_all_below(&mut self, idx: u32) {
        let Some(index) = self.index.clone() else {
            return;
        };
        if !self.is_dir(idx) {
            return;
        }
        self.expanded.insert(idx);
        self.expanded.extend(descendant_dirs(&index, idx));
        self.version += 1;
    }

    fn is_dir(&self, idx: u32) -> bool {
        self.index
            .as_ref()
            .and_then(|i| i.entry(idx))
            .is_some_and(|e| e.kind == EntryKind::Dir)
    }

    /// Carries out a menu entry's effect: actions go onto `queue`, copying and the file manager
    /// go through [`ExplorerServices`]; a copy is confirmed and a failure reported in the status
    /// bar.
    pub fn apply(&mut self, effect: MenuEffect, ctx: &egui::Context, queue: &mut Vec<Action>) {
        match effect {
            MenuEffect::Queue(action) => queue.push(action),
            MenuEffect::Fold { idx, open } => self.set_expanded(idx, open),
            MenuEffect::ExpandAll(idx) => self.expand_all_below(idx),
            MenuEffect::CollapseAll => self.collapse_all(),
            MenuEffect::Pick(path) => self.compare_pick = Some(path),
            MenuEffect::Copy(text) => {
                queue.push(Action::ShowNotice(
                    tr("explorer.copied").replace("{}", &text),
                ));
                self.services.clipboard.copy_text(ctx, text);
            }
            MenuEffect::Edit(edit) => self.edit = Some(edit),
            MenuEffect::Reveal(dir) => {
                let (errors, ctx) = (Arc::clone(&self.reveal_errors), ctx.clone());
                self.services.opener.open_folder(
                    &dir,
                    Box::new(move |result| {
                        if let Err(err) = result {
                            log::warn!("cannot show the folder in the file manager: {err}");
                            errors
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(err);
                            ctx.request_repaint();
                        }
                    }),
                );
            }
        }
    }

    /// Turns the file manager's errors reported since the last frame into status bar notices.
    fn report_reveal_errors(&self, queue: &mut Vec<Action>) {
        let errors = std::mem::take(
            &mut *self
                .reveal_errors
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for err in errors {
            queue.push(Action::ShowNotice(
                tr("explorer.reveal_failed").replace("{}", &err),
            ));
        }
    }

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
        // The cursor follows its entry by path, like the folding.
        let cursor = self
            .cursor
            .and_then(|c| self.index.as_ref()?.entry(c))
            .map(|e| e.rel_path.clone());
        self.expanded = import_expanded(index, &paths);
        self.cursor = cursor.and_then(|p| index.find(&p));
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
        if !self.is_dir(idx) {
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
    /// The project's name, when the folder is a project (PROJ-5).
    pub project: Option<String>,
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
            project: workspace.project.as_ref().map(|p| p.name.clone()),
        }
    }

    /// The index, if it has a file to show.
    fn tree(&self) -> Option<&Arc<WorkspaceIndex>> {
        self.index.as_ref().filter(|i| i.file_count() > 0)
    }

    /// The name shown in the section header: the project's name, else the folder's.
    pub fn name(&self) -> String {
        if let Some(project) = &self.project {
            return project.clone();
        }
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
    let project = crate::panels::project::ProjectInfo::read(&root);
    state.workspace = Some(Workspace {
        root,
        scan,
        watch,
        changed,
        project,
    });
    for window in &mut state.windows {
        window.explorer.reset();
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
    match &mut state.workspace {
        Some(ws) if ws.root == root => {
            ws.scan.rescan();
            // It may have become a project since it was opened (WP-91).
            ws.project = crate::panels::project::ProjectInfo::read(&root);
        }
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
        window.explorer.reset();
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
        Some(v) if v.project.is_some() && v.index.is_some() => Some(EMPTY_PROJECT_BODY_H),
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
    if view.project.is_some() {
        let response = ui
            .add(egui::Button::new(RichText::new("+").small()).frame(false))
            .on_hover_text(tr("explorer.import_tip"));
        response.widget_info(|| {
            WidgetInfo::labeled(egui::Role::Button, true, tr("explorer.import_button"))
        });
        if response.clicked() {
            queue.push(Action::Project(ProjectAction::ImportDialog {
                into: explorer.cursor_folder().unwrap_or_default(),
            }));
        }
    }
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
    /// A folder row was clicked.
    Toggle(u32),
    /// A file row was clicked.
    Open(u32, ExplorerOpen),
    /// A row was clicked: the cursor goes there and the tree takes the keyboard.
    Select(u32),
    /// An entry of a right-click menu was chosen.
    Menu(MenuEffect),
}

/// The action that opens file `idx` of the index from the explorer of `window` in the kind of
/// tab `how` asks for. Clicks, keys and the right-click menu all open files through here.
pub fn open_action(
    index: &WorkspaceIndex,
    idx: u32,
    window: WindowId,
    how: ExplorerOpen,
) -> Action {
    Action::OpenFromExplorer {
        path: index.abs_path(idx),
        window,
        how,
    }
}

/// The id of the filter box of a window's explorer.
fn filter_id(window: WindowId) -> Id {
    Id::unique(("explorer_filter", window))
}

/// The id that holds the keyboard focus of a window's tree (its empty background).
fn tree_id(window: WindowId) -> Id {
    Id::unique(("explorer_tree", window))
}

/// Whether the tree of `window`'s explorer has the keyboard (WS-10).
#[cfg(test)]
pub fn tree_has_focus(ctx: &egui::Context, window: WindowId) -> bool {
    ctx.memory(|m| m.has_focus(tree_id(window)))
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
    explorer.report_reveal_errors(queue);
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
    show_edit(ui, explorer, queue);
    if index.file_count() == 0 {
        match &view.error {
            Some(err) => hint(ui, &tr("explorer.failed").replace("{}", err)),
            None if view.project.is_some() => empty_project(ui, explorer, window, queue),
            None => hint(ui, tr("explorer.empty")),
        }
        return;
    }

    // ↓ in the filter box goes down into the list; it is taken before the box sees it.
    let filter_id = filter_id(window);
    let down = ui.memory(|m| m.has_focus(filter_id))
        && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown));
    let edit = ui.add(
        TextEdit::singleline(&mut explorer.filter)
            .id(filter_id)
            .hint_text(tr("explorer.filter_hint"))
            .desired_width(f32::INFINITY),
    );
    if std::mem::take(&mut explorer.focus_filter) {
        edit.request_focus();
    }
    // The text field gives up the keyboard on Esc by itself, which hands it back to the
    // document view. A filter is cleared first, keeping the keyboard in the box.
    if (edit.has_focus() || edit.lost_focus())
        && ui.input(|i| i.key_pressed(Key::Escape))
        && !explorer.filter.is_empty()
    {
        explorer.filter.clear();
        edit.request_focus();
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
    if down {
        explorer.cursor = prepared.rows.first().map(|r| r.idx);
        explorer.scroll_to_cursor = true;
        ui.memory_mut(|m| {
            m.request_focus(tree_id(window));
            // egui would move the focus on with the arrow itself.
            m.move_focus(FocusDirection::None);
        });
    }
    let in_project = view.project.is_some();
    show_tree(
        ui,
        &index,
        explorer,
        prepared,
        current,
        (window, in_project),
        queue,
    );
}

/// What the explorer of a project without documents shows: a line, "Import PDFs…" and a hint
/// about pasting (PROJ-1). Right-clicking the space offers the folder menu.
fn empty_project(
    ui: &mut Ui,
    explorer: &mut ExplorerState,
    window: WindowId,
    queue: &mut Vec<Action>,
) {
    ui.label(RichText::new(tr("explorer.project_empty")).weak());
    let button = ui.button(tr("explorer.import_first"));
    button
        .widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, tr("explorer.import_first")));
    if button.clicked() {
        queue.push(Action::Project(ProjectAction::ImportDialog {
            into: PathBuf::new(),
        }));
    }
    ui.add(egui::Label::new(RichText::new(tr("explorer.paste_hint")).weak().small()).wrap());
    let rest = ui.interact(
        ui.available_rect_before_wrap(),
        tree_id(window),
        Sense::click(),
    );
    let m = MenuContext {
        on: MenuOn::Empty,
        idx: None,
        path: PathBuf::new(),
        rel: PathBuf::new(),
        expanded: false,
        filtering: false,
        pick: None,
        current: None,
        window,
    };
    let mut events = Vec::new();
    rest.context_menu(|ui| menu_ui(ui, &m, true, &mut events));
    for event in events {
        if let RowEvent::Menu(effect) = event {
            explorer.apply(effect, ui.ctx(), queue);
        }
    }
}

/// The field of a new folder or a rename, above the tree, while one is being typed (PROJ-5).
/// Enter makes it, Esc gives up.
fn show_edit(ui: &mut Ui, explorer: &mut ExplorerState, queue: &mut Vec<Action>) {
    let Some(edit) = &mut explorer.edit else {
        return;
    };
    let label = match &edit.kind {
        EditKind::NewFolder { .. } => tr("explorer.new_folder_label"),
        EditKind::Rename { .. } => tr("explorer.rename_label"),
    };
    let id = Id::unique("explorer_inline_edit");
    let mut done = false;
    let mut cancel = false;
    ui.horizontal(|ui| {
        ui.label(label);
        let field = ui.add(
            TextEdit::singleline(&mut edit.text)
                .id(id)
                .desired_width(f32::INFINITY),
        );
        if std::mem::take(&mut edit.focus) {
            field.request_focus();
        }
        if field.lost_focus() {
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                done = true;
            } else {
                cancel = true;
            }
        }
    });
    if let Some(err) = &edit.error {
        ui.colored_label(ui.visuals().error_fg_color, err);
    }
    if cancel {
        explorer.edit = None;
        return;
    }
    if done {
        let target = match &edit.kind {
            EditKind::NewFolder { parent } => project::new_folder_target(parent, &edit.text),
            EditKind::Rename { rel, is_dir } => project::rename_target(rel, &edit.text, *is_dir),
        };
        match (target, &edit.kind) {
            (Ok(rel), EditKind::NewFolder { .. }) => {
                queue.push(Action::Project(ProjectAction::NewFolder { rel }));
                explorer.edit = None;
            }
            (Ok(to), EditKind::Rename { rel, .. }) => {
                if to != *rel {
                    queue.push(Action::Project(ProjectAction::Rename {
                        from: rel.clone(),
                        to,
                    }));
                }
                explorer.edit = None;
            }
            (Err(err), _) => {
                edit.error = Some(err);
                edit.focus = true;
            }
        }
    }
}

/// What every row of one frame needs besides itself.
struct RowEnv<'a> {
    index: &'a WorkspaceIndex,
    prepared: &'a Prepared,
    cursor: Option<u32>,
    tree_focused: bool,
    /// The entry of the file picked for comparing, if it is in the index.
    pick_idx: Option<u32>,
    pick: Option<&'a Path>,
    current: Option<&'a Path>,
    window: WindowId,
    /// The workspace is a project: its menu entries are listed.
    in_project: bool,
}

impl RowEnv<'_> {
    fn menu_context(&self, on: MenuOn, idx: Option<u32>) -> MenuContext<'_> {
        let entry = idx.and_then(|i| self.index.entry(i));
        MenuContext {
            on,
            idx,
            path: idx.map_or_else(|| self.index.root.clone(), |i| self.index.abs_path(i)),
            rel: entry.map(|e| e.rel_path.clone()).unwrap_or_default(),
            expanded: idx.is_some_and(|i| {
                self.prepared
                    .rows
                    .iter()
                    .any(|r| r.idx == i && r.fold == Fold::Open)
            }),
            filtering: self.prepared.filtering,
            pick: self.pick,
            current: self.current,
            window: self.window,
        }
    }
}

/// Which tree a frame draws: the index, the document of the window's focused tab, the window.
#[derive(Clone, Copy)]
struct TreeOf<'a> {
    index: &'a Arc<WorkspaceIndex>,
    current: Option<&'a Path>,
    window: WindowId,
}

/// Reads the keys of the tree while it has the keyboard and applies them (WS-10). Returns the
/// rows to draw, worked out again when a folder opened or folded.
fn handle_tree_keys(
    ui: &mut Ui,
    tree: TreeOf<'_>,
    explorer: &mut ExplorerState,
    mut prepared: Prepared,
    page: usize,
    queue: &mut Vec<Action>,
) -> Prepared {
    let TreeOf {
        index,
        current,
        window,
    } = tree;
    let tree_id = tree_id(window);
    // The tree moves with the arrows itself; egui would move the focus to another widget.
    ui.memory_mut(|m| {
        m.set_focus_lock_filter(
            tree_id,
            EventFilter {
                tab: false,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: false,
            },
        );
    });
    // Taking the keys out of the input also keeps them from the view behind (drawn later).
    let keys: Vec<TreeKey> = ui.input_mut(|i| {
        TreeKey::ALL
            .iter()
            .filter(|(key, _)| i.consume_key(Modifiers::NONE, *key))
            .map(|(_, tree_key)| *tree_key)
            .collect()
    });
    if keys.is_empty() {
        return prepared;
    }
    for key in keys {
        let step = tree_key(
            index,
            &prepared.rows,
            explorer.cursor,
            key,
            page,
            prepared.filtering,
        );
        match step {
            TreeMove::Cursor(idx) => {
                explorer.cursor = Some(idx);
                explorer.scroll_to_cursor = true;
            }
            TreeMove::Fold { idx, open } => {
                explorer.cursor = Some(idx);
                explorer.set_expanded(idx, open);
                prepared = explorer.prepare(index, current);
            }
            TreeMove::Open { idx, how } => queue.push(open_action(index, idx, window, how)),
            TreeMove::ToFilter => ui.memory_mut(|m| m.request_focus(filter_id(window))),
            TreeMove::Nothing => {}
        }
    }
    ui.memory_mut(|m| m.move_focus(FocusDirection::None));
    prepared
}

/// The rows of the tree, only those in view, with the keyboard and the right-click menus.
fn show_tree(
    ui: &mut Ui,
    index: &Arc<WorkspaceIndex>,
    explorer: &mut ExplorerState,
    prepared: Prepared,
    current: Option<&Path>,
    (window, in_project): (WindowId, bool),
    queue: &mut Vec<Action>,
) {
    let row_h = ui.text_style_height(&TextStyle::Body) + 4.0;
    let stride = row_h + ui.spacing().item_spacing.y;
    let tree_id = tree_id(window);
    let tree_focused = ui.memory(|m| m.has_focus(tree_id));
    let prepared = if tree_focused {
        let page = ((explorer.view_height / stride) as usize).saturating_sub(1);
        let tree = TreeOf {
            index,
            current,
            window,
        };
        handle_tree_keys(ui, tree, explorer, prepared, page, queue)
    } else {
        prepared
    };

    let mut events = Vec::new();
    let pick_idx = explorer
        .compare_pick
        .as_deref()
        .and_then(|p| p.strip_prefix(&index.root).ok())
        .and_then(|rel| index.find(rel));
    let env = RowEnv {
        index,
        prepared: &prepared,
        cursor: explorer.cursor,
        tree_focused,
        pick_idx,
        pick: explorer.compare_pick.as_deref(),
        current,
        window,
        in_project,
    };

    // The background of the list: clicking it gives the tree the keyboard, right-clicking it
    // opens the menu of the folder as a whole. The rows lie on top of it.
    let background = ui.interact(ui.available_rect_before_wrap(), tree_id, Sense::click());
    if background.clicked() {
        background.request_focus();
    }
    background.context_menu(|ui| {
        menu_ui(
            ui,
            &env.menu_context(MenuOn::Empty, None),
            env.in_project,
            &mut events,
        );
    });

    let mut area = ScrollArea::vertical()
        .id_salt("explorer_tree")
        .auto_shrink([false, false]);
    let cursor_row = env
        .cursor
        .and_then(|c| prepared.rows.iter().position(|r| r.idx == c));
    // The list follows the reader only when the pointer is not over it: a row being clicked
    // must not move away. A cursor moved by key is always brought into view.
    let in_panel = ui.rect_contains_pointer(ui.max_rect());
    if std::mem::take(&mut explorer.scroll_to_cursor)
        && let Some(row) = cursor_row
    {
        if let Some(offset) =
            scroll_target(row, stride, explorer.scroll_offset, explorer.view_height)
        {
            area = area.vertical_scroll_offset(offset);
        }
    } else if !in_panel
        && let Some(row) = prepared.current_row
        && let Some(offset) =
            scroll_target(row, stride, explorer.scroll_offset, explorer.view_height)
    {
        area = area.vertical_scroll_offset(offset);
    }

    let output = area.show_rows(ui, row_h, prepared.rows.len(), |ui, range| {
        for row in &prepared.rows[range] {
            show_row(ui, row_h, row, &env, &mut events);
        }
    });
    explorer.scroll_offset = output.state.clamped_offset().y;
    explorer.view_height = output.inner_rect.height();

    for event in events {
        match event {
            RowEvent::Select(idx) => {
                explorer.cursor = Some(idx);
                ui.memory_mut(|m| m.request_focus(tree_id));
            }
            RowEvent::Toggle(idx) => explorer.toggle(idx),
            RowEvent::Open(idx, how) => queue.push(open_action(index, idx, window, how)),
            RowEvent::Menu(effect) => explorer.apply(effect, ui.ctx(), queue),
        }
    }
}

/// The entries of a right-click menu for `m` (with the project's when `in_project`); a chosen
/// one becomes an event.
fn menu_ui(ui: &mut Ui, m: &MenuContext<'_>, in_project: bool, events: &mut Vec<RowEvent>) {
    for entry in menu_entries_for(m.on, in_project) {
        if entry.separator_before {
            ui.separator();
        }
        let button = egui::Button::new(tr((entry.name)(m)));
        if ui.add_enabled((entry.enabled)(m), button).clicked() {
            events.extend((entry.effect)(m).map(RowEvent::Menu));
            ui.close();
        }
    }
}

/// Draws one row: the fold triangle of a folder, the name (one line, shortened with "…" when
/// too long; the relative path in a tooltip), the keyboard cursor and the comparison pick, and
/// turns its clicks into events. A click on a folder folds it; on a file it opens the file in
/// the kind of tab [`explorer_click`] decides. The row is a button with its name (and, for a
/// folder, whether it is open) for the accessibility tree.
fn show_row(ui: &mut Ui, row_h: f32, row: &Row, env: &RowEnv<'_>, events: &mut Vec<RowEvent>) {
    let Some(entry) = env.index.entry(row.idx) else {
        return;
    };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::hover());
    let indent = f32::from(row.depth.min(MAX_INDENT_DEPTH)) * INDENT;
    let fold_rect =
        Rect::from_min_size(rect.min + egui::vec2(indent, 0.0), egui::vec2(row_h, row_h));
    let picked = env.pick_idx == Some(row.idx);
    let mark_w = if picked { 14.0 } else { 0.0 };
    let name_rect = Rect::from_min_max(
        Pos2::new(fold_rect.right(), rect.top()),
        Pos2::new(rect.right() - mark_w, rect.bottom()),
    );
    // Clicks only: the tree as a whole holds the keyboard, not each row.
    let response = ui.interact(
        rect,
        ui.make_persistent_id(("explorer_row", row.idx)),
        Sense::CLICK,
    );
    response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, &entry.name));
    if row.fold != Fold::Leaf {
        ui.ctx().accesskit_node_builder(response.id, |node| {
            node.set_expanded(row.fold == Fold::Open);
        });
    }

    let is_current = env.prepared.current == Some(row.idx);
    let visuals = ui.visuals().clone();
    let painter = ui.painter();
    if is_current {
        painter.rect_filled(rect, 2.0, visuals.selection.bg_fill);
    } else if response.hovered() {
        painter.rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
    }
    // The keyboard cursor is an outline, unlike the filled current document.
    if env.tree_focused && env.cursor == Some(row.idx) {
        painter.rect_stroke(
            rect.shrink(0.5),
            2.0,
            egui::Stroke::new(1.0, visuals.strong_text_color()),
            StrokeKind::Inside,
        );
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
    if picked {
        let center = Pos2::new(rect.right() - mark_w / 2.0, rect.center().y);
        painter.circle_filled(center, 3.5, visuals.warn_fg_color);
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
        if picked {
            ui.weak(tr("explorer.picked"));
        }
    });
    let (clicked, middle) = (response.clicked(), response.middle_clicked());
    if clicked || middle || response.secondary_clicked() {
        events.push(RowEvent::Select(row.idx));
    }
    match entry.kind {
        EntryKind::Dir if clicked && !env.prepared.filtering => {
            events.push(RowEvent::Toggle(row.idx));
        }
        EntryKind::Pdf if clicked || middle => {
            let ctrl = ui.input(|i| i.modifiers.ctrl);
            let how = explorer_click(response.double_clicked(), middle, ctrl);
            events.push(RowEvent::Open(row.idx, how));
        }
        _ => {}
    }
    response.context_menu(|ui| {
        let on = match entry.kind {
            EntryKind::Dir => MenuOn::Folder,
            EntryKind::Pdf => MenuOn::File,
        };
        menu_ui(
            ui,
            &env.menu_context(on, Some(row.idx)),
            env.in_project,
            events,
        );
    });
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
                project: None,
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
    fn find_file_row(
        ctx: &egui::Context,
        state: &mut AppState,
    ) -> (Pos2, PathBuf, WindowId, ExplorerOpen) {
        for y in (28..300).step_by(4) {
            let pos = Pos2::new(100.0, y as f32);
            for action in click_at(ctx, state, pos) {
                if let Action::OpenFromExplorer { path, window, how } = action {
                    return (pos, path, window, how);
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
        let (pos, path, window, how) = find_file_row(&ctx, &mut state);
        assert_eq!(path, a, "the first file row");
        assert_eq!(window, WindowId::MAIN);
        assert_eq!(how, ExplorerOpen::Preview, "a single click");
        dispatch(
            &mut state,
            Action::OpenFromExplorer {
                path: path.clone(),
                window,
                how,
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

    // --- WP-72: keyboard, right-click menu, accessibility ------------------------------------

    /// sample() with "specs" open: guides, specs, net, intro.pdf, a.pdf, B.pdf.
    fn sample_rows() -> (Arc<WorkspaceIndex>, Vec<Row>) {
        let index = sample();
        let expanded: HashSet<u32> = [idx(&index, "specs")].into();
        let rows = visible_rows(&index, &expanded);
        assert_eq!(
            names(&index, &rows),
            ["guides", "specs", "net", "intro.pdf", "a.pdf", "B.pdf"]
        );
        (index, rows)
    }

    #[test]
    fn arrow_keys_move_the_cursor_and_stop_at_the_ends() {
        let (index, rows) = sample_rows();
        let key = |cursor: Option<&str>, k| {
            tree_key(&index, &rows, cursor.map(|c| idx(&index, c)), k, 2, false)
        };
        let to = |rel: &str| TreeMove::Cursor(idx(&index, rel));
        // Without a cursor the first key lands on the first row, End on the last.
        assert_eq!(key(None, TreeKey::Down), to("guides"));
        assert_eq!(key(None, TreeKey::Enter), to("guides"));
        assert_eq!(key(None, TreeKey::End), to("B.pdf"));
        assert_eq!(key(Some("guides"), TreeKey::Down), to("specs"));
        assert_eq!(key(Some("specs"), TreeKey::Up), to("guides"));
        assert_eq!(key(Some("B.pdf"), TreeKey::Down), to("B.pdf"));
        // ↑ on the first row goes back to the filter box.
        assert_eq!(key(Some("guides"), TreeKey::Up), TreeMove::ToFilter);
        assert_eq!(key(Some("a.pdf"), TreeKey::Home), to("guides"));
        assert_eq!(key(Some("guides"), TreeKey::End), to("B.pdf"));
        assert_eq!(key(Some("guides"), TreeKey::PageDown), to("specs/net"));
        assert_eq!(key(Some("B.pdf"), TreeKey::PageUp), to("specs/intro.pdf"));
        assert_eq!(key(Some("specs"), TreeKey::PageUp), to("guides"));
        assert_eq!(key(Some("a.pdf"), TreeKey::PageDown), to("B.pdf"));
        // A cursor on an entry that is not shown counts as none.
        assert_eq!(key(Some("guides/deep"), TreeKey::Down), to("guides"));
        assert_eq!(
            tree_key(&index, &[], None, TreeKey::Down, 2, false),
            TreeMove::Nothing
        );
    }

    #[test]
    fn right_and_left_open_fold_and_walk_the_tree() {
        let (index, rows) = sample_rows();
        let key = |cursor: &str, k| tree_key(&index, &rows, Some(idx(&index, cursor)), k, 2, false);
        let i = |rel: &str| idx(&index, rel);
        // → opens a folded folder, then goes to its first child; nothing on a file.
        assert_eq!(
            key("guides", TreeKey::Right),
            TreeMove::Fold {
                idx: i("guides"),
                open: true
            }
        );
        assert_eq!(
            key("specs", TreeKey::Right),
            TreeMove::Cursor(i("specs/net"))
        );
        assert_eq!(key("a.pdf", TreeKey::Right), TreeMove::Nothing);
        // ← folds an open folder, else goes to the parent; nothing at the top level.
        assert_eq!(
            key("specs", TreeKey::Left),
            TreeMove::Fold {
                idx: i("specs"),
                open: false
            }
        );
        assert_eq!(
            key("specs/net", TreeKey::Left),
            TreeMove::Cursor(i("specs"))
        );
        assert_eq!(
            key("specs/intro.pdf", TreeKey::Left),
            TreeMove::Cursor(i("specs"))
        );
        assert_eq!(key("guides", TreeKey::Left), TreeMove::Nothing);
        assert_eq!(key("a.pdf", TreeKey::Left), TreeMove::Nothing);
    }

    #[test]
    fn enter_opens_an_ordinary_tab_space_a_preview_and_both_fold_folders() {
        let (index, rows) = sample_rows();
        let key = |cursor: &str, k| tree_key(&index, &rows, Some(idx(&index, cursor)), k, 2, false);
        let i = |rel: &str| idx(&index, rel);
        assert_eq!(
            key("a.pdf", TreeKey::Enter),
            TreeMove::Open {
                idx: i("a.pdf"),
                how: ExplorerOpen::Pinned
            }
        );
        assert_eq!(
            key("a.pdf", TreeKey::Space),
            TreeMove::Open {
                idx: i("a.pdf"),
                how: ExplorerOpen::Preview
            }
        );
        for k in [TreeKey::Enter, TreeKey::Space] {
            assert_eq!(
                key("specs", k),
                TreeMove::Fold {
                    idx: i("specs"),
                    open: false
                }
            );
            assert_eq!(
                key("guides", k),
                TreeMove::Fold {
                    idx: i("guides"),
                    open: true
                }
            );
        }
    }

    #[test]
    fn while_filtering_folders_do_not_fold_and_the_arrows_only_move() {
        let index = sample();
        let rows = filtered_rows(&index, &folded_names(&index), "tcp");
        assert_eq!(names(&index, &rows), ["specs", "net", "tcp.pdf"]);
        let key = |cursor: &str, k| tree_key(&index, &rows, Some(idx(&index, cursor)), k, 2, true);
        assert_eq!(key("specs", TreeKey::Left), TreeMove::Nothing);
        assert_eq!(key("specs", TreeKey::Enter), TreeMove::Nothing);
        assert_eq!(
            key("specs", TreeKey::Right),
            TreeMove::Cursor(idx(&index, "specs/net"))
        );
        assert_eq!(
            key("specs/net", TreeKey::Left),
            TreeMove::Cursor(idx(&index, "specs"))
        );
        assert_eq!(
            key("specs/net/tcp.pdf", TreeKey::Enter),
            TreeMove::Open {
                idx: idx(&index, "specs/net/tcp.pdf"),
                how: ExplorerOpen::Pinned
            }
        );
    }

    #[test]
    fn expand_all_below_opens_every_folder_inside() {
        let index = sample();
        assert_eq!(
            descendant_dirs(&index, idx(&index, "guides")),
            [
                idx(&index, "guides/deep"),
                idx(&index, "guides/deep/deeper")
            ]
        );
        assert!(descendant_dirs(&index, idx(&index, "a.pdf")).is_empty());
        let mut state = ExplorerState::default();
        state.prepare(&index, None);
        state.expand_all_below(idx(&index, "guides"));
        assert_eq!(
            names(&index, &state.prepare(&index, None).rows)[..5],
            ["guides", "deep", "deeper", "x.pdf", "specs"]
        );
        assert!(!state.is_expanded(idx(&index, "specs")));
        state.set_expanded(idx(&index, "guides"), false);
        assert_eq!(state.prepare(&index, None).rows.len(), 4);
    }

    /// A clipboard that records what it was given.
    #[derive(Default)]
    struct FakeClipboard(Mutex<Vec<String>>);

    impl TextClipboard for FakeClipboard {
        fn copy_text(&self, _ctx: &egui::Context, text: String) {
            self.0.lock().expect("lock").push(text);
        }
    }

    /// A file manager that records the folders it was asked to show, and fails when told to.
    #[derive(Default)]
    struct FakeOpener {
        opened: Mutex<Vec<PathBuf>>,
        fail: Option<String>,
    }

    impl FolderOpener for FakeOpener {
        fn open_folder(&self, dir: &Path, done: OpenDone) {
            self.opened.lock().expect("lock").push(dir.to_path_buf());
            done(self.fail.clone().map_or(Ok(()), Err));
        }
    }

    fn fake_services(
        fail: Option<&str>,
    ) -> (Arc<FakeClipboard>, Arc<FakeOpener>, ExplorerServices) {
        let clipboard = Arc::new(FakeClipboard::default());
        let opener = Arc::new(FakeOpener {
            opened: Mutex::default(),
            fail: fail.map(str::to_owned),
        });
        let services = ExplorerServices {
            clipboard: Arc::clone(&clipboard) as Arc<dyn TextClipboard>,
            opener: Arc::clone(&opener) as Arc<dyn FolderOpener>,
        };
        (clipboard, opener, services)
    }

    fn menu_context<'a>(
        index: &WorkspaceIndex,
        on: MenuOn,
        rel: Option<&str>,
        pick: Option<&'a Path>,
        current: Option<&'a Path>,
    ) -> MenuContext<'a> {
        let i = rel.map(|r| idx(index, r));
        MenuContext {
            on,
            idx: i,
            path: i.map_or_else(|| index.root.clone(), |i| index.abs_path(i)),
            rel: rel.map(PathBuf::from).unwrap_or_default(),
            expanded: false,
            filtering: false,
            pick,
            current,
            window: WindowId::MAIN,
        }
    }

    fn menu_rows(m: &MenuContext<'_>) -> Vec<(&'static str, bool)> {
        menu_entries(m.on)
            .map(|e| ((e.name)(m), (e.enabled)(m)))
            .collect()
    }

    #[test]
    fn each_kind_of_click_has_its_menu_and_every_entry_has_its_texts() {
        let index = sample();
        let file = menu_context(&index, MenuOn::File, Some("a.pdf"), None, None);
        assert_eq!(
            menu_rows(&file),
            [
                ("explorer.menu.open", true),
                ("explorer.menu.open_background", true),
                ("explorer.menu.open_split", true),
                ("explorer.menu.open_window", true),
                ("explorer.menu.pick_compare", true),
                ("explorer.menu.compare_picked", false),
                ("explorer.menu.compare_current", false),
                ("explorer.menu.copy_path", true),
                ("explorer.menu.copy_relative", true),
                ("explorer.menu.reveal", true),
            ]
        );
        let mut folder = menu_context(&index, MenuOn::Folder, Some("specs"), None, None);
        assert_eq!(
            menu_rows(&folder),
            [
                ("explorer.menu.expand", true),
                ("explorer.menu.expand_all", true),
                ("explorer.menu.copy_path", true),
                ("explorer.menu.reveal", true),
            ]
        );
        folder.expanded = true;
        assert_eq!(menu_rows(&folder)[0], ("explorer.menu.collapse", true));
        folder.filtering = true;
        assert_eq!(
            menu_rows(&folder)[..2],
            [
                ("explorer.menu.collapse", false),
                ("explorer.menu.expand_all", false)
            ]
        );
        let empty = menu_context(&index, MenuOn::Empty, None, None, None);
        assert_eq!(
            menu_rows(&empty),
            [
                ("explorer.refresh", true),
                ("explorer.collapse_all", true),
                ("cmd.file.close_folder", true),
            ]
        );
        for m in [&file, &folder, &empty] {
            for e in menu_entries(m.on) {
                for lang in [crate::i18n::Lang::Ko, crate::i18n::Lang::En] {
                    assert!(crate::i18n::lookup(lang, (e.name)(m)).is_some());
                }
            }
        }
        let separators: Vec<bool> = menu_entries(MenuOn::File)
            .map(|e| e.separator_before)
            .collect();
        assert_eq!(
            separators,
            [
                false, false, false, false, true, false, false, true, false, false
            ]
        );
    }

    fn effect_of(m: &MenuContext<'_>, name: &str) -> Option<MenuEffect> {
        menu_entries(m.on)
            .find(|e| (e.name)(m) == name)
            .and_then(|e| (e.effect)(m))
    }

    #[test]
    fn the_file_menu_opens_in_four_ways_and_copies_both_paths() {
        let index = sample();
        let m = menu_context(&index, MenuOn::File, Some("specs/net/tcp.pdf"), None, None);
        let path = PathBuf::from("/ws/specs/net/tcp.pdf");
        for (name, how) in [
            ("explorer.menu.open", ExplorerOpen::Pinned),
            ("explorer.menu.open_background", ExplorerOpen::Background),
            ("explorer.menu.open_split", ExplorerOpen::SplitSide),
            ("explorer.menu.open_window", ExplorerOpen::NewWindow),
        ] {
            assert_eq!(
                effect_of(&m, name),
                Some(MenuEffect::Queue(Action::OpenFromExplorer {
                    path: path.clone(),
                    window: WindowId::MAIN,
                    how
                })),
                "{name}"
            );
        }
        assert_eq!(
            effect_of(&m, "explorer.menu.copy_path"),
            Some(MenuEffect::Copy("/ws/specs/net/tcp.pdf".into()))
        );
        assert_eq!(
            effect_of(&m, "explorer.menu.copy_relative"),
            Some(MenuEffect::Copy("specs/net/tcp.pdf".into()))
        );
        assert_eq!(
            effect_of(&m, "explorer.menu.reveal"),
            Some(MenuEffect::Reveal(PathBuf::from("/ws/specs/net")))
        );
        let folder = menu_context(&index, MenuOn::Folder, Some("specs"), None, None);
        assert_eq!(
            effect_of(&folder, "explorer.menu.reveal"),
            Some(MenuEffect::Reveal(PathBuf::from("/ws/specs")))
        );
        assert_eq!(
            effect_of(&folder, "explorer.menu.expand"),
            Some(MenuEffect::Fold {
                idx: idx(&index, "specs"),
                open: true
            })
        );
        let empty = menu_context(&index, MenuOn::Empty, None, None, None);
        assert_eq!(
            effect_of(&empty, "cmd.file.close_folder"),
            Some(MenuEffect::Queue(Action::CloseFolder))
        );
    }

    #[test]
    fn a_picked_file_is_compared_as_the_older_side_and_the_current_document_too() {
        let index = sample();
        let tcp = PathBuf::from("/ws/specs/net/tcp.pdf");
        let udp = PathBuf::from("/ws/specs/net/udp.pdf");
        let reading = PathBuf::from("/elsewhere/reading.pdf");
        let picking = menu_context(&index, MenuOn::File, Some("specs/net/tcp.pdf"), None, None);
        assert_eq!(
            effect_of(&picking, "explorer.menu.pick_compare"),
            Some(MenuEffect::Pick(tcp.clone()))
        );
        // The picked file itself cannot be compared with itself.
        let same = menu_context(
            &index,
            MenuOn::File,
            Some("specs/net/tcp.pdf"),
            Some(&tcp),
            Some(&tcp),
        );
        assert!(!menu_rows(&same)[5].1 && !menu_rows(&same)[6].1);
        let other = menu_context(
            &index,
            MenuOn::File,
            Some("specs/net/udp.pdf"),
            Some(&tcp),
            Some(&reading),
        );
        assert!(menu_rows(&other)[5].1 && menu_rows(&other)[6].1);
        assert_eq!(
            effect_of(&other, "explorer.menu.compare_picked"),
            Some(MenuEffect::Queue(Action::StartDiff {
                left: tcp.clone(),
                right: udp.clone()
            }))
        );
        assert_eq!(
            effect_of(&other, "explorer.menu.compare_current"),
            Some(MenuEffect::Queue(Action::StartDiff {
                left: reading,
                right: udp
            }))
        );
    }

    #[test]
    fn copying_goes_to_the_clipboard_and_showing_to_the_file_manager() {
        let ctx = egui::Context::default();
        let (clipboard, opener, services) = fake_services(None);
        let mut explorer = ExplorerState {
            services,
            ..Default::default()
        };
        let mut queue = Vec::new();
        explorer.apply(MenuEffect::Copy("/ws/a.pdf".into()), &ctx, &mut queue);
        assert_eq!(*clipboard.0.lock().expect("lock"), ["/ws/a.pdf"]);
        assert!(matches!(&queue[..], [Action::ShowNotice(text)] if text.contains("/ws/a.pdf")));
        queue.clear();
        explorer.apply(
            MenuEffect::Reveal(PathBuf::from("/ws/specs")),
            &ctx,
            &mut queue,
        );
        assert_eq!(
            *opener.opened.lock().expect("lock"),
            [PathBuf::from("/ws/specs")]
        );
        explorer.report_reveal_errors(&mut queue);
        assert!(queue.is_empty(), "no failure, no notice");
        explorer.apply(
            MenuEffect::Pick(PathBuf::from("/ws/a.pdf")),
            &ctx,
            &mut queue,
        );
        assert_eq!(explorer.compare_pick(), Some(Path::new("/ws/a.pdf")));
    }

    #[test]
    fn a_file_manager_that_fails_is_reported_in_the_status_bar() {
        let ctx = egui::Context::default();
        let (_, opener, services) = fake_services(Some("no xdg-open"));
        let mut explorer = ExplorerState {
            services,
            ..Default::default()
        };
        let mut queue = Vec::new();
        explorer.apply(MenuEffect::Reveal(PathBuf::from("/ws")), &ctx, &mut queue);
        assert_eq!(opener.opened.lock().expect("lock").len(), 1);
        explorer.report_reveal_errors(&mut queue);
        assert!(
            matches!(&queue[..], [Action::ShowNotice(text)] if text.contains("no xdg-open")),
            "{queue:?}"
        );
        // Reported once.
        queue.clear();
        explorer.report_reveal_errors(&mut queue);
        assert!(queue.is_empty());
        // The notice reaches the status bar through dispatch.
        let mut state = AppState::new();
        dispatch(&mut state, Action::ShowNotice("hello".into()), &ctx);
        assert_eq!(
            state.status_notice.as_ref().map(|n| n.text.as_str()),
            Some("hello")
        );
    }

    #[test]
    fn the_pick_is_used_up_by_a_comparison_and_dropped_with_the_folder() {
        let first = TempDir::new("pick-first");
        first.file("a.pdf");
        let second = TempDir::new("pick-second");
        second.file("b.pdf");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        let (clipboard, _, services) = fake_services(None);
        state.windows[0].explorer.services = services;
        open_folder_and_wait(&ctx, &repaints, &mut state, &first.0, 1);
        let pick = |state: &mut AppState| {
            state.windows[0].explorer.apply(
                MenuEffect::Pick(PathBuf::from("/refdesk-test/wp72-old.pdf")),
                &egui::Context::default(),
                &mut Vec::new(),
            );
            assert!(state.windows[0].explorer.compare_pick().is_some());
        };
        pick(&mut state);
        dispatch(
            &mut state,
            Action::StartDiff {
                left: PathBuf::from("/refdesk-test/wp72-old.pdf"),
                right: PathBuf::from("/refdesk-test/wp72-new.pdf"),
            },
            &ctx,
        );
        assert_eq!(state.windows[0].explorer.compare_pick(), None);
        pick(&mut state);
        open_folder_and_wait(&ctx, &repaints, &mut state, &second.0, 1);
        assert_eq!(state.windows[0].explorer.compare_pick(), None);
        // The injected services survive the new folder.
        state.windows[0]
            .explorer
            .apply(MenuEffect::Copy("x".into()), &ctx, &mut Vec::new());
        assert_eq!(*clipboard.0.lock().expect("lock"), ["x"]);
    }

    // --- through drawn frames --------------------------------------------------------------

    /// A frame at a given time (so double clicks are told apart from two clicks) with modifiers.
    fn frame_at(
        ctx: &egui::Context,
        state: &mut AppState,
        events: Vec<egui::Event>,
        time: f64,
        modifiers: egui::Modifiers,
    ) -> Vec<Action> {
        let mut queue = Vec::new();
        let mut all = vec![egui::Event::ModifiersChanged(modifiers)];
        all.extend(events);
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1000.0, 700.0))),
            time: Some(time),
            events: all,
            ..Default::default()
        };
        ctx.run_ui(input, |ui| show_window(ui, state, 0, &mut queue))
            .drop_without_applying_deltas();
        queue
    }

    /// One labeled button of the accessibility tree.
    #[derive(Debug)]
    struct Node {
        label: String,
        rect: Rect,
        expanded: Option<bool>,
    }

    /// The labeled buttons of one drawn frame of the main window.
    fn buttons(ctx: &egui::Context, state: &mut AppState, time: f64) -> Vec<Node> {
        ctx.enable_accesskit();
        let mut queue = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1000.0, 700.0))),
            time: Some(time),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| show_window(ui, state, 0, &mut queue));
        let update = output.platform_output.accesskit_update.clone();
        output.drop_without_applying_deltas();
        update
            .map(|u| u.nodes)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, n)| n.role() == egui::accesskit::Role::Button)
            .filter_map(|(_, n)| {
                let b = n.bounds()?;
                Some(Node {
                    label: n.label()?.to_owned(),
                    rect: Rect::from_min_max(
                        Pos2::new(b.x0 as f32, b.y0 as f32),
                        Pos2::new(b.x1 as f32, b.y1 as f32),
                    ),
                    expanded: n.is_expanded(),
                })
            })
            .collect()
    }

    fn row_center(nodes: &[Node], label: &str) -> Pos2 {
        nodes
            .iter()
            .find(|n| n.label == label)
            .unwrap_or_else(|| panic!("no row {label} in {nodes:?}"))
            .rect
            .center()
    }

    fn press(button: egui::PointerButton, pos: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// A click at `pos` over three frames from `time`; returns the queued actions.
    fn click_kind(
        ctx: &egui::Context,
        state: &mut AppState,
        pos: Pos2,
        button: egui::PointerButton,
        modifiers: egui::Modifiers,
        time: f64,
    ) -> Vec<Action> {
        let mut queue = frame_at(
            ctx,
            state,
            vec![egui::Event::PointerMoved(pos)],
            time,
            modifiers,
        );
        queue.extend(frame_at(
            ctx,
            state,
            vec![press(button, pos, true)],
            time + 0.02,
            modifiers,
        ));
        queue.extend(frame_at(
            ctx,
            state,
            vec![press(button, pos, false)],
            time + 0.04,
            modifiers,
        ));
        queue
    }

    fn opened(queue: &[Action]) -> Vec<(String, ExplorerOpen)> {
        queue
            .iter()
            .filter_map(|a| match a {
                Action::OpenFromExplorer { path, how, .. } => {
                    Some((path.file_name()?.to_string_lossy().into_owned(), *how))
                }
                _ => None,
            })
            .collect()
    }

    /// A workspace with sub/c.pdf, a.pdf, b.pdf, scanned and drawn.
    fn drawn_workspace(name: &str) -> (TempDir, egui::Context, AppState) {
        let dir = TempDir::new(name);
        dir.file("a.pdf");
        dir.file("b.pdf");
        dir.file("sub/c.pdf");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        open_folder_and_wait(&ctx, &repaints, &mut state, &dir.0, 3);
        for i in 0..3 {
            frame_at(
                &ctx,
                &mut state,
                Vec::new(),
                i as f64 * 0.02,
                egui::Modifiers::NONE,
            );
        }
        (dir, ctx, state)
    }

    #[test]
    fn single_double_middle_and_ctrl_clicks_open_the_right_kind_of_tab() {
        use egui::PointerButton::{Middle, Primary};
        let (_dir, ctx, mut state) = drawn_workspace("clicks");
        let nodes = buttons(&ctx, &mut state, 1.0);
        let a = row_center(&nodes, "a.pdf");
        let none = egui::Modifiers::NONE;
        let q = click_kind(&ctx, &mut state, a, Primary, none, 10.0);
        assert_eq!(opened(&q), [("a.pdf".into(), ExplorerOpen::Preview)]);
        // The second click of a double click.
        let mut q = frame_at(&ctx, &mut state, vec![press(Primary, a, true)], 10.10, none);
        q.extend(frame_at(
            &ctx,
            &mut state,
            vec![press(Primary, a, false)],
            10.12,
            none,
        ));
        assert_eq!(opened(&q), [("a.pdf".into(), ExplorerOpen::Pinned)]);
        let q = click_kind(&ctx, &mut state, a, Middle, none, 20.0);
        assert_eq!(opened(&q), [("a.pdf".into(), ExplorerOpen::Background)]);
        let q = click_kind(&ctx, &mut state, a, Primary, egui::Modifiers::CTRL, 30.0);
        assert_eq!(opened(&q), [("a.pdf".into(), ExplorerOpen::Background)]);
        // A folder only folds.
        let sub = row_center(&nodes, "sub");
        let q = click_kind(&ctx, &mut state, sub, Primary, none, 40.0);
        assert!(opened(&q).is_empty());
        let index = index_now(&state).expect("index");
        assert!(state.windows[0].explorer.is_expanded(idx(&index, "sub")));
    }

    fn key_at(ctx: &egui::Context, state: &mut AppState, k: Key, time: f64) -> Vec<Action> {
        frame_at(ctx, state, key(k), time, egui::Modifiers::NONE)
    }

    #[test]
    fn the_keyboard_walks_the_tree_and_opens_files() {
        let (_dir, ctx, mut state) = drawn_workspace("keys");
        let index = index_now(&state).expect("index");
        let i = |rel: &str| idx(&index, rel);
        let nodes = buttons(&ctx, &mut state, 1.0);
        // Clicking a row gives the tree the keyboard and puts the cursor there.
        click_kind(
            &ctx,
            &mut state,
            row_center(&nodes, "a.pdf"),
            egui::PointerButton::Primary,
            egui::Modifiers::NONE,
            10.0,
        );
        frame_at(&ctx, &mut state, Vec::new(), 10.1, egui::Modifiers::NONE);
        assert!(tree_has_focus(&ctx, WindowId::MAIN));
        assert_eq!(state.windows[0].explorer.cursor(), Some(i("a.pdf")));
        let mut t = 11.0;
        let mut step = |state: &mut AppState, k: Key| {
            t += 1.0;
            key_at(&ctx, state, k, t)
        };
        assert!(step(&mut state, Key::ArrowDown).is_empty());
        assert_eq!(state.windows[0].explorer.cursor(), Some(i("b.pdf")));
        assert_eq!(
            opened(&step(&mut state, Key::Enter)),
            [("b.pdf".into(), ExplorerOpen::Pinned)]
        );
        assert_eq!(
            opened(&step(&mut state, Key::Space)),
            [("b.pdf".into(), ExplorerOpen::Preview)]
        );
        step(&mut state, Key::Home);
        assert_eq!(state.windows[0].explorer.cursor(), Some(i("sub")));
        step(&mut state, Key::ArrowRight);
        assert!(state.windows[0].explorer.is_expanded(i("sub")));
        step(&mut state, Key::ArrowRight);
        assert_eq!(state.windows[0].explorer.cursor(), Some(i("sub/c.pdf")));
        step(&mut state, Key::ArrowLeft);
        assert_eq!(state.windows[0].explorer.cursor(), Some(i("sub")));
        step(&mut state, Key::ArrowLeft);
        assert!(!state.windows[0].explorer.is_expanded(i("sub")));
        // Enter on a folder opens it and opens no file.
        assert!(opened(&step(&mut state, Key::Enter)).is_empty());
        assert!(state.windows[0].explorer.is_expanded(i("sub")));
        step(&mut state, Key::End);
        assert_eq!(state.windows[0].explorer.cursor(), Some(i("b.pdf")));
        step(&mut state, Key::PageUp);
        assert_eq!(state.windows[0].explorer.cursor(), Some(i("sub")));
        step(&mut state, Key::PageDown);
        assert_eq!(state.windows[0].explorer.cursor(), Some(i("b.pdf")));
        assert!(
            tree_has_focus(&ctx, WindowId::MAIN),
            "the arrows kept the keyboard"
        );
        // ↑ on the first row: back to the filter box; ↓ there: down to the first row.
        step(&mut state, Key::Home);
        step(&mut state, Key::ArrowUp);
        step(&mut state, Key::F1);
        let filter_focused =
            |ctx: &egui::Context| ctx.memory(|m| m.has_focus(filter_id(WindowId::MAIN)));
        assert!(filter_focused(&ctx));
        assert!(!tree_has_focus(&ctx, WindowId::MAIN));
        step(&mut state, Key::End);
        step(&mut state, Key::ArrowDown);
        step(&mut state, Key::F1);
        assert!(tree_has_focus(&ctx, WindowId::MAIN));
        assert_eq!(state.windows[0].explorer.cursor(), Some(i("sub")));
        // Esc lets go of the tree.
        step(&mut state, Key::Escape);
        step(&mut state, Key::F1);
        assert!(!tree_has_focus(&ctx, WindowId::MAIN));
    }

    #[test]
    fn escape_clears_the_filter_first_and_then_leaves_the_box() {
        let (_dir, ctx, mut state) = drawn_workspace("escape");
        dispatch(&mut state, Action::ShowExplorer, &ctx);
        let none = egui::Modifiers::NONE;
        frame_at(&ctx, &mut state, Vec::new(), 1.0, none);
        frame_at(
            &ctx,
            &mut state,
            vec![egui::Event::Text("b".into())],
            1.1,
            none,
        );
        assert_eq!(state.windows[0].explorer.filter, "b");
        let filter_focused =
            |ctx: &egui::Context| ctx.memory(|m| m.has_focus(filter_id(WindowId::MAIN)));
        key_at(&ctx, &mut state, Key::Escape, 2.0);
        frame_at(&ctx, &mut state, Vec::new(), 2.1, none);
        assert_eq!(state.windows[0].explorer.filter, "");
        assert!(filter_focused(&ctx), "the box keeps the keyboard");
        key_at(&ctx, &mut state, Key::Escape, 3.0);
        frame_at(&ctx, &mut state, Vec::new(), 3.1, none);
        assert!(!filter_focused(&ctx), "back to the document view");
        assert!(!ctx.text_edit_focused());
    }

    #[test]
    fn the_view_does_not_scroll_while_the_explorer_has_the_keyboard() {
        let dir = TempDir::new("noscroll");
        let mut builder = refdesk_core::testutil::PdfBuilder::new();
        for page in 0..6 {
            builder = builder.page(400.0, 600.0, |p| {
                p.text(72.0, 100.0, 12.0, &format!("page {page}"));
            });
        }
        let doc_path = dir.0.join("long.pdf");
        std::fs::write(&doc_path, builder.build()).expect("write pdf");
        dir.file("other.pdf");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        open_folder_and_wait(&ctx, &repaints, &mut state, &dir.0, 2);
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget {
                path: doc_path.clone(),
                page: None,
            }]),
            &ctx,
        );
        let tab = state.focus.expect("focus");
        let doc = state
            .docs
            .get(state.tabs.pdf(tab).expect("pdf").doc)
            .expect("doc")
            .clone();
        wait_for(&ctx, &repaints, "the document", || doc.info().is_some());
        let none = egui::Modifiers::NONE;
        for i in 0..4 {
            frame_at(&ctx, &mut state, Vec::new(), i as f64 * 0.02, none);
        }
        let anchor = |state: &AppState| state.tabs.pdf(tab).expect("pdf").view.anchor;
        let start = anchor(&state);
        let nodes = buttons(&ctx, &mut state, 1.0);
        click_kind(
            &ctx,
            &mut state,
            row_center(&nodes, "other.pdf"),
            egui::PointerButton::Primary,
            none,
            10.0,
        );
        // The pointer rests over the view, which would scroll with the keys if it saw them.
        frame_at(
            &ctx,
            &mut state,
            vec![egui::Event::PointerMoved(Pos2::new(700.0, 400.0))],
            10.1,
            none,
        );
        assert!(tree_has_focus(&ctx, WindowId::MAIN));
        for (n, k) in [Key::PageDown, Key::ArrowDown, Key::End, Key::Space]
            .into_iter()
            .enumerate()
        {
            key_at(&ctx, &mut state, k, 11.0 + n as f64);
        }
        assert_eq!(anchor(&state), start, "the view stayed");
        // Without the explorer's keyboard the same key scrolls the view.
        key_at(&ctx, &mut state, Key::Escape, 20.0);
        key_at(&ctx, &mut state, Key::End, 21.0);
        frame_at(&ctx, &mut state, Vec::new(), 21.1, none);
        assert_ne!(
            anchor(&state),
            start,
            "the view scrolls when the explorer lets go"
        );
    }

    #[test]
    fn file_and_outline_rows_are_named_buttons_in_the_accessibility_tree() {
        let dir = TempDir::new("a11y");
        let pdf = refdesk_core::testutil::PdfBuilder::new()
            .page(400.0, 600.0, |p| {
                p.text(72.0, 100.0, 18.0, "1 Introduction");
            })
            .page(400.0, 600.0, |p| {
                p.text(72.0, 100.0, 18.0, "2 Design");
            })
            .outline("1 Introduction", 1, 100.0, |o| {
                o.child("1.1 Scope", 1, 300.0);
            })
            .outline_leaf("2 Design", 2, 100.0)
            .build();
        let doc_path = dir.0.join("guide.pdf");
        std::fs::write(&doc_path, pdf).expect("write pdf");
        dir.file("specs/net.pdf");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        open_folder_and_wait(&ctx, &repaints, &mut state, &dir.0, 2);
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget {
                path: doc_path,
                page: None,
            }]),
            &ctx,
        );
        let tab = state.focus.expect("focus");
        let doc = state
            .docs
            .get(state.tabs.pdf(tab).expect("pdf").doc)
            .expect("doc")
            .clone();
        wait_for(&ctx, &repaints, "the document", || doc.info().is_some());
        for i in 0..3 {
            frame_at(
                &ctx,
                &mut state,
                Vec::new(),
                i as f64 * 0.02,
                egui::Modifiers::NONE,
            );
        }
        let nodes = buttons(&ctx, &mut state, 1.0);
        let find = |label: &str| {
            nodes
                .iter()
                .find(|n| n.label == label)
                .unwrap_or_else(|| panic!("no button {label} in {nodes:?}"))
        };
        assert_eq!(find("guide.pdf").expanded, None);
        assert_eq!(find("specs").expanded, Some(false));
        // The outline starts with its top level shown.
        assert_eq!(find("1 Introduction").expanded, Some(false));
        assert_eq!(find("2 Design").expanded, None);
        assert!(find("2 Design").rect.is_positive());
    }
}
