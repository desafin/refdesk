//! Projects: a folder that refdesk manages (`docs/SPEC.md` §7.15, `docs/ARCHITECTURE.md` §4.15).
//!
//! A project is a folder with a project file in it. PDFs are copied into the folder when they
//! are imported; what refdesk keeps about the project (translations made ahead, the project's
//! trash) lives in a hidden data folder inside it, which the workspace scan skips.
//!
//! What is here:
//!
//! * the project file ([`Project`]): its name and when it was made, written so that entries
//!   other parts add to it survive;
//! * file operations for the explorer ([`create_folder`], [`rename`]) that never leave the
//!   project;
//! * the project's trash ([`trash`], [`restore`], [`list_trash`], [`empty_trash`]);
//! * importing ([`ImportJob`]): copying PDFs into the project on a thread of its own, skipping
//!   what is already there and asking about name clashes;
//! * a look at a folder before importing from it ([`SourceSurvey`]).
//!
//! Everything that reads or writes the disk beyond the small project file runs on a background
//! thread (R-1); the app reads snapshots.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::engine::Notifier;
use crate::fingerprint::fingerprint_file;
use crate::store::atomic_write;
use crate::workspace::{ScanLimits, scan};

/// Name of the project file in the project's folder.
pub const PROJECT_FILE: &str = "refdesk-project.toml";

/// Name of the hidden folder inside a project where refdesk keeps its own data.
pub const PROJECT_DATA_DIR: &str = ".refdesk";

/// Whether `root` is the folder of a project (it has a project file).
pub fn is_project(root: &Path) -> bool {
    root.join(PROJECT_FILE).is_file()
}

/// The hidden data folder of the project at `root`. It may not exist yet.
pub fn data_dir(root: &Path) -> PathBuf {
    root.join(PROJECT_DATA_DIR)
}

/// The format version written to the project file.
pub const PROJECT_FORMAT_VERSION: i64 = 1;

/// The folder below [`data_dir`] that holds the project's trash.
pub const TRASH_DIR: &str = "trash";

/// What is appended to the hidden name a file is copied to while it is imported.
pub const IMPORTING_SUFFIX: &str = ".importing";

/// The longest folder name [`folder_name`] makes, in characters.
pub const MAX_FOLDER_NAME_CHARS: usize = 80;

/// The longest folder name [`folder_name`] makes, in bytes (file systems allow 255).
const MAX_FOLDER_NAME_BYTES: usize = 200;

/// Size of the copy buffer; progress is reported after every buffer.
const COPY_BUFFER: usize = 1 << 20;

/// The name of the file that describes one item of the trash, inside its folder.
const TRASH_MANIFEST: &str = ".refdesk-trash.json";

/// What can go wrong with a project or a file in it.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// The name is empty once the spaces around it are taken off.
    #[error("the project name is empty")]
    EmptyName,
    /// The folder already has a project file.
    #[error("{} is already a project", .0.display())]
    AlreadyProject(PathBuf),
    /// The folder has no project file.
    #[error("{} is not a project", .0.display())]
    NotProject(PathBuf),
    /// The path would lead out of the project (`..`, an absolute path).
    #[error("{} is not inside the project", .0.display())]
    Outside(PathBuf),
    /// The path names the project itself or refdesk's data folder.
    #[error("{} cannot be changed", .0.display())]
    Protected(PathBuf),
    /// Something is already there.
    #[error("{} already exists", .0.display())]
    Exists(PathBuf),
    /// Nothing is there.
    #[error("{} does not exist", .0.display())]
    Missing(PathBuf),
    /// Reading or writing failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file or folder.
        path: PathBuf,
        /// What failed.
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> ProjectError + '_ {
    move |source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Unix seconds now.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------------------------------------
// Names

/// A project name as it is kept: the spaces around it and control characters taken out.
///
/// # Errors
///
/// [`ProjectError::EmptyName`] when nothing is left.
pub fn clean_name(name: &str) -> Result<String, ProjectError> {
    let cleaned: String = name.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        Err(ProjectError::EmptyName)
    } else {
        Ok(cleaned.to_owned())
    }
}

/// A folder name made from a project name: `/` and control characters taken out, the spaces
/// around it and leading dots (a hidden name) taken off, and cut to [`MAX_FOLDER_NAME_CHARS`]
/// characters (and 200 bytes). Empty when nothing usable is left (`"..."`, `"//"`).
pub fn folder_name(name: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|c| *c != '/' && !c.is_control())
        .collect();
    let trimmed = kept.trim().trim_start_matches('.').trim_start();
    let mut out = String::new();
    for (count, c) in trimmed.chars().enumerate() {
        if count >= MAX_FOLDER_NAME_CHARS || out.len() + c.len_utf8() > MAX_FOLDER_NAME_BYTES {
            break;
        }
        out.push(c);
    }
    out.trim_end().to_owned()
}

/// Whether a file name ends in `.pdf` (any case).
pub fn is_pdf_name(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

fn is_hidden(name: &OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

// ---------------------------------------------------------------------------------------------
// Paths inside the project

/// `rel` as a clean path below the project root: `.` dropped, empty for the root.
///
/// # Errors
///
/// [`ProjectError::Outside`] for an absolute path or one with `..`;
/// [`ProjectError::Protected`] for a path into refdesk's data folder.
pub fn clean_relative(rel: &Path) -> Result<PathBuf, ProjectError> {
    let mut out = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ProjectError::Outside(rel.to_path_buf()));
            }
        }
    }
    if out.components().next() == Some(Component::Normal(OsStr::new(PROJECT_DATA_DIR))) {
        return Err(ProjectError::Protected(rel.to_path_buf()));
    }
    Ok(out)
}

/// Like [`clean_relative`], but the root itself is refused too.
fn clean_entry(rel: &Path) -> Result<PathBuf, ProjectError> {
    let out = clean_relative(rel)?;
    if out.as_os_str().is_empty() {
        return Err(ProjectError::Protected(rel.to_path_buf()));
    }
    Ok(out)
}

fn exists_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Creates the folder `rel` (and the folders above it) in the project at `root`. Returns its
/// path.
///
/// # Errors
///
/// The path is not inside the project, something is already there, or the disk refused.
pub fn create_folder(root: &Path, rel: &Path) -> Result<PathBuf, ProjectError> {
    let rel = clean_entry(rel)?;
    let path = root.join(&rel);
    if exists_no_follow(&path) {
        return Err(ProjectError::Exists(rel));
    }
    fs::create_dir_all(&path).map_err(io_error(&path))?;
    Ok(path)
}

/// Renames (or moves) `from` to `to`, both below the project at `root`. Returns the new path.
///
/// # Errors
///
/// Either path is not inside the project, `from` is missing, `to` already exists, a folder
/// would move into itself, or the disk refused.
pub fn rename(root: &Path, from: &Path, to: &Path) -> Result<PathBuf, ProjectError> {
    let from = clean_entry(from)?;
    let to = clean_entry(to)?;
    let (src, dst) = (root.join(&from), root.join(&to));
    if !exists_no_follow(&src) {
        return Err(ProjectError::Missing(from));
    }
    if from == to {
        return Ok(dst);
    }
    if to.starts_with(&from) {
        return Err(ProjectError::Protected(to));
    }
    if exists_no_follow(&dst) {
        return Err(ProjectError::Exists(to));
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    fs::rename(&src, &dst).map_err(io_error(&src))?;
    Ok(dst)
}

/// The first of `name (2).ext`, `name (3).ext`, … next to `rel` for which `taken` is false.
pub fn keep_both_name(rel: &Path, taken: impl Fn(&Path) -> bool) -> PathBuf {
    let stem = rel
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = rel.extension().map(|e| e.to_string_lossy().into_owned());
    let parent = rel.parent().unwrap_or(Path::new(""));
    let mut n: u64 = 2;
    loop {
        let name = match &ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = parent.join(name);
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

// ---------------------------------------------------------------------------------------------
// The project file

/// An open project: its folder and what its project file says.
///
/// The file is read as a TOML table and only the entries this type knows are changed when it is
/// written again, so tables that other features add survive a rename.
#[derive(Clone, Debug)]
pub struct Project {
    root: PathBuf,
    table: toml::Table,
    name: String,
    created: Option<i64>,
    /// Why the project file could not be read, when it could not.
    warning: Option<String>,
}

impl Project {
    /// Makes `root` a project called `name` (see [`Project::create_at`]); `created` is now.
    ///
    /// # Errors
    ///
    /// As [`Project::create_at`].
    pub fn create(root: &Path, name: &str) -> Result<Project, ProjectError> {
        Project::create_at(root, name, unix_now())
    }

    /// Makes `root` a project called `name`, made at `now` (Unix seconds).
    ///
    /// The folder is created when it is missing. A folder that already exists becomes the
    /// project as it is: the PDFs in it are the project's documents.
    ///
    /// # Errors
    ///
    /// The name is empty, the folder already is a project, `root` is a file, or the disk refused.
    pub fn create_at(root: &Path, name: &str, now: i64) -> Result<Project, ProjectError> {
        let name = clean_name(name)?;
        if is_project(root) {
            return Err(ProjectError::AlreadyProject(root.to_path_buf()));
        }
        if root.exists() && !root.is_dir() {
            return Err(ProjectError::Exists(root.to_path_buf()));
        }
        fs::create_dir_all(root).map_err(io_error(root))?;
        let mut table = toml::Table::new();
        table.insert(
            "version".into(),
            toml::Value::Integer(PROJECT_FORMAT_VERSION),
        );
        table.insert("name".into(), toml::Value::String(name.clone()));
        table.insert("created".into(), created_value(now));
        let project = Project {
            root: root.to_path_buf(),
            table,
            name,
            created: Some(now),
            warning: None,
        };
        project.write()?;
        Ok(project)
    }

    /// Opens the project at `root`.
    ///
    /// A project file that cannot be read as TOML, or has no name, still makes a project: its
    /// name is the folder's and [`Project::warning`] says what was wrong. The file is left
    /// alone.
    ///
    /// # Errors
    ///
    /// [`ProjectError::NotProject`] when there is no project file; an I/O error when it cannot
    /// be read at all.
    pub fn open(root: &Path) -> Result<Project, ProjectError> {
        let file = root.join(PROJECT_FILE);
        if !file.is_file() {
            return Err(ProjectError::NotProject(root.to_path_buf()));
        }
        let bytes = fs::read(&file).map_err(io_error(&file))?;
        let folder = root.file_name().map_or_else(
            || root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let parsed = std::str::from_utf8(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|text| text.parse::<toml::Table>().map_err(|e| e.to_string()));
        let project = match parsed {
            Ok(table) => {
                let name = table
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .and_then(|n| clean_name(n).ok());
                let created = table.get("created").and_then(created_from_value);
                let warning = name
                    .is_none()
                    .then(|| "the project file has no name".to_owned());
                Project {
                    root: root.to_path_buf(),
                    name: name.unwrap_or(folder),
                    table,
                    created,
                    warning,
                }
            }
            Err(e) => Project {
                root: root.to_path_buf(),
                table: toml::Table::new(),
                name: folder,
                created: None,
                warning: Some(format!("the project file cannot be read: {e}")),
            },
        };
        if let Some(w) = &project.warning {
            log::warn!("{}: {w}", file.display());
        }
        Ok(project)
    }

    /// The project's folder, as given.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The project's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// When the project was made (Unix seconds), if the file says.
    pub fn created(&self) -> Option<i64> {
        self.created
    }

    /// What was wrong with the project file when it was opened.
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    /// An entry of the project file that this type does not know, for features that add
    /// their own.
    pub fn entry(&self, key: &str) -> Option<&toml::Value> {
        self.table.get(key)
    }

    /// Renames the project (not its folder) and writes the project file. Entries it does not
    /// know are kept. When the file could not be read before, a copy of it is kept as `.bad`.
    ///
    /// # Errors
    ///
    /// The name is empty, or the file cannot be written.
    pub fn rename(&mut self, name: &str) -> Result<(), ProjectError> {
        let name = clean_name(name)?;
        if self.warning.is_some() {
            let file = self.root.join(PROJECT_FILE);
            let bad = crate::store::atomic::bad_path(&file);
            if let Err(e) = fs::copy(&file, &bad) {
                log::warn!("cannot keep a copy of {}: {e}", file.display());
            }
        }
        self.table
            .insert("name".into(), toml::Value::String(name.clone()));
        self.table
            .entry("version")
            .or_insert(toml::Value::Integer(PROJECT_FORMAT_VERSION));
        self.write()?;
        self.name = name;
        self.warning = None;
        Ok(())
    }

    fn write(&self) -> Result<(), ProjectError> {
        let file = self.root.join(PROJECT_FILE);
        let text = toml::to_string(&self.table).map_err(|e| ProjectError::Io {
            path: file.clone(),
            source: io::Error::other(e),
        })?;
        atomic_write(&file, text.as_bytes()).map_err(io_error(&file))
    }
}

/// Days since 1970-01-01 as a (year, month, day) of the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// (year, month, day) as days since 1970-01-01.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339_utc(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// `YYYY-MM-DDTHH:MM:SS…Z` (or with a space instead of `T`) as Unix seconds; fractions and
/// other offsets than `Z` are not read.
fn parse_rfc3339_utc(text: &str) -> Option<i64> {
    let num = |s: Option<&str>| s?.parse::<i64>().ok();
    let date = text.get(0..10)?;
    let time = text.get(11..19)?;
    let mut d = date.split('-');
    let (y, m, day) = (num(d.next())?, num(d.next())?, num(d.next())?);
    let mut t = time.split(':');
    let (h, min, s) = (num(t.next())?, num(t.next())?, num(t.next())?);
    let days = days_from_civil(y, u32::try_from(m).ok()?, u32::try_from(day).ok()?);
    Some(days * 86_400 + h * 3600 + min * 60 + s)
}

fn created_value(now: i64) -> toml::Value {
    let text = rfc3339_utc(now);
    match text.parse::<toml::value::Datetime>() {
        Ok(dt) => toml::Value::Datetime(dt),
        Err(_) => toml::Value::String(text),
    }
}

fn created_from_value(value: &toml::Value) -> Option<i64> {
    match value {
        toml::Value::Datetime(dt) => parse_rfc3339_utc(&dt.to_string()),
        toml::Value::String(s) => parse_rfc3339_utc(s),
        toml::Value::Integer(n) => Some(*n),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// The trash

/// One item in the project's trash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashEntry {
    /// The name of its folder in the trash; what [`restore`] takes.
    pub id: String,
    /// Where it was in the project.
    pub rel_path: PathBuf,
    /// When it was moved to the trash (Unix seconds).
    pub deleted: i64,
    /// It is a folder.
    pub is_dir: bool,
}

#[derive(Serialize, Deserialize)]
struct TrashManifest {
    version: u32,
    path: PathBuf,
    deleted: i64,
}

fn trash_dir(root: &Path) -> PathBuf {
    data_dir(root).join(TRASH_DIR)
}

/// Moves the file or folder `rel` of the project at `root` to the project's trash, now.
///
/// # Errors
///
/// As [`trash_at`].
pub fn trash(root: &Path, rel: &Path) -> Result<TrashEntry, ProjectError> {
    trash_at(root, rel, unix_now())
}

/// Moves the file or folder `rel` of the project at `root` to `.refdesk/trash/<time>-<n>/rel`.
/// Each call gets a folder of its own, so the same name can be deleted twice.
///
/// # Errors
///
/// The path is not inside the project, nothing is there, or the disk refused.
pub fn trash_at(root: &Path, rel: &Path, now: i64) -> Result<TrashEntry, ProjectError> {
    let rel = clean_entry(rel)?;
    let src = root.join(&rel);
    let meta = fs::symlink_metadata(&src).map_err(|_| ProjectError::Missing(rel.clone()))?;
    let trash = trash_dir(root);
    fs::create_dir_all(&trash).map_err(io_error(&trash))?;
    let mut n: u32 = 0;
    let (id, slot) = loop {
        let id = format!("{now}-{n}");
        let slot = trash.join(&id);
        match fs::create_dir(&slot) {
            Ok(()) => break (id, slot),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(io_error(&slot)(e)),
        }
    };
    let dest = slot.join(&rel);
    let moved = dest
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::rename(&src, &dest));
    if let Err(e) = moved {
        let _ = fs::remove_dir_all(&slot);
        return Err(io_error(&src)(e));
    }
    let manifest = TrashManifest {
        version: 1,
        path: rel.clone(),
        deleted: now,
    };
    let bytes = serde_json::to_vec(&manifest).unwrap_or_default();
    if let Err(e) = atomic_write(&slot.join(TRASH_MANIFEST), &bytes) {
        log::warn!("cannot describe {} in the trash: {e}", rel.display());
    }
    Ok(TrashEntry {
        id,
        rel_path: rel,
        deleted: now,
        is_dir: meta.is_dir(),
    })
}

fn read_trash_entry(trash: &Path, id: &str) -> Option<TrashEntry> {
    let slot = trash.join(id);
    let bytes = fs::read(slot.join(TRASH_MANIFEST)).ok()?;
    let manifest: TrashManifest = serde_json::from_slice(&bytes).ok()?;
    let rel = clean_entry(&manifest.path).ok()?;
    let meta = fs::symlink_metadata(slot.join(&rel)).ok()?;
    Some(TrashEntry {
        id: id.to_owned(),
        rel_path: rel,
        deleted: manifest.deleted,
        is_dir: meta.is_dir(),
    })
}

/// The items in the project's trash, the most recently deleted first. Folders in the trash that
/// do not describe an item are left out.
///
/// # Errors
///
/// The trash exists but cannot be read.
pub fn list_trash(root: &Path) -> Result<Vec<TrashEntry>, ProjectError> {
    let trash = trash_dir(root);
    let listing = match fs::read_dir(&trash) {
        Ok(listing) => listing,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error(&trash)(e)),
    };
    let mut out: Vec<TrashEntry> = listing
        .filter_map(Result::ok)
        .filter_map(|item| read_trash_entry(&trash, &item.file_name().to_string_lossy()))
        .collect();
    out.sort_by(|a, b| {
        b.deleted
            .cmp(&a.deleted)
            .then_with(|| trash_seq(&b.id).cmp(&trash_seq(&a.id)))
    });
    Ok(out)
}

fn trash_seq(id: &str) -> u32 {
    id.rsplit('-')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Puts the item `id` of the trash back where it was. When something else is there now, it
/// comes back as `name (2).ext`. Returns the path below the root it got.
///
/// # Errors
///
/// There is no such item, or the disk refused.
pub fn restore(root: &Path, id: &str) -> Result<PathBuf, ProjectError> {
    let trash = trash_dir(root);
    if id.contains('/') || id.starts_with('.') || id.is_empty() {
        return Err(ProjectError::Missing(PathBuf::from(id)));
    }
    let entry =
        read_trash_entry(&trash, id).ok_or_else(|| ProjectError::Missing(PathBuf::from(id)))?;
    let slot = trash.join(id);
    let mut rel = entry.rel_path.clone();
    if exists_no_follow(&root.join(&rel)) {
        rel = keep_both_name(&rel, |p| exists_no_follow(&root.join(p)));
    }
    let dest = root.join(&rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    let src = slot.join(&entry.rel_path);
    fs::rename(&src, &dest).map_err(io_error(&src))?;
    if let Err(e) = fs::remove_dir_all(&slot) {
        log::warn!("cannot clean up {}: {e}", slot.display());
    }
    Ok(rel)
}

/// Deletes everything in the project's trash for good.
///
/// # Errors
///
/// The disk refused.
pub fn empty_trash(root: &Path) -> Result<(), ProjectError> {
    let trash = trash_dir(root);
    match fs::remove_dir_all(&trash) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(&trash)(e)),
    }
}

// ---------------------------------------------------------------------------------------------
// What the project already holds, by content

/// The PDFs of a project by size, with their fingerprints once they were needed. Comparing
/// only files of the same size keeps the hashing down.
struct ContentIndex {
    by_size: HashMap<u64, Vec<Known>>,
}

struct Known {
    rel: PathBuf,
    fingerprint: Option<String>,
}

impl ContentIndex {
    /// Lists the PDFs of the project at `root` with their sizes (no hashing yet).
    fn build(root: &Path, cancel: &AtomicBool) -> ContentIndex {
        let mut by_size: HashMap<u64, Vec<Known>> = HashMap::new();
        if let Ok(index) = scan(root, &ScanLimits::default(), cancel) {
            for idx in index.files() {
                let Some(entry) = index.entry(idx) else {
                    continue;
                };
                if let Ok(meta) = fs::metadata(root.join(&entry.rel_path)) {
                    by_size.entry(meta.len()).or_default().push(Known {
                        rel: entry.rel_path.clone(),
                        fingerprint: None,
                    });
                }
            }
        }
        ContentIndex { by_size }
    }

    fn has_size(&self, size: u64) -> bool {
        self.by_size.contains_key(&size)
    }

    /// The project file with the same content as `source` (of `size` bytes), if there is one.
    /// `fingerprint` is the source's, computed here on first need and kept by the caller.
    fn same_as(
        &mut self,
        root: &Path,
        source: &Path,
        size: u64,
        fingerprint: &mut Option<String>,
        cancel: &AtomicBool,
    ) -> io::Result<Option<PathBuf>> {
        let Some(known) = self.by_size.get_mut(&size) else {
            return Ok(None);
        };
        if fingerprint.is_none() {
            *fingerprint = Some(fingerprint_file(source)?);
        }
        let wanted = fingerprint.as_deref();
        for k in known.iter_mut() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            if k.fingerprint.is_none() {
                // A project file that cannot be read now does not count as a match.
                k.fingerprint = fingerprint_file(&root.join(&k.rel)).ok();
            }
            if k.fingerprint.as_deref() == wanted {
                return Ok(Some(k.rel.clone()));
            }
        }
        Ok(None)
    }

    fn add(&mut self, rel: PathBuf, size: u64, fingerprint: Option<String>) {
        let list = self.by_size.entry(size).or_default();
        list.retain(|k| k.rel != rel);
        list.push(Known { rel, fingerprint });
    }

    fn remove(&mut self, rel: &Path) {
        for list in self.by_size.values_mut() {
            list.retain(|k| k.rel != rel);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Importing

/// What to do when the target name is taken by a file with other content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictChoice {
    /// Move the old file to the project's trash and copy the new one in its place.
    Replace,
    /// Keep the old file and copy the new one as `name (2).pdf`.
    KeepBoth,
    /// Leave the old file and do not copy the new one.
    Skip,
}

/// How name clashes are settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Stop at each clash and wait for [`ImportJob::resolve`].
    Ask,
    /// Always the same choice.
    Always(ConflictChoice),
}

/// One file to copy: where from and where to, below the project root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportItem {
    /// The file to copy.
    pub source: PathBuf,
    /// The path it gets below the project root (file name included).
    pub dest: PathBuf,
}

/// Why a source was not copied without being an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// It is not a PDF (by its name).
    NotPdf,
    /// Its name starts with `.`.
    Hidden,
    /// It is already inside the project.
    InsideProject,
    /// Its target name was taken and "skip" was chosen.
    Declined,
}

/// What happened to one source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportOutcome {
    /// Copied.
    Imported,
    /// The project already has a file with the same content: the one at this path below the
    /// root.
    AlreadyThere {
        /// The file with the same content.
        existing: PathBuf,
    },
    /// Not copied, for a reason that is not an error.
    Skipped(SkipReason),
    /// Copied over a file of the same name, which went to the project's trash.
    Replaced,
    /// It could not be copied.
    Failed(String),
}

/// One line of an import's result list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportResult {
    /// The source.
    pub source: PathBuf,
    /// Where it went (or would have gone) below the project root; for "already there" the
    /// file it matched is in the outcome.
    pub dest: PathBuf,
    /// What happened.
    pub outcome: ImportOutcome,
}

/// A name clash the import waits on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The file being imported.
    pub source: PathBuf,
    /// The taken name below the project root.
    pub dest: PathBuf,
    /// Size of the file being imported, in bytes.
    pub source_size: u64,
    /// Its modification time (Unix seconds), if known.
    pub source_mtime: Option<i64>,
    /// Size of the file that is there, in bytes.
    pub existing_size: u64,
    /// Its modification time (Unix seconds), if known.
    pub existing_mtime: Option<i64>,
}

/// Where an import is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ImportState {
    /// Copying.
    #[default]
    Running,
    /// Stopped at a name clash, waiting for [`ImportJob::resolve`].
    Waiting(Conflict),
    /// Every source was handled.
    Done,
    /// Cancelled; the sources not yet handled are not in the results.
    Cancelled,
}

/// What an import has done so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportSnapshot {
    /// Number of PDF files to copy (known once the sources have been looked at).
    pub total: usize,
    /// Number of them handled.
    pub done: usize,
    /// The file being handled.
    pub current: Option<PathBuf>,
    /// Bytes copied so far, over all files.
    pub bytes_copied: u64,
    /// One line per source handled (and per source refused before copying), in order.
    pub results: Vec<ImportResult>,
    /// Files inside imported folders that were left out because they are not PDFs.
    pub not_pdf: usize,
    /// Where the import is.
    pub state: ImportState,
}

impl ImportSnapshot {
    /// The import has ended (done or cancelled).
    pub fn finished(&self) -> bool {
        matches!(self.state, ImportState::Done | ImportState::Cancelled)
    }

    /// Number of results with an outcome that `pick` accepts.
    pub fn count(&self, pick: impl Fn(&ImportOutcome) -> bool) -> usize {
        self.results.iter().filter(|r| pick(&r.outcome)).count()
    }
}

struct ImportShared {
    cancel: AtomicBool,
    dropped: AtomicBool,
    snapshot: Mutex<Arc<ImportSnapshot>>,
    answer: Mutex<Option<(ConflictChoice, bool)>>,
    wake: Condvar,
}

impl ImportShared {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn current(&self) -> Arc<ImportSnapshot> {
        Arc::clone(&self.snapshot.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn answer(&self) -> MutexGuard<'_, Option<(ConflictChoice, bool)>> {
        self.answer.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The sources of an import.
enum Plan {
    /// Files and folders, all into one folder.
    Sources {
        sources: Vec<PathBuf>,
        into: PathBuf,
    },
    /// Files, each with its own target.
    Items(Vec<ImportItem>),
}

/// An import running on its own thread. Dropping it cancels the import without waiting; the
/// file being copied is cleaned up by the thread.
pub struct ImportJob {
    shared: Arc<ImportShared>,
}

impl std::fmt::Debug for ImportJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportJob").finish_non_exhaustive()
    }
}

impl Drop for ImportJob {
    fn drop(&mut self) {
        self.shared.dropped.store(true, Ordering::Relaxed);
        self.cancel();
    }
}

impl ImportJob {
    /// Starts copying `sources` into the folder `into` (below the project root) of the project
    /// at `project_root` and returns at once.
    ///
    /// A source is a PDF file or a folder. A folder brings the PDFs below it, keeping its
    /// structure, under its own name. Files that are not PDFs are skipped (and counted), as are
    /// names starting with `.` and anything already inside the project. A link is followed and
    /// what it points to is copied. A file whose content the project already has is not copied
    /// again. `notifier` is called whenever the snapshot changes (never after the job was
    /// dropped).
    pub fn start(
        project_root: PathBuf,
        sources: Vec<PathBuf>,
        into: PathBuf,
        policy: ConflictPolicy,
        notifier: Notifier,
    ) -> ImportJob {
        ImportJob::spawn(
            project_root,
            Plan::Sources { sources, into },
            policy,
            notifier,
        )
    }

    /// Like [`ImportJob::start`], with the target of every file given.
    pub fn start_items(
        project_root: PathBuf,
        items: Vec<ImportItem>,
        policy: ConflictPolicy,
        notifier: Notifier,
    ) -> ImportJob {
        ImportJob::spawn(project_root, Plan::Items(items), policy, notifier)
    }

    fn spawn(root: PathBuf, plan: Plan, policy: ConflictPolicy, notifier: Notifier) -> ImportJob {
        let shared = Arc::new(ImportShared {
            cancel: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            snapshot: Mutex::new(Arc::new(ImportSnapshot::default())),
            answer: Mutex::new(None),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let note = Arc::clone(&notifier);
        let spawned = std::thread::Builder::new()
            .name("refdesk-import".into())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    let mut run = ImportRun::new(&worker, root, policy, notifier);
                    run.run(plan);
                }));
                if outcome.is_err() {
                    // A bug must not leave the UI waiting for an import that never ends.
                    let mut last = ImportSnapshot::clone(&worker.current());
                    last.state = ImportState::Done;
                    last.current = None;
                    *worker
                        .snapshot
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Arc::new(last);
                    if !worker.dropped.load(Ordering::Relaxed) {
                        note();
                    }
                }
            });
        if let Err(e) = spawned {
            let snapshot = ImportSnapshot {
                state: ImportState::Done,
                results: vec![ImportResult {
                    source: PathBuf::new(),
                    dest: PathBuf::new(),
                    outcome: ImportOutcome::Failed(format!("cannot start the import: {e}")),
                }],
                ..ImportSnapshot::default()
            };
            *shared
                .snapshot
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Arc::new(snapshot);
        }
        ImportJob { shared }
    }

    /// What the import has done so far. Cheap: a shared copy the thread replaces as it goes.
    pub fn snapshot(&self) -> Arc<ImportSnapshot> {
        self.shared.current()
    }

    /// Settles the name clash the import waits on. With `apply_to_all` the same choice is made
    /// for every clash after it. Does nothing while the import does not wait.
    pub fn resolve(&self, choice: ConflictChoice, apply_to_all: bool) {
        *self.shared.answer() = Some((choice, apply_to_all));
        self.shared.wake.notify_all();
    }

    /// Stops the import after cleaning up the file being copied. The snapshot then says
    /// [`ImportState::Cancelled`].
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        // Taking the lock makes sure a thread about to wait sees the flag.
        drop(self.shared.answer());
        self.shared.wake.notify_all();
    }
}

/// The copy was cancelled.
struct Cancelled;

/// The import thread's state.
struct ImportRun<'a> {
    shared: &'a ImportShared,
    root: PathBuf,
    /// The real path of the root, to tell what is inside the project.
    real_root: PathBuf,
    policy: ConflictPolicy,
    notifier: Notifier,
    snap: ImportSnapshot,
}

impl<'a> ImportRun<'a> {
    fn new(
        shared: &'a ImportShared,
        root: PathBuf,
        policy: ConflictPolicy,
        notifier: Notifier,
    ) -> ImportRun<'a> {
        let real_root = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        ImportRun {
            shared,
            root,
            real_root,
            policy,
            notifier,
            snap: ImportSnapshot::default(),
        }
    }

    fn publish(&self) {
        *self
            .shared
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(self.snap.clone());
        if !self.shared.dropped.load(Ordering::Relaxed) {
            (self.notifier)();
        }
    }

    fn finish(&mut self, state: ImportState) {
        self.snap.state = state;
        self.snap.current = None;
        self.publish();
    }

    fn result(&mut self, source: &Path, dest: &Path, outcome: ImportOutcome) {
        self.snap.results.push(ImportResult {
            source: source.to_path_buf(),
            dest: dest.to_path_buf(),
            outcome,
        });
    }

    fn inside_project(&self, path: &Path) -> bool {
        fs::canonicalize(path).is_ok_and(|real| real.starts_with(&self.real_root))
    }

    fn run(&mut self, plan: Plan) {
        let items = match plan {
            Plan::Sources { sources, into } => self.expand(sources, &into),
            Plan::Items(items) => self.check_items(items),
        };
        if self.shared.cancelled() {
            self.finish(ImportState::Cancelled);
            return;
        }
        self.snap.total = items.len();
        self.publish();
        let mut index = ContentIndex::build(&self.root, &self.shared.cancel);
        for item in items {
            if self.shared.cancelled() {
                self.finish(ImportState::Cancelled);
                return;
            }
            self.snap.current = Some(item.source.clone());
            self.publish();
            match self.import_one(&item, &mut index) {
                Ok((dest, outcome)) => self.result(&item.source, &dest, outcome),
                Err(Cancelled) => {
                    self.finish(ImportState::Cancelled);
                    return;
                }
            }
            self.snap.done += 1;
            self.publish();
        }
        self.finish(ImportState::Done);
    }

    /// The files to copy for `sources` into `into`; refused sources become results.
    fn expand(&mut self, sources: Vec<PathBuf>, into: &Path) -> Vec<ImportItem> {
        let into = match clean_relative(into) {
            Ok(into) => into,
            Err(e) => {
                for source in sources {
                    self.result(&source, into, ImportOutcome::Failed(e.to_string()));
                }
                return Vec::new();
            }
        };
        let mut items = Vec::new();
        for source in sources {
            if self.shared.cancelled() {
                break;
            }
            let Some(name) = source.file_name().map(OsStr::to_os_string) else {
                self.result(
                    &source,
                    &into,
                    ImportOutcome::Failed("the path has no file name".into()),
                );
                continue;
            };
            let dest = into.join(&name);
            if is_hidden(&name) {
                self.result(&source, &dest, ImportOutcome::Skipped(SkipReason::Hidden));
                continue;
            }
            let meta = match fs::metadata(&source) {
                Ok(meta) => meta,
                Err(e) => {
                    self.result(&source, &dest, ImportOutcome::Failed(e.to_string()));
                    continue;
                }
            };
            if self.inside_project(&source) {
                self.result(
                    &source,
                    &dest,
                    ImportOutcome::Skipped(SkipReason::InsideProject),
                );
                continue;
            }
            if meta.is_dir() {
                let mut visited = HashSet::new();
                if let Ok(real) = fs::canonicalize(&source) {
                    visited.insert(real);
                }
                self.walk(&source, &dest, &mut visited, &mut items);
            } else if is_pdf_name(&name)
                || fs::canonicalize(&source).is_ok_and(|r| is_pdf_name(r.as_os_str()))
            {
                items.push(ImportItem { source, dest });
            } else {
                self.result(&source, &dest, ImportOutcome::Skipped(SkipReason::NotPdf));
            }
        }
        items
    }

    /// Adds the PDFs below `dir` (going to `dest`) to `items`, in name order.
    fn walk(
        &mut self,
        dir: &Path,
        dest: &Path,
        visited: &mut HashSet<PathBuf>,
        items: &mut Vec<ImportItem>,
    ) {
        let Ok(listing) = fs::read_dir(dir) else {
            log::debug!("import: cannot read {}", dir.display());
            return;
        };
        let mut names: Vec<_> = listing
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .filter(|n| !is_hidden(n))
            .collect();
        names.sort_by(|a, b| {
            crate::workspace::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy())
                .then_with(|| a.cmp(b))
        });
        for name in names {
            if self.shared.cancelled() {
                return;
            }
            let path = dir.join(&name);
            // A broken link has no metadata; it is left out like in the workspace scan.
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if self.inside_project(&path) {
                continue;
            }
            if meta.is_dir() {
                let Ok(real) = fs::canonicalize(&path) else {
                    continue;
                };
                if visited.insert(real) {
                    self.walk(&path, &dest.join(&name), visited, items);
                }
            } else if meta.is_file() {
                if is_pdf_name(&name)
                    || fs::canonicalize(&path).is_ok_and(|r| is_pdf_name(r.as_os_str()))
                {
                    items.push(ImportItem {
                        source: path,
                        dest: dest.join(&name),
                    });
                } else {
                    self.snap.not_pdf += 1;
                }
            }
        }
    }

    /// The items that can be copied; refused ones become results.
    fn check_items(&mut self, items: Vec<ImportItem>) -> Vec<ImportItem> {
        let mut out = Vec::new();
        for item in items {
            let dest = match clean_entry(&item.dest) {
                Ok(dest) => dest,
                Err(e) => {
                    self.result(
                        &item.source,
                        &item.dest,
                        ImportOutcome::Failed(e.to_string()),
                    );
                    continue;
                }
            };
            let name = item.source.file_name().unwrap_or_default();
            if is_hidden(name) {
                self.result(
                    &item.source,
                    &dest,
                    ImportOutcome::Skipped(SkipReason::Hidden),
                );
            } else if !exists_no_follow(&item.source) {
                let e = io::Error::from(io::ErrorKind::NotFound);
                self.result(&item.source, &dest, ImportOutcome::Failed(e.to_string()));
            } else if self.inside_project(&item.source) {
                self.result(
                    &item.source,
                    &dest,
                    ImportOutcome::Skipped(SkipReason::InsideProject),
                );
            } else if !is_pdf_name(name)
                && !fs::canonicalize(&item.source).is_ok_and(|r| is_pdf_name(r.as_os_str()))
            {
                self.result(
                    &item.source,
                    &dest,
                    ImportOutcome::Skipped(SkipReason::NotPdf),
                );
            } else {
                out.push(ImportItem {
                    source: item.source,
                    dest,
                });
            }
        }
        out
    }

    /// Waits for the answer to a clash. `None` when the import was cancelled meanwhile.
    fn ask(&mut self, conflict: Conflict) -> Option<ConflictChoice> {
        *self.shared.answer() = None;
        self.snap.state = ImportState::Waiting(conflict);
        self.publish();
        let mut answer = self.shared.answer();
        let (choice, all) = loop {
            if self.shared.cancelled() {
                return None;
            }
            if let Some(a) = answer.take() {
                break a;
            }
            answer = self
                .shared
                .wake
                .wait(answer)
                .unwrap_or_else(PoisonError::into_inner);
        };
        drop(answer);
        if all {
            self.policy = ConflictPolicy::Always(choice);
        }
        self.snap.state = ImportState::Running;
        self.publish();
        Some(choice)
    }

    /// Copies one file. The outcome comes with the path the file got.
    fn import_one(
        &mut self,
        item: &ImportItem,
        index: &mut ContentIndex,
    ) -> Result<(PathBuf, ImportOutcome), Cancelled> {
        let failed = |e: &dyn std::fmt::Display| ImportOutcome::Failed(e.to_string());
        let meta = match fs::metadata(&item.source) {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => return Ok((item.dest.clone(), failed(&"not a file"))),
            Err(e) => return Ok((item.dest.clone(), failed(&e))),
        };
        let size = meta.len();
        let mut fingerprint = None;
        if index.has_size(size) {
            match index.same_as(
                &self.root,
                &item.source,
                size,
                &mut fingerprint,
                &self.shared.cancel,
            ) {
                Ok(Some(existing)) => {
                    return Ok((item.dest.clone(), ImportOutcome::AlreadyThere { existing }));
                }
                Ok(None) => {}
                Err(e) => return Ok((item.dest.clone(), failed(&e))),
            }
        }
        if self.shared.cancelled() {
            return Err(Cancelled);
        }
        let mut dest = item.dest.clone();
        let mut replaced = false;
        let target = self.root.join(&dest);
        if let Ok(existing) = fs::symlink_metadata(&target) {
            let choice = match self.policy {
                ConflictPolicy::Always(choice) => choice,
                ConflictPolicy::Ask => {
                    let conflict = Conflict {
                        source: item.source.clone(),
                        dest: dest.clone(),
                        source_size: size,
                        source_mtime: mtime(&meta),
                        existing_size: existing.len(),
                        existing_mtime: mtime(&existing),
                    };
                    self.ask(conflict).ok_or(Cancelled)?
                }
            };
            match choice {
                ConflictChoice::Skip => {
                    return Ok((dest, ImportOutcome::Skipped(SkipReason::Declined)));
                }
                ConflictChoice::KeepBoth => {
                    let root = &self.root;
                    dest = keep_both_name(&dest, |p| exists_no_follow(&root.join(p)));
                }
                ConflictChoice::Replace => {
                    if let Err(e) = trash(&self.root, &dest) {
                        return Ok((dest, failed(&e)));
                    }
                    index.remove(&dest);
                    replaced = true;
                }
            }
        }
        match self.copy(&item.source, &self.root.join(&dest), &meta) {
            Ok(()) => {}
            Err(CopyError::Cancelled) => return Err(Cancelled),
            Err(CopyError::Io(e)) => return Ok((dest, failed(&e))),
        }
        index.add(dest.clone(), size, fingerprint);
        let outcome = if replaced {
            ImportOutcome::Replaced
        } else {
            ImportOutcome::Imported
        };
        Ok((dest, outcome))
    }

    /// Copies `source` to `dest` through a hidden temporary name next to it, reporting progress.
    /// The temporary file is removed when the copy fails or is cancelled.
    fn copy(&mut self, source: &Path, dest: &Path, meta: &fs::Metadata) -> Result<(), CopyError> {
        let dir = dest.parent().unwrap_or(Path::new(""));
        let name = dest
            .file_name()
            .ok_or_else(|| CopyError::Io(io::Error::other("the target has no file name")))?;
        fs::create_dir_all(dir).map_err(CopyError::Io)?;
        let tmp = dir.join(format!(".{}{IMPORTING_SUFFIX}", name.to_string_lossy()));
        let result = self.copy_to(source, &tmp, meta);
        let result = result.and_then(|()| fs::rename(&tmp, dest).map_err(CopyError::Io));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    fn copy_to(&mut self, source: &Path, tmp: &Path, meta: &fs::Metadata) -> Result<(), CopyError> {
        let mut input = File::open(source).map_err(CopyError::Io)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp)
            .map_err(CopyError::Io)?;
        let mut buf = vec![0u8; COPY_BUFFER];
        loop {
            if self.shared.cancelled() {
                return Err(CopyError::Cancelled);
            }
            let n = match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(CopyError::Io(e)),
            };
            output.write_all(&buf[..n]).map_err(CopyError::Io)?;
            self.snap.bytes_copied += n as u64;
            self.publish();
        }
        output.sync_all().map_err(CopyError::Io)?;
        if let Ok(modified) = meta.modified()
            && let Err(e) = output.set_modified(modified)
        {
            log::debug!("cannot set the time of {}: {e}", tmp.display());
        }
        Ok(())
    }
}

enum CopyError {
    Cancelled,
    Io(io::Error),
}

fn mtime(meta: &fs::Metadata) -> Option<i64> {
    let t = meta.modified().ok()?;
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    i64::try_from(secs).ok()
}

// ---------------------------------------------------------------------------------------------
// A look at a folder before importing from it

/// One PDF found below the surveyed folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurveyFile {
    /// Its path below the surveyed folder.
    pub rel: PathBuf,
    /// Its size in bytes.
    pub size: u64,
}

/// Where a survey is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SurveyState {
    /// Looking for the PDFs.
    #[default]
    Scanning,
    /// The list is complete; comparing contents with the project.
    Checking,
    /// Done.
    Done,
    /// The folder could not be read.
    Failed(String),
}

/// What a survey has found so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SurveySnapshot {
    /// The PDFs below the folder, in the order of the workspace scan (folders first, natural
    /// order). Empty until the scan has finished.
    pub files: Arc<Vec<SurveyFile>>,
    /// For files (by index into `files`) whose content the project already has: that project
    /// file, below the project root. Grows while the state is [`SurveyState::Checking`].
    pub existing: Arc<HashMap<usize, PathBuf>>,
    /// The scan stopped at a limit; the list is incomplete.
    pub truncated: bool,
    /// Where the survey is.
    pub state: SurveyState,
}

struct SurveyShared {
    cancel: AtomicBool,
    snapshot: Mutex<Arc<SurveySnapshot>>,
}

/// Lists the PDFs below a folder (with the workspace scan) with their sizes, then finds those
/// whose content the project already has, on a thread of its own. Only files whose size matches
/// a project file are hashed. Dropping it cancels the survey without waiting.
pub struct SourceSurvey {
    shared: Arc<SurveyShared>,
}

impl std::fmt::Debug for SourceSurvey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceSurvey").finish_non_exhaustive()
    }
}

impl Drop for SourceSurvey {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}

impl SourceSurvey {
    /// Starts surveying `folder` for an import into the project at `project_root`. `notifier`
    /// is called whenever the snapshot changes (never after the survey was dropped).
    pub fn start(project_root: PathBuf, folder: PathBuf, notifier: Notifier) -> SourceSurvey {
        let shared = Arc::new(SurveyShared {
            cancel: AtomicBool::new(false),
            snapshot: Mutex::new(Arc::new(SurveySnapshot::default())),
        });
        let worker = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("refdesk-import-survey".into())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    survey(&worker, &project_root, &folder, &notifier);
                }));
                if outcome.is_err() && !worker.cancel.load(Ordering::Relaxed) {
                    set_survey(&worker, |s| {
                        s.state = SurveyState::Failed("the survey failed unexpectedly".into());
                    });
                    notifier();
                }
            });
        if let Err(e) = spawned {
            set_survey(&shared, |s| {
                s.state = SurveyState::Failed(format!("cannot start the survey: {e}"));
            });
        }
        SourceSurvey { shared }
    }

    /// What the survey has found so far. Cheap.
    pub fn snapshot(&self) -> Arc<SurveySnapshot> {
        Arc::clone(
            &self
                .shared
                .snapshot
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

fn set_survey(shared: &SurveyShared, change: impl FnOnce(&mut SurveySnapshot)) {
    let mut guard = shared
        .snapshot
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let mut next = SurveySnapshot::clone(&guard);
    change(&mut next);
    *guard = Arc::new(next);
}

fn survey(shared: &SurveyShared, root: &Path, folder: &Path, notifier: &Notifier) {
    let cancelled = || shared.cancel.load(Ordering::Relaxed);
    let index = match scan(folder, &ScanLimits::default(), &shared.cancel) {
        Ok(index) => index,
        Err(e) => {
            if !cancelled() {
                set_survey(shared, |s| s.state = SurveyState::Failed(e.to_string()));
                notifier();
            }
            return;
        }
    };
    let mut files = Vec::with_capacity(index.file_count());
    for idx in index.files() {
        if cancelled() {
            return;
        }
        let Some(entry) = index.entry(idx) else {
            continue;
        };
        if let Ok(meta) = fs::metadata(folder.join(&entry.rel_path)) {
            files.push(SurveyFile {
                rel: entry.rel_path.clone(),
                size: meta.len(),
            });
        }
    }
    let files = Arc::new(files);
    set_survey(shared, |s| {
        s.files = Arc::clone(&files);
        s.truncated = index.truncated;
        s.state = SurveyState::Checking;
    });
    notifier();
    let mut contents = ContentIndex::build(root, &shared.cancel);
    let mut existing = HashMap::new();
    for (i, file) in files.iter().enumerate() {
        if cancelled() {
            return;
        }
        if !contents.has_size(file.size) {
            continue;
        }
        let mut fingerprint = None;
        if let Ok(Some(rel)) = contents.same_as(
            root,
            &folder.join(&file.rel),
            file.size,
            &mut fingerprint,
            &shared.cancel,
        ) {
            existing.insert(i, rel);
            let found = Arc::new(existing.clone());
            set_survey(shared, |s| s.existing = found);
            notifier();
        }
    }
    if cancelled() {
        return;
    }
    set_survey(shared, |s| s.state = SurveyState::Done);
    notifier();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_a_project_when_it_has_the_project_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dir = dir.path();
        assert!(!is_project(dir));
        std::fs::write(dir.join(PROJECT_FILE), "").expect("project file");
        assert!(is_project(dir));
        assert_eq!(data_dir(dir), dir.join(".refdesk"));
    }

    #[test]
    fn dates_round_trip_through_the_text_form() {
        for secs in [0, 86_399, 951_782_400, 1_764_201_600, 4_102_444_800] {
            let text = rfc3339_utc(secs);
            assert_eq!(parse_rfc3339_utc(&text), Some(secs), "{text}");
        }
        assert_eq!(rfc3339_utc(1_764_201_600), "2025-11-27T00:00:00Z");
        assert_eq!(parse_rfc3339_utc("nonsense"), None);
    }

    #[test]
    fn keep_both_names_count_up_from_two() {
        let taken = [PathBuf::from("a/x (2).pdf"), PathBuf::from("a/x (3).pdf")];
        let next = keep_both_name(Path::new("a/x.pdf"), |p| taken.iter().any(|t| t == p));
        assert_eq!(next, PathBuf::from("a/x (4).pdf"));
        assert_eq!(
            keep_both_name(Path::new("README"), |_| false),
            PathBuf::from("README (2)")
        );
    }

    #[test]
    fn relative_paths_stay_inside_the_project() {
        assert_eq!(
            clean_relative(Path::new("./a/b.pdf")).expect("ok"),
            PathBuf::from("a/b.pdf")
        );
        assert_eq!(clean_relative(Path::new("")).expect("ok"), PathBuf::new());
        assert!(matches!(
            clean_relative(Path::new("a/../../b")),
            Err(ProjectError::Outside(_))
        ));
        assert!(matches!(
            clean_relative(Path::new("/etc/passwd")),
            Err(ProjectError::Outside(_))
        ));
        assert!(matches!(
            clean_relative(Path::new(".refdesk/trash")),
            Err(ProjectError::Protected(_))
        ));
        assert!(matches!(
            clean_entry(Path::new(".")),
            Err(ProjectError::Protected(_))
        ));
    }
}
