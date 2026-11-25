//! Application state: open documents, tabs and windows (`docs/ARCHITECTURE.md` §5.1).
//!
//! Only [`crate::action::dispatch`] changes the structure held here (rule R-6).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use refdesk_core::{Config, DocStateDb, DocumentHandle, Paths, Prefs, ViewState};

use crate::action::Action;
use crate::overlays::capture::Clipboard;
use crate::overlays::palette::PaletteState;
use crate::panels::toc::{DEFAULT_TOC_WIDTH, TocState};
use crate::session::Memory;
use crate::views::diff_view::{DiffDialog, DiffTab};
use crate::views::pdf_view::PdfViewUiState;
use crate::views::preload::Preloader;
use crate::views::textures::TextureCache;
use crate::windows::dock::DockLayout;

/// Identifies one open document. Issued by [`DocStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocId(u64);

impl DocId {
    /// A document id made up for tests that do not go through a [`DocStore`].
    #[cfg(test)]
    pub fn for_test(n: u64) -> DocId {
        DocId(n)
    }
}

/// Identifies one tab. Issued by [`TabStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(u64);

impl TabId {
    /// A tab id made up for tests that do not go through a [`TabStore`].
    #[cfg(test)]
    pub fn for_test(n: u64) -> TabId {
        TabId(n)
    }
}

/// Identifies one application window. Issued by [`AppState`]; never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(u64);

impl WindowId {
    /// The id of the main window (the first one, which lives as long as the app).
    pub const MAIN: WindowId = WindowId(0);
}

/// Makes `path` absolute and free of `.`, `..` and symlinks, so two spellings of one file compare
/// equal. A file that does not exist (yet) is cleaned lexically instead.
pub fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // `pop` refuses to remove the root, which is what `/..` means.
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The open documents. A file is opened once, however many tabs show it.
#[derive(Default)]
pub struct DocStore {
    docs: HashMap<DocId, DocumentHandle>,
    by_path: HashMap<PathBuf, DocId>,
    next: u64,
}

impl DocStore {
    /// An empty store.
    pub fn new() -> DocStore {
        DocStore::default()
    }

    /// The document opened from `path` (compared after [`normalize_path`]).
    pub fn find(&self, path: &Path) -> Option<DocId> {
        self.by_path.get(&normalize_path(path)).copied()
    }

    /// Stores a handle under its normalized path and returns its new id.
    pub fn insert(&mut self, handle: DocumentHandle) -> DocId {
        let id = DocId(self.next);
        self.next += 1;
        self.by_path.insert(normalize_path(handle.path()), id);
        self.docs.insert(id, handle);
        id
    }

    /// The handle of a document.
    pub fn get(&self, id: DocId) -> Option<&DocumentHandle> {
        self.docs.get(&id)
    }

    /// Drops a document and its handle. The worker ends with the last handle clone.
    pub fn remove(&mut self, id: DocId) {
        if let Some(handle) = self.docs.remove(&id) {
            self.by_path.remove(&normalize_path(handle.path()));
        }
    }

    /// Every open document, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (DocId, &DocumentHandle)> {
        self.docs.iter().map(|(id, handle)| (*id, handle))
    }

    /// Number of open documents.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.docs.len()
    }
}

/// A tab showing one document. Several tabs may show the same document, each with its own view.
#[derive(Clone, Debug)]
pub struct PdfTab {
    /// The document shown.
    pub doc: DocId,
    /// Scroll position, zoom and history of this view.
    pub view: ViewState,
    /// View state that is not saved: scroll in pixels, cached layout, inputs (WP-15).
    pub ui: PdfViewUiState,
    /// Filter text and collapsed entries of the table of contents panel; not saved (WP-31).
    /// Boxed so that the `Tab` enum stays small.
    pub toc: Box<TocState>,
}

/// The content of a tab.
// Nearly every tab is a PDF tab (there is at most one start page per window), so boxing the
// large variant would only add an indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Tab {
    /// A PDF document.
    Pdf(PdfTab),
    /// A comparison of two documents (WP-62). Boxed: it holds two views.
    Diff(Box<DiffTab>),
    /// The start page.
    Start,
}

/// All tabs, by id.
#[derive(Default)]
pub struct TabStore {
    tabs: HashMap<TabId, Tab>,
    next: u64,
}

impl TabStore {
    /// An empty store.
    pub fn new() -> TabStore {
        TabStore::default()
    }

    /// Adds a tab and returns its new id.
    pub fn insert(&mut self, tab: Tab) -> TabId {
        let id = TabId(self.next);
        self.next += 1;
        self.tabs.insert(id, tab);
        id
    }

    /// The tab with this id.
    pub fn get(&self, id: TabId) -> Option<&Tab> {
        self.tabs.get(&id)
    }

    /// The tab with this id, for changing its view.
    pub fn get_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.get_mut(&id)
    }

    /// The PDF tab with this id; `None` for other kinds of tab.
    pub fn pdf(&self, id: TabId) -> Option<&PdfTab> {
        match self.tabs.get(&id) {
            Some(Tab::Pdf(tab)) => Some(tab),
            _ => None,
        }
    }

    /// The comparison tab with this id; `None` for other kinds of tab.
    pub fn diff(&self, id: TabId) -> Option<&DiffTab> {
        match self.tabs.get(&id) {
            Some(Tab::Diff(tab)) => Some(tab),
            _ => None,
        }
    }

    /// Every tab, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (TabId, &Tab)> {
        self.tabs.iter().map(|(id, tab)| (*id, tab))
    }

    /// Removes and returns a tab.
    pub fn remove(&mut self, id: TabId) -> Option<Tab> {
        self.tabs.remove(&id)
    }

    /// True while some tab shows `doc` (a comparison tab shows two documents).
    pub fn doc_in_use(&self, doc: DocId) -> bool {
        self.tabs.values().any(|tab| match tab {
            Tab::Pdf(p) => p.doc == doc,
            Tab::Diff(d) => d.docs().contains(&doc),
            Tab::Start => false,
        })
    }

    /// Number of tabs.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.tabs.len()
    }
}

/// One application window: its tab layout (`docs/ARCHITECTURE.md` §5.1, §5.4).
#[derive(Clone, Debug)]
pub struct WindowState {
    /// The window's id.
    pub id: WindowId,
    /// The panes and tabs of the window.
    pub dock: DockLayout,
    /// The table of contents panel is shown (TOC-3).
    pub toc_open: bool,
    /// Width of the table of contents panel in logical pixels.
    pub toc_width: f32,
}

impl WindowState {
    /// A window without tabs.
    pub fn new(id: WindowId) -> WindowState {
        WindowState {
            id,
            dock: DockLayout::new(),
            toc_open: false,
            toc_width: DEFAULT_TOC_WIDTH,
        }
    }
}

/// A closed PDF tab, remembered so it can be reopened (TAB-4). Only the path is kept, not the
/// document handle, so a closed document is really closed.
#[derive(Clone, Debug, PartialEq)]
pub struct ClosedTab {
    /// The document the tab showed.
    pub path: PathBuf,
    /// Where the tab was.
    pub view: ViewState,
}

/// A short message in the status bar that goes away by itself.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusNotice {
    /// The text shown.
    pub text: String,
    /// When it goes away, in egui time (seconds).
    pub until: f64,
}

/// How many closed tabs are remembered.
pub const MAX_CLOSED_TABS: usize = 20;

/// Everything the application knows. Widgets read it; `dispatch` changes it.
pub struct AppState {
    /// The open documents.
    pub docs: DocStore,
    /// All tabs.
    pub tabs: TabStore,
    /// The windows; `[0]` is the main window.
    pub windows: Vec<WindowState>,
    /// The tab commands apply to.
    pub focus: Option<TabId>,
    /// Textures of rendered pages, shared by all views and windows.
    pub textures: TextureCache,
    /// Background rendering of every page's preview (WP-17).
    pub preload: Preloader,
    /// Actions collected during this frame.
    pub actions: Vec<Action>,
    /// Set by `dispatch` when the file dialog should be opened; the app starts it on the next
    /// frame and clears the flag.
    pub file_dialog_requested: bool,
    /// The dialog for choosing two documents to compare, while it is open (WP-62).
    pub diff_dialog: Option<DiffDialog>,
    /// Recently closed PDF tabs, oldest first, at most [`MAX_CLOSED_TABS`].
    pub closed_tabs: Vec<ClosedTab>,
    /// The command palette and quick open (WP-43).
    pub palette: PaletteState,
    /// Where the session and the document records are saved; `None` saves nothing (tests).
    pub paths: Option<Paths>,
    /// The settings from `config.toml`, read once at start. The app never writes that file.
    pub config: Config,
    /// The settings changed inside the app (`prefs.json`); they win over `config`.
    pub prefs: Prefs,
    /// A short message at the right end of the status bar, e.g. the new scroll speed.
    pub status_notice: Option<StatusNotice>,
    /// What is remembered about each document (`documents.json`, SES-3).
    pub doc_db: DocStateDb,
    /// The section path line above PDF views is shown (TOC-6). Not saved.
    pub show_breadcrumb: bool,
    /// Where captures put their image. `None` uses the system clipboard; tests set a fake (WP-54).
    pub capture_clipboard: Option<Arc<dyn Clipboard>>,
    /// Session bookkeeping: focus order, fingerprints, window sizes (WP-41).
    pub memory: Memory,
    next_window: u64,
}

impl Default for AppState {
    fn default() -> Self {
        AppState::new()
    }
}

impl AppState {
    /// A state with one window that shows the start page. Reads no files; `session::start` fills
    /// in the stored values.
    pub fn new() -> AppState {
        let mut tabs = TabStore::new();
        let start = tabs.insert(Tab::Start);
        let mut window = WindowState::new(WindowId::MAIN);
        window.dock.add_tab(start, None);
        AppState {
            docs: DocStore::new(),
            tabs,
            windows: vec![window],
            focus: Some(start),
            textures: TextureCache::default(),
            preload: Preloader::default(),
            actions: Vec::new(),
            file_dialog_requested: false,
            diff_dialog: None,
            closed_tabs: Vec::new(),
            palette: PaletteState::default(),
            paths: None,
            config: Config::default(),
            prefs: Prefs::default(),
            status_notice: None,
            doc_db: DocStateDb::default(),
            show_breadcrumb: true,
            capture_clipboard: None,
            memory: Memory::default(),
            next_window: 1,
        }
    }

    /// The scroll speed multiplier in effect: the preference, else `config.toml`'s (VIEW-12).
    pub fn scroll_speed(&self) -> f32 {
        refdesk_core::effective_scroll_speed(&self.prefs, &self.config)
    }

    /// Issues a window id that was never used before, even by a window that is closed now.
    pub fn new_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window);
        self.next_window += 1;
        id
    }

    /// Adds a window without tabs at the end of the window list and returns its id.
    pub fn add_window(&mut self) -> WindowId {
        let id = self.new_window_id();
        self.windows.push(WindowState::new(id));
        id
    }

    /// Removes a window from the list. The main window is never removed. Its tabs are left
    /// alone; the caller moves or closes them first.
    pub fn remove_window(&mut self, id: WindowId) {
        if id != self.main_window().id {
            self.windows.retain(|w| w.id != id);
        }
    }

    /// The index in `windows` of the window with this id.
    pub fn window_index(&self, id: WindowId) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    /// The main window.
    pub fn main_window(&self) -> &WindowState {
        // `new` creates window 0 and `remove_window` never removes it.
        &self.windows[0]
    }

    /// Follows the window that has the keyboard: when the focused tab is not in window
    /// `index`, the focus moves to that window's focused tab, so commands apply there (WIN-6).
    pub fn follow_window_focus(&mut self, index: usize) {
        let Some(window) = self.windows.get(index) else {
            return;
        };
        if self.focus.is_some_and(|t| window.dock.contains(t)) {
            return;
        }
        if let Some(tab) = window.dock.focused_tab() {
            self.focus = Some(tab);
        }
    }

    /// Adds a closed tab to the reopen list, dropping the oldest beyond [`MAX_CLOSED_TABS`].
    pub fn remember_closed(&mut self, closed: ClosedTab) {
        self.closed_tabs.push(closed);
        if self.closed_tabs.len() > MAX_CLOSED_TABS {
            let excess = self.closed_tabs.len() - MAX_CLOSED_TABS;
            self.closed_tabs.drain(..excess);
        }
    }

    /// The index in `windows` of the window that holds `tab`.
    pub fn window_of(&self, tab: TabId) -> Option<usize> {
        self.windows.iter().position(|w| w.dock.contains(tab))
    }

    /// The index of the window that holds the focused tab; the main window when no window does.
    pub fn focused_window_index(&self) -> usize {
        self.focus.and_then(|t| self.window_of(t)).unwrap_or(0)
    }

    /// The window that holds the focused tab (see [`AppState::focused_window_index`]).
    pub fn focused_window(&self) -> &WindowState {
        // `focused_window_index` returns a valid index or 0, and window 0 always exists.
        &self.windows[self.focused_window_index()]
    }

    /// The PDF tab commands apply to: the focused tab, if it is a PDF tab.
    pub fn focused_pdf(&self) -> Option<(TabId, &PdfTab)> {
        let id = self.focus?;
        self.tabs.pdf(id).map(|tab| (id, tab))
    }

    /// The comparison tab commands apply to: the focused tab, if it is a comparison tab.
    pub fn focused_diff(&self) -> Option<(TabId, &DiffTab)> {
        let id = self.focus?;
        self.tabs.diff(id).map(|tab| (id, tab))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handles_dot_and_dotdot_for_missing_files() {
        let a = normalize_path(Path::new("/refdesk-test-missing/dir/./a.pdf"));
        let b = normalize_path(Path::new("/refdesk-test-missing/other/../dir/a.pdf"));
        assert_eq!(a, PathBuf::from("/refdesk-test-missing/dir/a.pdf"));
        assert_eq!(a, b);
    }

    #[test]
    fn normalize_makes_relative_paths_absolute() {
        let cwd = std::env::current_dir().unwrap_or_default();
        assert_eq!(
            normalize_path(Path::new("./refdesk-test-missing.pdf")),
            normalize_path(&cwd.join("refdesk-test-missing.pdf"))
        );
    }

    #[test]
    fn normalize_resolves_symlinks_of_existing_files() {
        let dir = std::env::temp_dir().join(format!(
            "refdesk-wp14-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.pdf");
        let link = dir.join("link.pdf");
        std::fs::write(&file, b"x").unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert_eq!(normalize_path(&file), normalize_path(&link));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
