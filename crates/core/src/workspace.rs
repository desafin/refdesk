//! The workspace folder: a scan that lists the folders and PDF files below a root, a background
//! runner for it, and a watcher that tells when the folder changed. See `docs/ARCHITECTURE.md`
//! §4.13.
//!
//! Nothing here knows about the screen. The scan reads the disk, so the app runs it through
//! [`WorkspaceScan`] (R-1) and shows [`WorkspaceIndex`] snapshots.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Config, EventKind, EventKindMask, RecommendedWatcher, RecursiveMode, Watcher};

use crate::engine::Notifier;

/// What an entry of the index is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A folder (one that has a PDF file somewhere below it).
    Dir,
    /// A file whose name ends in `.pdf` (any case).
    Pdf,
}

/// One folder or PDF file below the workspace root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// The file name (for display; a name that is not UTF-8 is shown lossily).
    pub name: String,
    /// The path below the root, through the names as they are in the tree (a symbolic link is
    /// not resolved).
    pub rel_path: PathBuf,
    /// Folder or PDF file.
    pub kind: EntryKind,
    /// Index into [`WorkspaceIndex::entries`] of the folder this entry is in; `None` for the
    /// top level.
    pub parent: Option<u32>,
    /// Number of folders between the root and this entry: 0 for the top level.
    pub depth: u16,
}

/// The folders and PDF files below a root, as drawn in a tree.
///
/// `entries` is in pre-order: the order of the tree drawn from top to bottom with every folder
/// expanded. Inside one folder the sub-folders come first, then the files, each by
/// [`natural_cmp`] of the name. A folder with no PDF file anywhere below it is not in the list
/// (unless the scan was asked to keep them, [`ScanOptions`]), and neither is anything whose name
/// starts with `.`. The root itself is not an entry.
///
/// The lookups ([`children`], [`find`], ...) are built when the index is made. Do not change
/// `entries` afterwards.
///
/// [`children`]: WorkspaceIndex::children
/// [`find`]: WorkspaceIndex::find
#[derive(Clone, Debug)]
pub struct WorkspaceIndex {
    /// The folder that was scanned, as it was given to the scan.
    pub root: PathBuf,
    /// The entries in pre-order.
    pub entries: Vec<WorkspaceEntry>,
    /// Whether the scan stopped at a limit and the list is therefore incomplete.
    pub truncated: bool,
    /// `root` with `.` and `..` resolved by name.
    norm_root: PathBuf,
    /// The real path of `root`, when it differs from `norm_root`.
    real_root: Option<PathBuf>,
    top: Vec<u32>,
    children: Vec<Vec<u32>>,
    by_path: HashMap<PathBuf, u32>,
    files: usize,
    dirs: usize,
}

impl WorkspaceIndex {
    /// Builds an index (and its lookups) from entries that are already in pre-order with valid
    /// `parent` indexes. [`scan`] does this; the app uses it in tests that need an index
    /// without a disk.
    pub fn new(root: PathBuf, entries: Vec<WorkspaceEntry>, truncated: bool) -> WorkspaceIndex {
        WorkspaceIndex::build(root, None, entries, truncated)
    }

    fn build(
        root: PathBuf,
        real_root: Option<PathBuf>,
        entries: Vec<WorkspaceEntry>,
        truncated: bool,
    ) -> WorkspaceIndex {
        let n = entries.len();
        let mut top = Vec::new();
        let mut children: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut by_path = HashMap::with_capacity(n);
        let (mut files, mut dirs) = (0, 0);
        for (i, entry) in entries.iter().enumerate() {
            let idx = i as u32;
            match entry.parent {
                None => top.push(idx),
                Some(p) => {
                    if let Some(list) = children.get_mut(p as usize) {
                        list.push(idx);
                    }
                }
            }
            if let Some(key) = normalize(&entry.rel_path) {
                by_path.insert(key, idx);
            }
            match entry.kind {
                EntryKind::Dir => dirs += 1,
                EntryKind::Pdf => files += 1,
            }
        }
        let norm_root = normalize(&root).unwrap_or_else(|| root.clone());
        let real_root = real_root.filter(|r| *r != norm_root);
        WorkspaceIndex {
            root,
            entries,
            truncated,
            norm_root,
            real_root,
            top,
            children,
            by_path,
            files,
            dirs,
        }
    }

    /// The entry at `idx`, if there is one.
    pub fn entry(&self, idx: u32) -> Option<&WorkspaceEntry> {
        self.entries.get(idx as usize)
    }

    /// The entries directly inside `parent` (`None`: directly inside the root) in display order.
    /// An index out of range has no children.
    pub fn children(&self, parent: Option<u32>) -> Vec<u32> {
        self.children_of(parent).to_vec()
    }

    /// [`children`](WorkspaceIndex::children) without the copy.
    pub fn children_of(&self, parent: Option<u32>) -> &[u32] {
        match parent {
            None => &self.top,
            Some(p) => self.children.get(p as usize).map_or(&[], Vec::as_slice),
        }
    }

    /// Whether the entry has entries inside it.
    pub fn has_children(&self, idx: u32) -> bool {
        self.children
            .get(idx as usize)
            .is_some_and(|c| !c.is_empty())
    }

    /// The folders that contain the entry, the nearest first (the last one is in the top level).
    pub fn ancestors(&self, idx: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut current = self.entry(idx).and_then(|e| e.parent);
        while let Some(p) = current {
            // A parent always comes before its child, so this ends; the guard keeps a
            // hand-made index with a bad `parent` from looping.
            if out.len() > self.entries.len() {
                break;
            }
            out.push(p);
            current = self.entry(p).and_then(|e| e.parent);
        }
        out
    }

    /// The path of the entry on disk: `root` joined with its relative path. An index out of
    /// range gives `root`.
    pub fn abs_path(&self, idx: u32) -> PathBuf {
        match self.entry(idx) {
            Some(e) => self.root.join(&e.rel_path),
            None => self.root.clone(),
        }
    }

    /// The entry for a path, given absolute (below the root) or relative to the root. `.` and
    /// `..` are resolved by name; the disk is not read, so a symbolic link is not resolved and
    /// the path need not exist. `None` for the root itself, a path outside the root and a path
    /// that is not in the index.
    pub fn find(&self, path: &Path) -> Option<u32> {
        let normal = normalize(path)?;
        let rel = if normal.is_absolute() {
            match normal.strip_prefix(&self.norm_root) {
                Ok(rel) => rel,
                Err(_) => normal.strip_prefix(self.real_root.as_ref()?).ok()?,
            }
        } else {
            normal.as_path()
        };
        self.by_path.get(rel).copied()
    }

    /// The PDF entries in document order.
    pub fn files(&self) -> impl Iterator<Item = u32> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == EntryKind::Pdf)
            .map(|(i, _)| i as u32)
    }

    /// Number of PDF entries.
    pub fn file_count(&self) -> usize {
        self.files
    }

    /// Number of folder entries.
    pub fn dir_count(&self) -> usize {
        self.dirs
    }
}

/// Resolves `.` and `..` by name. `None` for a relative path that climbs above its start.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    return None;
                }
            }
        }
    }
    Some(out)
}

/// Compares two names for the explorer: letters without regard to case, and a run of digits as a
/// number (`doc2` < `doc10` < `doc10a`). Names that differ only in case, or in leading zeros of
/// a number, are ordered by what remains (`a1` before `a01`); names equal apart from case compare
/// as `Equal`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    loop {
        match (i < ab.len(), j < bb.len()) {
            (false, false) => return Ordering::Equal,
            (false, true) => return Ordering::Less,
            (true, false) => return Ordering::Greater,
            (true, true) => {}
        }
        if ab[i].is_ascii_digit() && bb[j].is_ascii_digit() {
            let (ie, je) = (digits_end(ab, i), digits_end(bb, j));
            let (ra, rb) = (&a[i..ie], &b[j..je]);
            let (ta, tb) = (ra.trim_start_matches('0'), rb.trim_start_matches('0'));
            let order = ta
                .len()
                .cmp(&tb.len())
                .then_with(|| ta.cmp(tb))
                .then_with(|| ra.len().cmp(&rb.len()));
            if order != Ordering::Equal {
                return order;
            }
            (i, j) = (ie, je);
        } else {
            let (Some(ca), Some(cb)) = (a[i..].chars().next(), b[j..].chars().next()) else {
                return Ordering::Equal; // not reachable: `i` and `j` are inside the strings
            };
            let order = fold(ca).cmp(&fold(cb));
            if order != Ordering::Equal {
                return order;
            }
            i += ca.len_utf8();
            j += cb.len_utf8();
        }
    }
}

fn digits_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..]
        .iter()
        .position(|b| !b.is_ascii_digit())
        .map_or(bytes.len(), |n| from + n)
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Limits that keep a scan of a huge tree from running away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanLimits {
    /// The scan stops with `truncated` when there would be more entries than this.
    pub max_entries: usize,
    /// Entries are listed down to this depth (0 is the top level); folders deeper than that are
    /// not entered and `truncated` is set.
    pub max_depth: u16,
}

impl Default for ScanLimits {
    fn default() -> ScanLimits {
        ScanLimits {
            max_entries: 100_000,
            max_depth: 16,
        }
    }
}

/// What a scan lists besides PDFs and the folders above them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanOptions {
    /// Folders without any PDF below them are listed too (a project's explorer shows the folders
    /// the user made before anything is imported into them). Names starting with `.` are still
    /// left out.
    pub keep_empty_dirs: bool,
}

/// One thing found in a folder, before it is put in the index.
struct Child {
    name: OsString,
    /// `name` for display and for sorting.
    label: String,
    is_dir: bool,
    /// For a symbolic link to a folder: the real path of the folder.
    link_target: Option<PathBuf>,
}

fn is_pdf_name(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

/// The folders and PDF files directly inside `dir`, folders first, in display order.
fn list_dir(dir: &Path) -> io::Result<Vec<Child>> {
    let mut out = Vec::new();
    for item in fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let name = item.file_name();
        if name.as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        let Ok(file_type) = item.file_type() else {
            continue;
        };
        let (is_dir, link_target) = if file_type.is_dir() {
            (true, None)
        } else if file_type.is_file() {
            if !is_pdf_name(&name) {
                continue;
            }
            (false, None)
        } else if file_type.is_symlink() {
            let path = item.path();
            // A broken link has no metadata.
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                match fs::canonicalize(&path) {
                    Ok(real) => (true, Some(real)),
                    Err(_) => continue,
                }
            } else if meta.is_file() {
                // The link's own name counts, and so does the name of what it points to.
                let pdf = is_pdf_name(&name)
                    || fs::canonicalize(&path).is_ok_and(|real| is_pdf_name(real.as_os_str()));
                if !pdf {
                    continue;
                }
                (false, None)
            } else {
                continue;
            }
        } else {
            continue;
        };
        let label = name.to_string_lossy().into_owned();
        out.push(Child {
            name,
            label,
            is_dir,
            link_target,
        });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| natural_cmp(&a.label, &b.label))
            .then_with(|| a.label.cmp(&b.label))
    });
    Ok(out)
}

fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "the folder scan was cancelled")
}

struct Scanner<'a> {
    limits: &'a ScanLimits,
    keep_empty_dirs: bool,
    max_entries: usize,
    cancel: &'a AtomicBool,
    entries: Vec<WorkspaceEntry>,
    truncated: bool,
    /// A limit was hit: the walk ends.
    stop: bool,
    /// Real paths of the folders entered so far (the ones being walked and the finished ones).
    visited: HashSet<PathBuf>,
}

impl Scanner<'_> {
    /// Adds what is in `dir` (already listed) below `parent`. `real` is the real path of `dir`
    /// and `rel` its path below the root. Whether a PDF file was added somewhere below is the
    /// result.
    fn walk(
        &mut self,
        dir: &Path,
        real: &Path,
        rel: &Path,
        parent: Option<u32>,
        depth: u16,
        listing: Vec<Child>,
    ) -> io::Result<bool> {
        if self.cancel.load(AtomicOrdering::Relaxed) {
            return Err(interrupted());
        }
        let mut found = false;
        for child in listing {
            if self.stop {
                break;
            }
            if self.entries.len() >= self.max_entries {
                self.truncated = true;
                self.stop = true;
                break;
            }
            let child_rel = rel.join(&child.name);
            if !child.is_dir {
                self.entries.push(WorkspaceEntry {
                    name: child.label,
                    rel_path: child_rel,
                    kind: EntryKind::Pdf,
                    parent,
                    depth,
                });
                found = true;
                continue;
            }
            let child_real = match &child.link_target {
                Some(target) => {
                    if self.visited.contains(target) {
                        continue; // a way back into a folder already walked
                    }
                    target.clone()
                }
                None => real.join(&child.name),
            };
            if depth >= self.limits.max_depth {
                self.truncated = true;
                continue;
            }
            let child_abs = dir.join(&child.name);
            self.visited.insert(child_real.clone());
            // The folder is added first so that it comes before what is in it, and taken out
            // again if it turns out to hold no PDF file.
            let idx = self.entries.len();
            self.entries.push(WorkspaceEntry {
                name: child.label,
                rel_path: child_rel.clone(),
                kind: EntryKind::Dir,
                parent,
                depth,
            });
            let has_pdf = match list_dir(&child_abs) {
                Ok(inner) => self.walk(
                    &child_abs,
                    &child_real,
                    &child_rel,
                    Some(idx as u32),
                    depth + 1,
                    inner,
                )?,
                Err(e) => {
                    log::debug!("workspace scan: skipping {}: {e}", child_abs.display());
                    false
                }
            };
            if has_pdf {
                found = true;
            } else if !self.keep_empty_dirs {
                self.entries.truncate(idx);
            }
        }
        Ok(found)
    }
}

/// Lists the folders and PDF files below `root`.
///
/// Files that are not PDF (by extension, case ignored; the content is not read), names that
/// start with `.`, folders without any PDF file below them, broken links and folders that cannot
/// be read are left out. A link to a file is included when it or its target is named `*.pdf`; a
/// link to a folder is followed only if the real path of the folder has not been entered yet,
/// so a link back to an ancestor is not followed. See [`WorkspaceIndex`] for the order.
///
/// `limits` bound the work; when one is reached `truncated` is set on the result. `cancel` is
/// looked at for every folder, and a scan that sees it set ends with
/// [`io::ErrorKind::Interrupted`].
///
/// # Errors
///
/// `root` does not exist or cannot be read, or the scan was cancelled.
pub fn scan(root: &Path, limits: &ScanLimits, cancel: &AtomicBool) -> io::Result<WorkspaceIndex> {
    scan_with(root, limits, ScanOptions::default(), cancel)
}

/// [`scan`] with `options`: with [`ScanOptions::keep_empty_dirs`] a folder without any PDF below
/// it is listed as well (in the same place a folder with PDFs would be).
///
/// # Errors
///
/// As [`scan`].
pub fn scan_with(
    root: &Path,
    limits: &ScanLimits,
    options: ScanOptions,
    cancel: &AtomicBool,
) -> io::Result<WorkspaceIndex> {
    if cancel.load(AtomicOrdering::Relaxed) {
        return Err(interrupted());
    }
    let real = fs::canonicalize(root)?;
    let listing = list_dir(root)?;
    let mut scanner = Scanner {
        limits,
        keep_empty_dirs: options.keep_empty_dirs,
        max_entries: limits.max_entries.min(u32::MAX as usize),
        cancel,
        entries: Vec::new(),
        truncated: false,
        stop: false,
        visited: HashSet::from([real.clone()]),
    };
    scanner.walk(root, &real, Path::new(""), None, 0, listing)?;
    Ok(WorkspaceIndex::build(
        root.to_path_buf(),
        Some(real),
        scanner.entries,
        scanner.truncated,
    ))
}

// ---------------------------------------------------------------------------------------------
// The scan in the background

struct ScanShared {
    root: PathBuf,
    limits: ScanLimits,
    options: ScanOptions,
    notifier: Notifier,
    cancel: AtomicBool,
    state: Mutex<ScanState>,
}

#[derive(Default)]
struct ScanState {
    index: Option<Arc<WorkspaceIndex>>,
    error: Option<String>,
    scanning: bool,
    /// [`WorkspaceScan::rescan`] was called while a scan was running.
    again: bool,
}

impl ScanShared {
    fn state(&self) -> std::sync::MutexGuard<'_, ScanState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(AtomicOrdering::Relaxed)
    }
}

/// A workspace scan running on its own thread. Dropping it cancels the scan; the drop does not
/// wait for the thread, so it is safe on the UI thread.
pub struct WorkspaceScan {
    shared: Arc<ScanShared>,
}

impl std::fmt::Debug for WorkspaceScan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceScan")
            .field("root", &self.shared.root)
            .finish_non_exhaustive()
    }
}

impl Drop for WorkspaceScan {
    fn drop(&mut self) {
        self.shared.cancel.store(true, AtomicOrdering::Relaxed);
    }
}

impl WorkspaceScan {
    /// Starts scanning `root` and returns at once. `notifier` is called each time a scan has
    /// finished, whether it worked or failed (never after the scan was dropped).
    pub fn start(root: PathBuf, limits: ScanLimits, notifier: Notifier) -> WorkspaceScan {
        WorkspaceScan::start_with(root, limits, ScanOptions::default(), notifier)
    }

    /// [`WorkspaceScan::start`] with scan options (see [`scan_with`]).
    pub fn start_with(
        root: PathBuf,
        limits: ScanLimits,
        options: ScanOptions,
        notifier: Notifier,
    ) -> WorkspaceScan {
        let scan = WorkspaceScan {
            shared: Arc::new(ScanShared {
                root,
                limits,
                options,
                notifier,
                cancel: AtomicBool::new(false),
                state: Mutex::new(ScanState::default()),
            }),
        };
        scan.rescan();
        scan
    }

    /// The folder being scanned.
    pub fn root(&self) -> &Path {
        &self.shared.root
    }

    /// The result of the latest scan that worked. While another scan runs, or after one failed,
    /// this is still the earlier result. `None` until the first scan has finished.
    pub fn index(&self) -> Option<Arc<WorkspaceIndex>> {
        self.shared.state().index.clone()
    }

    /// Whether a scan is running.
    pub fn scanning(&self) -> bool {
        self.shared.state().scanning
    }

    /// Why the latest scan failed; `None` if it worked or none has finished.
    pub fn error(&self) -> Option<String> {
        self.shared.state().error.clone()
    }

    /// Scans again. If a scan is running, one more starts after it has finished; any number of
    /// calls in the meantime count as one.
    pub fn rescan(&self) {
        {
            let mut state = self.shared.state();
            if state.scanning {
                state.again = true;
                return;
            }
            state.scanning = true;
        }
        let worker = Arc::clone(&self.shared);
        let spawned = std::thread::Builder::new()
            .name("refdesk-workspace-scan".to_string())
            .spawn(move || run_scans(&worker));
        if let Err(e) = spawned {
            {
                let mut state = self.shared.state();
                state.scanning = false;
                state.error = Some(format!("cannot start the folder scan: {e}"));
            }
            (self.shared.notifier)();
        }
    }
}

/// The scan thread: scans, publishes, and scans again while a rescan was asked for.
fn run_scans(shared: &ScanShared) {
    loop {
        let result = catch_unwind(AssertUnwindSafe(|| {
            scan_with(&shared.root, &shared.limits, shared.options, &shared.cancel)
        }))
        .unwrap_or_else(|_| Err(io::Error::other("the folder scan failed unexpectedly")));
        if shared.cancelled() {
            return;
        }
        let again = {
            let mut state = shared.state();
            match result {
                Ok(index) => {
                    state.index = Some(Arc::new(index));
                    state.error = None;
                }
                Err(e) => state.error = Some(e.to_string()),
            }
            if state.again {
                state.again = false;
                true
            } else {
                state.scanning = false;
                false
            }
        };
        (shared.notifier)();
        if !again {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Watching

/// How long changes are collected into one call of the callback.
pub const WATCH_WINDOW: Duration = Duration::from_millis(500);

enum WatchMsg {
    Event(notify::Result<notify::Event>),
    Stop,
}

/// Watches a workspace folder (and everything below it) for changes that matter to the tree:
/// folders and `.pdf` files created, deleted or renamed. Other files, changes to the content of
/// a file, and anything below a name that starts with `.` are ignored. Dropping the watcher ends
/// the watch.
pub struct WorkspaceWatcher {
    tx: mpsc::Sender<WatchMsg>,
    stopped: Arc<AtomicBool>,
    // Last, so that it is dropped (and stops sending) after `drop` has told the thread to end.
    _watcher: RecommendedWatcher,
}

impl std::fmt::Debug for WorkspaceWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceWatcher").finish_non_exhaustive()
    }
}

impl Drop for WorkspaceWatcher {
    fn drop(&mut self) {
        self.stopped.store(true, AtomicOrdering::Relaxed);
        let _ = self.tx.send(WatchMsg::Stop);
    }
}

fn watch_error(e: notify::Error) -> io::Error {
    match e.kind {
        notify::ErrorKind::Io(inner) => inner,
        kind => io::Error::other(format!("cannot watch the folder: {kind:?}")),
    }
}

impl WorkspaceWatcher {
    /// Starts watching `root`. `on_change` is called on a background thread, once for all the
    /// changes of the next [`WATCH_WINDOW`] after the first of them.
    ///
    /// # Errors
    ///
    /// `root` cannot be watched: it does not exist, or the system's limit of watches is used up.
    /// The caller then relies on refreshing by hand.
    pub fn start(
        root: &Path,
        on_change: impl Fn() + Send + 'static,
    ) -> io::Result<WorkspaceWatcher> {
        WorkspaceWatcher::start_with_window(root, WATCH_WINDOW, on_change)
    }

    /// [`WorkspaceWatcher::start`] with another window (tests make it short).
    pub fn start_with_window(
        root: &Path,
        window: Duration,
        on_change: impl Fn() + Send + 'static,
    ) -> io::Result<WorkspaceWatcher> {
        let root = fs::canonicalize(root)?;
        let (tx, rx) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let sender = tx.clone();
        let config = Config::default().with_event_kinds(
            EventKindMask::CREATE | EventKindMask::REMOVE | EventKindMask::MODIFY_NAME,
        );
        let mut watcher = RecommendedWatcher::new(
            move |event| {
                let _ = sender.send(WatchMsg::Event(event));
            },
            config,
        )
        .map_err(watch_error)?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(watch_error)?;
        let flag = Arc::clone(&stopped);
        std::thread::Builder::new()
            .name("refdesk-workspace-watch".to_string())
            .spawn(move || watch_loop(&rx, &root, window, &flag, &on_change))?;
        Ok(WorkspaceWatcher {
            tx,
            stopped,
            _watcher: watcher,
        })
    }
}

/// What the events of one window add up to.
#[derive(Default)]
struct Batch {
    changed: bool,
    /// Renames away from a path whose partner (the rename to) has not been seen: the thing may
    /// have been moved out of the folder, which only shows once the window ends.
    moved_away: HashSet<usize>,
}

fn watch_loop(
    rx: &mpsc::Receiver<WatchMsg>,
    root: &Path,
    window: Duration,
    stopped: &AtomicBool,
    on_change: &dyn Fn(),
) {
    let mut batch = Batch::default();
    let mut deadline: Option<Instant> = None;
    loop {
        let msg = match deadline {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
        };
        match msg {
            Ok(WatchMsg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(WatchMsg::Event(Err(e))) => {
                log::warn!("workspace watch: {e}");
                batch.changed = true;
            }
            Ok(WatchMsg::Event(Ok(event))) => classify(root, &event, &mut batch),
            Err(RecvTimeoutError::Timeout) => {
                let report = batch.changed || !batch.moved_away.is_empty();
                batch = Batch::default();
                deadline = None;
                if report && !stopped.load(AtomicOrdering::Relaxed) {
                    on_change();
                }
                continue;
            }
        }
        if deadline.is_none() && (batch.changed || !batch.moved_away.is_empty()) {
            deadline = Some(Instant::now() + window);
        }
    }
}

/// Whether the path (below `root`) is, or is inside, something hidden. A path that is not below
/// `root` counts as hidden: it is not ours.
fn is_hidden(root: &Path, path: &Path) -> bool {
    match path.strip_prefix(root) {
        Ok(rel) => rel.components().any(
            |c| matches!(c, Component::Normal(n) if n.as_encoded_bytes().first() == Some(&b'.')),
        ),
        Err(_) => true,
    }
}

fn is_dir_now(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_dir())
}

/// Adds what `event` means for the tree to `batch`.
fn classify(root: &Path, event: &notify::Event, batch: &mut Batch) {
    if event.need_rescan() {
        batch.changed = true;
        return;
    }
    let visible = |p: &PathBuf| !is_hidden(root, p);
    let pdf_or_dir = |p: &PathBuf| is_pdf_name(p.as_os_str()) || is_dir_now(p);
    match event.kind {
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => {
            batch.changed |= event.paths.iter().any(visible);
        }
        EventKind::Create(_) => {
            batch.changed |= event.paths.iter().filter(|p| visible(p)).any(pdf_or_dir);
        }
        EventKind::Remove(RemoveKind::File) => {
            batch.changed |= event
                .paths
                .iter()
                .any(|p| visible(p) && is_pdf_name(p.as_os_str()));
        }
        // What was removed is gone, so whether it was a folder is not known.
        EventKind::Remove(_) => {
            batch.changed |= event.paths.iter().any(visible);
        }
        EventKind::Modify(ModifyKind::Name(mode)) => {
            let tracker = event.tracker();
            match mode {
                RenameMode::From => {
                    for path in event.paths.iter().filter(|p| visible(p)) {
                        if is_pdf_name(path.as_os_str()) {
                            batch.changed = true;
                        } else if let Some(t) = tracker {
                            batch.moved_away.insert(t);
                        } else {
                            batch.changed = true;
                        }
                    }
                }
                RenameMode::To | RenameMode::Both => {
                    if let Some(t) = tracker {
                        batch.moved_away.remove(&t);
                    }
                    let any_visible = event.paths.iter().any(visible);
                    // For `Both` the last path is the new one; for `To` the only one.
                    let named_pdf = event
                        .paths
                        .iter()
                        .any(|p| visible(p) && is_pdf_name(p.as_os_str()));
                    let dir = any_visible && event.paths.last().is_some_and(|p| is_dir_now(p));
                    batch.changed |= named_pdf || dir;
                }
                // Which way the rename went is not known: take it as a change.
                _ => batch.changed |= event.paths.iter().any(visible),
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calls of `rescan` while a scan runs make one more scan, however many they are. The
    /// running scan is played by the test: it sets `scanning` and then runs the scan loop itself.
    #[test]
    fn rescans_asked_during_a_scan_make_one_more_scan() {
        let dir = tempfile::tempdir().unwrap();
        assert!(fs::write(dir.path().join("a.pdf"), b"").is_ok());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let notifier: Notifier = Arc::new(move || {
            counter.fetch_add(1, AtomicOrdering::SeqCst);
        });
        let job = WorkspaceScan {
            shared: Arc::new(ScanShared {
                root: dir.path().to_path_buf(),
                limits: ScanLimits::default(),
                options: ScanOptions::default(),
                notifier,
                cancel: AtomicBool::new(false),
                state: Mutex::new(ScanState {
                    scanning: true,
                    ..ScanState::default()
                }),
            }),
        };
        for _ in 0..7 {
            job.rescan();
        }
        assert!(job.shared.state().again);
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0, "nothing ran yet");

        run_scans(&job.shared);
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            2,
            "the scan and one more"
        );
        assert!(!job.scanning());
        assert!(!job.shared.state().again);
        assert_eq!(job.index().map(|i| i.entries.len()), Some(1));
    }

    #[test]
    fn natural_order_is_total_enough_for_sorting_names_that_differ_only_in_case() {
        let mut names = ["b", "B", "a", "A"];
        names.sort_by(|x, y| natural_cmp(x, y).then_with(|| x.cmp(y)));
        assert_eq!(names, ["A", "a", "B", "b"]);
    }
}
