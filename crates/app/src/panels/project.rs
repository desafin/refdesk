//! Projects in the app (WP-91, `docs/SPEC.md` §7.15): making and opening projects, importing
//! PDFs into them, and managing their files from the explorer.
//!
//! A project is a workspace folder with a project file (`docs/ARCHITECTURE.md` §4.15). There is
//! no separate "open project" state: every way of opening a workspace folder reads the project
//! file, and [`ProjectInfo`] sits in the workspace (`state::Workspace::project`).
//!
//! Every way to import from outside (the file chooser, the explorer's menu, pasting) ends in one
//! action, [`Action::ImportIntoProject`]. The folder picker's checked list ends in
//! [`ProjectAction::ImportItems`]. Imports run one at a time on a core [`ImportJob`]; further
//! requests wait in a queue. The file chooser and the clipboard sit behind traits
//! ([`ProjectServices`]) so tests can use fakes; both are only called on threads of their own
//! (R-1), and what they return comes back through an inbox that [`tick`] empties.
//!
//! The decisions (the new project dialog, the folder picker's list, the progress and result
//! texts, pasted text, rename targets) are plain functions tested without egui (R-10).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use egui::{Align, Layout, RichText, Ui, UiBuilder, WidgetInfo};
use refdesk_core::project::{self as core_project, TrashEntry, folder_name, is_pdf_name};
use refdesk_core::workspace::scan;
use refdesk_core::{
    ConflictChoice, ConflictPolicy, ImportItem, ImportJob, ImportOutcome, ImportSnapshot,
    ImportState, Project, ProjectError, ScanLimits, SkipReason, SourceSurvey, SurveyFile,
    SurveyState,
};

use crate::action::{self, Action, ExplorerOpen, OpenTarget};
use crate::i18n::tr;
use crate::state::{AppState, Tab, TabId};

// ---------------------------------------------------------------------------------------------
// The project of the workspace

/// What the app knows about the project of the open workspace folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectInfo {
    /// The project's name.
    pub name: String,
    /// What was wrong with the project file, if anything.
    pub warning: Option<String>,
}

impl ProjectInfo {
    /// Reads the project file of `root`; `None` when `root` is not a project. The file is small
    /// and read once when the folder is opened.
    pub fn read(root: &Path) -> Option<ProjectInfo> {
        if !core_project::is_project(root) {
            return None;
        }
        match Project::open(root) {
            Ok(p) => Some(ProjectInfo {
                name: p.name().to_owned(),
                warning: p.warning().map(str::to_owned),
            }),
            Err(e) => {
                log::warn!("cannot read the project at {}: {e}", root.display());
                let name = root.file_name().map_or_else(
                    || root.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                Some(ProjectInfo {
                    name,
                    warning: Some(e.to_string()),
                })
            }
        }
    }
}

/// The root and name of the open project, if the workspace is one.
pub fn open_project(state: &AppState) -> Option<(&Path, &ProjectInfo)> {
    let ws = state.workspace.as_ref()?;
    ws.project.as_ref().map(|p| (ws.root.as_path(), p))
}

/// A project is open (the workspace folder has a project file).
pub fn has_project(state: &AppState) -> bool {
    open_project(state).is_some()
}

/// The folder (below the project root) imports go to when nothing else is said: the folder
/// chosen in the explorer of the focused window (the folder of a chosen file), else the root.
pub fn import_target(state: &AppState) -> PathBuf {
    state
        .focused_window()
        .explorer
        .cursor_folder()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// The outside world

/// The file chooser. Its methods block until the user is done; the app calls them on threads of
/// their own. Tests use a fake that answers at once.
pub trait Chooser: Send + Sync {
    /// Lets the user pick any number of PDF files; empty when cancelled.
    fn pick_pdfs(&self, title: &str, start: Option<&Path>) -> Vec<PathBuf>;
    /// Lets the user pick a folder; `None` when cancelled.
    fn pick_folder(&self, title: &str, start: Option<&Path>) -> Option<PathBuf>;
}

/// The files on the clipboard (copied in a file manager). Blocking; called on a thread of its
/// own. Tests use a fake.
pub trait FileClipboard: Send + Sync {
    /// The files on the clipboard; empty when there are none.
    fn file_list(&self) -> Vec<PathBuf>;
}

/// The XDG desktop portal's file chooser, through `rfd`.
struct RfdChooser;

impl Chooser for RfdChooser {
    fn pick_pdfs(&self, title: &str, start: Option<&Path>) -> Vec<PathBuf> {
        let mut dialog = rfd::FileDialog::new()
            .set_title(title)
            .add_filter(tr("dialog.filter"), &["pdf", "PDF"]);
        if let Some(dir) = start.filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        dialog.pick_files().unwrap_or_default()
    }

    fn pick_folder(&self, title: &str, start: Option<&Path>) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new().set_title(title);
        if let Some(dir) = start.filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        dialog.pick_folder()
    }
}

/// The system clipboard's file list, through `arboard` (`text/uri-list`).
struct ArboardFiles;

impl FileClipboard for ArboardFiles {
    fn file_list(&self) -> Vec<PathBuf> {
        match arboard::Clipboard::new() {
            Ok(mut clipboard) => clipboard.get().file_list().unwrap_or_default(),
            Err(e) => {
                log::debug!("no clipboard: {e}");
                Vec::new()
            }
        }
    }
}

/// How the project features reach the file chooser and the clipboard.
#[derive(Clone)]
pub struct ProjectServices {
    /// The file chooser.
    pub chooser: Arc<dyn Chooser>,
    /// The clipboard's file list.
    pub clipboard: Arc<dyn FileClipboard>,
}

impl Default for ProjectServices {
    fn default() -> Self {
        ProjectServices {
            chooser: Arc::new(RfdChooser),
            clipboard: Arc::new(ArboardFiles),
        }
    }
}

impl std::fmt::Debug for ProjectServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProjectServices")
    }
}

/// The files named by pasted text: one per line, as an absolute path or a `file://` URI, each
/// of which must exist. Empty unless every non-empty line names a file or folder (so pasting a
/// sentence does nothing). Reads the disk; call it off the UI thread.
pub fn paths_from_text(text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.starts_with('#') {
            continue; // a comment line of text/uri-list
        }
        let path = match line.strip_prefix("file://") {
            Some(rest) => {
                // `file:///a/b` or `file://localhost/a/b`
                let rest = rest.strip_prefix("localhost").unwrap_or(rest);
                PathBuf::from(percent_decode(rest))
            }
            None => PathBuf::from(line),
        };
        if !path.is_absolute() || std::fs::symlink_metadata(&path).is_err() {
            return Vec::new();
        }
        out.push(path);
    }
    out
}

/// Decodes `%XX` escapes (as UTF-8); a broken escape is kept as it is.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------------------------
// Actions

/// What the project features ask `dispatch` for (besides [`Action::ImportIntoProject`]).
#[derive(Clone, Debug, PartialEq)]
pub enum ProjectAction {
    /// Show the new project dialog (PROJ-1).
    NewDialog,
    /// Close the new project dialog.
    CloseNewDialog,
    /// Let the user choose the folder new projects are made in, for the dialog.
    BrowseLocation,
    /// The folder chosen for the dialog: the project goes into it.
    SetLocationBase(PathBuf),
    /// Make the project and open it.
    Create {
        /// The project's folder.
        root: PathBuf,
        /// Its name.
        name: String,
    },
    /// Ask for a folder to open as a project (PROJ-4).
    OpenDialog,
    /// Open this folder as a project: at once when it is one, else after asking.
    Open(PathBuf),
    /// Close the "not a project" question.
    ClosePrompt,
    /// Ask for PDF files to import into the folder `into` of the project (PROJ-2).
    ImportDialog {
        /// The target folder below the project root.
        into: PathBuf,
    },
    /// Ask for a folder whose PDFs are picked to be imported into `into`.
    ImportFolderDialog {
        /// The target folder below the project root.
        into: PathBuf,
    },
    /// Show the picker for the PDFs below `folder`, to import into `into`.
    OpenPicker {
        /// The folder to import from.
        folder: PathBuf,
        /// The target folder below the project root.
        into: PathBuf,
    },
    /// Close the picker.
    ClosePicker,
    /// Import these files, each to its own target (the picker's checked rows).
    ImportItems(Vec<ImportItem>),
    /// Read the clipboard's files (Ctrl+V outside a text field); `text` is the text egui
    /// delivered with the paste, used when the clipboard has no file list.
    Paste {
        /// The pasted text, if any.
        text: Option<String>,
    },
    /// The clipboard's files have been read.
    Pasted {
        /// The files and folders.
        files: Vec<PathBuf>,
        /// The target folder below the project root.
        into: PathBuf,
    },
    /// Cancel the running import.
    Cancel,
    /// Settle the name clash the import waits on.
    Resolve {
        /// What to do.
        choice: ConflictChoice,
        /// Do the same for the clashes after it.
        all: bool,
    },
    /// Show the list of what the last import did not copy.
    ShowDetails,
    /// Close that list.
    CloseDetails,
    /// Make the folder `rel` in the project (PROJ-5).
    NewFolder {
        /// The new folder below the project root.
        rel: PathBuf,
    },
    /// Rename `from` to `to`, both below the project root. Open documents follow.
    Rename {
        /// The old path below the root.
        from: PathBuf,
        /// The new path below the root.
        to: PathBuf,
    },
    /// Ask whether to move `rel` to the project's trash.
    AskTrash {
        /// The file or folder below the project root.
        rel: PathBuf,
    },
    /// Close that question.
    CancelTrash,
    /// Move `rel` to the project's trash, closing its tabs.
    Trash {
        /// The file or folder below the project root.
        rel: PathBuf,
    },
    /// Show the project's trash.
    ShowTrash,
    /// Close the trash dialog.
    CloseTrash,
    /// Put an item of the trash back.
    Restore(String),
    /// Delete everything in the trash.
    EmptyTrash,
}

// ---------------------------------------------------------------------------------------------
// The new project dialog

/// What the folder of a new project looks like now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderState {
    /// It does not exist yet.
    Missing,
    /// It exists and is empty.
    Empty,
    /// It exists, has things in it, and this many PDFs below it.
    HasFiles(usize),
    /// It already is a project with this name.
    Project(String),
    /// Something that is not a folder is there.
    NotFolder,
}

/// Looks at `path` for the dialog. Reads the disk (a scan of the folder); call it off the UI
/// thread.
pub fn probe_folder(path: &Path, cancel: &AtomicBool) -> FolderState {
    match std::fs::metadata(path) {
        Err(_) => return FolderState::Missing,
        Ok(meta) if !meta.is_dir() => return FolderState::NotFolder,
        Ok(_) => {}
    }
    if core_project::is_project(path) {
        let name = ProjectInfo::read(path).map(|p| p.name).unwrap_or_default();
        return FolderState::Project(name);
    }
    let empty = std::fs::read_dir(path).map_or(true, |mut d| d.next().is_none());
    if empty {
        return FolderState::Empty;
    }
    let pdfs = scan(path, &ScanLimits::default(), cancel).map_or(0, |i| i.file_count());
    FolderState::HasFiles(pdfs)
}

/// The main button of the new project dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateButton {
    /// "Create", possible.
    Create,
    /// "Open": the folder already is a project.
    Open,
    /// "Create", not possible now.
    Disabled,
}

/// What the new project dialog shows for its fields: an error, a note and the main button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewProjectView {
    /// Why the project cannot be made.
    pub error: Option<String>,
    /// What will happen.
    pub note: Option<String>,
    /// The main button.
    pub button: CreateButton,
}

/// The location that follows the name: `base/<folder name of name>`.
pub fn default_location(base: &Path, name: &str) -> PathBuf {
    base.join(folder_name(name))
}

/// Decides what the dialog shows for `name`, `location` (which `follows_name` unless it was
/// changed by hand) and what is known about the folder there (`None` while it is being looked
/// at).
pub fn new_project_view(
    name: &str,
    location: &str,
    follows_name: bool,
    folder: Option<&FolderState>,
) -> NewProjectView {
    let view = |error: Option<String>, note: Option<String>, button| NewProjectView {
        error,
        note,
        button,
    };
    let location = location.trim();
    if let Some(FolderState::Project(existing)) = folder {
        return view(
            None,
            Some(tr("project.new.is_project").replace("{}", existing)),
            CreateButton::Open,
        );
    }
    if name.trim().is_empty() {
        return view(
            Some(tr("project.new.empty_name").to_owned()),
            None,
            CreateButton::Disabled,
        );
    }
    if follows_name && folder_name(name).is_empty() {
        return view(
            Some(tr("project.new.unusable_name").to_owned()),
            None,
            CreateButton::Disabled,
        );
    }
    let path = Path::new(location);
    if location.is_empty() || !path.is_absolute() {
        return view(
            Some(tr("project.new.bad_location").to_owned()),
            None,
            CreateButton::Disabled,
        );
    }
    if path
        .file_name()
        .is_none_or(|n| folder_name(&n.to_string_lossy()).is_empty())
    {
        return view(
            Some(tr("project.new.unusable_name").to_owned()),
            None,
            CreateButton::Disabled,
        );
    }
    match folder {
        None => view(
            None,
            Some(tr("project.new.checking").to_owned()),
            CreateButton::Disabled,
        ),
        Some(FolderState::NotFolder) => view(
            Some(tr("project.new.not_folder").to_owned()),
            None,
            CreateButton::Disabled,
        ),
        Some(FolderState::Missing | FolderState::Empty) => view(None, None, CreateButton::Create),
        Some(FolderState::HasFiles(0)) => view(
            None,
            Some(tr("project.new.existing").to_owned()),
            CreateButton::Create,
        ),
        Some(FolderState::HasFiles(n)) => view(
            None,
            Some(tr("project.new.has_pdfs").replace("{n}", &n.to_string())),
            CreateButton::Create,
        ),
        Some(FolderState::Project(_)) => view(None, None, CreateButton::Open),
    }
}

/// A look at a folder for the dialog, on a thread of its own.
struct FolderProbe {
    path: PathBuf,
    result: Arc<Mutex<Option<FolderState>>>,
    cancel: Arc<AtomicBool>,
}

impl Drop for FolderProbe {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl FolderProbe {
    fn start(path: PathBuf, ctx: &egui::Context) -> FolderProbe {
        let result = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let (slot, stop, dir, ctx) = (
            Arc::clone(&result),
            Arc::clone(&cancel),
            path.clone(),
            ctx.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("refdesk-project-probe".into())
            .spawn(move || {
                let state = probe_folder(&dir, &stop);
                if !stop.load(Ordering::Relaxed) {
                    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(state);
                    ctx.request_repaint();
                }
            });
        if let Err(e) = spawned {
            log::warn!("cannot look at {}: {e}", path.display());
            *result.lock().unwrap_or_else(PoisonError::into_inner) = Some(FolderState::Missing);
        }
        FolderProbe {
            path,
            result,
            cancel,
        }
    }

    fn state(&self) -> Option<FolderState> {
        self.result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The new project dialog (PROJ-1).
pub struct NewProjectDialog {
    /// The name field.
    pub name: String,
    /// The location field: the project's folder.
    pub location: String,
    /// The location was changed by hand; it no longer follows the name.
    pub location_edited: bool,
    /// The folder the location follows the name in.
    pub base: PathBuf,
    /// Why making the project failed, from the last try.
    pub failure: Option<String>,
    probe: Option<FolderProbe>,
}

impl std::fmt::Debug for NewProjectDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewProjectDialog")
            .field("name", &self.name)
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl NewProjectDialog {
    /// A dialog with an empty name whose location follows the name below `base`.
    pub fn new(base: PathBuf) -> NewProjectDialog {
        NewProjectDialog {
            location: path_text(&base),
            name: String::new(),
            location_edited: false,
            base,
            failure: None,
            probe: None,
        }
    }

    /// The name changed: the location follows unless it was changed by hand.
    pub fn name_changed(&mut self) {
        if !self.location_edited {
            self.location = path_text(&default_location(&self.base, &self.name));
        }
        self.failure = None;
    }

    /// The location was changed by hand.
    pub fn location_changed(&mut self) {
        self.location_edited = true;
        self.failure = None;
    }

    /// A folder was chosen with "Browse…": the project goes into it, and the location follows
    /// the name again.
    pub fn set_base(&mut self, base: PathBuf) {
        self.base = base;
        self.location_edited = false;
        self.name_changed();
    }

    /// The project's folder as typed.
    pub fn root(&self) -> PathBuf {
        PathBuf::from(self.location.trim())
    }

    /// Starts looking at the location when it changed. Called every frame the dialog shows.
    pub fn check(&mut self, ctx: &egui::Context) {
        let root = self.root();
        if !root.is_absolute() {
            self.probe = None;
            return;
        }
        if self.probe.as_ref().is_none_or(|p| p.path != root) {
            self.probe = Some(FolderProbe::start(root, ctx));
        }
    }

    /// What is known about the folder at the location.
    pub fn folder_state(&self) -> Option<FolderState> {
        self.probe
            .as_ref()
            .filter(|p| p.path == self.root())
            .and_then(FolderProbe::state)
    }

    /// What the dialog shows now.
    pub fn view(&self) -> NewProjectView {
        let mut view = new_project_view(
            &self.name,
            &self.location,
            !self.location_edited,
            self.folder_state().as_ref(),
        );
        if let Some(failure) = &self.failure {
            view.error = Some(failure.clone());
        }
        view
    }
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

// ---------------------------------------------------------------------------------------------
// The folder picker ("Import from Folder…")

/// One PDF of the picker's list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickRow {
    /// The path below the picked folder.
    pub rel: PathBuf,
    /// `rel` for display.
    pub label: String,
    /// `rel` folded to lower case, for the filter.
    folded: String,
    /// Size in bytes.
    pub size: u64,
    /// It will be imported.
    pub checked: bool,
    /// The user changed its check by hand; "already there" leaves it alone.
    pub touched: bool,
    /// The project file with the same content, once that is known.
    pub existing: Option<PathBuf>,
}

/// The rows of the picker and their checks. Plain data: the drawing only reads and toggles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PickList {
    /// The rows, in the order of the scan.
    pub rows: Vec<PickRow>,
    /// The filter the cached `shown` list is for, and the list.
    cache: Option<(String, Arc<Vec<usize>>)>,
}

impl PickList {
    /// A list of `files`, all checked.
    pub fn new(files: &[SurveyFile]) -> PickList {
        PickList {
            rows: files
                .iter()
                .map(|f| {
                    let label = f.rel.display().to_string();
                    PickRow {
                        folded: label.to_lowercase(),
                        label,
                        rel: f.rel.clone(),
                        size: f.size,
                        checked: true,
                        touched: false,
                        existing: None,
                    }
                })
                .collect(),
            cache: None,
        }
    }

    /// The rows the filter lets through (case ignored, anywhere in the path), as indexes.
    pub fn shown(&mut self, filter: &str) -> Arc<Vec<usize>> {
        let key = filter.trim().to_lowercase();
        if let Some((cached, list)) = &self.cache
            && *cached == key
        {
            return Arc::clone(list);
        }
        let list: Arc<Vec<usize>> = Arc::new(
            self.rows
                .iter()
                .enumerate()
                .filter(|(_, r)| key.is_empty() || r.folded.contains(&key))
                .map(|(i, _)| i)
                .collect(),
        );
        self.cache = Some((key, Arc::clone(&list)));
        list
    }

    /// Checks or unchecks the rows the filter lets through (only those).
    pub fn set_shown(&mut self, filter: &str, checked: bool) {
        for i in self.shown(filter).iter() {
            if let Some(row) = self.rows.get_mut(*i) {
                row.checked = checked;
                row.touched = true;
            }
        }
    }

    /// Flips the check of row `i` by hand.
    pub fn toggle(&mut self, i: usize) {
        if let Some(row) = self.rows.get_mut(i) {
            row.checked = !row.checked;
            row.touched = true;
        }
    }

    /// Marks the rows whose content the project already has (by row index); a row the user has
    /// not touched is unchecked.
    pub fn apply_existing(&mut self, existing: &HashMap<usize, PathBuf>) {
        for (i, rel) in existing {
            if let Some(row) = self.rows.get_mut(*i)
                && row.existing.is_none()
            {
                row.existing = Some(rel.clone());
                if !row.touched {
                    row.checked = false;
                }
            }
        }
    }

    /// Number and total size of the checked rows.
    pub fn totals(&self) -> (usize, u64) {
        self.rows
            .iter()
            .filter(|r| r.checked)
            .fold((0, 0), |(n, b), r| (n + 1, b + r.size))
    }

    /// What to import for the checked rows from `folder` into `into`: below `into/<folder
    /// name>/` with the sub-folders kept, or straight into `into` without them.
    pub fn items(&self, folder: &Path, into: &Path, keep_structure: bool) -> Vec<ImportItem> {
        let base = match folder.file_name() {
            Some(name) if keep_structure => into.join(name),
            _ => into.to_path_buf(),
        };
        self.rows
            .iter()
            .filter(|r| r.checked)
            .filter_map(|r| {
                let dest = if keep_structure {
                    base.join(&r.rel)
                } else {
                    base.join(r.rel.file_name()?)
                };
                Some(ImportItem {
                    source: folder.join(&r.rel),
                    dest,
                })
            })
            .collect()
    }
}

/// The accessible name of a picker row: "Import: docs/a.pdf, 2.1 MB, checked".
pub fn pick_row_name(row: &PickRow) -> String {
    let check = if row.checked {
        tr("project.pick.checked")
    } else {
        tr("project.pick.unchecked")
    };
    let mut text = tr("project.pick.row")
        .replace("{path}", &row.label)
        .replace("{size}", &human_size(row.size))
        .replace("{check}", check);
    if row.existing.is_some() {
        text.push_str(", ");
        text.push_str(tr("project.pick.already"));
    }
    text
}

/// The picker of "Import from Folder…".
pub struct PickerDialog {
    /// The folder the PDFs are picked from.
    pub folder: PathBuf,
    /// The target folder below the project root.
    pub into: PathBuf,
    /// The rows, once the scan has finished.
    pub list: Option<PickList>,
    /// The filter field.
    pub filter: String,
    /// "Keep sub-folders".
    pub keep_structure: bool,
    /// Why the folder could not be read.
    pub error: Option<String>,
    /// Comparing contents with the project is still running.
    pub checking: bool,
    survey: SourceSurvey,
    /// How many "already there" results were applied.
    applied: usize,
}

impl std::fmt::Debug for PickerDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PickerDialog")
            .field("folder", &self.folder)
            .finish_non_exhaustive()
    }
}

impl PickerDialog {
    /// Starts surveying `folder` for an import into `into` of the project at `root`.
    pub fn start(root: &Path, folder: PathBuf, into: PathBuf, ctx: &egui::Context) -> Self {
        let survey = SourceSurvey::start(root.to_path_buf(), folder.clone(), action::notifier(ctx));
        PickerDialog {
            folder,
            into,
            list: None,
            filter: String::new(),
            keep_structure: true,
            error: None,
            checking: true,
            survey,
            applied: 0,
        }
    }

    /// Takes in what the survey found since the last call.
    pub fn poll(&mut self) {
        let snap = self.survey.snapshot();
        match &snap.state {
            SurveyState::Scanning => return,
            SurveyState::Failed(e) => {
                self.error = Some(e.clone());
                self.checking = false;
                return;
            }
            SurveyState::Checking | SurveyState::Done => {}
        }
        let list = self.list.get_or_insert_with(|| PickList::new(&snap.files));
        if snap.existing.len() != self.applied {
            list.apply_existing(&snap.existing);
            self.applied = snap.existing.len();
        }
        self.checking = snap.state != SurveyState::Done;
    }
}

/// `1.5 MB`, `320 KB`, `12 B`.
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
    }
}

// ---------------------------------------------------------------------------------------------
// Imports: the queue, progress and result texts

/// An import waiting for its turn.
#[derive(Clone, Debug, PartialEq)]
enum Request {
    Sources {
        root: PathBuf,
        sources: Vec<PathBuf>,
        into: PathBuf,
    },
    Items {
        root: PathBuf,
        items: Vec<ImportItem>,
    },
}

/// The import that runs now.
pub struct RunningImport {
    /// The job.
    pub job: ImportJob,
    /// The project it imports into.
    pub root: PathBuf,
}

/// The text of the status bar while an import runs: "Importing 3 / 12 — name.pdf".
pub fn progress_text(snap: &ImportSnapshot, waiting: usize) -> String {
    let mut text = if snap.total == 0 && !snap.finished() {
        tr("project.import.starting").to_owned()
    } else {
        let name = snap
            .current
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        tr("project.import.progress")
            .replace("{i}", &(snap.done + 1).min(snap.total).to_string())
            .replace("{n}", &snap.total.to_string())
            .replace("{name}", &name)
    };
    if waiting > 0 {
        text.push_str(&tr("project.import.queued").replace("{n}", &waiting.to_string()));
    }
    text
}

/// Counts of an import's results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportCounts {
    /// Copied (replacing or not).
    pub imported: usize,
    /// The project had the same content.
    pub already: usize,
    /// Left out (not a PDF, hidden, inside the project, "skip" chosen).
    pub skipped: usize,
    /// Could not be copied.
    pub failed: usize,
}

impl ImportCounts {
    /// The counts of `snap`.
    pub fn of(snap: &ImportSnapshot) -> ImportCounts {
        let mut c = ImportCounts {
            skipped: snap.not_pdf,
            ..ImportCounts::default()
        };
        for r in &snap.results {
            match r.outcome {
                ImportOutcome::Imported | ImportOutcome::Replaced => c.imported += 1,
                ImportOutcome::AlreadyThere { .. } => c.already += 1,
                ImportOutcome::Skipped(_) => c.skipped += 1,
                ImportOutcome::Failed(_) => c.failed += 1,
            }
        }
        c
    }

    /// Whether "Details" has something to show.
    pub fn has_details(&self) -> bool {
        self.skipped + self.failed > 0
    }
}

/// The result notice: "9 imported, 2 already there, 1 failed".
pub fn summary_text(snap: &ImportSnapshot) -> String {
    let c = ImportCounts::of(snap);
    let mut parts = vec![tr("project.import.done").replace("{n}", &c.imported.to_string())];
    for (n, key) in [
        (c.already, "project.import.already"),
        (c.skipped, "project.import.skipped"),
        (c.failed, "project.import.failed"),
    ] {
        if n > 0 {
            parts.push(tr(key).replace("{n}", &n.to_string()));
        }
    }
    let text = parts.join(", ");
    if snap.state == ImportState::Cancelled {
        tr("project.import.cancelled").replace("{}", &text)
    } else {
        text
    }
}

/// The lines of "Details": every source that was not simply copied, with the reason.
pub fn detail_lines(snap: &ImportSnapshot) -> Vec<String> {
    let mut out = Vec::new();
    for r in &snap.results {
        let name = r.source.file_name().map_or_else(
            || r.source.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let reason = match &r.outcome {
            ImportOutcome::Imported | ImportOutcome::Replaced => continue,
            ImportOutcome::AlreadyThere { existing } => {
                tr("project.detail.already").replace("{}", &existing.display().to_string())
            }
            ImportOutcome::Skipped(SkipReason::NotPdf) => tr("project.detail.not_pdf").to_owned(),
            ImportOutcome::Skipped(SkipReason::Hidden) => tr("project.detail.hidden").to_owned(),
            ImportOutcome::Skipped(SkipReason::InsideProject) => {
                tr("project.detail.inside").to_owned()
            }
            ImportOutcome::Skipped(SkipReason::Declined) => {
                tr("project.detail.declined").to_owned()
            }
            ImportOutcome::Failed(e) => tr("project.detail.failed").replace("{}", e),
        };
        out.push(format!("{name} — {reason}"));
    }
    if snap.not_pdf > 0 {
        out.push(tr("project.detail.not_pdf_count").replace("{n}", &snap.not_pdf.to_string()));
    }
    out
}

/// The result of the last import, shown for a while in the status bar.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportSummary {
    /// The notice text.
    pub text: String,
    /// The lines of "Details".
    pub details: Vec<String>,
    /// "Details" is offered.
    pub has_details: bool,
    /// When the notice goes away, in egui time (seconds).
    pub until: f64,
}

/// Seconds the result of an import stays in the status bar.
const SUMMARY_SECS: f64 = 8.0;

// ---------------------------------------------------------------------------------------------
// Explorer file management: names

/// The path a file or folder gets when renamed to `text` in place: next to the old one, a file
/// keeping its extension (typing it again is allowed).
///
/// # Errors
///
/// The text, as an i18n'd message, when it cannot be a name.
pub fn rename_target(rel: &Path, text: &str, is_dir: bool) -> Result<PathBuf, String> {
    let mut name = check_entry_name(text)?;
    if !is_dir && let Some(ext) = rel.extension() {
        let ext = ext.to_string_lossy();
        let typed = Path::new(&name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(&*ext));
        if !typed {
            name = format!("{name}.{ext}");
        }
    }
    Ok(rel.parent().unwrap_or(Path::new("")).join(name))
}

/// The path of a new folder called `text` inside `parent`.
///
/// # Errors
///
/// As [`rename_target`].
pub fn new_folder_target(parent: &Path, text: &str) -> Result<PathBuf, String> {
    Ok(parent.join(check_entry_name(text)?))
}

fn check_entry_name(text: &str) -> Result<String, String> {
    let name = text.trim();
    if name.is_empty() {
        return Err(tr("project.name.empty").to_owned());
    }
    if name.contains('/') || name.starts_with('.') || name.chars().any(char::is_control) {
        return Err(tr("project.name.invalid").to_owned());
    }
    Ok(name.to_owned())
}

/// The text the rename field starts with: a file's name without its extension.
pub fn rename_start_text(rel: &Path, is_dir: bool) -> String {
    let part = if is_dir {
        rel.file_name()
    } else {
        rel.file_stem()
    };
    part.map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// The state

/// Something a chooser or clipboard thread brought back.
#[derive(Debug)]
enum Incoming {
    Action(Action),
}

/// Everything the project features keep. One for the whole application. Only `dispatch` (and
/// the dialogs' own fields while they are drawn) change it.
#[derive(Default)]
pub struct ProjectApp {
    /// The chooser and the clipboard.
    pub services: ProjectServices,
    /// The new project dialog, while it is open.
    pub new_dialog: Option<NewProjectDialog>,
    /// "Not a project. Make it one?" for this folder, while it is asked.
    pub prompt: Option<PathBuf>,
    /// The import that runs now.
    pub import: Option<RunningImport>,
    /// Imports waiting for their turn.
    queue: VecDeque<Request>,
    /// "Apply to the remaining clashes" in the clash dialog.
    pub apply_to_all: bool,
    /// The result of the last import, while it is shown.
    pub summary: Option<ImportSummary>,
    /// The list of "Details" is open.
    pub details_open: bool,
    /// The folder picker, while it is open.
    pub picker: Option<PickerDialog>,
    /// Pasted folders waiting for the picker, with their target.
    pending_folders: VecDeque<(PathBuf, PathBuf)>,
    /// "Move to the project's trash?" for this path below the root, while it is asked.
    pub confirm_trash: Option<PathBuf>,
    /// The trash dialog's list, while it is open.
    pub trash: Option<Vec<TrashEntry>>,
    /// After an import of several files: the first one, to be chosen in the explorer once the
    /// scan has it.
    select_after: Option<PathBuf>,
    inbox: Arc<Mutex<Vec<Incoming>>>,
}

impl std::fmt::Debug for ProjectApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectApp").finish_non_exhaustive()
    }
}

impl ProjectApp {
    /// Runs `work` on a thread of its own; the action it returns is dispatched on a later
    /// frame.
    fn in_background(
        &self,
        ctx: &egui::Context,
        name: &str,
        work: impl FnOnce(&ProjectServices) -> Option<Action> + Send + 'static,
    ) {
        let (services, inbox, ctx) = (self.services.clone(), Arc::clone(&self.inbox), ctx.clone());
        let spawned = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                if let Some(action) = work(&services) {
                    inbox
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(Incoming::Action(action));
                }
                ctx.request_repaint();
            });
        if let Err(e) = spawned {
            log::error!("cannot start the {name} thread: {e}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Dispatch

fn notice(state: &mut AppState, text: String, ctx: &egui::Context) {
    action::dispatch(state, Action::ShowNotice(text), ctx);
}

fn base_dir(state: &AppState) -> PathBuf {
    state
        .paths
        .as_ref()
        .map(refdesk_core::Paths::default_projects_dir)
        .unwrap_or_default()
}

/// `Action::ImportIntoProject`: copies `sources` into the folder `into` of the open project.
/// Without a project, PDFs open as tabs and a single folder opens as the workspace (PROJ-7).
pub fn import_into(
    state: &mut AppState,
    sources: Vec<PathBuf>,
    into: PathBuf,
    ctx: &egui::Context,
) {
    let Some((root, _)) = open_project(state) else {
        open_without_project(state, sources, ctx);
        return;
    };
    let root = root.to_path_buf();
    if sources.is_empty() {
        return;
    }
    enqueue(
        state,
        Request::Sources {
            root,
            sources,
            into,
        },
        ctx,
    );
}

/// What pasted or dropped files do while no project is open: PDFs open as tabs, a single folder
/// becomes the workspace.
fn open_without_project(state: &mut AppState, sources: Vec<PathBuf>, ctx: &egui::Context) {
    let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) = sources.into_iter().partition(|p| p.is_dir());
    let pdfs: Vec<OpenTarget> = files
        .into_iter()
        .filter(|p| p.file_name().is_some_and(is_pdf_name))
        .map(|path| OpenTarget { path, page: None })
        .collect();
    if let [dir] = dirs.as_slice() {
        action::dispatch(state, Action::OpenFolder(dir.clone()), ctx);
    }
    if !pdfs.is_empty() {
        action::dispatch(state, Action::OpenFiles(pdfs), ctx);
    }
}

fn enqueue(state: &mut AppState, request: Request, ctx: &egui::Context) {
    state.project.queue.push_back(request);
    start_next(state, ctx);
}

/// Starts the next waiting import when none runs.
fn start_next(state: &mut AppState, ctx: &egui::Context) {
    if state.project.import.is_some() {
        return;
    }
    let Some(request) = state.project.queue.pop_front() else {
        return;
    };
    let notifier = action::notifier(ctx);
    let (root, job) = match request {
        Request::Sources {
            root,
            sources,
            into,
        } => {
            let job = ImportJob::start(root.clone(), sources, into, ConflictPolicy::Ask, notifier);
            (root, job)
        }
        Request::Items { root, items } => {
            let job = ImportJob::start_items(root.clone(), items, ConflictPolicy::Ask, notifier);
            (root, job)
        }
    };
    state.project.apply_to_all = false;
    state.project.summary = None;
    state.project.details_open = false;
    state.project.import = Some(RunningImport { job, root });
    ctx.request_repaint();
}

/// Applies one [`ProjectAction`].
pub fn dispatch(state: &mut AppState, action: ProjectAction, ctx: &egui::Context) {
    match action {
        ProjectAction::NewDialog => {
            state.project.new_dialog = Some(NewProjectDialog::new(base_dir(state)));
        }
        ProjectAction::CloseNewDialog => state.project.new_dialog = None,
        ProjectAction::BrowseLocation => {
            let start = state.project.new_dialog.as_ref().map(|d| d.base.clone());
            state
                .project
                .in_background(ctx, "refdesk-project-chooser", move |s| {
                    let title = tr("project.new.browse_title");
                    let picked = s.chooser.pick_folder(title, start.as_deref())?;
                    Some(Action::Project(ProjectAction::SetLocationBase(picked)))
                });
        }
        ProjectAction::SetLocationBase(base) => {
            if let Some(dialog) = &mut state.project.new_dialog {
                dialog.set_base(base);
            }
        }
        ProjectAction::Create { root, name } => create(state, &root, &name, ctx),
        ProjectAction::OpenDialog => {
            let start = base_dir(state);
            state
                .project
                .in_background(ctx, "refdesk-project-chooser", move |s| {
                    let picked = s
                        .chooser
                        .pick_folder(tr("project.open_title"), Some(&start))?;
                    Some(Action::Project(ProjectAction::Open(picked)))
                });
        }
        ProjectAction::Open(path) => {
            if core_project::is_project(&path) {
                state.project.prompt = None;
                action::dispatch(state, Action::OpenFolder(path), ctx);
            } else if path.is_dir() {
                state.project.prompt = Some(path);
            } else {
                notice(
                    state,
                    tr("project.missing").replace("{}", &path_text(&path)),
                    ctx,
                );
            }
        }
        ProjectAction::ClosePrompt => state.project.prompt = None,
        ProjectAction::ImportDialog { into } => {
            let Some((root, _)) = open_project(state) else {
                return;
            };
            let start = root.join(&into);
            state
                .project
                .in_background(ctx, "refdesk-import-chooser", move |s| {
                    let files = s
                        .chooser
                        .pick_pdfs(tr("project.import_title"), Some(&start));
                    (!files.is_empty()).then_some(Action::ImportIntoProject {
                        sources: files,
                        into,
                    })
                });
        }
        ProjectAction::ImportFolderDialog { into } => {
            if !has_project(state) {
                return;
            }
            state
                .project
                .in_background(ctx, "refdesk-import-chooser", move |s| {
                    let folder = s
                        .chooser
                        .pick_folder(tr("project.import_folder_title"), None)?;
                    Some(Action::Project(ProjectAction::OpenPicker { folder, into }))
                });
        }
        ProjectAction::OpenPicker { folder, into } => {
            let Some((root, _)) = open_project(state) else {
                return;
            };
            let root = root.to_path_buf();
            if state.project.picker.is_some() {
                state.project.pending_folders.push_back((folder, into));
            } else {
                state.project.picker = Some(PickerDialog::start(&root, folder, into, ctx));
            }
        }
        ProjectAction::ClosePicker => {
            state.project.picker = None;
            if let Some((folder, into)) = state.project.pending_folders.pop_front() {
                dispatch(state, ProjectAction::OpenPicker { folder, into }, ctx);
            }
        }
        ProjectAction::ImportItems(items) => {
            let Some((root, _)) = open_project(state) else {
                return;
            };
            let root = root.to_path_buf();
            if !items.is_empty() {
                enqueue(state, Request::Items { root, items }, ctx);
            }
        }
        ProjectAction::Paste { text } => {
            let into = import_target(state);
            state.project.in_background(ctx, "refdesk-paste", move |s| {
                let mut files = s.clipboard.file_list();
                if files.is_empty()
                    && let Some(text) = &text
                {
                    files = paths_from_text(text);
                }
                (!files.is_empty())
                    .then_some(Action::Project(ProjectAction::Pasted { files, into }))
            });
        }
        ProjectAction::Pasted { files, into } => pasted(state, files, into, ctx),
        ProjectAction::Cancel => {
            if let Some(run) = &state.project.import {
                run.job.cancel();
            }
        }
        ProjectAction::Resolve { choice, all } => {
            if let Some(run) = &state.project.import {
                run.job.resolve(choice, all);
            }
        }
        ProjectAction::ShowDetails => state.project.details_open = true,
        ProjectAction::CloseDetails => state.project.details_open = false,
        ProjectAction::NewFolder { rel } => {
            let Some((root, _)) = open_project(state) else {
                return;
            };
            match core_project::create_folder(root, &rel) {
                Ok(_) => notice(
                    state,
                    tr("project.folder_made").replace("{}", &path_text(&rel)),
                    ctx,
                ),
                Err(e) => notice(state, error_text(&e), ctx),
            }
            crate::panels::explorer::refresh(state);
        }
        ProjectAction::Rename { from, to } => rename(state, &from, &to, ctx),
        ProjectAction::AskTrash { rel } => state.project.confirm_trash = Some(rel),
        ProjectAction::CancelTrash => state.project.confirm_trash = None,
        ProjectAction::Trash { rel } => {
            state.project.confirm_trash = None;
            trash(state, &rel, ctx);
        }
        ProjectAction::ShowTrash => reload_trash(state),
        ProjectAction::CloseTrash => state.project.trash = None,
        ProjectAction::Restore(id) => {
            let Some((root, _)) = open_project(state) else {
                return;
            };
            match core_project::restore(root, &id) {
                Ok(rel) => notice(
                    state,
                    tr("project.restored").replace("{}", &path_text(&rel)),
                    ctx,
                ),
                Err(e) => notice(state, error_text(&e), ctx),
            }
            crate::panels::explorer::refresh(state);
            reload_trash(state);
        }
        ProjectAction::EmptyTrash => {
            if let Some((root, _)) = open_project(state)
                && let Err(e) = core_project::empty_trash(root)
            {
                notice(state, error_text(&e), ctx);
            }
            reload_trash(state);
        }
    }
    ctx.request_repaint();
}

/// A core error as a message for the user.
fn error_text(e: &ProjectError) -> String {
    let path = |p: &PathBuf| path_text(p);
    match e {
        ProjectError::EmptyName => tr("project.name.empty").to_owned(),
        ProjectError::AlreadyProject(p) => tr("project.error.already").replace("{}", &path(p)),
        ProjectError::Exists(p) => tr("project.error.exists").replace("{}", &path(p)),
        ProjectError::Missing(p) => tr("project.error.missing").replace("{}", &path(p)),
        ProjectError::Outside(p) | ProjectError::Protected(p) | ProjectError::NotProject(p) => {
            tr("project.error.path").replace("{}", &path(p))
        }
        ProjectError::Io { .. } => tr("project.error.io").replace("{}", &e.to_string()),
    }
}

fn create(state: &mut AppState, root: &Path, name: &str, ctx: &egui::Context) {
    match Project::create(root, name) {
        Ok(_) => {
            state.project.new_dialog = None;
            state.project.prompt = None;
            action::dispatch(state, Action::OpenFolder(root.to_path_buf()), ctx);
        }
        Err(e) => {
            let text = error_text(&e);
            match &mut state.project.new_dialog {
                Some(dialog) => dialog.failure = Some(text),
                None => notice(state, text, ctx),
            }
        }
    }
}

/// `ProjectAction::Pasted`: with a project, folders go through the picker and files are
/// imported; without one, PDFs open and a single folder becomes the workspace (PROJ-7).
fn pasted(state: &mut AppState, files: Vec<PathBuf>, into: PathBuf, ctx: &egui::Context) {
    if !has_project(state) {
        open_without_project(state, files, ctx);
        return;
    }
    let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) = files.into_iter().partition(|p| p.is_dir());
    for folder in dirs {
        dispatch(
            state,
            ProjectAction::OpenPicker {
                folder,
                into: into.clone(),
            },
            ctx,
        );
    }
    if !files.is_empty() {
        import_into(state, files, into, ctx);
    }
}

/// The open tabs (PDF tabs) whose document lies at or below `path`.
fn tabs_below(state: &AppState, path: &Path) -> Vec<(TabId, PathBuf)> {
    state
        .tabs
        .iter()
        .filter_map(|(id, tab)| match tab {
            Tab::Pdf(pdf) => {
                let doc = state.docs.get(pdf.doc)?;
                doc.path()
                    .starts_with(path)
                    .then(|| (id, doc.path().to_path_buf()))
            }
            _ => None,
        })
        .collect()
}

/// Renames inside the project; the PDF tabs of documents that moved follow them: they keep their
/// views and get a document at the new path.
fn rename(state: &mut AppState, from: &Path, to: &Path, ctx: &egui::Context) {
    let Some((root, _)) = open_project(state) else {
        return;
    };
    let root = root.to_path_buf();
    let old = crate::state::normalize_path(&root.join(from));
    if let Err(e) = core_project::rename(&root, from, to) {
        notice(state, error_text(&e), ctx);
        return;
    }
    let new = crate::state::normalize_path(&root.join(to));
    let moved = tabs_below(state, &old);
    let mut opened: HashMap<PathBuf, crate::state::DocId> = HashMap::new();
    let mut old_docs = HashSet::new();
    for (tab, path) in moved {
        let Ok(rest) = path.strip_prefix(&old) else {
            continue;
        };
        let target = if rest.as_os_str().is_empty() {
            new.clone()
        } else {
            new.join(rest)
        };
        let doc = *opened.entry(target.clone()).or_insert_with(|| {
            state.docs.find(&target).unwrap_or_else(|| {
                state.docs.insert(refdesk_core::DocumentHandle::open(
                    &target,
                    action::notifier(ctx),
                ))
            })
        });
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
            old_docs.insert(pdf.doc);
            pdf.doc = doc;
            pdf.ui = crate::views::pdf_view::PdfViewUiState::default();
        }
    }
    for doc in old_docs {
        if !state.tabs.doc_in_use(doc) {
            state.docs.remove(doc);
        }
    }
    crate::panels::explorer::refresh(state);
}

/// Moves `rel` to the project's trash after closing the tabs of the documents in it.
fn trash(state: &mut AppState, rel: &Path, ctx: &egui::Context) {
    let Some((root, _)) = open_project(state) else {
        return;
    };
    let root = root.to_path_buf();
    let path = crate::state::normalize_path(&root.join(rel));
    for (tab, _) in tabs_below(state, &path) {
        action::dispatch(state, Action::CloseTab(tab), ctx);
    }
    match core_project::trash(&root, rel) {
        Ok(_) => notice(
            state,
            tr("project.trashed").replace("{}", &path_text(rel)),
            ctx,
        ),
        Err(e) => notice(state, error_text(&e), ctx),
    }
    crate::panels::explorer::refresh(state);
}

fn reload_trash(state: &mut AppState) {
    let Some((root, _)) = open_project(state) else {
        state.project.trash = None;
        return;
    };
    state.project.trash = Some(core_project::list_trash(root).unwrap_or_else(|e| {
        log::warn!("cannot read the project's trash: {e}");
        Vec::new()
    }));
}

// ---------------------------------------------------------------------------------------------
// Every frame

/// Takes in what the chooser and clipboard threads brought, follows the running import and the
/// picker's survey, and finishes an import that has ended. Called once per frame from
/// `logic()`.
pub fn tick(state: &mut AppState, ctx: &egui::Context) {
    let incoming = std::mem::take(
        &mut *state
            .project
            .inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    for Incoming::Action(a) in incoming {
        state.actions.push(a);
    }
    if let Some(picker) = &mut state.project.picker {
        picker.poll();
    }
    let finished = state
        .project
        .import
        .as_ref()
        .map(|run| run.job.snapshot())
        .filter(|snap| snap.finished());
    if let Some(snap) = finished
        && let Some(run) = state.project.import.take()
    {
        finish_import(state, &run.root, &snap, ctx);
        start_next(state, ctx);
    }
    select_imported(state);
}

/// An import ended: the result notice, the explorer scanned again, and the imported document
/// opened (one) or chosen in the explorer (several).
fn finish_import(state: &mut AppState, root: &Path, snap: &ImportSnapshot, ctx: &egui::Context) {
    let counts = ImportCounts::of(snap);
    state.project.summary = Some(ImportSummary {
        text: summary_text(snap),
        details: detail_lines(snap),
        has_details: counts.has_details(),
        until: ctx.input(|i| i.time) + SUMMARY_SECS,
    });
    // The watcher sees the new files too; this covers a folder that is not watched.
    if state.workspace.as_ref().is_some_and(|w| w.root == root) {
        crate::panels::explorer::refresh(state);
    }
    let copied: Vec<PathBuf> = snap
        .results
        .iter()
        .filter(|r| matches!(r.outcome, ImportOutcome::Imported | ImportOutcome::Replaced))
        .map(|r| root.join(&r.dest))
        .collect();
    match copied.as_slice() {
        [one] => {
            let window = state.focused_window().id;
            state.actions.push(Action::OpenFromExplorer {
                path: one.clone(),
                window,
                how: ExplorerOpen::Preview,
            });
        }
        [first, ..] => state.project.select_after = Some(first.clone()),
        [] => {}
    }
}

/// Chooses the first document of the last import in the focused window's explorer, once the
/// scan has it.
fn select_imported(state: &mut AppState) {
    let Some(path) = state.project.select_after.clone() else {
        return;
    };
    let Some(index) = state.workspace.as_ref().and_then(|w| w.scan.index()) else {
        return;
    };
    let Ok(rel) = path.strip_prefix(&index.root) else {
        state.project.select_after = None;
        return;
    };
    if index.find(rel).is_none() {
        return;
    }
    let w = state.focused_window_index();
    if let Some(window) = state.windows.get_mut(w)
        && window.explorer.select_path(&index, rel)
    {
        state.project.select_after = None;
    }
}

/// Takes a paste (Ctrl+V) out of this frame's input when no text field has the keyboard, and
/// turns it into [`ProjectAction::Paste`]. Called once per frame by the window that has the
/// keyboard, after the shortcuts.
pub fn take_paste(ctx: &egui::Context, state: &AppState) -> Option<Action> {
    if ctx.text_edit_focused() || state.palette.open.is_some() || state.key_grab.is_some() {
        return None;
    }
    ctx.input_mut(|input| {
        let at = input
            .events
            .iter()
            .position(|e| matches!(e, egui::Event::Paste(_) | egui::Event::PasteImage(_)))?;
        let text = match input.events.remove(at) {
            egui::Event::Paste(text) => Some(text),
            _ => None,
        };
        Some(Action::Project(ProjectAction::Paste { text }))
    })
}

// ---------------------------------------------------------------------------------------------
// Drawing

fn named_button(ui: &mut Ui, text: &str, enabled: bool) -> egui::Response {
    let response = ui.add_enabled(enabled, egui::Button::new(text));
    let label = text.to_owned();
    response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, enabled, &label));
    response
}

fn dialog_window(title: &str) -> egui::Window<'_> {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
}

/// Draws the project dialogs that are open. Called by the window that has the focus.
pub fn show_dialogs(ctx: &egui::Context, state: &mut AppState, queue: &mut Vec<Action>) {
    new_project_dialog(ctx, state, queue);
    prompt_dialog(ctx, state, queue);
    conflict_dialog(ctx, state, queue);
    picker_dialog(ctx, state, queue);
    details_dialog(ctx, state, queue);
    trash_dialogs(ctx, state, queue);
}

fn push(queue: &mut Vec<Action>, action: ProjectAction) {
    queue.push(Action::Project(action));
}

fn new_project_dialog(ctx: &egui::Context, state: &mut AppState, queue: &mut Vec<Action>) {
    let Some(dialog) = &mut state.project.new_dialog else {
        return;
    };
    dialog.check(ctx);
    let mut open = true;
    dialog_window(tr("project.new.title"))
        .open(&mut open)
        .show(ctx, |ui| {
            egui::Grid::new("project-new-grid")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label(tr("project.new.name"));
                    let name = ui.add(
                        egui::TextEdit::singleline(&mut dialog.name)
                            .desired_width(320.0)
                            .hint_text(tr("project.new.name_hint")),
                    );
                    if name.changed() {
                        dialog.name_changed();
                    }
                    ui.end_row();
                    ui.label(tr("project.new.location"));
                    ui.horizontal(|ui| {
                        let location = ui.add(
                            egui::TextEdit::singleline(&mut dialog.location).desired_width(240.0),
                        );
                        if location.changed() {
                            dialog.location_changed();
                        }
                        if named_button(ui, tr("project.new.browse"), true).clicked() {
                            push(queue, ProjectAction::BrowseLocation);
                        }
                    });
                    ui.end_row();
                });
            let view = dialog.view();
            if let Some(error) = &view.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            if let Some(note) = &view.note {
                ui.label(RichText::new(note).weak());
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let (label, enabled) = match view.button {
                    CreateButton::Open => (tr("project.new.open"), true),
                    CreateButton::Create => (tr("project.new.create"), true),
                    CreateButton::Disabled => (tr("project.new.create"), false),
                };
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                if named_button(ui, label, enabled).clicked() || (enabled && enter) {
                    match view.button {
                        CreateButton::Open => {
                            queue.push(Action::OpenFolder(dialog.root()));
                            push(queue, ProjectAction::CloseNewDialog);
                        }
                        _ => push(
                            queue,
                            ProjectAction::Create {
                                root: dialog.root(),
                                name: dialog.name.clone(),
                            },
                        ),
                    }
                }
                if named_button(ui, tr("project.cancel"), true).clicked() {
                    push(queue, ProjectAction::CloseNewDialog);
                }
            });
        });
    if !open {
        push(queue, ProjectAction::CloseNewDialog);
    }
}

fn prompt_dialog(ctx: &egui::Context, state: &mut AppState, queue: &mut Vec<Action>) {
    let Some(folder) = state.project.prompt.clone() else {
        return;
    };
    let mut open = true;
    dialog_window(tr("project.prompt.title"))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(tr("project.prompt.text").replace("{}", &path_text(&folder)));
            ui.horizontal(|ui| {
                if named_button(ui, tr("project.prompt.make"), true).clicked() {
                    let name = folder
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| tr("project.prompt.default_name").to_owned());
                    push(
                        queue,
                        ProjectAction::Create {
                            root: folder.clone(),
                            name,
                        },
                    );
                }
                if named_button(ui, tr("project.prompt.folder"), true).clicked() {
                    queue.push(Action::OpenFolder(folder.clone()));
                    push(queue, ProjectAction::ClosePrompt);
                }
                if named_button(ui, tr("project.cancel"), true).clicked() {
                    push(queue, ProjectAction::ClosePrompt);
                }
            });
        });
    if !open {
        push(queue, ProjectAction::ClosePrompt);
    }
}

/// "12.3 KB · 2026-11-27 10:00" for one side of a clash.
fn side_text(size: u64, mtime: Option<i64>) -> String {
    let when = mtime
        .and_then(|t| jiff::Timestamp::from_second(t).ok())
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default();
    format!("{} · {when}", human_size(size))
}

fn conflict_dialog(ctx: &egui::Context, state: &mut AppState, queue: &mut Vec<Action>) {
    let Some(run) = &state.project.import else {
        return;
    };
    let snap = run.job.snapshot();
    let ImportState::Waiting(conflict) = &snap.state else {
        return;
    };
    let apply_to_all = &mut state.project.apply_to_all;
    dialog_window(tr("project.conflict.title")).show(ctx, |ui| {
        let name = conflict
            .dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        ui.label(tr("project.conflict.text").replace("{}", &name));
        egui::Grid::new("project-conflict")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label(tr("project.conflict.existing"));
                ui.label(side_text(conflict.existing_size, conflict.existing_mtime));
                ui.end_row();
                ui.label(tr("project.conflict.incoming"));
                ui.label(side_text(conflict.source_size, conflict.source_mtime));
                ui.end_row();
            });
        ui.checkbox(apply_to_all, tr("project.conflict.all"));
        ui.horizontal(|ui| {
            for (key, choice) in [
                ("project.conflict.replace", ConflictChoice::Replace),
                ("project.conflict.keep_both", ConflictChoice::KeepBoth),
                ("project.conflict.skip", ConflictChoice::Skip),
            ] {
                if named_button(ui, tr(key), true).clicked() {
                    push(
                        queue,
                        ProjectAction::Resolve {
                            choice,
                            all: *apply_to_all,
                        },
                    );
                }
            }
        });
    });
}

fn picker_dialog(ctx: &egui::Context, state: &mut AppState, queue: &mut Vec<Action>) {
    let target_name = |into: &Path| match (open_project(state), into.as_os_str().is_empty()) {
        (Some((_, info)), true) => info.name.clone(),
        _ => path_text(into),
    };
    let Some(into_name) = state.project.picker.as_ref().map(|p| target_name(&p.into)) else {
        return;
    };
    let Some(picker) = &mut state.project.picker else {
        return;
    };
    let mut open = true;
    let title = tr("project.pick.title").replace("{}", &path_text(&picker.folder));
    egui::Window::new(title)
        .id(egui::Id::unique("project-picker"))
        .collapsible(false)
        .default_size([520.0, 420.0])
        .open(&mut open)
        .show(ctx, |ui| {
            if let Some(err) = &picker.error {
                ui.colored_label(ui.visuals().error_fg_color, err);
                if named_button(ui, tr("project.cancel"), true).clicked() {
                    push(queue, ProjectAction::ClosePicker);
                }
                return;
            }
            let Some(list) = &mut picker.list else {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label(tr("project.pick.scanning"));
                });
                return;
            };
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut picker.filter)
                        .hint_text(tr("project.pick.filter"))
                        .desired_width(200.0),
                );
                if named_button(ui, tr("project.pick.all"), true).clicked() {
                    list.set_shown(&picker.filter, true);
                }
                if named_button(ui, tr("project.pick.none"), true).clicked() {
                    list.set_shown(&picker.filter, false);
                }
                if picker.checking {
                    ui.add(egui::Spinner::new().size(12.0));
                }
            });
            let (count, bytes) = list.totals();
            ui.label(
                tr("project.pick.totals")
                    .replace("{n}", &count.to_string())
                    .replace("{size}", &human_size(bytes)),
            );
            let shown = list.shown(&picker.filter);
            let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
            egui::ScrollArea::vertical()
                .max_height(280.0)
                .auto_shrink([false, true])
                .show_rows(ui, row_h, shown.len(), |ui, range| {
                    for &i in &shown[range] {
                        let Some(row) = list.rows.get(i) else {
                            continue;
                        };
                        let mut checked = row.checked;
                        let mut text = format!("{}  ·  {}", row.label, human_size(row.size));
                        if row.existing.is_some() {
                            text.push_str("  ·  ");
                            text.push_str(tr("project.pick.already"));
                        }
                        let name = pick_row_name(row);
                        let response = ui.checkbox(&mut checked, text);
                        response.widget_info(|| {
                            WidgetInfo::selected(egui::Role::CheckBox, true, checked, &name)
                        });
                        if response.clicked() {
                            list.toggle(i);
                        }
                    }
                });
            ui.separator();
            ui.checkbox(&mut picker.keep_structure, tr("project.pick.keep"));
            ui.label(tr("project.pick.into").replace("{}", &into_name));
            ui.horizontal(|ui| {
                let label = tr("project.pick.import").replace("{n}", &count.to_string());
                if named_button(ui, &label, count > 0).clicked() {
                    let items = list.items(&picker.folder, &picker.into, picker.keep_structure);
                    push(queue, ProjectAction::ImportItems(items));
                    push(queue, ProjectAction::ClosePicker);
                }
                if named_button(ui, tr("project.cancel"), true).clicked() {
                    push(queue, ProjectAction::ClosePicker);
                }
            });
        });
    if !open {
        push(queue, ProjectAction::ClosePicker);
    }
}

fn details_dialog(ctx: &egui::Context, state: &mut AppState, queue: &mut Vec<Action>) {
    if !state.project.details_open {
        return;
    }
    let Some(summary) = &state.project.summary else {
        return;
    };
    let mut open = true;
    egui::Window::new(tr("project.details.title"))
        .collapsible(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(&summary.text);
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .show(ui, |ui| {
                    for line in &summary.details {
                        ui.label(line);
                    }
                });
            if named_button(ui, tr("project.close"), true).clicked() {
                push(queue, ProjectAction::CloseDetails);
            }
        });
    if !open {
        push(queue, ProjectAction::CloseDetails);
    }
}

fn trash_dialogs(ctx: &egui::Context, state: &mut AppState, queue: &mut Vec<Action>) {
    if let Some(rel) = state.project.confirm_trash.clone() {
        dialog_window(tr("project.trash.confirm_title")).show(ctx, |ui| {
            ui.label(tr("project.trash.confirm").replace("{}", &path_text(&rel)));
            ui.horizontal(|ui| {
                if named_button(ui, tr("project.trash.move"), true).clicked() {
                    push(queue, ProjectAction::Trash { rel: rel.clone() });
                }
                if named_button(ui, tr("project.cancel"), true).clicked() {
                    push(queue, ProjectAction::CancelTrash);
                }
            });
        });
    }
    let Some(entries) = &state.project.trash else {
        return;
    };
    let mut open = true;
    egui::Window::new(tr("project.trash.title"))
        .collapsible(false)
        .open(&mut open)
        .show(ctx, |ui| {
            if entries.is_empty() {
                ui.weak(tr("project.trash.empty"));
            }
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .show(ui, |ui| {
                    for entry in entries {
                        ui.horizontal(|ui| {
                            ui.label(path_text(&entry.rel_path));
                            let label = tr("project.trash.restore");
                            let response = ui.button(label);
                            let name = format!("{label}: {}", path_text(&entry.rel_path));
                            response.widget_info(|| {
                                WidgetInfo::labeled(egui::Role::Button, true, &name)
                            });
                            if response.clicked() {
                                push(queue, ProjectAction::Restore(entry.id.clone()));
                            }
                        });
                    }
                });
            ui.horizontal(|ui| {
                if named_button(ui, tr("project.trash.empty_button"), !entries.is_empty()).clicked()
                {
                    push(queue, ProjectAction::EmptyTrash);
                }
                if named_button(ui, tr("project.close"), true).clicked() {
                    push(queue, ProjectAction::CloseTrash);
                }
            });
        });
    if !open {
        push(queue, ProjectAction::CloseTrash);
    }
}

/// Draws the import's progress (with a cancel button) or its result (with "Details") at the
/// right end of the status bar `bar`.
pub fn show_status(ui: &mut Ui, state: &mut AppState, bar: egui::Rect, queue: &mut Vec<Action>) {
    let now = ui.input(|i| i.time);
    if state
        .project
        .summary
        .as_ref()
        .is_some_and(|s| now >= s.until)
        && !state.project.details_open
    {
        state.project.summary = None;
    }
    let running = state
        .project
        .import
        .as_ref()
        .map(|run| progress_text(&run.job.snapshot(), state.project.queue.len()));
    let summary = state.project.summary.clone();
    if running.is_none() && summary.is_none() {
        return;
    }
    let rect = egui::Rect::from_min_max(egui::pos2(bar.center().x, bar.top()), bar.max);
    let mut strip = ui.new_child(
        UiBuilder::new()
            .id_salt("project_status")
            .max_rect(rect.shrink2(egui::vec2(8.0, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
    );
    if let Some(text) = running {
        if named_button(&mut strip, tr("project.import.cancel"), true).clicked() {
            push(queue, ProjectAction::Cancel);
        }
        strip.add(egui::Label::new(text).truncate());
        strip.add(egui::Spinner::new().size(12.0));
    } else if let Some(summary) = summary {
        if summary.has_details
            && named_button(&mut strip, tr("project.import.details"), true).clicked()
        {
            push(queue, ProjectAction::ShowDetails);
        }
        strip.add(egui::Label::new(summary.text).truncate());
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(
                (summary.until - now).max(0.0),
            ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{dispatch as app_dispatch, dispatch_queued};
    use crate::panels::explorer::{
        MenuOn, WorkspaceView, header_title, menu_entries, menu_entries_for,
    };
    use crate::views::test_support::{Repaints, watch_repaints};
    use refdesk_core::Paths;
    use refdesk_core::testutil::PdfBuilder;
    use std::sync::atomic::AtomicU32;

    // --- helpers ---------------------------------------------------------------------------

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "refdesk-wp91-{}-{}-{name}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp dir");
            TempDir(crate::state::normalize_path(&path))
        }

        fn join(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn pdf(text: &str) -> Vec<u8> {
        PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, text);
            })
            .build()
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("dirs");
        }
        std::fs::write(path, bytes).expect("write");
    }

    #[derive(Default)]
    struct FakeChooser {
        pdfs: Mutex<Vec<PathBuf>>,
        folder: Mutex<Option<PathBuf>>,
        asked: Mutex<Vec<String>>,
    }

    impl Chooser for FakeChooser {
        fn pick_pdfs(&self, title: &str, _start: Option<&Path>) -> Vec<PathBuf> {
            self.asked.lock().expect("lock").push(title.to_owned());
            self.pdfs.lock().expect("lock").clone()
        }
        fn pick_folder(&self, title: &str, _start: Option<&Path>) -> Option<PathBuf> {
            self.asked.lock().expect("lock").push(title.to_owned());
            self.folder.lock().expect("lock").clone()
        }
    }

    #[derive(Default)]
    struct FakeClipboard(Mutex<Vec<PathBuf>>);

    impl FileClipboard for FakeClipboard {
        fn file_list(&self) -> Vec<PathBuf> {
            self.0.lock().expect("lock").clone()
        }
    }

    struct Env {
        ctx: egui::Context,
        repaints: Repaints,
        state: AppState,
        dir: TempDir,
        chooser: Arc<FakeChooser>,
        clipboard: Arc<FakeClipboard>,
    }

    impl Env {
        fn new(name: &str) -> Env {
            let dir = TempDir::new(name);
            let ctx = egui::Context::default();
            let repaints = watch_repaints(&ctx);
            let mut state = AppState::new();
            state.paths = Some(Paths::under(&dir.join("app")));
            let chooser = Arc::new(FakeChooser::default());
            let clipboard = Arc::new(FakeClipboard::default());
            state.project.services = ProjectServices {
                chooser: Arc::clone(&chooser) as Arc<dyn Chooser>,
                clipboard: Arc::clone(&clipboard) as Arc<dyn FileClipboard>,
            };
            Env {
                ctx,
                repaints,
                state,
                dir,
                chooser,
                clipboard,
            }
        }

        fn run(&mut self, action: Action) {
            app_dispatch(&mut self.state, action, &self.ctx);
            dispatch_queued(&mut self.state, &self.ctx);
        }

        fn project(&mut self, action: ProjectAction) {
            self.run(Action::Project(action));
        }

        /// Runs the app's per-frame work until `check` holds.
        fn settle(&mut self, what: &str, check: impl Fn(&AppState) -> bool) {
            let Env {
                ctx,
                repaints,
                state,
                ..
            } = self;
            repaints.wait_until(what, || {
                // egui passes on one repaint request per frame.
                ctx.run_ui(egui::RawInput::default(), |_| {})
                    .drop_without_applying_deltas();
                tick(state, ctx);
                crate::panels::explorer::poll_workspace(state);
                dispatch_queued(state, ctx);
                check(state)
            });
        }

        /// A project called `name` at `<dir>/<rel>`, opened, with its first scan done.
        fn open_project(&mut self, rel: &str, name: &str) -> PathBuf {
            let root = self.dir.join(rel);
            Project::create_at(&root, name, 1_000).expect("project");
            self.run(Action::OpenFolder(root.clone()));
            self.settle("the first scan", |s| index_of(s).is_some());
            root
        }

        fn import_done(&mut self) {
            self.settle("the import", |s| {
                s.project.import.is_none() && s.project.queue.is_empty()
            });
        }

        fn has_file(&mut self, rel: &str) {
            let rel = PathBuf::from(rel);
            self.settle("the file in the explorer", |s| {
                index_of(s).is_some_and(|i| i.find(&rel).is_some())
            });
        }
    }

    fn index_of(state: &AppState) -> Option<Arc<refdesk_core::WorkspaceIndex>> {
        state.workspace.as_ref().and_then(|w| w.scan.index())
    }

    fn snapshot(state: &AppState) -> Option<Arc<ImportSnapshot>> {
        state.project.import.as_ref().map(|r| r.job.snapshot())
    }

    fn waiting_clash(state: &AppState) -> bool {
        snapshot(state).is_some_and(|s| matches!(s.state, ImportState::Waiting(_)))
    }

    fn pdf_tabs(state: &AppState) -> Vec<(TabId, PathBuf, bool)> {
        let mut out: Vec<(TabId, PathBuf, bool)> = state
            .tabs
            .iter()
            .filter_map(|(id, t)| match t {
                Tab::Pdf(p) => Some((id, state.docs.get(p.doc)?.path().to_path_buf(), p.preview)),
                _ => None,
            })
            .collect();
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    // --- pure decisions ---------------------------------------------------------------------

    #[test]
    fn the_location_follows_the_name_until_it_is_changed_by_hand() {
        let base = PathBuf::from("/home/u/Documents/refdesk");
        let mut d = NewProjectDialog::new(base.clone());
        assert_eq!(d.location, base.display().to_string());
        d.name = "Net / Specs".into();
        d.name_changed();
        assert_eq!(d.root(), base.join("Net  Specs"));
        d.location = "/data/mine".into();
        d.location_changed();
        d.name = "Other".into();
        d.name_changed();
        assert_eq!(d.root(), PathBuf::from("/data/mine"));
        // Choosing a folder with "Browse…" puts the project in it and follows the name again.
        d.set_base(PathBuf::from("/data"));
        assert_eq!(d.root(), PathBuf::from("/data/Other"));
        assert_eq!(default_location(&base, "..x"), base.join("x"));
    }

    #[test]
    fn the_dialog_says_what_is_wrong_and_what_will_happen() {
        let loc = "/p/Specs";
        let v = new_project_view("", loc, true, Some(&FolderState::Missing));
        assert_eq!(v.error.as_deref(), Some(tr("project.new.empty_name")));
        assert_eq!(v.button, CreateButton::Disabled);
        let v = new_project_view("...", "/p", true, Some(&FolderState::Missing));
        assert_eq!(v.error.as_deref(), Some(tr("project.new.unusable_name")));
        let v = new_project_view("Specs", "relative/x", false, Some(&FolderState::Missing));
        assert_eq!(v.error.as_deref(), Some(tr("project.new.bad_location")));
        let v = new_project_view("Specs", loc, true, None);
        assert_eq!(
            (v.note.as_deref(), v.button),
            (Some(tr("project.new.checking")), CreateButton::Disabled)
        );
        for state in [FolderState::Missing, FolderState::Empty] {
            let v = new_project_view("Specs", loc, true, Some(&state));
            assert_eq!(
                (v.error, v.note, v.button),
                (None, None, CreateButton::Create)
            );
        }
        let v = new_project_view("Specs", loc, true, Some(&FolderState::HasFiles(3)));
        assert_eq!(v.note, Some(tr("project.new.has_pdfs").replace("{n}", "3")));
        assert_eq!(v.button, CreateButton::Create);
        let v = new_project_view("Specs", loc, true, Some(&FolderState::HasFiles(0)));
        assert_eq!(v.note.as_deref(), Some(tr("project.new.existing")));
        let v = new_project_view("", loc, true, Some(&FolderState::Project("Old".into())));
        assert_eq!(v.button, CreateButton::Open);
        assert_eq!(
            v.note,
            Some(tr("project.new.is_project").replace("{}", "Old"))
        );
        let v = new_project_view("Specs", loc, true, Some(&FolderState::NotFolder));
        assert_eq!(v.error.as_deref(), Some(tr("project.new.not_folder")));
        assert_eq!(v.button, CreateButton::Disabled);
    }

    #[test]
    fn folders_are_looked_at_for_the_dialog() {
        let dir = TempDir::new("probe");
        let no = AtomicBool::new(false);
        assert_eq!(
            probe_folder(&dir.join("missing"), &no),
            FolderState::Missing
        );
        std::fs::create_dir_all(dir.join("empty")).expect("dir");
        assert_eq!(probe_folder(&dir.join("empty"), &no), FolderState::Empty);
        write(&dir.join("full/a.pdf"), b"a");
        write(&dir.join("full/x/b.PDF"), b"b");
        write(&dir.join("full/notes.txt"), b"n");
        assert_eq!(
            probe_folder(&dir.join("full"), &no),
            FolderState::HasFiles(2)
        );
        write(&dir.join("other/notes.txt"), b"n");
        assert_eq!(
            probe_folder(&dir.join("other"), &no),
            FolderState::HasFiles(0)
        );
        write(&dir.join("file"), b"f");
        assert_eq!(probe_folder(&dir.join("file"), &no), FolderState::NotFolder);
        Project::create_at(&dir.join("proj"), "Old One", 1).expect("project");
        assert_eq!(
            probe_folder(&dir.join("proj"), &no),
            FolderState::Project("Old One".into())
        );
    }

    fn files(paths: &[(&str, u64)]) -> Vec<SurveyFile> {
        paths
            .iter()
            .map(|(p, size)| SurveyFile {
                rel: PathBuf::from(p),
                size: *size,
            })
            .collect()
    }

    #[test]
    fn the_picker_starts_with_everything_checked_and_filters_by_path() {
        let mut list = PickList::new(&files(&[
            ("docs/spec/a.pdf", 100),
            ("docs/guide.pdf", 50),
            ("Other/B.pdf", 7),
        ]));
        assert!(list.rows.iter().all(|r| r.checked && !r.touched));
        assert_eq!(list.totals(), (3, 157));
        assert_eq!(*list.shown("SPEC"), vec![0]);
        assert_eq!(*list.shown(" docs "), vec![0, 1]);
        assert_eq!(*list.shown(""), vec![0, 1, 2]);
        // "Select none" and "Select all" only touch what the filter lets through.
        list.set_shown("docs", false);
        assert_eq!(
            list.rows.iter().map(|r| r.checked).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(list.totals(), (1, 7));
        list.set_shown("guide", true);
        assert_eq!(list.totals(), (2, 57));
        list.toggle(2);
        assert_eq!(list.totals(), (1, 50));
    }

    #[test]
    fn already_there_unchecks_only_the_rows_left_alone() {
        let mut list = PickList::new(&files(&[("a.pdf", 1), ("b.pdf", 2), ("c.pdf", 3)]));
        list.toggle(1);
        list.toggle(1); // checked again, by hand
        let existing = HashMap::from([
            (0, PathBuf::from("have/a.pdf")),
            (1, PathBuf::from("have/b.pdf")),
        ]);
        list.apply_existing(&existing);
        assert_eq!(
            list.rows
                .iter()
                .map(|r| (r.checked, r.existing.is_some()))
                .collect::<Vec<_>>(),
            [(false, true), (true, true), (true, false)]
        );
        assert!(pick_row_name(&list.rows[0]).contains(tr("project.pick.already")));
        assert_eq!(
            pick_row_name(&list.rows[2]),
            tr("project.pick.row")
                .replace("{path}", "c.pdf")
                .replace("{size}", "3 B")
                .replace("{check}", tr("project.pick.checked"))
        );
    }

    #[test]
    fn the_picker_targets_keep_or_flatten_the_sub_folders() {
        let list = PickList::new(&files(&[("x/a.pdf", 1), ("b.pdf", 2)]));
        let folder = Path::new("/src/Manuals");
        let keep = list.items(folder, Path::new("refs"), true);
        assert_eq!(
            keep,
            vec![
                ImportItem {
                    source: PathBuf::from("/src/Manuals/x/a.pdf"),
                    dest: PathBuf::from("refs/Manuals/x/a.pdf")
                },
                ImportItem {
                    source: PathBuf::from("/src/Manuals/b.pdf"),
                    dest: PathBuf::from("refs/Manuals/b.pdf")
                },
            ]
        );
        let flat = list.items(folder, Path::new(""), false);
        assert_eq!(
            flat.iter().map(|i| i.dest.clone()).collect::<Vec<_>>(),
            [PathBuf::from("a.pdf"), PathBuf::from("b.pdf")]
        );
    }

    #[test]
    fn thousands_of_picker_rows_filter_quickly_and_the_result_is_kept() {
        let many: Vec<SurveyFile> = (0..20_000)
            .map(|i| SurveyFile {
                rel: PathBuf::from(format!("dir{}/doc{i}.pdf", i % 50)),
                size: 1,
            })
            .collect();
        let mut list = PickList::new(&many);
        let start = std::time::Instant::now();
        let shown = list.shown("dir7/");
        assert_eq!(shown.len(), 400);
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        // The same filter again is the same list, not a new one.
        assert!(Arc::ptr_eq(&shown, &list.shown("dir7/")));
        list.set_shown("dir7/", false);
        assert_eq!(list.totals().0, 19_600);
    }

    fn snap(results: Vec<ImportOutcome>, not_pdf: usize, state: ImportState) -> ImportSnapshot {
        ImportSnapshot {
            total: results.len(),
            done: results.len(),
            results: results
                .into_iter()
                .enumerate()
                .map(|(i, outcome)| refdesk_core::ImportResult {
                    source: PathBuf::from(format!("/src/f{i}.pdf")),
                    dest: PathBuf::from(format!("f{i}.pdf")),
                    outcome,
                })
                .collect(),
            not_pdf,
            state,
            ..ImportSnapshot::default()
        }
    }

    #[test]
    fn progress_texts_count_the_file_being_copied() {
        let mut s = ImportSnapshot::default();
        assert_eq!(progress_text(&s, 0), tr("project.import.starting"));
        s.total = 12;
        s.done = 2;
        s.current = Some(PathBuf::from("/src/spec.pdf"));
        let expected = tr("project.import.progress")
            .replace("{i}", "3")
            .replace("{n}", "12")
            .replace("{name}", "spec.pdf");
        assert_eq!(progress_text(&s, 0), expected);
        assert_eq!(
            progress_text(&s, 2),
            format!(
                "{expected}{}",
                tr("project.import.queued").replace("{n}", "2")
            )
        );
    }

    #[test]
    fn result_texts_name_each_kind_of_outcome() {
        use ImportOutcome as O;
        let n = |key: &str, n: usize| tr(key).replace("{n}", &n.to_string());
        let all = snap(
            vec![
                O::Imported,
                O::Replaced,
                O::AlreadyThere {
                    existing: PathBuf::from("have.pdf"),
                },
                O::Skipped(SkipReason::Declined),
                O::Failed("denied".into()),
            ],
            2,
            ImportState::Done,
        );
        assert_eq!(
            summary_text(&all),
            [
                n("project.import.done", 2),
                n("project.import.already", 1),
                n("project.import.skipped", 3),
                n("project.import.failed", 1)
            ]
            .join(", ")
        );
        let counts = ImportCounts::of(&all);
        assert!(counts.has_details());
        let lines = detail_lines(&all);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("f2.pdf — "));
        assert!(lines[2].contains("denied"));

        let clean = snap(vec![O::Imported; 9], 0, ImportState::Done);
        assert_eq!(summary_text(&clean), n("project.import.done", 9));
        assert!(!ImportCounts::of(&clean).has_details());
        let only_dupes = snap(
            vec![O::AlreadyThere {
                existing: PathBuf::from("x.pdf"),
            }],
            0,
            ImportState::Done,
        );
        assert_eq!(
            summary_text(&only_dupes),
            format!(
                "{}, {}",
                n("project.import.done", 0),
                n("project.import.already", 1)
            )
        );
        assert!(!ImportCounts::of(&only_dupes).has_details());
        let cancelled = snap(vec![O::Imported], 0, ImportState::Cancelled);
        assert_eq!(
            summary_text(&cancelled),
            tr("project.import.cancelled").replace("{}", &n("project.import.done", 1))
        );
    }

    #[test]
    fn pasted_text_names_files_only_when_every_line_does() {
        let dir = TempDir::new("paste-text");
        write(&dir.join("a b.pdf"), b"a");
        write(&dir.join("c.pdf"), b"c");
        let a = dir.join("a b.pdf");
        let c = dir.join("c.pdf");
        let uri = format!("file://{}", a.display().to_string().replace(' ', "%20"));
        let text = format!("# copied\n{uri}\n{}\n\n", c.display());
        assert_eq!(paths_from_text(&text), vec![a.clone(), c.clone()]);
        assert_eq!(
            paths_from_text(&format!("file://localhost{}", c.display())),
            vec![c.clone()]
        );
        assert!(paths_from_text("hello world").is_empty());
        assert!(paths_from_text(&format!("{}\nnot a path", c.display())).is_empty());
        assert!(paths_from_text(&dir.join("gone.pdf").display().to_string()).is_empty());
        assert!(paths_from_text("").is_empty());
    }

    #[test]
    fn names_typed_in_the_explorer_become_paths_next_to_the_old_ones() {
        let file = Path::new("specs/tcp.pdf");
        assert_eq!(rename_start_text(file, false), "tcp");
        assert_eq!(
            rename_target(file, " udp ", false),
            Ok(PathBuf::from("specs/udp.pdf"))
        );
        assert_eq!(
            rename_target(file, "udp.PDF", false),
            Ok(PathBuf::from("specs/udp.PDF"))
        );
        assert_eq!(rename_start_text(Path::new("specs"), true), "specs");
        assert_eq!(
            rename_target(Path::new("specs"), "rfc", true),
            Ok(PathBuf::from("rfc"))
        );
        for bad in ["", "  ", "a/b", ".hidden"] {
            assert!(rename_target(file, bad, false).is_err(), "{bad:?}");
            assert!(new_folder_target(Path::new(""), bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            new_folder_target(Path::new("specs"), "drafts"),
            Ok(PathBuf::from("specs/drafts"))
        );
    }

    #[test]
    fn project_only_menu_entries_are_listed_only_for_a_project() {
        let names = |on, project| -> Vec<&'static str> {
            let m = crate::panels::explorer::MenuContext {
                on,
                idx: None,
                path: PathBuf::new(),
                rel: PathBuf::new(),
                expanded: false,
                filtering: false,
                pick: None,
                current: None,
                window: crate::state::WindowId::MAIN,
            };
            menu_entries_for(on, project)
                .map(|e| (e.name)(&m))
                .collect()
        };
        for on in [MenuOn::File, MenuOn::Folder, MenuOn::Empty] {
            let m_names: Vec<&str> = {
                let m = crate::panels::explorer::MenuContext {
                    on,
                    idx: None,
                    path: PathBuf::new(),
                    rel: PathBuf::new(),
                    expanded: false,
                    filtering: false,
                    pick: None,
                    current: None,
                    window: crate::state::WindowId::MAIN,
                };
                menu_entries(on).map(|e| (e.name)(&m)).collect()
            };
            let plain = names(on, false);
            assert_eq!(plain, m_names);
            let project_only = [
                "explorer.menu.import_here",
                "explorer.menu.import_folder",
                "explorer.menu.new_folder",
                "explorer.menu.rename",
                "explorer.menu.trash",
            ];
            assert!(plain.iter().all(|n| !project_only.contains(n)), "{on:?}");
            let with = names(on, true);
            assert!(with.len() > plain.len());
            assert_eq!(with[..plain.len()], plain[..]);
            for n in &with {
                for lang in [crate::i18n::Lang::Ko, crate::i18n::Lang::En] {
                    assert!(crate::i18n::lookup(lang, n).is_some(), "{n}");
                }
            }
        }
        assert!(names(MenuOn::Folder, true).contains(&"explorer.menu.import_here"));
        assert!(names(MenuOn::File, true).contains(&"explorer.menu.trash"));
        assert!(names(MenuOn::Empty, true).contains(&"explorer.menu.import_folder"));
    }

    // --- opening projects -------------------------------------------------------------------

    #[test]
    fn a_project_folder_opens_as_a_project_and_a_plain_folder_as_before() {
        let mut env = Env::new("open");
        let root = env.open_project("proj", "Network Specs");
        let ws = env.state.workspace.as_ref().expect("workspace");
        assert_eq!(ws.root, root);
        assert_eq!(
            ws.project.as_ref().map(|p| p.name.as_str()),
            Some("Network Specs")
        );
        let view = WorkspaceView::of(ws);
        assert_eq!(
            header_title(Some(&view)),
            ("Network Specs".to_owned(), Some(root.display().to_string()))
        );
        assert_eq!(
            crate::windows::window_title(&env.state, 0),
            tr("window.title").replace("{title}", "Network Specs")
        );

        let plain = env.dir.join("plain");
        write(&plain.join("a.pdf"), &pdf("a"));
        env.run(Action::OpenFolder(plain.clone()));
        let ws = env.state.workspace.as_ref().expect("workspace");
        assert!(ws.project.is_none());
        assert_eq!(header_title(Some(&WorkspaceView::of(ws))).0, "plain");
        assert_eq!(crate::windows::window_title(&env.state, 0), "refdesk");
    }

    #[test]
    fn the_project_comes_back_with_the_session_and_from_the_command_line() {
        let mut env = Env::new("session");
        let root = env.open_project("proj", "Kept");
        let session = crate::session::to_session(&env.state);
        let mut restored = AppState::new();
        crate::session::restore(&mut restored, &session, &env.ctx);
        let ws = restored.workspace.as_ref().expect("restored");
        assert_eq!(ws.root, root);
        assert_eq!(ws.project.as_ref().map(|p| p.name.as_str()), Some("Kept"));

        // `refdesk FOLDER` asks for the folder as an open target; the folder opens as the
        // workspace, so a project folder opens as the project.
        let cli = crate::cli::parse([std::ffi::OsString::from(root.clone())]);
        let crate::cli::Cli::Run {
            request: refdesk_core::IpcRequest::Open { targets, .. },
            ..
        } = cli
        else {
            panic!("not an open: {cli:?}");
        };
        assert_eq!(targets.len(), 1);
        let mut fresh = AppState::new();
        app_dispatch(
            &mut fresh,
            Action::OpenFolder(targets[0].path.clone()),
            &env.ctx,
        );
        assert_eq!(
            fresh
                .workspace
                .as_ref()
                .and_then(|w| w.project.as_ref())
                .map(|p| p.name.clone()),
            Some("Kept".into())
        );
    }

    #[test]
    fn the_new_project_dialog_makes_the_project_and_opens_it() {
        let mut env = Env::new("create");
        let other = env.dir.join("other");
        write(&other.join("x.pdf"), &pdf("x"));
        env.run(Action::OpenFolder(other.clone()));
        env.project(ProjectAction::NewDialog);
        let dialog = env.state.project.new_dialog.as_mut().expect("dialog");
        assert_eq!(dialog.base, env.dir.join("app/projects"));
        dialog.name = "My Specs".into();
        dialog.name_changed();
        let root = dialog.root();
        assert_eq!(root, env.dir.join("app/projects/My Specs"));
        dialog.check(&env.ctx);
        env.settle("the folder check", |s| {
            s.project
                .new_dialog
                .as_ref()
                .is_some_and(|d| d.folder_state().is_some())
        });
        let dialog = env.state.project.new_dialog.as_ref().expect("dialog");
        assert_eq!(dialog.folder_state(), Some(FolderState::Missing));
        assert_eq!(dialog.view().button, CreateButton::Create);
        env.project(ProjectAction::Create {
            root: root.clone(),
            name: "My Specs".into(),
        });
        assert!(env.state.project.new_dialog.is_none());
        assert!(core_project::is_project(&root));
        let ws = env.state.workspace.as_ref().expect("workspace");
        assert_eq!(ws.root, root);
        assert_eq!(
            ws.project.as_ref().map(|p| p.name.as_str()),
            Some("My Specs")
        );

        // Making it again fails inside the dialog.
        env.project(ProjectAction::NewDialog);
        env.project(ProjectAction::Create {
            root,
            name: "Again".into(),
        });
        assert!(
            env.state
                .project
                .new_dialog
                .as_ref()
                .and_then(|d| d.failure.as_ref())
                .is_some()
        );
    }

    #[test]
    fn opening_a_folder_that_is_not_a_project_asks_first() {
        let mut env = Env::new("prompt");
        let folder = env.dir.join("loose");
        write(&folder.join("a.pdf"), &pdf("a"));
        *env.chooser.folder.lock().expect("lock") = Some(folder.clone());
        env.project(ProjectAction::OpenDialog);
        env.settle("the question", |s| s.project.prompt.is_some());
        assert_eq!(env.state.project.prompt.as_deref(), Some(folder.as_path()));
        assert!(env.state.workspace.is_none());
        env.project(ProjectAction::Create {
            root: folder.clone(),
            name: "loose".into(),
        });
        assert!(env.state.project.prompt.is_none());
        assert!(has_project(&env.state));
        assert!(folder.join("a.pdf").is_file());
    }

    #[test]
    fn the_open_folder_becomes_a_project_in_place_and_is_remembered_as_one() {
        let mut env = Env::new("in-place");
        let folder = env.dir.join("library");
        write(&folder.join("a.pdf"), &pdf("a"));
        env.run(Action::OpenFolder(folder.clone()));
        assert!(!has_project(&env.state));
        env.project(ProjectAction::Create {
            root: folder.clone(),
            name: "Library".into(),
        });
        assert_eq!(
            open_project(&env.state).map(|(r, p)| (r.to_path_buf(), p.name.clone())),
            Some((folder.clone(), "Library".to_owned()))
        );
        let recent = env.state.doc_db.recent_folders(1);
        assert_eq!(recent[0].path, folder);
        assert_eq!(recent[0].project.as_deref(), Some("Library"));
    }

    // --- importing ----------------------------------------------------------------------------

    #[test]
    fn importing_one_file_copies_it_shows_it_and_opens_a_preview_tab() {
        let mut env = Env::new("import-one");
        let root = env.open_project("proj", "P");
        let src = env.dir.join("src/one.pdf");
        write(&src, &pdf("one"));
        env.run(Action::ImportIntoProject {
            sources: vec![src.clone()],
            into: PathBuf::new(),
        });
        assert!(env.state.project.import.is_some());
        env.import_done();
        assert!(root.join("one.pdf").is_file());
        env.has_file("one.pdf");
        assert_eq!(
            pdf_tabs(&env.state),
            vec![(pdf_tabs(&env.state)[0].0, root.join("one.pdf"), true)]
        );
        let summary = env.state.project.summary.clone().expect("summary");
        assert_eq!(summary.text, tr("project.import.done").replace("{n}", "1"));
        assert!(!summary.has_details);
        assert!(src.is_file(), "the original stays");
    }

    #[test]
    fn importing_several_files_chooses_the_first_in_the_explorer_without_opening_it() {
        let mut env = Env::new("import-many");
        let root = env.open_project("proj", "P");
        let a = env.dir.join("src/a.pdf");
        let b = env.dir.join("src/b.pdf");
        write(&a, &pdf("a"));
        write(&b, &pdf("b"));
        write(&env.dir.join("src/n.txt"), b"n");
        *env.chooser.pdfs.lock().expect("lock") = vec![a, b, env.dir.join("src/n.txt")];
        env.project(ProjectAction::ImportDialog {
            into: PathBuf::from("refs"),
        });
        env.settle("the import to start", |s| s.project.import.is_some());
        env.import_done();
        env.has_file("refs/b.pdf");
        assert!(root.join("refs/a.pdf").is_file());
        let index = index_of(&env.state).expect("index");
        let first = index.find(Path::new("refs/a.pdf")).expect("a");
        env.settle("the selection", |s| {
            s.windows[0].explorer.cursor() == Some(first)
        });
        assert!(pdf_tabs(&env.state).is_empty());
        let summary = env.state.project.summary.clone().expect("summary");
        assert!(summary.has_details);
        assert_eq!(summary.details.len(), 1);
        assert_eq!(
            env.state.windows[0].explorer.cursor_folder(),
            Some(PathBuf::from("refs"))
        );
        assert_eq!(import_target(&env.state), PathBuf::from("refs"));
    }

    fn clash(env: &mut Env, root: &Path) -> Vec<PathBuf> {
        write(&root.join("doc.pdf"), &pdf("old"));
        ["x", "y"]
            .iter()
            .map(|d| {
                let p = env.dir.join(&format!("{d}/doc.pdf"));
                write(&p, &pdf(d));
                p
            })
            .collect()
    }

    #[test]
    fn a_clash_waits_for_the_dialog_and_apply_to_all_settles_the_rest() {
        for (choice, all, expected) in [
            (
                ConflictChoice::KeepBoth,
                true,
                vec!["doc (2).pdf", "doc (3).pdf"],
            ),
            (ConflictChoice::Skip, true, vec![]),
            (ConflictChoice::Replace, true, vec!["doc.pdf", "doc.pdf"]),
            (
                ConflictChoice::KeepBoth,
                false,
                vec!["doc (2).pdf", "doc (3).pdf"],
            ),
        ] {
            let mut env = Env::new("clash");
            let root = env.open_project("proj", "P");
            let sources = clash(&mut env, &root);
            env.run(Action::ImportIntoProject {
                sources,
                into: PathBuf::new(),
            });
            env.settle("the clash", waiting_clash);
            env.project(ProjectAction::Resolve { choice, all });
            if !all {
                env.settle("the second clash", |s| {
                    waiting_clash(s) && snapshot(s).is_some_and(|x| x.done == 1)
                });
                env.project(ProjectAction::Resolve { choice, all: false });
            }
            env.import_done();
            let copied: Vec<String> = std::fs::read_dir(&root)
                .expect("dir")
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("doc ("))
                .collect();
            let mut copied = copied;
            copied.sort();
            let want: Vec<String> = expected
                .iter()
                .filter(|n| n.starts_with("doc ("))
                .map(|s| s.to_string())
                .collect();
            assert_eq!(copied, want, "{choice:?} {all}");
            if choice == ConflictChoice::Replace {
                assert_eq!(core_project::list_trash(&root).expect("trash").len(), 2);
            }
        }
    }

    #[test]
    fn a_cancelled_import_says_so_and_imports_wait_in_a_queue() {
        let mut env = Env::new("cancel");
        let root = env.open_project("proj", "P");
        let sources = clash(&mut env, &root);
        let later = env.dir.join("later.pdf");
        write(&later, &pdf("later"));
        env.run(Action::ImportIntoProject {
            sources,
            into: PathBuf::new(),
        });
        env.settle("the clash", waiting_clash);
        env.run(Action::ImportIntoProject {
            sources: vec![later],
            into: PathBuf::new(),
        });
        assert_eq!(env.state.project.queue.len(), 1);
        let snap = snapshot(&env.state).expect("running");
        assert!(
            progress_text(&snap, 1).ends_with(&tr("project.import.queued").replace("{n}", "1"))
        );
        env.project(ProjectAction::Cancel);
        // The cancelled one ends, then the waiting one runs.
        env.import_done();
        assert!(root.join("later.pdf").is_file());
        assert!(!root.join("doc (2).pdf").exists());
        assert_eq!(
            env.state.project.summary.as_ref().map(|s| s.text.clone()),
            Some(tr("project.import.done").replace("{n}", "1"))
        );
    }

    #[test]
    fn without_a_project_importing_opens_the_files_instead() {
        let mut env = Env::new("no-project");
        let a = env.dir.join("a.pdf");
        write(&a, &pdf("a"));
        env.run(Action::ImportIntoProject {
            sources: vec![a.clone()],
            into: PathBuf::new(),
        });
        assert!(env.state.project.import.is_none());
        assert_eq!(pdf_tabs(&env.state).len(), 1);
        assert_eq!(pdf_tabs(&env.state)[0].1, a);
    }

    // --- pasting ------------------------------------------------------------------------------

    #[test]
    fn pasting_files_imports_them_into_the_chosen_folder() {
        let mut env = Env::new("paste");
        let root = env.open_project("proj", "P");
        write(&root.join("refs/old.pdf"), &pdf("old"));
        // The watcher may not be set up yet; scan by hand.
        env.run(Action::RefreshWorkspace);
        env.has_file("refs/old.pdf");
        let index = index_of(&env.state).expect("index");
        let old = index.find(Path::new("refs/old.pdf")).expect("old");
        env.state.windows[0]
            .explorer
            .select_path(&index, Path::new("refs/old.pdf"));
        assert_eq!(env.state.windows[0].explorer.cursor(), Some(old));
        let a = env.dir.join("a.pdf");
        write(&a, &pdf("a"));
        *env.clipboard.0.lock().expect("lock") = vec![a];
        env.project(ProjectAction::Paste { text: None });
        env.settle("the paste", |s| {
            s.project.import.is_some() || s.project.summary.is_some()
        });
        env.import_done();
        assert!(root.join("refs/a.pdf").is_file());
    }

    #[test]
    fn pasting_nothing_does_nothing_and_text_paths_are_used_without_a_file_list() {
        let mut env = Env::new("paste-empty");
        let root = env.open_project("proj", "P");
        env.project(ProjectAction::Paste {
            text: Some("just some words".into()),
        });
        // The clipboard thread ends without an action; nothing starts.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while Arc::strong_count(&env.state.project.inbox) > 1 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        tick(&mut env.state, &env.ctx);
        assert!(env.state.actions.is_empty());
        assert!(env.state.project.import.is_none());

        let b = env.dir.join("b.pdf");
        write(&b, &pdf("b"));
        env.project(ProjectAction::Paste {
            text: Some(format!("{}\n", b.display())),
        });
        env.settle("the paste", |s| {
            s.project.import.is_some() || s.project.summary.is_some()
        });
        env.import_done();
        assert!(root.join("b.pdf").is_file());
    }

    #[test]
    fn pasting_without_a_project_opens_tabs_or_the_folder() {
        let mut env = Env::new("paste-plain");
        let a = env.dir.join("a.pdf");
        write(&a, &pdf("a"));
        *env.clipboard.0.lock().expect("lock") = vec![a.clone()];
        env.project(ProjectAction::Paste { text: None });
        env.settle("the tab", |s| !pdf_tabs(s).is_empty());
        assert_eq!(pdf_tabs(&env.state)[0].1, a);

        let folder = env.dir.join("f");
        write(&folder.join("c.pdf"), &pdf("c"));
        env.project(ProjectAction::Pasted {
            files: vec![folder.clone()],
            into: PathBuf::new(),
        });
        assert_eq!(
            env.state.workspace.as_ref().map(|w| w.root.clone()),
            Some(folder)
        );
    }

    #[test]
    fn ctrl_v_outside_a_text_field_becomes_a_paste_and_inside_one_types_text() {
        let ctx = egui::Context::default();
        let state = AppState::new();
        let field = egui::Id::unique("test-field");
        let mut text = String::new();
        let mut taken = Vec::new();
        let mut frame = |events: Vec<egui::Event>, focus: bool, text: &mut String| {
            let input = egui::RawInput {
                events,
                ..Default::default()
            };
            ctx.run_ui(input, |ui| {
                taken.push(take_paste(ui.ctx(), &state));
                let r = ui.add(egui::TextEdit::singleline(text).id(field));
                if focus {
                    r.request_focus();
                } else {
                    r.surrender_focus();
                }
            })
            .drop_without_applying_deltas();
        };
        frame(Vec::new(), true, &mut text);
        frame(vec![egui::Event::Paste("typed".into())], true, &mut text);
        assert_eq!(text, "typed");
        frame(Vec::new(), false, &mut text);
        frame(vec![egui::Event::Paste("/x.pdf".into())], false, &mut text);
        assert_eq!(text, "typed");
        assert_eq!(
            taken,
            vec![
                None,
                None,
                None,
                Some(Action::Project(ProjectAction::Paste {
                    text: Some("/x.pdf".into())
                }))
            ]
        );
    }

    #[test]
    fn a_pasted_folder_opens_the_picker_and_only_the_checked_files_are_imported() {
        let mut env = Env::new("picker");
        let root = env.open_project("proj", "P");
        write(&root.join("have.pdf"), &pdf("same"));
        let folder = env.dir.join("Manuals");
        write(&folder.join("a.pdf"), &pdf("same"));
        write(&folder.join("sub/b.pdf"), &pdf("b"));
        write(&folder.join("sub/c.pdf"), &pdf("c"));
        write(&folder.join("x.txt"), b"x");
        let loose = env.dir.join("loose.pdf");
        write(&loose, &pdf("loose"));
        *env.clipboard.0.lock().expect("lock") = vec![folder.clone(), loose];
        env.project(ProjectAction::Paste { text: None });
        env.settle("the picker's list", |s| {
            s.project
                .picker
                .as_ref()
                .is_some_and(|p| p.list.is_some() && !p.checking)
        });
        // The loose file was imported at once.
        env.import_done();
        assert!(root.join("loose.pdf").is_file());

        let picker = env.state.project.picker.as_mut().expect("picker");
        let list = picker.list.as_mut().expect("list");
        let labels: Vec<(String, bool, bool)> = list
            .rows
            .iter()
            .map(|r| (r.label.clone(), r.checked, r.existing.is_some()))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("sub/b.pdf".into(), true, false),
                ("sub/c.pdf".into(), true, false),
                ("a.pdf".into(), false, true),
            ]
        );
        list.set_shown("c.pdf", false);
        let items = list.items(&picker.folder, &picker.into, picker.keep_structure);
        env.project(ProjectAction::ImportItems(items));
        env.project(ProjectAction::ClosePicker);
        env.import_done();
        assert!(root.join("Manuals/sub/b.pdf").is_file());
        assert!(!root.join("Manuals/sub/c.pdf").exists());
        assert!(!root.join("Manuals/a.pdf").exists());
        assert!(env.state.project.picker.is_none());
    }

    #[test]
    fn import_from_folder_asks_for_the_folder_then_shows_the_picker() {
        let mut env = Env::new("from-folder");
        env.open_project("proj", "P");
        let folder = env.dir.join("in");
        write(&folder.join("a.pdf"), &pdf("a"));
        *env.chooser.folder.lock().expect("lock") = Some(folder.clone());
        let action = crate::commands::command("file.import_folder")
            .expect("command")
            .run(&env.state)
            .expect("enabled");
        env.run(action);
        env.settle("the picker", |s| {
            s.project.picker.as_ref().is_some_and(|p| p.list.is_some())
        });
        assert_eq!(
            env.state.project.picker.as_ref().map(|p| p.folder.clone()),
            Some(folder)
        );
    }

    // --- file management ------------------------------------------------------------------------

    #[test]
    fn folders_are_made_and_a_renamed_open_document_keeps_its_tab() {
        let mut env = Env::new("rename");
        let root = env.open_project("proj", "P");
        write(&root.join("a.pdf"), &pdf("a"));
        env.project(ProjectAction::NewFolder {
            rel: PathBuf::from("drafts"),
        });
        assert!(root.join("drafts").is_dir());

        env.run(Action::OpenFiles(vec![OpenTarget {
            path: root.join("a.pdf"),
            page: None,
        }]));
        let (tab, _, _) = pdf_tabs(&env.state)[0].clone();
        if let Some(Tab::Pdf(p)) = env.state.tabs.get_mut(tab) {
            p.view.anchor = refdesk_core::Location::new(0, 0.0, 120.0);
        }
        env.project(ProjectAction::Rename {
            from: PathBuf::from("a.pdf"),
            to: PathBuf::from("drafts/b.pdf"),
        });
        assert!(root.join("drafts/b.pdf").is_file());
        let tabs = pdf_tabs(&env.state);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].0, tab);
        assert_eq!(tabs[0].1, root.join("drafts/b.pdf"));
        assert_eq!(
            env.state.tabs.pdf(tab).map(|p| p.view.anchor),
            Some(refdesk_core::Location::new(0, 0.0, 120.0))
        );
        assert!(env.state.docs.find(&root.join("a.pdf")).is_none());

        // A name that is taken is refused and nothing moves.
        write(&root.join("c.pdf"), &pdf("c"));
        env.project(ProjectAction::Rename {
            from: PathBuf::from("c.pdf"),
            to: PathBuf::from("drafts/b.pdf"),
        });
        assert!(root.join("c.pdf").is_file());
    }

    #[test]
    fn removing_from_the_project_closes_the_tab_and_the_trash_gives_it_back() {
        let mut env = Env::new("trash");
        let root = env.open_project("proj", "P");
        write(&root.join("x.pdf"), &pdf("x"));
        env.run(Action::OpenFiles(vec![OpenTarget {
            path: root.join("x.pdf"),
            page: None,
        }]));
        assert_eq!(pdf_tabs(&env.state).len(), 1);
        env.project(ProjectAction::AskTrash {
            rel: PathBuf::from("x.pdf"),
        });
        assert_eq!(
            env.state.project.confirm_trash,
            Some(PathBuf::from("x.pdf"))
        );
        env.project(ProjectAction::Trash {
            rel: PathBuf::from("x.pdf"),
        });
        assert!(env.state.project.confirm_trash.is_none());
        assert!(pdf_tabs(&env.state).is_empty());
        assert!(!root.join("x.pdf").exists());
        env.project(ProjectAction::ShowTrash);
        let entries = env.state.project.trash.clone().expect("trash list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].rel_path, PathBuf::from("x.pdf"));
        env.project(ProjectAction::Restore(entries[0].id.clone()));
        assert!(root.join("x.pdf").is_file());
        assert_eq!(env.state.project.trash, Some(Vec::new()));
        env.has_file("x.pdf");
    }

    // --- drawing and the accessibility tree ----------------------------------------------------

    fn button_labels(env: &mut Env) -> Vec<String> {
        env.ctx.enable_accesskit();
        let mut queue = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 700.0),
            )),
            ..Default::default()
        };
        let output = env.ctx.run_ui(input, |ui| {
            crate::windows::show_window(ui, &mut env.state, 0, &mut queue)
        });
        let update = output.platform_output.accesskit_update.clone();
        output.drop_without_applying_deltas();
        update
            .map(|u| u.nodes)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, n)| {
                matches!(
                    n.role(),
                    egui::accesskit::Role::Button | egui::accesskit::Role::CheckBox
                )
            })
            .filter_map(|(_, n)| n.label().map(str::to_owned))
            .collect()
    }

    #[test]
    fn an_empty_project_shows_how_to_import_and_the_header_has_the_import_button() {
        let mut env = Env::new("a11y");
        env.open_project("proj", "Empty One");
        let labels = button_labels(&mut env);
        assert!(
            labels.iter().any(|l| l == tr("explorer.import_first")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == tr("explorer.import_button")),
            "{labels:?}"
        );

        // A plain folder has neither.
        let plain = env.dir.join("plain");
        write(&plain.join("a.pdf"), &pdf("a"));
        env.run(Action::OpenFolder(plain));
        env.settle("the scan", |s| index_of(s).is_some());
        let labels = button_labels(&mut env);
        assert!(
            !labels.iter().any(|l| l == tr("explorer.import_button")),
            "{labels:?}"
        );
    }

    #[test]
    fn every_dialog_draws_and_names_its_buttons() {
        let mut env = Env::new("dialogs");
        let root = env.open_project("proj", "P");
        let sources = clash(&mut env, &root);
        env.run(Action::ImportIntoProject {
            sources,
            into: PathBuf::new(),
        });
        env.settle("the clash", waiting_clash);
        let labels = button_labels(&mut env);
        for key in [
            "project.conflict.replace",
            "project.conflict.keep_both",
            "project.conflict.skip",
            "project.import.cancel",
        ] {
            assert!(labels.iter().any(|l| l == tr(key)), "{key}: {labels:?}");
        }
        env.project(ProjectAction::Resolve {
            choice: ConflictChoice::Skip,
            all: true,
        });
        env.import_done();

        let folder = env.dir.join("pick");
        write(&folder.join("a.pdf"), &pdf("a"));
        env.project(ProjectAction::OpenPicker {
            folder,
            into: PathBuf::new(),
        });
        env.settle("the picker", |s| {
            s.project
                .picker
                .as_ref()
                .is_some_and(|p| p.list.is_some() && !p.checking)
        });
        env.project(ProjectAction::NewDialog);
        env.project(ProjectAction::AskTrash {
            rel: PathBuf::from("doc.pdf"),
        });
        env.project(ProjectAction::ShowTrash);
        env.state.project.prompt = Some(root.clone());
        // A new egui window is measured, unseen, in its first frame.
        button_labels(&mut env);
        let labels = button_labels(&mut env);
        let row = tr("project.pick.row")
            .replace("{path}", "a.pdf")
            .replace(
                "{size}",
                &human_size(
                    std::fs::metadata(env.dir.join("pick/a.pdf"))
                        .expect("meta")
                        .len(),
                ),
            )
            .replace("{check}", tr("project.pick.checked"));
        for want in [
            row,
            tr("project.pick.import").replace("{n}", "1"),
            tr("project.new.create").to_owned(),
            tr("project.new.browse").to_owned(),
            tr("project.trash.move").to_owned(),
            tr("project.trash.empty_button").to_owned(),
            tr("project.prompt.make").to_owned(),
            tr("project.prompt.folder").to_owned(),
        ] {
            assert!(labels.contains(&want), "{want}: {labels:?}");
        }
    }

    // --- the start page and the commands --------------------------------------------------------

    #[test]
    fn the_start_page_lists_recent_projects_and_marks_the_gone_ones() {
        use egui_kittest::kittest::Queryable;
        let mut env = Env::new("start");
        let kept = env.open_project("kept", "Kept Project");
        let gone = env.open_project("gone", "Gone Project");
        let plain = env.dir.join("plain");
        write(&plain.join("a.pdf"), &pdf("a"));
        std::fs::remove_file(gone.join(core_project::PROJECT_FILE)).expect("remove");
        env.run(Action::OpenFolder(plain.clone()));
        let page = crate::session::start_page(&mut env.state, &env.ctx);
        assert_eq!(
            page.recent_projects
                .iter()
                .map(|p| (p.name.clone(), p.path.clone(), p.exists))
                .collect::<Vec<_>>(),
            vec![
                ("Gone Project".to_owned(), gone, false),
                ("Kept Project".to_owned(), kept, true)
            ]
        );
        assert_eq!(page.recent_folders, vec![plain]);

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, queue: &mut Vec<Action>| crate::views::start_page::show(ui, &page, queue),
            Vec::new(),
        );
        harness.run();
        harness.get_by_label("Kept Project").click();
        harness.run();
        harness.get_by_label(tr("start.new_project_button")).click();
        harness.run();
        assert_eq!(
            harness.state(),
            &vec![
                Action::Project(ProjectAction::Open(env.dir.join("kept"))),
                Action::Project(ProjectAction::NewDialog)
            ]
        );
    }

    #[test]
    fn the_project_commands_are_in_the_file_menu_and_import_needs_a_project() {
        use crate::commands::{MenuGroup, command, in_menu};
        let file: Vec<&str> = in_menu(MenuGroup::File).map(|c| c.id).collect();
        let at = file
            .iter()
            .position(|id| *id == "file.close_folder")
            .expect("close");
        assert_eq!(
            file[at + 1..at + 7],
            [
                "file.new_project",
                "file.open_project",
                "file.import",
                "file.import_folder",
                "file.import_paste",
                "file.project_trash"
            ]
        );
        let import = command("file.import").expect("import");
        assert_eq!(
            import.shortcut,
            Some(egui::KeyboardShortcut::new(
                egui::Modifiers::CTRL.plus(egui::Modifiers::SHIFT),
                egui::Key::I
            ))
        );
        let mut env = Env::new("commands");
        assert!(!(import.enabled)(&env.state));
        assert!(!(command("file.project_trash").expect("trash").enabled)(
            &env.state
        ));
        assert!((command("file.new_project").expect("new").enabled)(
            &env.state
        ));
        env.open_project("proj", "P");
        assert!((import.enabled)(&env.state));
        assert_eq!(
            import.run(&env.state),
            Some(Action::Project(ProjectAction::ImportDialog {
                into: PathBuf::new()
            }))
        );
    }
}
