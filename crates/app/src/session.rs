//! Session and per-document memory (`docs/ARCHITECTURE.md` §5.8, `docs/SPEC.md` §7.6).
//!
//! * [`to_session`] / [`restore`]: `AppState` ↔ [`Session`] (SES-1, SES-2, SES-6).
//! * [`Saver`] with the pure [`SaveTimer`]: save two seconds after the last change, and at once
//!   on exit (SES-5).
//! * [`remembered_view`], [`document_opened`], [`poll_fingerprints`], [`record_tab`]: where each
//!   document was left, found by path or by content fingerprint (SES-3, SES-4).
//! * [`recent_entries`], [`recent_document_paths`]: the recent documents (SES-7).
//!
//! Tab keys in the session file are `t0`, `t1`, … numbered by how recently the tab had the focus:
//! `t0` is the focused tab. The session format has no focus field, so this is how the focus (and
//! the focus order) survives a restart.
//!
//! Nothing here touches the disk unless `AppState::paths` is set; tests set it to a temporary
//! directory. Times come from [`Memory`]'s clock, which tests replace.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use refdesk_core::{
    Config, DocRecord, DocStateDb, DocStatus, DocumentHandle, FileStamp, History, Outline, Paths,
    Place, Session, SessionTab, SessionWindow, ViewState, fingerprint_file,
};

use crate::action::notifier;
use crate::i18n::tr;
use crate::panels::toc::{TOC_WIDTH_RANGE, TocState};
use crate::state::{AppState, DocId, PdfTab, Tab, TabId, WindowId, WindowState, normalize_path};
use crate::views::pdf_view::PdfViewUiState;
use crate::views::start_page::{RecentEntry, StartPage};
use crate::windows::dock::DockLayout;

/// How long the state must stay unchanged before it is saved (SES-5).
pub const SAVE_DELAY: Duration = Duration::from_secs(2);

/// How many recent documents the start page and quick open list.
pub const RECENT_COUNT: usize = 20;

/// A view counts as untouched while it stays on its first page within this many points of where
/// it started (the first frame moves the anchor a little when it lays the page out).
const UNTOUCHED_TOLERANCE_PT: f32 = 24.0;

/// Smallest and largest window size accepted from the session file, in points.
const MIN_WINDOW_SIZE: (f32, f32) = (320.0, 240.0);
const MAX_WINDOW_SIZE: f32 = 16384.0;

/// The commands whose shortcuts the start page shows, if the command table has them.
const START_PAGE_COMMANDS: &[&str] = &[
    "file.open",
    "palette.quick_open",
    "palette.commands",
    "view.toggle_toc",
];

/// Unix seconds now. Replaced in tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The system clock in Unix seconds.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
    })
}

/// Size and maximized state of a window. The position is not kept (SPEC §7.6).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowGeom {
    /// Inner size in points.
    pub size: (f32, f32),
    /// Whether the window is maximized.
    pub maximized: bool,
}

impl Default for WindowGeom {
    fn default() -> WindowGeom {
        let w = SessionWindow::default();
        WindowGeom {
            size: w.size,
            maximized: w.maximized,
        }
    }
}

/// A size from a file made usable: not NaN, not tiny, not absurd.
pub fn sane_size(size: (f32, f32)) -> [f32; 2] {
    let fix = |v: f32, min: f32, default: f32| {
        if v.is_finite() {
            v.clamp(min, MAX_WINDOW_SIZE)
        } else {
            default
        }
    };
    let default = WindowGeom::default().size;
    [
        fix(size.0, MIN_WINDOW_SIZE.0, default.0),
        fix(size.1, MIN_WINDOW_SIZE.1, default.1),
    ]
}

/// A computed fingerprint and the file stamp it belongs to.
#[derive(Clone, Debug, PartialEq)]
struct Hashed {
    stamp: FileStamp,
    fingerprint: String,
}

/// What a fingerprint thread reports.
struct HashResult {
    path: PathBuf,
    result: Result<Hashed, String>,
}

/// A tab opened without a remembered place, waiting for its document's fingerprint to look the
/// document up by content.
struct Waiting {
    path: PathBuf,
    /// The view the tab started with; the remembered one is applied only if it is unchanged.
    initial: ViewState,
}

/// Session bookkeeping kept in `AppState`.
pub struct Memory {
    clock: Clock,
    /// Increases every time the focus moves; `focus_seen` holds the value per tab.
    focus_counter: u64,
    focus_seen: HashMap<TabId, u64>,
    last_focus: Option<TabId>,
    /// When each document was opened or last had the focus (Unix seconds).
    last_used: HashMap<PathBuf, i64>,
    /// Fingerprints computed this run, by normalized path.
    fingerprints: HashMap<PathBuf, Hashed>,
    /// Paths with a fingerprint thread running.
    hashing: HashSet<PathBuf>,
    /// Paths whose fingerprint thread starts at the next poll.
    to_hash: Vec<PathBuf>,
    waiting: HashMap<TabId, Waiting>,
    hash_tx: Sender<HashResult>,
    hash_rx: Receiver<HashResult>,
    /// The size each window is created with (restored windows; the main window from main.rs).
    initial_geom: HashMap<WindowId, WindowGeom>,
    /// The size each window had in the last frame that drew it as a real window.
    seen_geom: HashMap<WindowId, WindowGeom>,
    /// `doc_db` changed since it was last written.
    db_dirty: bool,
    /// The start page's list, built once and dropped when `doc_db` changes.
    recent: Option<Arc<Vec<RecentEntry>>>,
}

impl Default for Memory {
    fn default() -> Memory {
        let (hash_tx, hash_rx) = mpsc::channel();
        Memory {
            clock: system_clock(),
            focus_counter: 0,
            focus_seen: HashMap::new(),
            last_focus: None,
            last_used: HashMap::new(),
            fingerprints: HashMap::new(),
            hashing: HashSet::new(),
            to_hash: Vec::new(),
            waiting: HashMap::new(),
            hash_tx,
            hash_rx,
            initial_geom: HashMap::new(),
            seen_geom: HashMap::new(),
            db_dirty: false,
            recent: None,
        }
    }
}

impl Memory {
    /// Replaces the clock (tests use a fixed one).
    #[cfg(test)]
    pub fn set_clock(&mut self, clock: Clock) {
        self.clock = clock;
    }

    /// Unix seconds now, from the clock.
    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    /// Sets the size window `id` is created with.
    pub fn set_initial_geometry(&mut self, id: WindowId, geom: WindowGeom) {
        self.initial_geom.insert(id, geom);
    }

    /// The size window `id` is created with, if it came from the session.
    pub fn initial_geometry(&self, id: WindowId) -> Option<WindowGeom> {
        self.initial_geom.get(&id).copied()
    }

    /// The size window `id` has now, as far as known.
    fn geometry(&self, id: WindowId) -> WindowGeom {
        self.seen_geom
            .get(&id)
            .or_else(|| self.initial_geom.get(&id))
            .copied()
            .unwrap_or_default()
    }

    /// The fingerprint computed for `path` this run.
    #[cfg(test)]
    pub fn fingerprint_of(&self, path: &Path) -> Option<&str> {
        self.fingerprints
            .get(&normalize_path(path))
            .map(|h| h.fingerprint.as_str())
    }

    /// Whether `tab` still waits for its document's fingerprint.
    #[cfg(test)]
    pub fn is_waiting(&self, tab: TabId) -> bool {
        self.waiting.contains_key(&tab)
    }
}

// ---------------------------------------------------------------------------------------------
// Focus and window size
// ---------------------------------------------------------------------------------------------

/// Notes a change of the focused tab: it becomes the most recently focused one, and its document
/// the most recently used.
pub fn note_focus(state: &mut AppState) {
    let Some(tab) = state.focus else {
        return;
    };
    if state.memory.last_focus == Some(tab) {
        return;
    }
    state.memory.last_focus = Some(tab);
    state.memory.focus_counter += 1;
    let counter = state.memory.focus_counter;
    state.memory.focus_seen.insert(tab, counter);
    if let Some(path) = doc_path_of(state, tab) {
        let now = state.memory.now();
        state.memory.last_used.insert(path, now);
    }
}

/// Reads the size of window `id` from this frame's viewport input. Call it while drawing that
/// window as a real window. A maximized window keeps its last normal size; a minimized one is
/// ignored.
pub fn note_window(ctx: &egui::Context, state: &mut AppState, id: WindowId) {
    let (rect, maximized, minimized) = ctx.input(|i| {
        let v = i.viewport();
        (v.inner_rect, v.maximized, v.minimized)
    });
    if minimized == Some(true) {
        return;
    }
    let previous = state.memory.geometry(id);
    let maximized = maximized.unwrap_or(false);
    let size = match rect {
        Some(r) if !maximized && r.is_positive() => (r.width(), r.height()),
        _ => previous.size,
    };
    state
        .memory
        .seen_geom
        .insert(id, WindowGeom { size, maximized });
}

fn doc_path_of(state: &AppState, tab: TabId) -> Option<PathBuf> {
    let pdf = state.tabs.pdf(tab)?;
    Some(state.docs.get(pdf.doc)?.path().to_path_buf())
}

// ---------------------------------------------------------------------------------------------
// AppState -> Session
// ---------------------------------------------------------------------------------------------

/// The tab keys of the session: `t0` for the focused tab, then by how recently each tab had the
/// focus, tabs never focused last in window order.
fn tab_keys(state: &AppState) -> HashMap<TabId, String> {
    let mut order: Vec<TabId> = state.windows.iter().flat_map(|w| w.dock.tabs()).collect();
    // Stable: ties keep window order.
    order.sort_by_key(|t| {
        (
            std::cmp::Reverse(state.focus == Some(*t)),
            std::cmp::Reverse(state.memory.focus_seen.get(t).copied().unwrap_or(0)),
        )
    });
    order
        .into_iter()
        .enumerate()
        .map(|(i, t)| (t, format!("t{i}")))
        .collect()
}

/// The rank encoded in a tab key (`t3` → 3); unknown forms rank last.
fn key_rank(key: &str) -> u64 {
    key.strip_prefix('t')
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX)
}

/// What is saved of one tab; `None` for tabs that are not saved (the start page).
fn session_tab(state: &AppState, tab: TabId) -> Option<SessionTab> {
    match state.tabs.get(tab)? {
        Tab::Pdf(pdf) => pdf_session_tab(state, pdf),
        Tab::Diff(diff) => diff.to_session(&state.docs),
        Tab::Start => None,
    }
}

fn pdf_session_tab(state: &AppState, pdf: &PdfTab) -> Option<SessionTab> {
    let doc = state.docs.get(pdf.doc)?;
    Some(SessionTab::Pdf {
        path: doc.path().to_path_buf(),
        view: pdf.view.clone(),
    })
}

/// The session for the current state: windows (main first) with their size, layout and table
/// of contents panel, and every saved tab.
pub fn to_session(state: &AppState) -> Session {
    let keys = tab_keys(state);
    let mut session = Session::default();
    for (i, window) in state.windows.iter().enumerate() {
        for tab in window.dock.tabs() {
            if let (Some(key), Some(saved)) = (keys.get(&tab), session_tab(state, tab)) {
                session.tabs.insert(key.clone(), saved);
            }
        }
        let geom = state.memory.geometry(window.id);
        session.windows.push(SessionWindow {
            main: i == 0,
            size: geom.size,
            maximized: geom.maximized,
            layout: window
                .dock
                .to_layout(|t| keys.get(&t).cloned().unwrap_or_default()),
            toc_open: window.toc_open,
            toc_width: window.toc_width,
        });
    }
    // Drops the start pages from the layouts.
    session.normalized()
}

// ---------------------------------------------------------------------------------------------
// Session -> AppState
// ---------------------------------------------------------------------------------------------

/// Whether to restore the last session: the command line did not say `--no-restore` and the
/// configuration allows it.
pub fn should_restore(cli_restore: bool, config: &Config) -> bool {
    cli_restore && config.restore_session
}

/// The main window's size from a session (its first window marked main).
pub fn main_geometry(session: &Session) -> WindowGeom {
    session
        .windows
        .iter()
        .find(|w| w.main)
        .or_else(|| session.windows.first())
        .map(|w| WindowGeom {
            size: w.size,
            maximized: w.maximized,
        })
        .unwrap_or_default()
}

/// Rebuilds the windows, layouts, tabs and focus of `session` in a fresh `state` (one main
/// window with the start page). Documents are opened as usual; a file that is missing gives a
/// tab in the failed state, which stays in the session (SES-6). Windows left without tabs are
/// not created. Nothing here fails.
pub fn restore(state: &mut AppState, session: &Session, ctx: &egui::Context) {
    let session = session.clone().normalized();
    let mut restored: HashMap<String, TabId> = HashMap::new();
    let main = session.windows.iter().position(|w| w.main).unwrap_or(0);
    let order = std::iter::once(main).chain((0..session.windows.len()).filter(|i| *i != main));
    for index in order {
        let Some(saved) = session.windows.get(index) else {
            continue;
        };
        for key in saved.layout.tab_keys() {
            if restored.contains_key(key) {
                continue;
            }
            if let Some(tab) = session
                .tabs
                .get(key)
                .and_then(|t| restore_tab(state, t, ctx))
            {
                restored.insert(key.to_owned(), tab);
            }
        }
        let dock = DockLayout::from_layout(&saved.layout, |k| restored.get(k).copied());
        let geom = WindowGeom {
            size: saved.size,
            maximized: saved.maximized,
        };
        let toc_width = if saved.toc_width.is_finite() {
            saved.toc_width.clamp(TOC_WIDTH_RANGE.0, TOC_WIDTH_RANGE.1)
        } else {
            crate::panels::toc::DEFAULT_TOC_WIDTH
        };
        if index == main {
            let id = state.main_window().id;
            state.memory.set_initial_geometry(id, geom);
            state.windows[0].toc_open = saved.toc_open;
            state.windows[0].toc_width = toc_width;
            if !dock.is_empty() {
                for old in state.windows[0].dock.tabs() {
                    state.tabs.remove(old);
                }
                state.windows[0].dock = dock;
            }
        } else if !dock.is_empty() {
            let id = state.new_window_id();
            let mut window = WindowState::new(id);
            window.dock = dock;
            window.toc_open = saved.toc_open;
            window.toc_width = toc_width;
            state.windows.push(window);
            state.memory.set_initial_geometry(id, geom);
        }
    }

    // Focus order: the lowest rank is the focused tab, so it is noted last.
    let mut ranked: Vec<(u64, TabId)> = restored
        .iter()
        .map(|(key, tab)| (key_rank(key), *tab))
        .collect();
    ranked.sort_by_key(|(rank, tab)| (std::cmp::Reverse(*rank), std::cmp::Reverse(*tab)));
    for (_, tab) in &ranked {
        state.memory.focus_counter += 1;
        let counter = state.memory.focus_counter;
        state.memory.focus_seen.insert(*tab, counter);
    }
    if let Some((_, focus)) = ranked.last()
        && let Some(w) = state.window_of(*focus)
    {
        // The focused tab was the active tab of its pane; this gives that pane the focus.
        state.windows[w].dock.activate(*focus);
        state.focus = Some(*focus);
        state.memory.last_focus = Some(*focus);
    }
}

/// Creates the tab for one saved tab; `None` for kinds that are not restored.
fn restore_tab(state: &mut AppState, saved: &SessionTab, ctx: &egui::Context) -> Option<TabId> {
    match saved {
        SessionTab::Pdf { path, view } => Some(restore_pdf_tab(state, path, view, ctx)),
        // The comparison is computed again; only the two documents are remembered.
        SessionTab::Diff { left, right } => {
            Some(crate::action::insert_diff_tab(state, left, right, ctx))
        }
    }
}

fn restore_pdf_tab(
    state: &mut AppState,
    path: &Path,
    view: &ViewState,
    ctx: &egui::Context,
) -> TabId {
    let path = normalize_path(path);
    let doc = ensure_doc(state, &path, ctx);
    let mut toc = Box::<TocState>::default();
    if let Some(rec) = state.doc_db.lookup(&path, None)
        && rec.path == path
        && !rec.toc_collapsed.is_empty()
    {
        toc.set_collapsed_paths(rec.toc_collapsed.clone());
    }
    let tab = state.tabs.insert(Tab::Pdf(PdfTab {
        doc,
        view: rebased(view, &path),
        ui: PdfViewUiState::default(),
        toc,
    }));
    document_opened(state, None, &path);
    tab
}

/// The open document for `path`, opening it when no tab shows it yet.
fn ensure_doc(state: &mut AppState, path: &Path, ctx: &egui::Context) -> DocId {
    match state.docs.find(path) {
        Some(doc) => doc,
        None => state.docs.insert(DocumentHandle::open(path, notifier(ctx))),
    }
}

// ---------------------------------------------------------------------------------------------
// Per-document memory
// ---------------------------------------------------------------------------------------------

/// How a newly opened document starts.
pub struct Remembered {
    /// The view to start with.
    pub view: ViewState,
    /// Folded table of contents entries to restore, if remembered.
    pub toc_collapsed: Option<Vec<Vec<String>>>,
    /// Whether a record was found (by path).
    pub found: bool,
}

/// The view a document opened from `path` (normalized) starts with: the remembered one if a
/// record has this path, else the top of the document at the configured zoom. Records found
/// only by content are applied later, when the fingerprint is known ([`poll_fingerprints`]).
pub fn remembered_view(state: &AppState, path: &Path) -> Remembered {
    match state.doc_db.lookup(path, None) {
        Some(rec) => Remembered {
            view: rebased(&rec.view, path),
            toc_collapsed: (!rec.toc_collapsed.is_empty()).then(|| rec.toc_collapsed.clone()),
            found: true,
        },
        None => {
            let mut view = ViewState::new(path.to_path_buf());
            view.zoom = state.config.default_zoom.to_zoom_mode();
            Remembered {
                view,
                toc_collapsed: None,
                found: false,
            }
        }
    }
}

/// Bookkeeping for a document that was just opened from `path` (normalized): it counts as used
/// now and its fingerprint is computed in the background, starting at the next
/// [`poll_fingerprints`]. `waiting_tab` is a tab that started without a remembered place; when
/// the fingerprint finds a record of the same content under another path, that place is
/// applied, unless the tab was moved by then.
pub fn document_opened(state: &mut AppState, waiting_tab: Option<TabId>, path: &Path) {
    let now = state.memory.now();
    state.memory.last_used.insert(path.to_path_buf(), now);
    if let Some(tab) = waiting_tab
        && let Some(pdf) = state.tabs.pdf(tab)
    {
        let initial = pdf.view.clone();
        state.memory.waiting.insert(
            tab,
            Waiting {
                path: path.to_path_buf(),
                initial,
            },
        );
    }
    if !state.memory.to_hash.iter().any(|p| p == path) {
        state.memory.to_hash.push(path.to_path_buf());
    }
}

/// Starts computing the fingerprint of `path` on a thread of its own (R-1), unless it is known
/// or already being computed. When the file's size and modification time equal those of its
/// record, the recorded fingerprint is taken without reading the file.
fn start_hashing(state: &mut AppState, path: &Path, ctx: &egui::Context) {
    let memory = &mut state.memory;
    if memory.fingerprints.contains_key(path) || !memory.hashing.insert(path.to_path_buf()) {
        return;
    }
    let known = state
        .doc_db
        .lookup(path, None)
        .filter(|rec| rec.path == path)
        .and_then(|rec| {
            rec.fingerprint.clone().map(|fingerprint| Hashed {
                stamp: FileStamp {
                    size: rec.size,
                    mtime: rec.mtime,
                },
                fingerprint,
            })
        });
    let tx = memory.hash_tx.clone();
    let ctx = ctx.clone();
    let owned = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("fingerprint".into())
        .spawn(move || {
            let result = hash_file(&owned, known);
            // The receiver is gone only when the app is shutting down.
            let _ = tx.send(HashResult {
                path: owned,
                result,
            });
            ctx.request_repaint();
        });
    if let Err(err) = spawned {
        log::warn!("could not start a fingerprint thread: {err}");
        memory.hashing.remove(path);
    }
}

/// Runs on the fingerprint thread.
fn hash_file(path: &Path, known: Option<Hashed>) -> Result<Hashed, String> {
    let stamp = refdesk_core::stamp(path).map_err(|e| e.to_string())?;
    if let Some(known) = known
        && known.stamp == stamp
    {
        return Ok(known);
    }
    let started = std::time::Instant::now();
    let fingerprint = fingerprint_file(path).map_err(|e| e.to_string())?;
    log::debug!("fingerprint {}: {:?}", path.display(), started.elapsed());
    Ok(Hashed { stamp, fingerprint })
}

/// Starts the fingerprints of newly opened documents and takes the finished ones. For a tab
/// waiting on one, a record of the same content (a moved or renamed file) is applied if the tab
/// is still where it started. Returns whether a remembered place was applied. Call once per
/// frame; a finished fingerprint asks `ctx` for a frame.
pub fn poll_fingerprints(state: &mut AppState, ctx: &egui::Context) -> bool {
    for path in std::mem::take(&mut state.memory.to_hash) {
        // A document closed before its fingerprint started does not need one.
        if state.docs.find(&path).is_some() {
            start_hashing(state, &path, ctx);
        }
    }
    let mut applied = false;
    while let Ok(done) = state.memory.hash_rx.try_recv() {
        state.memory.hashing.remove(&done.path);
        let waiting: Vec<TabId> = state
            .memory
            .waiting
            .iter()
            .filter(|(_, w)| w.path == done.path)
            .map(|(t, _)| *t)
            .collect();
        let hashed = match done.result {
            Ok(hashed) => hashed,
            Err(err) => {
                log::debug!("no fingerprint for {}: {err}", done.path.display());
                for tab in waiting {
                    state.memory.waiting.remove(&tab);
                }
                continue;
            }
        };
        let record = state
            .doc_db
            .lookup(&done.path, Some(&hashed.fingerprint))
            .cloned();
        if state.docs.find(&done.path).is_some() {
            // The record gets the fingerprint at the next save.
            state.memory.db_dirty = true;
        }
        state.memory.fingerprints.insert(done.path.clone(), hashed);
        for tab in waiting {
            let Some(w) = state.memory.waiting.remove(&tab) else {
                continue;
            };
            let (Some(rec), Some(Tab::Pdf(pdf))) = (&record, state.tabs.get_mut(tab)) else {
                continue;
            };
            if untouched(&w.initial, &pdf.view) {
                pdf.view = rebased(&rec.view, &done.path);
                if !rec.toc_collapsed.is_empty() {
                    pdf.toc.set_collapsed_paths(rec.toc_collapsed.clone());
                }
                applied = true;
            }
        }
    }
    applied
}

/// Whether a view is still where it started: same zoom, no jump, and within a few points of the
/// start on the same page.
pub fn untouched(initial: &ViewState, now: &ViewState) -> bool {
    now.zoom == initial.zoom
        && now.history.len() == initial.history.len()
        && now.anchor.page == initial.anchor.page
        && (now.anchor.y - initial.anchor.y).abs() <= UNTOUCHED_TOLERANCE_PT
}

/// Keeps a remembered place inside a document with `page_count` pages (a new version at the
/// same path may be shorter): a place past the end moves to the top of the last page.
pub fn fit_view_to_document(view: &mut ViewState, page_count: usize) {
    if page_count > 0 && view.anchor.page as usize >= page_count {
        view.anchor = refdesk_core::Location::new(page_count as u32 - 1, 0.0, 0.0);
    }
}

/// `view` with every history entry pointing at `doc` (the file may have moved since).
fn rebased(view: &ViewState, doc: &Path) -> ViewState {
    ViewState {
        anchor: view.anchor,
        zoom: view.zoom,
        history: rebase_history(&view.history, doc),
    }
}

/// The same history with every place in document `doc`. `History` does not expose its entries,
/// so they are read by stepping back and forward on a copy.
fn rebase_history(history: &History, doc: &Path) -> History {
    let mut walk = history.clone();
    let mut index = 0;
    while walk.can_back() {
        let here = walk.current().clone();
        let _ = walk.back(here);
        index += 1;
    }
    let mut places = vec![walk.current().clone()];
    while walk.can_forward() {
        let here = walk.current().clone();
        match walk.forward(here) {
            Some(place) => places.push(place),
            None => break,
        }
    }
    if places.iter().all(|p| p.doc == doc) {
        return history.clone();
    }
    let moved = |p: &Place| Place {
        doc: doc.to_path_buf(),
        loc: p.loc,
    };
    let mut out = History::new(moved(&places[0]));
    for place in &places[1..] {
        let from = out.current().clone();
        out.jump(from, moved(place));
    }
    for _ in index..out.len().saturating_sub(1) {
        let here = out.current().clone();
        let _ = out.back(here);
    }
    out
}

/// Records where `tab` is in its document (SES-3). Nothing for other kinds of tab and for a
/// document that could not be opened (its old record stays as it was).
pub fn record_tab(state: &mut AppState, tab: TabId) {
    let Some(pdf) = state.tabs.pdf(tab) else {
        return;
    };
    let Some(doc) = state.docs.get(pdf.doc) else {
        return;
    };
    if matches!(doc.status(), DocStatus::Failed(_)) {
        return;
    }
    let path = doc.path().to_path_buf();
    let previous = state
        .doc_db
        .lookup(&path, None)
        .filter(|r| r.path == path)
        .cloned();
    let toc_collapsed = match doc.info() {
        Some(info) => pdf.toc.collapsed_paths(&info.outline),
        None => {
            let pending = pdf.toc.collapsed_paths(&Outline::default());
            if pending.is_empty() {
                previous
                    .as_ref()
                    .map(|r| r.toc_collapsed.clone())
                    .unwrap_or_default()
            } else {
                pending
            }
        }
    };
    let (fingerprint, size, mtime) = match state.memory.fingerprints.get(&path) {
        Some(h) => (Some(h.fingerprint.clone()), h.stamp.size, h.stamp.mtime),
        None => previous
            .as_ref()
            .map_or((None, 0, 0), |r| (r.fingerprint.clone(), r.size, r.mtime)),
    };
    let now = state.memory.now();
    let record = DocRecord {
        last_opened: state.memory.last_used.get(&path).copied().unwrap_or(now),
        path,
        fingerprint,
        size,
        mtime,
        view: pdf.view.clone(),
        toc_collapsed,
    };
    state.doc_db.record(record);
    state.memory.db_dirty = true;
    state.memory.recent = None;
}

/// Records every open document from the tab that most recently had the focus among those that
/// show it.
pub fn record_open_documents(state: &mut AppState) {
    note_focus(state);
    let order: Vec<TabId> = state.windows.iter().flat_map(|w| w.dock.tabs()).collect();
    let mut best: HashMap<DocId, (u64, TabId)> = HashMap::new();
    for tab in order {
        let Some(pdf) = state.tabs.pdf(tab) else {
            continue;
        };
        let seen = state.memory.focus_seen.get(&tab).copied().unwrap_or(0);
        let entry = best.entry(pdf.doc).or_insert((seen, tab));
        if seen > entry.0 {
            *entry = (seen, tab);
        }
    }
    for (_, tab) in best.into_values() {
        record_tab(state, tab);
    }
}

/// Records the documents of window `id` before it closes.
pub fn record_window(state: &mut AppState, id: WindowId) {
    let Some(w) = state.window_index(id) else {
        return;
    };
    for tab in state.windows[w].dock.tabs() {
        record_tab(state, tab);
    }
}

// ---------------------------------------------------------------------------------------------
// Recent documents
// ---------------------------------------------------------------------------------------------

/// The paths of the most recently opened documents, newest first. Quick open takes its recent
/// documents from here.
#[allow(dead_code)] // for quick open (WP-43), which takes a `fn(&AppState) -> Vec<PathBuf>`
pub fn recent_document_paths(state: &AppState) -> Vec<PathBuf> {
    state
        .doc_db
        .recent(RECENT_COUNT)
        .into_iter()
        .map(|r| r.path.clone())
        .collect()
}

/// The start page's list of recent documents. Whether each file exists is checked when the list
/// is built, not every frame; the list is rebuilt after `doc_db` changes.
pub fn recent_entries(state: &mut AppState) -> Arc<Vec<RecentEntry>> {
    if let Some(list) = &state.memory.recent {
        return Arc::clone(list);
    }
    let list: Arc<Vec<RecentEntry>> = Arc::new(
        state
            .doc_db
            .recent(RECENT_COUNT)
            .into_iter()
            .map(|r| RecentEntry::new(&r.path, r.last_opened, r.path.is_file()))
            .collect(),
    );
    state.memory.recent = Some(Arc::clone(&list));
    list
}

/// Everything the start page shows.
pub fn start_page(state: &mut AppState, ctx: &egui::Context) -> StartPage {
    let shortcuts = START_PAGE_COMMANDS
        .iter()
        .filter_map(|id| crate::commands::command(id))
        .filter_map(|c| {
            c.shortcut
                .map(|s| (tr(c.name_key).to_owned(), ctx.format_shortcut(&s)))
        })
        .collect();
    StartPage {
        recent: recent_entries(state),
        shortcuts,
        now: state.memory.now(),
    }
}

// ---------------------------------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------------------------------

/// What the save timer decided for this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    /// Nothing to save.
    Idle,
    /// Something changed; save after this long unless it changes again.
    Wait(Duration),
    /// Save now.
    Save,
}

/// Decides when to save: [`SAVE_DELAY`] after the last change, once for a run of changes. Times
/// are seconds on any monotonic scale.
#[derive(Clone, Debug, Default)]
pub struct SaveTimer {
    since: Option<f64>,
}

impl SaveTimer {
    /// `dirty`: the state differs from what was saved. `changed`: it differs from the last call.
    pub fn update(&mut self, dirty: bool, changed: bool, now: f64) -> Decision {
        if !dirty {
            self.since = None;
            return Decision::Idle;
        }
        if changed || self.since.is_none() {
            self.since = Some(now);
        }
        let elapsed = now - self.since.unwrap_or(now);
        let delay = SAVE_DELAY.as_secs_f64();
        if elapsed >= delay {
            Decision::Save
        } else {
            Decision::Wait(Duration::from_secs_f64(delay - elapsed))
        }
    }

    /// The state was saved.
    pub fn saved(&mut self) {
        self.since = None;
    }

    /// Saving failed: try again after another delay.
    pub fn failed(&mut self, now: f64) {
        self.since = Some(now);
    }
}

/// Saves the session and the document records when they change (SES-5).
pub struct Saver {
    timer: SaveTimer,
    /// What is on disk.
    saved: Option<Session>,
    /// The session of the last tick.
    seen: Option<Session>,
}

impl Saver {
    /// A saver that knows `saved` is what the session file holds.
    pub fn new(saved: Option<Session>) -> Saver {
        Saver {
            timer: SaveTimer::default(),
            saved,
            seen: None,
        }
    }

    /// Runs once per frame at time `now` (seconds). Saves when the timer says so. Returns when
    /// the next frame is needed for a pending save; the caller schedules a repaint so the save
    /// is not held back by a quiet screen.
    pub fn tick(&mut self, state: &mut AppState, now: f64) -> Option<Duration> {
        state.paths.as_ref()?;
        note_focus(state);
        let current = to_session(state);
        let dirty = self.saved.as_ref() != Some(&current) || state.memory.db_dirty;
        let changed = self.seen.as_ref() != Some(&current);
        self.seen = Some(current);
        match self.timer.update(dirty, changed, now) {
            Decision::Idle => None,
            Decision::Wait(left) => Some(left),
            Decision::Save => {
                if self.save_now(state) {
                    None
                } else {
                    self.timer.failed(now);
                    Some(SAVE_DELAY)
                }
            }
        }
    }

    /// Records the open documents and writes `session.json` and `documents.json` now. Returns
    /// whether both were written (false when there is nowhere to save).
    pub fn save_now(&mut self, state: &mut AppState) -> bool {
        let Some(paths) = state.paths.clone() else {
            return false;
        };
        record_open_documents(state);
        let session = to_session(state);
        let ok = save_files(&paths, &session, &state.doc_db);
        if ok {
            self.saved = Some(session.clone());
            self.seen = Some(session);
            state.memory.db_dirty = false;
            self.timer.saved();
        }
        ok
    }
}

/// Writes both files; logs what failed.
fn save_files(paths: &Paths, session: &Session, db: &DocStateDb) -> bool {
    let mut ok = true;
    if let Err(err) = session.save(&paths.session_file()) {
        log::warn!(
            "could not save the session to {}: {err}",
            paths.session_file().display()
        );
        ok = false;
    }
    if let Err(err) = db.save(&paths.documents_file()) {
        log::warn!(
            "could not save the document records to {}: {err}",
            paths.documents_file().display()
        );
        ok = false;
    }
    ok
}

// ---------------------------------------------------------------------------------------------
// Start-up
// ---------------------------------------------------------------------------------------------

/// What the app starts with, read before the first window exists.
pub struct Startup {
    /// Where to save; `None` saves nothing.
    pub paths: Option<Paths>,
    /// The settings.
    pub config: Config,
    /// The document records.
    pub doc_db: DocStateDb,
    /// The last session.
    pub session: Session,
    /// `false` with `--no-restore`.
    pub restore: bool,
}

impl Startup {
    /// Reads the session and the document records under `paths`. Damaged files give empty
    /// values (the store keeps a `.bad` copy).
    pub fn load(paths: Paths, config: Config, restore: bool) -> Startup {
        Startup {
            session: Session::load(&paths.session_file()),
            doc_db: DocStateDb::load(&paths.documents_file()),
            paths: Some(paths),
            config,
            restore,
        }
    }

    /// Nothing remembered, nothing saved.
    #[cfg(test)]
    pub fn empty() -> Startup {
        Startup {
            paths: None,
            config: Config::default(),
            doc_db: DocStateDb::default(),
            session: Session::default(),
            restore: true,
        }
    }
}

/// Puts the start-up values into a fresh `state` and restores the session if allowed. Returns
/// the saver, which knows what the session file holds.
pub fn start(state: &mut AppState, startup: Startup, ctx: &egui::Context) -> Saver {
    let restore = should_restore(startup.restore, &startup.config);
    state.textures =
        crate::views::textures::TextureCache::new((startup.config.texture_cache_mb as usize) << 20);
    let main = state.main_window().id;
    state
        .memory
        .set_initial_geometry(main, main_geometry(&startup.session));
    state.paths = startup.paths;
    state.config = startup.config;
    state.doc_db = startup.doc_db;
    if restore {
        restore_session_logged(state, &startup.session, ctx);
    }
    Saver::new(Some(startup.session))
}

fn restore_session_logged(state: &mut AppState, session: &Session, ctx: &egui::Context) {
    let started = std::time::Instant::now();
    restore(state, session, ctx);
    log::debug!(
        "restored {} windows and {} tabs in {:?}",
        session.windows.len(),
        session.tabs.len(),
        started.elapsed()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, OpenHow, OpenTarget, dispatch};
    use crate::views::test_support::{Repaints, watch_repaints};
    use crate::windows::dock::SplitDir;
    use refdesk_core::testutil::PdfBuilder;
    use refdesk_core::{DefaultZoom, Location, ZoomMode};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A directory under the system temp directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "refdesk-wp41-{}-{}-{name}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir(normalize_path(&path))
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const NOW: i64 = 1_800_000_000;

    fn pdf_bytes(pages: usize, label: &str) -> Vec<u8> {
        let mut builder = PdfBuilder::new();
        for i in 0..pages {
            let text = format!("{label} page {}", i + 1);
            builder = builder.page(200.0, 200.0, move |p| {
                p.text(20.0, 40.0, 12.0, &text);
            });
        }
        builder.build()
    }

    fn write_pdf(path: &Path, pages: usize, label: &str) {
        std::fs::write(path, pdf_bytes(pages, label)).expect("write pdf");
    }

    fn new_state() -> AppState {
        let mut state = AppState::new();
        state.memory.set_clock(Arc::new(|| NOW));
        state
    }

    fn open(state: &mut AppState, ctx: &egui::Context, path: &Path) -> TabId {
        dispatch(
            state,
            Action::OpenFiles(vec![OpenTarget {
                path: path.to_path_buf(),
                page: None,
            }]),
            ctx,
        );
        state.focus.expect("the opened tab has the focus")
    }

    fn pdf(state: &AppState, tab: TabId) -> &PdfTab {
        state.tabs.pdf(tab).expect("a pdf tab")
    }

    fn view_mut(state: &mut AppState, tab: TabId) -> &mut ViewState {
        match state.tabs.get_mut(tab) {
            Some(Tab::Pdf(p)) => &mut p.view,
            _ => panic!("not a pdf tab"),
        }
    }

    fn doc_of(state: &AppState, tab: TabId) -> DocumentHandle {
        state
            .docs
            .get(pdf(state, tab).doc)
            .expect("document")
            .clone()
    }

    /// Waits for `check` like `Repaints::wait_until`, running an empty frame before each check:
    /// egui calls the repaint callback only for the first request after a frame, so without
    /// frames a second worker's request would never arrive.
    fn wait(ctx: &egui::Context, repaints: &Repaints, what: &str, mut check: impl FnMut() -> bool) {
        repaints.wait_until(what, || {
            ctx.run_ui(egui::RawInput::default(), |_| {})
                .drop_without_applying_deltas();
            check()
        });
    }

    fn wait_for_fingerprint(
        ctx: &egui::Context,
        state: &mut AppState,
        repaints: &Repaints,
        path: &Path,
    ) {
        wait(ctx, repaints, "the fingerprint", || {
            poll_fingerprints(state, ctx);
            state.memory.fingerprint_of(path).is_some()
        });
    }

    fn frame(ctx: &egui::Context, state: &mut AppState) -> Vec<Action> {
        let mut queue = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 700.0),
            )),
            ..Default::default()
        };
        ctx.run_ui(input, |ui| {
            crate::windows::show_window(ui, state, 0, &mut queue)
        })
        .drop_without_applying_deltas();
        queue
    }

    // --- save timer --------------------------------------------------------------------------

    #[test]
    fn the_save_timer_saves_two_seconds_after_the_last_change() {
        let mut timer = SaveTimer::default();
        assert_eq!(timer.update(true, true, 10.0), Decision::Wait(SAVE_DELAY));
        assert_eq!(
            timer.update(true, false, 11.0),
            Decision::Wait(Duration::from_secs(1))
        );
        // Another change starts the wait over: a run of changes is saved once.
        assert_eq!(timer.update(true, true, 11.5), Decision::Wait(SAVE_DELAY));
        assert!(matches!(timer.update(true, false, 13.4), Decision::Wait(_)));
        assert_eq!(timer.update(true, false, 13.5), Decision::Save);
        timer.saved();
        assert_eq!(timer.update(false, false, 14.0), Decision::Idle);
    }

    #[test]
    fn the_save_timer_does_nothing_without_a_difference() {
        let mut timer = SaveTimer::default();
        for t in 0..100 {
            assert_eq!(
                timer.update(false, t % 7 == 0, f64::from(t)),
                Decision::Idle
            );
        }
        // Changed and changed back before the delay: nothing to save.
        assert!(matches!(timer.update(true, true, 200.0), Decision::Wait(_)));
        assert_eq!(timer.update(false, true, 201.0), Decision::Idle);
        assert_eq!(timer.update(false, false, 205.0), Decision::Idle);
        // A failed save is tried again after another delay.
        assert!(matches!(timer.update(true, true, 300.0), Decision::Wait(_)));
        assert_eq!(timer.update(true, false, 302.0), Decision::Save);
        timer.failed(302.0);
        assert!(matches!(
            timer.update(true, false, 303.0),
            Decision::Wait(_)
        ));
        assert_eq!(timer.update(true, false, 304.0), Decision::Save);
    }

    #[test]
    fn the_saver_writes_after_the_delay_and_on_exit_and_the_files_read_back_equal() {
        let dir = TempDir::new("saver");
        let paths = Paths::under(&dir.0);
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = new_state();
        state.paths = Some(paths.clone());
        let mut saver = Saver::new(Some(to_session(&state)));

        assert_eq!(saver.tick(&mut state, 0.0), None, "nothing changed");
        assert!(!paths.session_file().exists());

        let file = dir.join("a.pdf");
        write_pdf(&file, 3, "a");
        let tab = open(&mut state, &ctx, &file);
        assert_eq!(saver.tick(&mut state, 10.0), Some(SAVE_DELAY));
        assert_eq!(saver.tick(&mut state, 11.0), Some(Duration::from_secs(1)));
        view_mut(&mut state, tab).zoom = ZoomMode::Custom(1.5);
        assert_eq!(saver.tick(&mut state, 11.5), Some(SAVE_DELAY));
        assert!(saver.tick(&mut state, 13.4).is_some());
        assert!(!paths.session_file().exists(), "not before two seconds");
        assert_eq!(saver.tick(&mut state, 13.5), None);
        assert!(paths.session_file().exists());
        assert!(paths.documents_file().exists());
        assert_eq!(Session::load(&paths.session_file()), to_session(&state));
        let db = DocStateDb::load(&paths.documents_file());
        let rec = db.lookup(&file, None).expect("a record");
        assert_eq!(rec.view.zoom, ZoomMode::Custom(1.5));
        assert_eq!(rec.last_opened, NOW);

        // Unchanged: nothing is written again.
        std::fs::remove_file(paths.session_file()).expect("remove");
        assert_eq!(saver.tick(&mut state, 20.0), None);
        assert_eq!(saver.tick(&mut state, 30.0), None);
        assert!(!paths.session_file().exists());

        // A finished fingerprint is a change to save (the record gets it).
        wait_for_fingerprint(&ctx, &mut state, &repaints, &file);
        assert!(saver.tick(&mut state, 40.0).is_some());

        // On exit the change is written at once.
        view_mut(&mut state, tab).anchor = Location::new(2, 0.0, 10.0);
        assert!(saver.save_now(&mut state));
        let db = DocStateDb::load(&paths.documents_file());
        let rec = db.lookup(&file, None).expect("a record");
        assert_eq!(
            rec.fingerprint.as_deref(),
            state.memory.fingerprint_of(&file)
        );
        assert!(rec.fingerprint.is_some() && rec.size > 0);
        let saved = Session::load(&paths.session_file());
        assert_eq!(saved, to_session(&state));
        let SessionTab::Pdf { view, .. } = saved.tabs.values().next().expect("one tab") else {
            panic!("a pdf tab");
        };
        assert_eq!(view.anchor, Location::new(2, 0.0, 10.0));
        if std::env::var_os("REFDESK_PRINT_EXAMPLE").is_some() {
            for f in [paths.session_file(), paths.documents_file()] {
                println!(
                    "--- {}\n{}",
                    f.display(),
                    std::fs::read_to_string(&f).unwrap_or_default()
                );
            }
        }
    }

    #[test]
    fn nothing_is_saved_without_paths() {
        let mut state = new_state();
        let mut saver = Saver::new(None);
        assert_eq!(saver.tick(&mut state, 0.0), None);
        assert!(!saver.save_now(&mut state));
    }

    #[test]
    fn a_damaged_session_file_starts_empty_and_leaves_a_bad_copy() {
        let dir = TempDir::new("damaged");
        let paths = Paths::under(&dir.0);
        std::fs::create_dir_all(&paths.state_dir).expect("mkdir");
        std::fs::write(paths.session_file(), b"{ this is not json").expect("write");
        let ctx = egui::Context::default();
        let startup = Startup::load(paths.clone(), Config::default(), true);
        let mut state = new_state();
        let _saver = start(&mut state, startup, &ctx);
        assert_eq!(state.windows.len(), 1);
        assert_eq!(state.tabs.len(), 1);
        assert!(matches!(
            state.focus.and_then(|t| state.tabs.get(t)),
            Some(Tab::Start)
        ));
        assert!(refdesk_core::store::bad_path(&paths.session_file()).exists());
    }

    // --- session round trip ------------------------------------------------------------------

    fn first_split_mut(node: &mut refdesk_core::LayoutNode) -> Option<&mut f32> {
        match node {
            refdesk_core::LayoutNode::Split { fraction, .. } => Some(fraction),
            refdesk_core::LayoutNode::Tabs { .. } => None,
        }
    }

    #[test]
    fn a_session_round_trips_windows_layouts_views_panels_and_focus() {
        let dir = TempDir::new("roundtrip");
        let ctx = egui::Context::default();
        let files: Vec<PathBuf> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| {
                let p = dir.join(&format!("{n}.pdf"));
                write_pdf(&p, 6, n);
                p
            })
            .collect();
        let mut state = new_state();
        // Main window: [a, b] | [a (2)]; second window: [c, d].
        let a = open(&mut state, &ctx, &files[0]);
        let b = open(&mut state, &ctx, &files[1]);
        dispatch(
            &mut state,
            Action::SplitTab {
                tab: a,
                dir: SplitDir::Right,
            },
            &ctx,
        );
        let a2 = state.focus.expect("split view");
        let c = open(&mut state, &ctx, &files[2]);
        dispatch(&mut state, Action::DetachTab(c), &ctx);
        let d = open(&mut state, &ctx, &files[3]);
        assert_eq!(state.windows.len(), 2);
        assert_eq!(state.tabs.len(), 5);
        assert_eq!(state.docs.len(), 4);
        for (i, tab) in [a, b, a2, c, d].into_iter().enumerate() {
            let view = view_mut(&mut state, tab);
            view.anchor = Location::new(i as u32, 3.0, 10.0 * i as f32);
            view.zoom = ZoomMode::Custom(1.0 + 0.25 * i as f32);
        }
        state.windows[1].toc_open = true;
        state.windows[1].toc_width = 300.0;
        let second = state.windows[1].id;
        state.memory.set_initial_geometry(
            WindowId::MAIN,
            WindowGeom {
                size: (1500.0, 1000.0),
                maximized: true,
            },
        );
        state.memory.set_initial_geometry(
            second,
            WindowGeom {
                size: (900.0, 700.0),
                maximized: false,
            },
        );
        dispatch(&mut state, Action::FocusTab(d), &ctx);
        note_focus(&mut state);
        dispatch(&mut state, Action::FocusTab(b), &ctx);
        note_focus(&mut state);

        let mut session = to_session(&state);
        let fraction = first_split_mut(&mut session.windows[0].layout).expect("main is split");
        assert_eq!(*fraction, 0.5);
        *fraction = 0.3;
        // Through the file and back.
        let file = dir.join("session.json");
        session.save(&file).expect("save");
        let loaded = Session::load(&file);
        assert_eq!(loaded, session);

        let mut restored = new_state();
        restore(&mut restored, &loaded, &ctx);
        assert_eq!(to_session(&restored), loaded);
        assert_eq!(restored.windows.len(), 2);
        assert_eq!(restored.tabs.len(), 5);
        assert_eq!(restored.docs.len(), 4, "one document per file");
        assert_eq!(restored.windows[0].dock.pane_count(), 2);
        assert!(restored.windows[1].toc_open);
        assert_eq!(restored.windows[1].toc_width, 300.0);
        let focus = restored.focus.expect("focus");
        assert_eq!(doc_of(&restored, focus).path(), files[1].as_path());
        assert_eq!(
            pdf(&restored, focus).view.anchor,
            Location::new(1, 3.0, 10.0)
        );
        assert_eq!(restored.windows[0].dock.focused_tab(), Some(focus));
        // The second window's active tab is d, which was focused before b.
        let active = restored.windows[1].dock.focused_tab().expect("active");
        assert_eq!(doc_of(&restored, active).path(), files[3].as_path());
        assert_eq!(
            restored.memory.initial_geometry(restored.windows[1].id),
            Some(WindowGeom {
                size: (900.0, 700.0),
                maximized: false
            })
        );
        assert_eq!(main_geometry(&loaded).size, (1500.0, 1000.0));
        assert!(main_geometry(&loaded).maximized);
    }

    #[test]
    fn the_start_page_is_not_saved_and_an_empty_session_restores_nothing() {
        let ctx = egui::Context::default();
        let state = new_state();
        let session = to_session(&state);
        assert!(session.tabs.is_empty());
        assert_eq!(session.windows.len(), 1);
        let mut restored = new_state();
        restore(&mut restored, &session, &ctx);
        assert_eq!(restored.tabs.len(), 1);
        assert_eq!(restored.windows.len(), 1);
        let mut restored = new_state();
        restore(&mut restored, &Session::default(), &ctx);
        assert_eq!(restored.tabs.len(), 1);
    }

    fn one_tab_session(path: &Path, view: ViewState) -> Session {
        let mut session = Session::default();
        session.tabs.insert(
            "t0".into(),
            SessionTab::Pdf {
                path: path.to_path_buf(),
                view,
            },
        );
        session.windows.push(SessionWindow {
            main: true,
            layout: refdesk_core::LayoutNode::Tabs {
                tabs: vec!["t0".into()],
                active: 0,
            },
            ..SessionWindow::default()
        });
        session
    }

    #[test]
    fn a_missing_file_stays_a_failed_tab_and_try_again_opens_it_once_it_is_back() {
        let dir = TempDir::new("missing");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let path = dir.join("later.pdf");
        let mut view = ViewState::new(path.clone());
        view.anchor = Location::new(2, 0.0, 30.0);
        let session = one_tab_session(&path, view.clone());
        let mut state = new_state();
        restore(&mut state, &session, &ctx);
        let tab = state.focus.expect("the restored tab");
        let doc = doc_of(&state, tab);
        wait(&ctx, &repaints, "the failure", || {
            matches!(doc.status(), DocStatus::Failed(_))
        });
        for _ in 0..2 {
            assert!(frame(&ctx, &mut state).is_empty(), "nothing was clicked");
        }
        // Saved again, the tab is still there with its place.
        let again = to_session(&state);
        assert_eq!(again, session);
        // Closing records nothing for a document that could not be opened.
        record_tab(&mut state, tab);
        assert!(state.doc_db.is_empty());

        write_pdf(&path, 4, "later");
        dispatch(&mut state, Action::RetryOpen(tab), &ctx);
        wait(&ctx, &repaints, "the document", || {
            doc.status() == DocStatus::Ready
        });
        assert_eq!(pdf(&state, tab).view.anchor, Location::new(2, 0.0, 30.0));
    }

    #[test]
    fn a_session_with_a_comparison_tab_restores_it_and_saves_it_again() {
        let ctx = egui::Context::default();
        let mut session = one_tab_session(
            Path::new("/refdesk-test/a.pdf"),
            ViewState::new(PathBuf::from("/refdesk-test/a.pdf")),
        );
        session.tabs.insert(
            "t1".into(),
            SessionTab::Diff {
                left: PathBuf::from("/refdesk-test/old.pdf"),
                right: PathBuf::from("/refdesk-test/new.pdf"),
            },
        );
        if let refdesk_core::LayoutNode::Tabs { tabs, .. } = &mut session.windows[0].layout {
            tabs.push("t1".into());
        }
        let mut state = new_state();
        restore(&mut state, &session, &ctx);
        // The PDF tab and the comparison tab, which opens its two documents.
        assert_eq!(state.tabs.len(), 2);
        assert_eq!(state.docs.len(), 3);
        let diff_tabs = state
            .tabs
            .iter()
            .filter(|(_, tab)| matches!(tab, Tab::Diff(_)))
            .count();
        assert_eq!(diff_tabs, 1);
        // Saved again, the comparison is still there with both paths.
        let saved = to_session(&state);
        assert!(saved.tabs.values().any(|tab| matches!(
            tab,
            SessionTab::Diff { left, right }
                if left.ends_with("old.pdf") && right.ends_with("new.pdf")
        )));
    }

    // --- start-up ----------------------------------------------------------------------------

    #[test]
    fn restore_is_skipped_with_no_restore_or_when_the_setting_is_off() {
        let dir = TempDir::new("no-restore");
        let paths = Paths::under(&dir.0);
        let session = one_tab_session(
            Path::new("/refdesk-test/a.pdf"),
            ViewState::new(PathBuf::from("/refdesk-test/a.pdf")),
        );
        session.save(&paths.session_file()).expect("save");
        let ctx = egui::Context::default();
        let pdf_tabs = |config: Config, restore: bool| {
            let mut state = new_state();
            let _saver = start(
                &mut state,
                Startup::load(paths.clone(), config, restore),
                &ctx,
            );
            state
                .tabs
                .iter()
                .filter(|(_, t)| matches!(t, Tab::Pdf(_)))
                .count()
        };
        assert_eq!(pdf_tabs(Config::default(), true), 1);
        assert_eq!(pdf_tabs(Config::default(), false), 0, "--no-restore");
        let off = Config {
            restore_session: false,
            ..Config::default()
        };
        assert_eq!(pdf_tabs(off, true), 0, "restore_session = false");
        assert!(!should_restore(
            true,
            &Config {
                restore_session: false,
                ..Config::default()
            }
        ));
        assert!(should_restore(true, &Config::default()));
    }

    #[test]
    fn window_sizes_from_a_file_are_made_usable() {
        assert_eq!(sane_size((800.0, 600.0)), [800.0, 600.0]);
        assert_eq!(
            sane_size((10.0, 10.0)),
            [MIN_WINDOW_SIZE.0, MIN_WINDOW_SIZE.1]
        );
        let default = WindowGeom::default().size;
        assert_eq!(sane_size((f32::NAN, f32::INFINITY)), [default.0, default.1]);
        assert_eq!(sane_size((1e9, 1e9)), [MAX_WINDOW_SIZE, MAX_WINDOW_SIZE]);
    }

    // --- per-document memory -----------------------------------------------------------------

    #[test]
    fn closing_and_opening_again_returns_to_the_place_zoom_and_history() {
        let dir = TempDir::new("reopen");
        let ctx = egui::Context::default();
        let file = dir.join("a.pdf");
        write_pdf(&file, 6, "a");
        let mut state = new_state();
        let tab = open(&mut state, &ctx, &file);
        for page in [2, 4] {
            dispatch(
                &mut state,
                Action::Navigate {
                    tab,
                    to: Location::new(page, 0.0, 50.0),
                    how: OpenHow::Here,
                },
                &ctx,
            );
        }
        dispatch(
            &mut state,
            Action::SetZoom {
                tab,
                zoom: ZoomMode::Custom(1.75),
            },
            &ctx,
        );
        let before = pdf(&state, tab).view.clone();
        assert_eq!(before.history.len(), 3);
        dispatch(&mut state, Action::CloseTab(tab), &ctx);
        assert_eq!(state.docs.len(), 0);
        let rec = state.doc_db.lookup(&file, None).expect("recorded on close");
        assert_eq!(rec.view, before);

        let again = open(&mut state, &ctx, &file);
        let view = &pdf(&state, again).view;
        assert_eq!(view, &before);
        assert!(
            view.history.can_back(),
            "Back still leads to the earlier places"
        );
        assert!(
            !state.memory.is_waiting(again),
            "found by path, nothing to wait for"
        );
    }

    #[test]
    fn a_page_request_jumps_from_the_remembered_place() {
        let ctx = egui::Context::default();
        let path = normalize_path(Path::new("/refdesk-test/remembered.pdf"));
        let mut view = ViewState::new(path.clone());
        view.anchor = Location::new(5, 0.0, 0.0);
        view.zoom = ZoomMode::Custom(2.0);
        let mut state = new_state();
        state.doc_db.record(DocRecord {
            path: path.clone(),
            fingerprint: None,
            size: 0,
            mtime: 0,
            last_opened: 1,
            view,
            toc_collapsed: Vec::new(),
        });
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget {
                path: path.clone(),
                page: Some(1),
            }]),
            &ctx,
        );
        let tab = state.focus.expect("tab");
        let view = &pdf(&state, tab).view;
        assert_eq!(view.anchor.page, 1);
        assert_eq!(view.zoom, ZoomMode::Custom(2.0));
        assert!(view.history.can_back());
    }

    #[test]
    fn a_moved_file_gets_its_place_once_the_fingerprint_is_known() {
        let dir = TempDir::new("moved");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let old = dir.join("old.pdf");
        write_pdf(&old, 6, "moved");
        let mut state = new_state();
        let tab = open(&mut state, &ctx, &old);
        wait_for_fingerprint(&ctx, &mut state, &repaints, &old);
        dispatch(
            &mut state,
            Action::Navigate {
                tab,
                to: Location::new(3, 0.0, 40.0),
                how: OpenHow::Here,
            },
            &ctx,
        );
        view_mut(&mut state, tab).zoom = ZoomMode::Custom(2.0);
        toc_paths(&mut state, tab, vec![vec!["Chapter".to_owned()]]);
        dispatch(&mut state, Action::CloseTab(tab), &ctx);
        assert!(
            state
                .doc_db
                .lookup(&old, None)
                .and_then(|r| r.fingerprint.clone())
                .is_some()
        );

        std::fs::create_dir_all(dir.join("elsewhere")).expect("mkdir");
        let new = dir.join("elsewhere/renamed.pdf");
        std::fs::rename(&old, &new).expect("move");
        let tab = open(&mut state, &ctx, &new);
        assert_eq!(pdf(&state, tab).view.anchor.page, 0, "not known by path");
        assert!(state.memory.is_waiting(tab));
        wait(&ctx, &repaints, "the remembered place", || {
            poll_fingerprints(&mut state, &ctx);
            !state.memory.is_waiting(tab)
        });
        let view = &pdf(&state, tab).view;
        assert_eq!(view.anchor, Location::new(3, 0.0, 40.0));
        assert_eq!(view.zoom, ZoomMode::Custom(2.0));
        assert_eq!(view.history.current().doc, new, "history follows the file");
        assert!(view.history.can_back());
        // The record under the new path keeps the folded contents.
        dispatch(&mut state, Action::CloseTab(tab), &ctx);
        let rec = state.doc_db.lookup(&new, None).expect("recorded");
        assert_eq!(rec.path, new);
        assert_eq!(rec.toc_collapsed, vec![vec!["Chapter".to_owned()]]);
    }

    fn toc_paths(state: &mut AppState, tab: TabId, paths: Vec<Vec<String>>) {
        if let Some(Tab::Pdf(p)) = state.tabs.get_mut(tab) {
            p.toc.set_collapsed_paths(paths);
        }
    }

    #[test]
    fn a_moved_file_keeps_the_readers_scroll_when_it_moved_first() {
        let dir = TempDir::new("moved-scrolled");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let old = dir.join("old.pdf");
        write_pdf(&old, 6, "scrolled");
        let mut state = new_state();
        let tab = open(&mut state, &ctx, &old);
        wait_for_fingerprint(&ctx, &mut state, &repaints, &old);
        view_mut(&mut state, tab).anchor = Location::new(4, 0.0, 0.0);
        dispatch(&mut state, Action::CloseTab(tab), &ctx);

        let new = dir.join("new-name.pdf");
        std::fs::rename(&old, &new).expect("move");
        let tab = open(&mut state, &ctx, &new);
        // The reader scrolls before the fingerprint is in.
        view_mut(&mut state, tab).anchor = Location::new(1, 0.0, 120.0);
        wait(&ctx, &repaints, "the fingerprint", || {
            poll_fingerprints(&mut state, &ctx);
            !state.memory.is_waiting(tab)
        });
        assert_eq!(pdf(&state, tab).view.anchor, Location::new(1, 0.0, 120.0));
    }

    #[test]
    fn a_new_version_at_the_same_path_keeps_the_place_within_its_pages() {
        let dir = TempDir::new("new-version");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let file = dir.join("spec.pdf");
        write_pdf(&file, 10, "v1");
        let mut state = new_state();
        let tab = open(&mut state, &ctx, &file);
        view_mut(&mut state, tab).anchor = Location::new(8, 0.0, 20.0);
        dispatch(&mut state, Action::CloseTab(tab), &ctx);

        write_pdf(&file, 3, "version two, different text");
        let tab = open(&mut state, &ctx, &file);
        assert_eq!(pdf(&state, tab).view.anchor, Location::new(8, 0.0, 20.0));
        let doc = doc_of(&state, tab);
        wait(&ctx, &repaints, "the document info", || {
            doc.info().is_some()
        });
        for _ in 0..3 {
            frame(&ctx, &mut state);
        }
        let page = pdf(&state, tab).view.anchor.page;
        assert!((1..3).contains(&page), "page {page} is inside the 3 pages");
    }

    #[test]
    fn fitting_a_view_moves_only_places_past_the_end() {
        let mut view = ViewState::new(PathBuf::from("/a.pdf"));
        view.anchor = Location::new(8, 5.0, 20.0);
        fit_view_to_document(&mut view, 10);
        assert_eq!(view.anchor, Location::new(8, 5.0, 20.0));
        fit_view_to_document(&mut view, 3);
        assert_eq!(view.anchor, Location::new(2, 0.0, 0.0));
        // A document without pages leaves the place alone.
        view.anchor = Location::new(4, 0.0, 0.0);
        fit_view_to_document(&mut view, 0);
        assert_eq!(view.anchor.page, 4);
    }

    #[test]
    fn matching_size_and_time_reuse_the_recorded_fingerprint() {
        let dir = TempDir::new("reuse");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let file = dir.join("a.pdf");
        write_pdf(&file, 2, "reuse");
        let stamp = refdesk_core::stamp(&file).expect("stamp");
        let fake = "0123456789abcdef0123456789abcdef";
        let record = |size: u64| DocRecord {
            path: file.clone(),
            fingerprint: Some(fake.to_owned()),
            size,
            mtime: stamp.mtime,
            last_opened: 1,
            view: ViewState::new(file.clone()),
            toc_collapsed: Vec::new(),
        };
        let mut state = new_state();
        state.doc_db.record(record(stamp.size));
        open(&mut state, &ctx, &file);
        wait_for_fingerprint(&ctx, &mut state, &repaints, &file);
        assert_eq!(
            state.memory.fingerprint_of(&file),
            Some(fake),
            "not read again"
        );

        // A different size means the file changed: it is hashed.
        let mut state = new_state();
        state.doc_db.record(record(stamp.size + 1));
        open(&mut state, &ctx, &file);
        wait_for_fingerprint(&ctx, &mut state, &repaints, &file);
        let real = fingerprint_file(&file).expect("hash");
        assert_eq!(state.memory.fingerprint_of(&file), Some(real.as_str()));
    }

    #[test]
    fn untouched_allows_layout_nudges_but_not_scrolling_zoom_or_jumps() {
        let start = ViewState::new(PathBuf::from("/a.pdf"));
        let mut nudged = start.clone();
        nudged.anchor = Location::new(0, -12.0, 3.0);
        assert!(untouched(&start, &nudged));
        let mut scrolled = start.clone();
        scrolled.anchor.y = 200.0;
        assert!(!untouched(&start, &scrolled));
        let mut paged = start.clone();
        paged.anchor.page = 1;
        assert!(!untouched(&start, &paged));
        let mut zoomed = start.clone();
        zoomed.zoom = ZoomMode::Custom(1.2);
        assert!(!untouched(&start, &zoomed));
        let mut jumped = start.clone();
        jumped.history.jump(
            Place {
                doc: PathBuf::from("/a.pdf"),
                loc: Location::new(0, 0.0, 0.0),
            },
            Place {
                doc: PathBuf::from("/a.pdf"),
                loc: Location::new(0, 0.0, 10.0),
            },
        );
        // A jump to the same spot adds nothing; to another page it does.
        jumped.history.jump(
            Place {
                doc: PathBuf::from("/a.pdf"),
                loc: Location::new(0, 0.0, 0.0),
            },
            Place {
                doc: PathBuf::from("/a.pdf"),
                loc: Location::new(3, 0.0, 0.0),
            },
        );
        assert!(!untouched(&start, &jumped));
    }

    #[test]
    fn rebasing_a_history_moves_every_place_and_keeps_the_position() {
        let old = PathBuf::from("/old/a.pdf");
        let new = PathBuf::from("/new/b.pdf");
        let at = |page| Place {
            doc: old.clone(),
            loc: Location::new(page, 0.0, 0.0),
        };
        let mut history = History::new(at(0));
        history.jump(at(0), at(3));
        history.jump(at(3), at(7));
        let _ = history.back(at(7));
        let moved = rebase_history(&history, &new);
        assert_eq!(moved.len(), 3);
        assert_eq!(moved.current().doc, new);
        assert_eq!(moved.current().loc.page, 3);
        assert!(moved.can_back() && moved.can_forward());
        // Already pointing at the document: unchanged.
        assert_eq!(rebase_history(&history, &old), history);
    }

    #[test]
    fn the_most_recently_focused_tab_is_recorded_for_its_document() {
        let dir = TempDir::new("shared");
        let ctx = egui::Context::default();
        let path = dir.join("shared.pdf");
        write_pdf(&path, 5, "shared");
        let path = path.as_path();
        let mut state = new_state();
        let first = open(&mut state, &ctx, path);
        view_mut(&mut state, first).anchor = Location::new(1, 0.0, 0.0);
        note_focus(&mut state);
        dispatch(&mut state, Action::DuplicateTab(first), &ctx);
        let second = state.focus.expect("copy");
        view_mut(&mut state, second).anchor = Location::new(3, 0.0, 0.0);
        note_focus(&mut state);
        dispatch(&mut state, Action::FocusTab(first), &ctx);
        record_open_documents(&mut state);
        let recorded = |state: &AppState| {
            state
                .doc_db
                .lookup(&normalize_path(path), None)
                .expect("record")
                .view
                .anchor
                .page
        };
        assert_eq!(recorded(&state), 1);
        dispatch(&mut state, Action::FocusTab(second), &ctx);
        record_open_documents(&mut state);
        assert_eq!(recorded(&state), 3);
    }

    #[test]
    fn the_default_zoom_applies_to_new_documents_only() {
        let ctx = egui::Context::default();
        let mut state = new_state();
        state.config.default_zoom = DefaultZoom::Percent(150.0);
        let tab = open(&mut state, &ctx, Path::new("/refdesk-test/new.pdf"));
        assert_eq!(pdf(&state, tab).view.zoom, ZoomMode::Custom(1.5));

        let known = normalize_path(Path::new("/refdesk-test/known.pdf"));
        let mut view = ViewState::new(known.clone());
        view.zoom = ZoomMode::FitPage;
        state.doc_db.record(DocRecord {
            path: known.clone(),
            fingerprint: None,
            size: 0,
            mtime: 0,
            last_opened: 1,
            view,
            toc_collapsed: Vec::new(),
        });
        let tab = open(&mut state, &ctx, &known);
        assert_eq!(pdf(&state, tab).view.zoom, ZoomMode::FitPage);
    }

    // --- recent documents --------------------------------------------------------------------

    fn record_at(state: &mut AppState, path: &Path, last_opened: i64) {
        state.doc_db.record(DocRecord {
            path: path.to_path_buf(),
            fingerprint: None,
            size: 0,
            mtime: 0,
            last_opened,
            view: ViewState::new(path.to_path_buf()),
            toc_collapsed: Vec::new(),
        });
    }

    #[test]
    fn recent_documents_are_newest_first_at_most_twenty_and_mark_missing_files() {
        let dir = TempDir::new("recent");
        let mut state = new_state();
        for i in 0..25 {
            let path = dir.join(&format!("doc{i:02}.pdf"));
            if i % 2 == 0 {
                std::fs::write(&path, b"x").expect("write");
            }
            record_at(&mut state, &path, 1000 + i);
        }
        let entries = recent_entries(&mut state);
        assert_eq!(entries.len(), RECENT_COUNT);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names[0], "doc24.pdf");
        assert_eq!(names[19], "doc05.pdf");
        for entry in entries.iter() {
            let n: i64 = entry.name[3..5].parse().expect("number");
            assert_eq!(entry.exists, n % 2 == 0, "{}", entry.name);
            assert_eq!(entry.last_opened, 1000 + n);
            assert_eq!(entry.folder, dir.0.display().to_string());
        }
        let paths = recent_document_paths(&state);
        assert_eq!(
            paths,
            entries.iter().map(|e| e.path.clone()).collect::<Vec<_>>()
        );
        // Built once; a new record makes a new list.
        assert!(Arc::ptr_eq(&entries, &recent_entries(&mut state)));
        let newest = dir.join("newest.pdf");
        record_at(&mut state, &newest, 5000);
        state.memory.recent = None;
        assert_eq!(recent_entries(&mut state)[0].path, newest);
    }

    #[test]
    fn the_start_page_draws_the_recent_documents_without_panicking() {
        let dir = TempDir::new("start-page");
        let ctx = egui::Context::default();
        let mut state = new_state();
        let present = dir.join("present.pdf");
        std::fs::write(&present, b"x").expect("write");
        record_at(&mut state, &present, NOW - 3600);
        record_at(&mut state, &dir.join("gone.pdf"), NOW - 86400 * 3);
        for _ in 0..3 {
            assert!(frame(&ctx, &mut state).is_empty(), "nothing was clicked");
        }
        let page = start_page(&mut state, &ctx);
        assert_eq!(page.recent.len(), 2);
        assert!(page.recent[0].exists && !page.recent[1].exists);
        assert_eq!(page.now, NOW);
        // Only commands the table has are listed, with their shortcut.
        assert!(page.shortcuts.len() <= START_PAGE_COMMANDS.len());
        let open = crate::commands::command("file.open").expect("file.open");
        assert!(
            page.shortcuts
                .iter()
                .any(|(name, _)| name == tr(open.name_key))
        );
    }

    #[test]
    fn clicking_a_recent_document_opens_it() {
        use egui_kittest::kittest::Queryable;
        let page = StartPage {
            recent: Arc::new(vec![
                RecentEntry::new(Path::new("/refdesk-test/dir/first.pdf"), NOW - 60, true),
                RecentEntry::new(Path::new("/refdesk-test/second.pdf"), NOW - 600, false),
            ]),
            shortcuts: vec![("Open".to_owned(), "Ctrl+O".to_owned())],
            now: NOW,
        };
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, queue: &mut Vec<Action>| {
                crate::views::start_page::show(ui, &page, queue);
            },
            Vec::new(),
        );
        harness.run();
        harness.get_by_label("second.pdf").click();
        harness.run();
        assert_eq!(
            harness.state(),
            &vec![Action::OpenFiles(vec![OpenTarget {
                path: PathBuf::from("/refdesk-test/second.pdf"),
                page: None,
            }])]
        );
    }
}
