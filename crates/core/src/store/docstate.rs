//! `documents.json`: what is remembered about each document (position, zoom, history, ...).

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::atomic::{Loaded, atomic_write, load_with};
use super::{FORMAT_VERSION, SAVED_HISTORY_ENTRIES, json_version, lenient_items, warn_if_newer};
use crate::model::ViewState;

/// The most records kept. Beyond this the least recently opened are dropped.
pub const MAX_RECORDS: usize = 2000;

/// The most recent workspace folders kept.
pub const MAX_RECENT_FOLDERS: usize = 50;

/// A folder that was opened as a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentFolder {
    /// The folder, as given to [`DocStateDb::record_folder`].
    pub path: PathBuf,
    /// When it was last opened (Unix seconds, from the caller).
    #[serde(default)]
    pub last_opened: i64,
    /// The project's name, when the folder was a project when it was opened (PROJ-4). Files
    /// written before projects existed have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// What is remembered about one document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "DocRecordRepr")]
pub struct DocRecord {
    /// Where the file was when last opened. Compared as given: the caller decides whether to
    /// canonicalize it.
    pub path: PathBuf,
    /// XXH3-128 of the file contents (see `fingerprint_file`), once it has been computed.
    pub fingerprint: Option<String>,
    /// File size in bytes when the fingerprint was computed.
    pub size: u64,
    /// File modification time (Unix seconds) when the fingerprint was computed.
    pub mtime: i64,
    /// When the document was last opened (Unix seconds, from the caller).
    pub last_opened: i64,
    /// Where the reader was and how the view was set.
    pub view: ViewState,
    /// Table-of-contents nodes the reader collapsed, each as the path of titles down to it.
    pub toc_collapsed: Vec<Vec<String>>,
}

/// Stored form of [`DocRecord`] in which everything but `path` may be missing.
#[derive(Deserialize)]
struct DocRecordRepr {
    path: PathBuf,
    #[serde(default)]
    fingerprint: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    mtime: i64,
    #[serde(default)]
    last_opened: i64,
    #[serde(default)]
    view: Option<ViewState>,
    #[serde(default)]
    toc_collapsed: Vec<Vec<String>>,
}

impl From<DocRecordRepr> for DocRecord {
    fn from(r: DocRecordRepr) -> DocRecord {
        let view = r.view.unwrap_or_else(|| ViewState::new(r.path.clone()));
        DocRecord {
            path: r.path,
            fingerprint: r.fingerprint,
            size: r.size,
            mtime: r.mtime,
            last_opened: r.last_opened,
            view,
            toc_collapsed: r.toc_collapsed,
        }
    }
}

/// All document records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DocStateDb {
    /// Format version of the file.
    pub version: u32,
    records: Vec<DocRecord>,
    /// Recently opened workspace folders, newest first.
    recent_folders: Vec<RecentFolder>,
}

impl Default for DocStateDb {
    fn default() -> DocStateDb {
        DocStateDb {
            version: FORMAT_VERSION,
            records: Vec::new(),
            recent_folders: Vec::new(),
        }
    }
}

impl DocStateDb {
    /// Reads the database. Never fails: a missing or empty file gives an empty database, a
    /// damaged one gives an empty database and is moved to `.bad`. Records that cannot be
    /// parsed are dropped (the original is then copied to `.bad`).
    pub fn load(path: &Path) -> DocStateDb {
        load_with(path, "documents", parse_db)
    }

    /// Writes the database with [`atomic_write`]. Each history is cut to the 50 entries around
    /// the current one.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut out = self.clone();
        out.version = FORMAT_VERSION;
        for rec in &mut out.records {
            rec.view.history = rec.view.history.trimmed(SAVED_HISTORY_ENTRIES);
        }
        let bytes = serde_json::to_vec_pretty(&out).map_err(io::Error::other)?;
        atomic_write(path, &bytes)
    }

    /// Number of records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether there are no records.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The record of a document: among the records with the same `path` or the same
    /// `fingerprint`, the one with the greatest `last_opened` (on a tie, the path match wins).
    ///
    /// The same path is the same document even if the fingerprint differs (a new version at the
    /// same place). A record without a fingerprint never matches by fingerprint, and neither does
    /// a lookup with `None`.
    pub fn lookup(&self, path: &Path, fingerprint: Option<&str>) -> Option<&DocRecord> {
        self.records
            .iter()
            .filter_map(|rec| {
                let by_path = rec.path == path;
                let by_fingerprint = matches!(
                    (fingerprint, rec.fingerprint.as_deref()),
                    (Some(a), Some(b)) if a == b
                );
                (by_path || by_fingerprint).then_some((rec, by_path))
            })
            .max_by_key(|(rec, by_path)| (rec.last_opened, *by_path))
            .map(|(rec, _)| rec)
    }

    /// Stores `rec`, replacing the record with the same path if there is one. When there are
    /// more than [`MAX_RECORDS`] records, the least recently opened are dropped.
    pub fn record(&mut self, rec: DocRecord) {
        match self.records.iter_mut().find(|r| r.path == rec.path) {
            Some(existing) => *existing = rec,
            None => self.records.push(rec),
        }
        self.enforce_limit();
    }

    /// The `n` most recently opened records, newest first.
    pub fn recent(&self, n: usize) -> Vec<&DocRecord> {
        let mut all: Vec<&DocRecord> = self.records.iter().collect();
        all.sort_by_key(|r| std::cmp::Reverse(r.last_opened));
        all.truncate(n);
        all
    }

    /// Remembers that the folder `path` was opened as a workspace at `now` (Unix seconds). The
    /// same path is kept once. At most [`MAX_RECENT_FOLDERS`] are kept; the least recent go.
    pub fn record_folder(&mut self, path: &Path, now: i64) {
        self.record_project_folder(path, None, now);
    }

    /// Like [`DocStateDb::record_folder`] for a folder that is a project called `project` (or
    /// not a project, with `None`).
    pub fn record_project_folder(&mut self, path: &Path, project: Option<&str>, now: i64) {
        self.recent_folders.retain(|f| f.path != path);
        self.recent_folders.insert(
            0,
            RecentFolder {
                path: path.to_path_buf(),
                last_opened: now,
                project: project.map(str::to_owned),
            },
        );
        self.sort_folders();
    }

    /// The `n` most recently opened workspace folders, newest first.
    pub fn recent_folders(&self, n: usize) -> Vec<&RecentFolder> {
        self.recent_folders.iter().take(n).collect()
    }

    /// Newest first (a stable sort, so on equal times the one recorded last stays first), cut
    /// to the limit.
    fn sort_folders(&mut self) {
        self.recent_folders
            .sort_by_key(|f| std::cmp::Reverse(f.last_opened));
        self.recent_folders.truncate(MAX_RECENT_FOLDERS);
    }

    fn enforce_limit(&mut self) {
        while self.records.len() > MAX_RECORDS {
            let oldest = self
                .records
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| r.last_opened)
                .map(|(i, _)| i);
            match oldest {
                Some(i) => {
                    self.records.remove(i);
                }
                None => break,
            }
        }
    }
}

fn parse_db(bytes: &[u8]) -> Result<Loaded<DocStateDb>, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let obj = value
        .as_object()
        .ok_or_else(|| "top level is not an object".to_string())?;
    warn_if_newer("documents", json_version(obj));

    let mut dropped = Vec::new();
    let records = lenient_items(obj.get("records"), "records", &mut dropped);
    let recent_folders = lenient_items(obj.get("recent_folders"), "recent_folders", &mut dropped);
    let mut db = DocStateDb {
        version: FORMAT_VERSION,
        records,
        recent_folders,
    };
    // A hand-edited file may hold a folder twice; newest first, then keep the first of each.
    db.recent_folders
        .sort_by_key(|f| std::cmp::Reverse(f.last_opened));
    let mut seen = std::collections::HashSet::new();
    db.recent_folders.retain(|f| seen.insert(f.path.clone()));
    db.sort_folders();
    // A hand-edited file may hold the same path twice; keep the most recent.
    let mut unique: Vec<DocRecord> = Vec::with_capacity(db.records.len());
    for rec in db.records.drain(..) {
        match unique.iter_mut().find(|r| r.path == rec.path) {
            Some(existing) if existing.last_opened < rec.last_opened => *existing = rec,
            Some(_) => {}
            None => unique.push(rec),
        }
    }
    db.records = unique;
    db.enforce_limit();
    Ok(Loaded {
        value: db,
        dropped: (!dropped.is_empty()).then(|| dropped.join("; ")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Location, ZoomMode};
    use crate::nav::history::Place;
    use crate::store::atomic::bad_path;
    use std::fs;

    fn rec(path: &str, fp: Option<&str>, last_opened: i64) -> DocRecord {
        DocRecord {
            path: PathBuf::from(path),
            fingerprint: fp.map(str::to_string),
            size: 10,
            mtime: 20,
            last_opened,
            view: ViewState::new(PathBuf::from(path)),
            toc_collapsed: Vec::new(),
        }
    }

    fn db_of(recs: Vec<DocRecord>) -> DocStateDb {
        let mut db = DocStateDb::default();
        for r in recs {
            db.record(r);
        }
        db
    }

    #[test]
    fn roundtrip_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.json");
        let mut a = rec("/docs/a.pdf", Some("abc"), 100);
        a.toc_collapsed = vec![vec!["1 Intro".into(), "1.2 Scope".into()]];
        a.view.zoom = ZoomMode::Custom(1.5);
        a.view.anchor = Location::new(7, 10.0, 200.5);
        let db = db_of(vec![a, rec("/docs/b.pdf", None, 50)]);
        db.save(&path).unwrap();
        assert_eq!(DocStateDb::load(&path), db);
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn garbage_gives_empty_db_and_bad_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.json");
        let junk = b"\xde\xad\xbe\xef";
        fs::write(&path, junk).unwrap();
        assert!(DocStateDb::load(&path).is_empty());
        assert_eq!(fs::read(bad_path(&path)).unwrap(), junk);
    }

    #[test]
    fn empty_and_missing_files_give_empty_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.json");
        assert!(DocStateDb::load(&path).is_empty());
        fs::write(&path, "").unwrap();
        assert!(DocStateDb::load(&path).is_empty());
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn unknown_fields_ignored_missing_fields_default_higher_version_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.json");
        fs::write(
            &path,
            r#"{"version": 7, "something_new": {}, "records": [
                 {"path": "/docs/a.pdf", "last_opened": 5, "mystery": 1}]}"#,
        )
        .unwrap();
        let db = DocStateDb::load(&path);
        assert_eq!(db.len(), 1);
        let r = db.lookup(Path::new("/docs/a.pdf"), None).unwrap();
        assert_eq!(r.last_opened, 5);
        assert_eq!(r.fingerprint, None);
        assert_eq!(r.size, 0);
        assert!(r.toc_collapsed.is_empty());
        assert_eq!(r.view, ViewState::new(PathBuf::from("/docs/a.pdf")));
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn unparseable_record_is_dropped_and_original_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.json");
        let text = r#"{"version":1,"records":[{"path":"/docs/a.pdf"},{"no_path":true}]}"#;
        fs::write(&path, text).unwrap();
        let db = DocStateDb::load(&path);
        assert_eq!(db.len(), 1);
        assert_eq!(fs::read_to_string(bad_path(&path)).unwrap(), text);
    }

    #[test]
    fn duplicate_paths_in_a_file_keep_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.json");
        fs::write(
            &path,
            r#"{"records":[{"path":"/a","last_opened":1,"size":1},{"path":"/a","last_opened":9,"size":9}]}"#,
        )
        .unwrap();
        let db = DocStateDb::load(&path);
        assert_eq!(db.len(), 1);
        assert_eq!(db.lookup(Path::new("/a"), None).unwrap().size, 9);
    }

    // ---- lookup ----

    #[test]
    fn lookup_by_path() {
        let db = db_of(vec![
            rec("/docs/a.pdf", Some("aaa"), 1),
            rec("/docs/b.pdf", Some("bbb"), 2),
        ]);
        let r = db.lookup(Path::new("/docs/a.pdf"), None).unwrap();
        assert_eq!(r.path, PathBuf::from("/docs/a.pdf"));
    }

    #[test]
    fn lookup_finds_a_moved_file_by_fingerprint() {
        let db = db_of(vec![rec("/old/a.pdf", Some("aaa"), 1)]);
        let r = db
            .lookup(Path::new("/new/renamed.pdf"), Some("aaa"))
            .unwrap();
        assert_eq!(r.path, PathBuf::from("/old/a.pdf"));
    }

    #[test]
    fn lookup_same_path_with_a_new_fingerprint_is_the_same_document() {
        let db = db_of(vec![rec("/docs/a.pdf", Some("old"), 1)]);
        assert!(db.lookup(Path::new("/docs/a.pdf"), Some("new")).is_some());
    }

    #[test]
    fn lookup_prefers_the_most_recent_of_path_and_fingerprint_matches() {
        let db = db_of(vec![
            rec("/docs/a.pdf", Some("old"), 10),
            rec("/elsewhere/copy.pdf", Some("same"), 20),
        ]);
        let r = db.lookup(Path::new("/docs/a.pdf"), Some("same")).unwrap();
        assert_eq!(r.path, PathBuf::from("/elsewhere/copy.pdf"));

        let db = db_of(vec![
            rec("/docs/a.pdf", Some("old"), 30),
            rec("/elsewhere/copy.pdf", Some("same"), 20),
        ]);
        let r = db.lookup(Path::new("/docs/a.pdf"), Some("same")).unwrap();
        assert_eq!(r.path, PathBuf::from("/docs/a.pdf"));
    }

    #[test]
    fn lookup_tie_goes_to_the_path_match() {
        let db = db_of(vec![
            rec("/elsewhere/copy.pdf", Some("same"), 10),
            rec("/docs/a.pdf", Some("old"), 10),
        ]);
        let r = db.lookup(Path::new("/docs/a.pdf"), Some("same")).unwrap();
        assert_eq!(r.path, PathBuf::from("/docs/a.pdf"));
    }

    #[test]
    fn lookup_without_fingerprint_never_matches_by_fingerprint() {
        let db = db_of(vec![rec("/old/a.pdf", None, 1)]);
        assert!(db.lookup(Path::new("/new/a.pdf"), None).is_none());
        let db = db_of(vec![rec("/old/a.pdf", Some("aaa"), 1)]);
        assert!(db.lookup(Path::new("/new/a.pdf"), None).is_none());
    }

    #[test]
    fn lookup_misses_when_nothing_matches() {
        let db = db_of(vec![rec("/docs/a.pdf", Some("aaa"), 1)]);
        assert!(db.lookup(Path::new("/docs/z.pdf"), Some("zzz")).is_none());
        assert!(
            DocStateDb::default()
                .lookup(Path::new("/x"), Some("y"))
                .is_none()
        );
    }

    // ---- record / recent ----

    #[test]
    fn record_replaces_the_same_path() {
        let mut db = db_of(vec![rec("/docs/a.pdf", Some("old"), 1)]);
        let mut newer = rec("/docs/a.pdf", Some("new"), 2);
        newer.view.anchor = Location::new(3, 0.0, 0.0);
        db.record(newer.clone());
        assert_eq!(db.len(), 1);
        assert_eq!(db.lookup(Path::new("/docs/a.pdf"), None), Some(&newer));
    }

    #[test]
    fn record_caps_at_max_records_dropping_the_least_recent() {
        let mut db = DocStateDb::default();
        // Newest first and oldest first both end with the oldest dropped.
        for i in 0..MAX_RECORDS as i64 + 5 {
            db.record(rec(&format!("/d/{i}.pdf"), None, 1000 + i));
        }
        assert_eq!(db.len(), MAX_RECORDS);
        for i in 0..5 {
            assert!(db.lookup(Path::new(&format!("/d/{i}.pdf")), None).is_none());
        }
        assert!(db.lookup(Path::new("/d/5.pdf"), None).is_some());

        // A record older than everything else is the one that goes.
        db.record(rec("/d/ancient.pdf", None, 1));
        assert_eq!(db.len(), MAX_RECORDS);
        assert!(db.lookup(Path::new("/d/ancient.pdf"), None).is_none());
    }

    #[test]
    fn recent_is_newest_first_and_limited() {
        let db = db_of(vec![
            rec("/a", None, 5),
            rec("/b", None, 30),
            rec("/c", None, 10),
            rec("/d", None, 20),
        ]);
        let paths: Vec<_> = db.recent(3).iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("/b"),
                PathBuf::from("/d"),
                PathBuf::from("/c")
            ]
        );
        assert_eq!(db.recent(100).len(), 4);
        assert!(db.recent(0).is_empty());
    }

    #[test]
    fn saving_trims_history_to_fifty_and_keeps_the_current_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.json");
        let doc = PathBuf::from("/docs/a.pdf");
        let place = |p: u32| Place {
            doc: doc.clone(),
            loc: Location::new(p, 0.0, 0.0),
        };
        let mut r = rec("/docs/a.pdf", None, 1);
        for i in 0..199 {
            r.view.history.jump(place(i), place(i + 1));
        }
        assert_eq!(r.view.history.len(), 200);
        let current = r.view.history.current().clone();
        db_of(vec![r]).save(&path).unwrap();

        let db = DocStateDb::load(&path);
        let back = db.lookup(Path::new("/docs/a.pdf"), None).unwrap();
        assert_eq!(back.view.history.len(), 50);
        assert_eq!(back.view.history.current(), &current);
    }

    #[test]
    fn recent_folders_are_newest_first_and_unique() {
        let mut db = DocStateDb::default();
        db.record_folder(Path::new("/w/a"), 10);
        db.record_folder(Path::new("/w/b"), 20);
        db.record_folder(Path::new("/w/c"), 30);
        db.record_folder(Path::new("/w/a"), 40);
        let paths: Vec<&Path> = db
            .recent_folders(10)
            .iter()
            .map(|f| f.path.as_path())
            .collect();
        assert_eq!(
            paths,
            [Path::new("/w/a"), Path::new("/w/c"), Path::new("/w/b")]
        );
        assert_eq!(db.recent_folders(10)[0].last_opened, 40);
        assert_eq!(db.recent_folders(2).len(), 2);
        assert!(db.recent_folders(0).is_empty());
    }

    #[test]
    fn recent_folders_on_equal_times_put_the_last_recorded_first() {
        let mut db = DocStateDb::default();
        db.record_folder(Path::new("/w/a"), 5);
        db.record_folder(Path::new("/w/b"), 5);
        assert_eq!(db.recent_folders(1)[0].path, Path::new("/w/b"));
    }

    #[test]
    fn recent_folders_remember_the_project_name_and_old_files_read_as_plain_folders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.json");
        let mut db = DocStateDb::default();
        db.record_project_folder(Path::new("/w/proj"), Some("Specs"), 20);
        db.record_folder(Path::new("/w/plain"), 10);
        db.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("\"project\"").count(), 1, "{text}");
        let loaded = DocStateDb::load(&path);
        let folders = loaded.recent_folders(5);
        assert_eq!(folders[0].project.as_deref(), Some("Specs"));
        assert_eq!(folders[1].project, None);

        std::fs::write(
            &path,
            r#"{"version": 1, "recent_folders": [{"path": "/w/a", "last_opened": 5}]}"#,
        )
        .unwrap();
        let old = DocStateDb::load(&path);
        assert_eq!(old.recent_folders(5)[0].path, Path::new("/w/a"));
        assert_eq!(old.recent_folders(5)[0].project, None);
    }

    #[test]
    fn recent_folders_are_capped() {
        let mut db = DocStateDb::default();
        for i in 0..(MAX_RECENT_FOLDERS as i64 + 10) {
            db.record_folder(&PathBuf::from(format!("/w/{i}")), i);
        }
        let all = db.recent_folders(usize::MAX);
        assert_eq!(all.len(), MAX_RECENT_FOLDERS);
        assert_eq!(all[0].path, Path::new("/w/59"));
        assert_eq!(all[MAX_RECENT_FOLDERS - 1].path, Path::new("/w/10"));
    }

    #[test]
    fn recent_folders_roundtrip_and_old_files_have_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.json");
        let mut db = db_of(vec![rec("/docs/a.pdf", None, 1)]);
        db.record_folder(Path::new("/w/a"), 10);
        db.record_folder(Path::new("/w/b"), 20);
        db.save(&path).unwrap();
        let loaded = DocStateDb::load(&path);
        assert_eq!(loaded, db);
        assert_eq!(loaded.recent_folders(5).len(), 2);

        fs::write(
            &path,
            r#"{"version": 1, "records": [{"path": "/docs/a.pdf"}]}"#,
        )
        .unwrap();
        let old = DocStateDb::load(&path);
        assert_eq!(old.len(), 1);
        assert!(old.recent_folders(5).is_empty());
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn a_file_with_recent_folders_still_reads_by_the_old_shape() {
        #[derive(Deserialize)]
        struct OldDb {
            records: Vec<DocRecord>,
        }
        let mut db = db_of(vec![rec("/docs/a.pdf", None, 1)]);
        db.record_folder(Path::new("/w/a"), 10);
        let json = serde_json::to_string(&db).unwrap();
        let old: OldDb = serde_json::from_str(&json).unwrap();
        assert_eq!(old.records.len(), 1);
    }

    #[test]
    fn duplicate_and_unordered_recent_folders_in_a_hand_edited_file_are_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documents.json");
        fs::write(
            &path,
            r#"{"version": 1, "recent_folders": [
                 {"path": "/w/a", "last_opened": 5}, {"path": "/w/b", "last_opened": 9},
                 {"path": "/w/a", "last_opened": 1}, {"nope": 1}]}"#,
        )
        .unwrap();
        let db = DocStateDb::load(&path);
        let paths: Vec<&Path> = db
            .recent_folders(10)
            .iter()
            .map(|f| f.path.as_path())
            .collect();
        assert_eq!(paths, [Path::new("/w/b"), Path::new("/w/a")]);
        assert!(
            bad_path(&path).exists(),
            "the dropped item keeps the original"
        );
    }
}
