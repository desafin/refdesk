//! Tests of projects (`docs/ARCHITECTURE.md` §4.15): the project file, importing, the trash and
//! the file operations. Everything happens in temporary directories. Waiting is done on
//! conditions, woken by the job's notifier, with generous deadlines.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use refdesk_core::project::{
    self, PROJECT_FILE, clean_name, create_folder, empty_trash, folder_name, is_project,
    list_trash, restore, trash_at,
};
use refdesk_core::workspace::scan;
use refdesk_core::{
    ConflictChoice, ConflictPolicy, ImportItem, ImportJob, ImportOutcome, ImportSnapshot,
    ImportState, Notifier, Project, ProjectError, ScanLimits, SkipReason, SourceSurvey,
    SurveyState,
};

const TIMEOUT: Duration = Duration::from_secs(60);

fn write(path: &Path, bytes: &[u8]) {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

/// A notifier that wakes a waiting test.
fn wake() -> (Notifier, Receiver<()>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let notifier: Notifier = Arc::new(move || {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(());
        }
    });
    (notifier, rx)
}

fn wait_for(rx: &Receiver<()>, what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !check() {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "timed out waiting for {what}");
        match rx.recv_timeout(left) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {what}"),
        }
    }
}

fn run_import(
    root: &Path,
    sources: Vec<PathBuf>,
    into: &str,
    policy: ConflictPolicy,
) -> Arc<ImportSnapshot> {
    let (notifier, rx) = wake();
    let job = ImportJob::start(root.to_path_buf(), sources, into.into(), policy, notifier);
    wait_for(&rx, "the import", || job.snapshot().finished());
    job.snapshot()
}

fn outcomes(snap: &ImportSnapshot) -> Vec<(String, ImportOutcome)> {
    snap.results
        .iter()
        .map(|r| (r.dest.to_string_lossy().into_owned(), r.outcome.clone()))
        .collect()
}

/// Every file below `dir` (relative, sorted), hidden ones included.
fn files_below(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn new_project(dir: &Path) -> PathBuf {
    let root = dir.join("proj");
    Project::create_at(&root, "Proj", 1_000).unwrap();
    root
}

const ASK: ConflictPolicy = ConflictPolicy::Ask;

// ---------------------------------------------------------------------------------------------
// The project file

#[test]
fn a_new_project_has_a_file_with_its_version_name_and_time() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Specs");
    let p = Project::create_at(&root, "  Network specs  ", 1_764_201_600).unwrap();
    assert!(is_project(&root));
    assert_eq!(p.name(), "Network specs");
    let text = fs::read_to_string(root.join(PROJECT_FILE)).unwrap();
    assert!(text.contains("version = 1"), "{text}");
    assert!(text.contains("name = \"Network specs\""), "{text}");
    assert!(text.contains("created = 2025-11-27T00:00:00Z"), "{text}");

    let opened = Project::open(&root).unwrap();
    assert_eq!(opened.name(), "Network specs");
    assert_eq!(opened.created(), Some(1_764_201_600));
    assert_eq!(opened.warning(), None);
}

#[test]
fn renaming_keeps_entries_the_project_file_does_not_know() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    let file = root.join(PROJECT_FILE);
    let mut text = fs::read_to_string(&file).unwrap();
    text.push_str("future = 3\n\n[pretranslate]\nenabled = true\nmodel = \"x\"\n");
    fs::write(&file, text).unwrap();

    let mut p = Project::open(&root).unwrap();
    assert_eq!(p.entry("future").and_then(|v| v.as_integer()), Some(3));
    p.rename("Renamed").unwrap();
    assert_eq!(p.name(), "Renamed");

    let reopened = Project::open(&root).unwrap();
    assert_eq!(reopened.name(), "Renamed");
    assert_eq!(reopened.created(), Some(1_000));
    let table = reopened.entry("pretranslate").and_then(|v| v.as_table());
    assert_eq!(
        table
            .and_then(|t| t.get("enabled"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        reopened.entry("future").and_then(|v| v.as_integer()),
        Some(3)
    );
    assert!(matches!(p.rename("   "), Err(ProjectError::EmptyName)));
}

#[test]
fn a_damaged_project_file_still_opens_with_the_folder_name_and_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("My Docs");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(PROJECT_FILE), b"name = = broken [[").unwrap();
    let p = Project::open(&root).unwrap();
    assert_eq!(p.name(), "My Docs");
    assert!(p.warning().is_some());
    assert_eq!(
        fs::read(root.join(PROJECT_FILE)).unwrap(),
        b"name = = broken [["
    );
    assert!(!root.join(format!("{PROJECT_FILE}.bad")).exists());

    // A file without a name is a project too.
    fs::write(root.join(PROJECT_FILE), b"version = 1\n").unwrap();
    let p = Project::open(&root).unwrap();
    assert_eq!(p.name(), "My Docs");
    assert!(p.warning().is_some());

    // An empty file (what the agreed `is_project` test writes) as well.
    fs::write(root.join(PROJECT_FILE), b"").unwrap();
    assert_eq!(Project::open(&root).unwrap().name(), "My Docs");

    assert!(matches!(
        Project::open(&dir.path().join("nothing")),
        Err(ProjectError::NotProject(_))
    ));
}

#[test]
fn renaming_a_damaged_project_keeps_a_copy_of_the_old_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("p");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(PROJECT_FILE), b"[[[").unwrap();
    let mut p = Project::open(&root).unwrap();
    p.rename("Fixed").unwrap();
    assert_eq!(Project::open(&root).unwrap().name(), "Fixed");
    assert_eq!(
        fs::read(root.join(format!("{PROJECT_FILE}.bad"))).unwrap(),
        b"[[["
    );
}

#[test]
fn names_are_cleaned_for_the_project_and_for_its_folder() {
    assert_eq!(clean_name("  a\tb\n ").unwrap(), "ab");
    assert!(matches!(clean_name(" \n "), Err(ProjectError::EmptyName)));

    assert_eq!(folder_name("Network specs"), "Network specs");
    assert_eq!(folder_name("  a/b\u{7}c  "), "abc");
    assert_eq!(folder_name("..hidden"), "hidden");
    assert_eq!(folder_name(". x"), "x");
    assert_eq!(folder_name("..."), "");
    assert_eq!(folder_name("//"), "");
    assert_eq!(folder_name("한글 문서"), "한글 문서");
    let long = "가".repeat(300);
    let cut = folder_name(&long);
    assert!(cut.chars().count() <= project::MAX_FOLDER_NAME_CHARS);
    assert!(cut.len() <= 200);
    assert!(cut.chars().all(|c| c == '가'));
}

#[test]
fn a_folder_that_is_already_a_project_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    assert!(matches!(
        Project::create(&root, "Again"),
        Err(ProjectError::AlreadyProject(_))
    ));
    assert!(matches!(
        Project::create(&dir.path().join("x"), " "),
        Err(ProjectError::EmptyName)
    ));
    assert!(!dir.path().join("x").exists());
}

#[test]
fn a_folder_with_pdfs_becomes_a_project_with_its_files_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("library");
    write(&root.join("a.pdf"), b"a");
    write(&root.join("sub/b.pdf"), b"b");
    let p = Project::create(&root, "Library").unwrap();
    assert_eq!(p.name(), "Library");
    assert_eq!(
        files_below(&root),
        ["a.pdf", PROJECT_FILE, "sub/b.pdf"]
            .map(String::from)
            .to_vec()
    );
    let index = scan(&root, &ScanLimits::default(), &AtomicBool::new(false)).unwrap();
    assert_eq!(index.file_count(), 2);
}

// ---------------------------------------------------------------------------------------------
// Importing

#[test]
fn several_files_are_copied_into_the_project_with_their_times() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    let src = dir.path().join("src");
    write(&src.join("one.pdf"), b"one");
    write(&src.join("Two.PDF"), b"two");
    let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    fs::File::options()
        .write(true)
        .open(src.join("one.pdf"))
        .unwrap()
        .set_modified(old)
        .unwrap();

    let snap = run_import(
        &root,
        vec![src.join("one.pdf"), src.join("Two.PDF")],
        "",
        ASK,
    );
    assert_eq!(snap.state, ImportState::Done);
    assert_eq!((snap.total, snap.done), (2, 2));
    assert_eq!(
        outcomes(&snap),
        vec![
            ("one.pdf".into(), ImportOutcome::Imported),
            ("Two.PDF".into(), ImportOutcome::Imported)
        ]
    );
    assert_eq!(fs::read(root.join("one.pdf")).unwrap(), b"one");
    assert_eq!(fs::read(root.join("Two.PDF")).unwrap(), b"two");
    assert_eq!(snap.bytes_copied, 6);
    assert_eq!(
        fs::metadata(root.join("one.pdf"))
            .unwrap()
            .modified()
            .unwrap(),
        old
    );
    // The originals are untouched.
    assert_eq!(fs::read(src.join("one.pdf")).unwrap(), b"one");
}

#[test]
fn a_folder_brings_its_pdfs_with_their_structure_and_counts_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    let src = dir.path().join("Manuals");
    write(&src.join("a.pdf"), b"a");
    write(&src.join("notes.txt"), b"n");
    write(&src.join("deep/er/b.pdf"), b"b");
    write(&src.join("deep/pic.png"), b"p");
    write(&src.join(".hidden/c.pdf"), b"c");
    write(&src.join(".d.pdf"), b"d");
    let snap = run_import(&root, vec![src.clone()], "", ASK);
    assert_eq!(snap.total, 2);
    assert_eq!(snap.not_pdf, 2);
    assert_eq!(
        files_below(&root.join("Manuals")),
        vec!["a.pdf".to_string(), "deep/er/b.pdf".to_string()]
    );
}

#[test]
fn files_can_go_into_a_sub_folder_of_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    write(&dir.path().join("x.pdf"), b"x");
    let snap = run_import(&root, vec![dir.path().join("x.pdf")], "refs/net", ASK);
    assert_eq!(
        outcomes(&snap),
        vec![("refs/net/x.pdf".into(), ImportOutcome::Imported)]
    );
    assert!(root.join("refs/net/x.pdf").is_file());

    let bad = run_import(&root, vec![dir.path().join("x.pdf")], "../out", ASK);
    assert!(matches!(bad.results[0].outcome, ImportOutcome::Failed(_)));
    assert!(!dir.path().join("out").exists());
}

#[test]
fn a_file_with_content_the_project_has_is_skipped_whatever_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    write(&root.join("kept/original.pdf"), b"same bytes");
    write(&root.join("other.pdf"), b"same size!"); // same size, other content
    write(&dir.path().join("copy.pdf"), b"same bytes");
    write(&dir.path().join("twin1.pdf"), b"twin");
    write(&dir.path().join("twin2.pdf"), b"twin");
    let snap = run_import(
        &root,
        vec![
            dir.path().join("copy.pdf"),
            dir.path().join("twin1.pdf"),
            dir.path().join("twin2.pdf"),
        ],
        "",
        ASK,
    );
    assert_eq!(
        outcomes(&snap),
        vec![
            (
                "copy.pdf".into(),
                ImportOutcome::AlreadyThere {
                    existing: PathBuf::from("kept/original.pdf")
                }
            ),
            ("twin1.pdf".into(), ImportOutcome::Imported),
            // A file copied by the same import counts as already there.
            (
                "twin2.pdf".into(),
                ImportOutcome::AlreadyThere {
                    existing: PathBuf::from("twin1.pdf")
                }
            ),
        ]
    );
    assert!(!root.join("copy.pdf").exists());
}

/// A project with `doc.pdf` ("old") and sources `a/doc.pdf`, `b/doc.pdf`, `c/doc.pdf` with other
/// contents.
fn clash_setup(dir: &Path) -> (PathBuf, Vec<PathBuf>) {
    let root = new_project(dir);
    write(&root.join("doc.pdf"), b"old");
    let sources: Vec<PathBuf> = ["a", "b", "c"]
        .iter()
        .map(|d| {
            let p = dir.join(d).join("doc.pdf");
            write(&p, format!("new {d}").as_bytes());
            p
        })
        .collect();
    (root, sources)
}

fn waiting(snap: &ImportSnapshot) -> bool {
    matches!(snap.state, ImportState::Waiting(_))
}

#[test]
fn a_name_clash_stops_the_import_until_it_is_settled_one_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let (root, sources) = clash_setup(dir.path());
    let (notifier, rx) = wake();
    let job = ImportJob::start(root.clone(), sources.clone(), "".into(), ASK, notifier);

    wait_for(&rx, "the first clash", || waiting(&job.snapshot()));
    let snap = job.snapshot();
    let ImportState::Waiting(conflict) = &snap.state else {
        unreachable!()
    };
    assert_eq!(conflict.source, sources[0]);
    assert_eq!(conflict.dest, PathBuf::from("doc.pdf"));
    assert_eq!((conflict.source_size, conflict.existing_size), (5, 3));
    assert!(conflict.source_mtime.is_some() && conflict.existing_mtime.is_some());
    assert_eq!(snap.current.as_deref(), Some(sources[0].as_path()));
    job.resolve(ConflictChoice::KeepBoth, false);

    wait_for(&rx, "the second clash", || {
        let s = job.snapshot();
        waiting(&s) && s.done == 1
    });
    job.resolve(ConflictChoice::Skip, false);
    wait_for(&rx, "the third clash", || {
        let s = job.snapshot();
        waiting(&s) && s.done == 2
    });
    job.resolve(ConflictChoice::Replace, false);
    wait_for(&rx, "the end", || job.snapshot().finished());

    let snap = job.snapshot();
    assert_eq!(
        outcomes(&snap),
        vec![
            ("doc (2).pdf".into(), ImportOutcome::Imported),
            (
                "doc.pdf".into(),
                ImportOutcome::Skipped(SkipReason::Declined)
            ),
            ("doc.pdf".into(), ImportOutcome::Replaced),
        ]
    );
    assert_eq!(fs::read(root.join("doc (2).pdf")).unwrap(), b"new a");
    assert_eq!(fs::read(root.join("doc.pdf")).unwrap(), b"new c");
    // The replaced file is in the project's trash.
    let trash = list_trash(&root).unwrap();
    assert_eq!(trash.len(), 1);
    assert_eq!(trash[0].rel_path, PathBuf::from("doc.pdf"));
    let back = restore(&root, &trash[0].id).unwrap();
    assert_eq!(back, PathBuf::from("doc (3).pdf"));
    assert_eq!(fs::read(root.join("doc (3).pdf")).unwrap(), b"old");
}

#[test]
fn apply_to_all_settles_the_remaining_clashes() {
    for (choice, expected) in [
        (
            ConflictChoice::KeepBoth,
            vec!["doc (2).pdf", "doc (3).pdf", "doc (4).pdf"],
        ),
        (ConflictChoice::Skip, vec!["doc.pdf", "doc.pdf", "doc.pdf"]),
        (
            ConflictChoice::Replace,
            vec!["doc.pdf", "doc.pdf", "doc.pdf"],
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (root, sources) = clash_setup(dir.path());
        let (notifier, rx) = wake();
        let job = ImportJob::start(root.clone(), sources, "".into(), ASK, notifier);
        wait_for(&rx, "the first clash", || waiting(&job.snapshot()));
        job.resolve(choice, true);
        wait_for(&rx, "the end", || job.snapshot().finished());
        let snap = job.snapshot();
        let dests: Vec<String> = outcomes(&snap).into_iter().map(|(d, _)| d).collect();
        assert_eq!(dests, expected, "{choice:?}");
        if choice == ConflictChoice::Replace {
            assert_eq!(fs::read(root.join("doc.pdf")).unwrap(), b"new c");
            assert_eq!(list_trash(&root).unwrap().len(), 3);
        }
    }
}

#[test]
fn a_fixed_policy_never_waits() {
    let dir = tempfile::tempdir().unwrap();
    let (root, sources) = clash_setup(dir.path());
    let snap = run_import(
        &root,
        sources,
        "",
        ConflictPolicy::Always(ConflictChoice::KeepBoth),
    );
    assert_eq!(snap.count(|o| *o == ImportOutcome::Imported), 3);
    assert!(root.join("doc (4).pdf").is_file());
}

#[test]
fn sources_inside_the_project_are_not_copied_onto_themselves() {
    let dir = tempfile::tempdir().unwrap();
    let holder = dir.path().join("holder");
    let root = new_project(&holder);
    write(&root.join("in.pdf"), b"in");
    write(&root.join("sub/also.pdf"), b"also");
    let snap = run_import(
        &root,
        vec![root.join("in.pdf"), root.join("sub")],
        "elsewhere",
        ASK,
    );
    assert_eq!(snap.total, 0);
    assert!(
        snap.results
            .iter()
            .all(|r| r.outcome == ImportOutcome::Skipped(SkipReason::InsideProject))
    );
    assert!(!root.join("elsewhere").exists());

    // A folder that holds the project brings its other files only.
    write(&holder.join("outside.pdf"), b"out");
    let snap = run_import(&root, vec![holder.clone()], "all", ASK);
    assert_eq!(
        outcomes(&snap),
        vec![("all/holder/outside.pdf".into(), ImportOutcome::Imported)]
    );
}

#[test]
fn a_missing_or_unreadable_source_fails_and_the_rest_go_on() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    write(&dir.path().join("locked.pdf"), b"secret");
    fs::set_permissions(
        dir.path().join("locked.pdf"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let readable_anyway = fs::read(dir.path().join("locked.pdf")).is_ok(); // running as root
    write(&dir.path().join("ok.pdf"), b"ok");
    write(&dir.path().join("note.txt"), b"t");
    let snap = run_import(
        &root,
        vec![
            dir.path().join("gone.pdf"),
            dir.path().join("locked.pdf"),
            dir.path().join("note.txt"),
            dir.path().join("ok.pdf"),
        ],
        "",
        ASK,
    );
    let got = outcomes(&snap);
    assert!(matches!(got[0].1, ImportOutcome::Failed(_)), "{got:?}");
    assert_eq!(
        got[1],
        (
            "note.txt".into(),
            ImportOutcome::Skipped(SkipReason::NotPdf)
        )
    );
    if !readable_anyway {
        assert!(matches!(got[2].1, ImportOutcome::Failed(_)), "{got:?}");
        assert!(!root.join("locked.pdf").exists());
    }
    assert_eq!(got[3], ("ok.pdf".into(), ImportOutcome::Imported));
    assert_eq!(snap.state, ImportState::Done);
    assert!(
        files_below(&root)
            .iter()
            .all(|f| !f.ends_with(".importing"))
    );
    fs::set_permissions(
        dir.path().join("locked.pdf"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
}

/// A notifier that runs `on_progress` (once, on the import thread) with the job the first time
/// some bytes were copied, and records every `bytes_copied` it sees.
struct Probe {
    job: Arc<OnceLock<Arc<ImportJob>>>,
    seen: Arc<Mutex<Vec<u64>>>,
}

fn probe(
    on_progress: impl Fn(&ImportJob) + Send + Sync + 'static,
) -> (Probe, Notifier, Receiver<()>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let job: Arc<OnceLock<Arc<ImportJob>>> = Arc::new(OnceLock::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let fired = AtomicBool::new(false);
    let (job2, seen2) = (Arc::clone(&job), Arc::clone(&seen));
    let notifier: Notifier = Arc::new(move || {
        if let Some(job) = job2.get() {
            let bytes = job.snapshot().bytes_copied;
            seen2.lock().unwrap().push(bytes);
            if bytes > 0 && !fired.swap(true, Ordering::SeqCst) {
                on_progress(job);
            }
        }
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(());
        }
    });
    (Probe { job, seen }, notifier, rx)
}

/// `n` MiB of bytes that differ from file to file.
fn big(n: usize, seed: u8) -> Vec<u8> {
    (0..n << 20)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}

#[test]
fn a_cancelled_import_leaves_no_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    write(&dir.path().join("big.pdf"), &big(6, 1));
    write(&dir.path().join("after.pdf"), b"after");
    // An import that waits until the probe is in place, so the first progress is seen.
    write(&root.join("big.pdf"), b"clash");
    let (probe, notifier, rx) = probe(|job| job.cancel());
    let job = Arc::new(ImportJob::start(
        root.clone(),
        vec![dir.path().join("big.pdf"), dir.path().join("after.pdf")],
        "".into(),
        ASK,
        notifier,
    ));
    probe.job.set(Arc::clone(&job)).unwrap();
    wait_for(&rx, "the clash", || waiting(&job.snapshot()));
    job.resolve(ConflictChoice::KeepBoth, false);
    wait_for(&rx, "the cancel", || job.snapshot().finished());

    let snap = job.snapshot();
    assert_eq!(snap.state, ImportState::Cancelled);
    assert!(snap.results.is_empty());
    assert_eq!(
        files_below(&root),
        vec!["big.pdf".to_string(), PROJECT_FILE.to_string()]
    );
}

#[test]
fn dropping_an_import_cancels_it_without_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let (root, sources) = clash_setup(dir.path());
    let calls = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let counter = Arc::clone(&calls);
    let notifier: Notifier = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        let _ = tx.lock().unwrap().send(());
    });
    let job = ImportJob::start(root.clone(), sources, "".into(), ASK, Arc::clone(&notifier));
    wait_for(&rx, "the clash", || waiting(&job.snapshot()));
    let before = calls.load(Ordering::SeqCst);
    drop(job);
    // The thread ends (it held the only other reference to the notifier).
    let deadline = Instant::now() + TIMEOUT;
    while Arc::strong_count(&notifier) > 1 {
        assert!(Instant::now() < deadline, "the import thread did not end");
        std::thread::yield_now();
    }
    assert_eq!(calls.load(Ordering::SeqCst), before);
    assert_eq!(fs::read(root.join("doc.pdf")).unwrap(), b"old");
    assert!(
        files_below(&root)
            .iter()
            .all(|f| !f.ends_with(".importing"))
    );
}

#[test]
fn a_big_file_reports_progress_several_times_and_its_temporary_name_is_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    let data = big(5, 7);
    write(&dir.path().join("big.pdf"), &data);
    write(&root.join("big.pdf"), b"clash");
    let listed = Arc::new(Mutex::new(None));
    let listed2 = Arc::clone(&listed);
    let scan_root = root.clone();
    let (probe, notifier, rx) = probe(move |_| {
        // While the copy runs: the temporary file exists but the scan does not list it.
        let temps: Vec<String> = files_below(&scan_root)
            .into_iter()
            .filter(|f| f.ends_with(".importing"))
            .collect();
        let index = scan(&scan_root, &ScanLimits::default(), &AtomicBool::new(false)).unwrap();
        let names: Vec<String> = index
            .entries
            .iter()
            .map(|e| e.rel_path.to_string_lossy().into_owned())
            .collect();
        *listed2.lock().unwrap() = Some((temps, names));
    });
    let job = Arc::new(ImportJob::start(
        root.clone(),
        vec![dir.path().join("big.pdf")],
        "".into(),
        ASK,
        notifier,
    ));
    probe.job.set(Arc::clone(&job)).unwrap();
    wait_for(&rx, "the clash", || waiting(&job.snapshot()));
    job.resolve(ConflictChoice::KeepBoth, false);
    wait_for(&rx, "the end", || job.snapshot().finished());

    let mut steps: Vec<u64> = probe.seen.lock().unwrap().clone();
    steps.dedup();
    let progress: Vec<u64> = steps.into_iter().filter(|b| *b > 0).collect();
    assert!(progress.len() >= 4, "{progress:?}");
    assert_eq!(progress.last().copied(), Some(data.len() as u64));
    let (temps, names) = listed.lock().unwrap().clone().expect("probed");
    assert_eq!(temps, vec![".big (2).pdf.importing".to_string()]);
    assert_eq!(names, vec!["big.pdf".to_string()]);
    assert_eq!(fs::read(root.join("big (2).pdf")).unwrap(), data);
}

#[test]
fn items_with_their_own_targets_are_imported_as_given() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    write(&dir.path().join("s/a.pdf"), b"a");
    write(&dir.path().join("s/x/b.pdf"), b"b");
    let (notifier, rx) = wake();
    let job = ImportJob::start_items(
        root.clone(),
        vec![
            ImportItem {
                source: dir.path().join("s/a.pdf"),
                dest: "flat/a.pdf".into(),
            },
            ImportItem {
                source: dir.path().join("s/x/b.pdf"),
                dest: "flat/b.pdf".into(),
            },
            ImportItem {
                source: dir.path().join("s/a.pdf"),
                dest: "../escape.pdf".into(),
            },
        ],
        ASK,
        notifier,
    );
    wait_for(&rx, "the import", || job.snapshot().finished());
    let snap = job.snapshot();
    assert_eq!(snap.total, 2);
    assert!(matches!(snap.results[0].outcome, ImportOutcome::Failed(_)));
    assert_eq!(
        files_below(&root.join("flat")),
        vec!["a.pdf".to_string(), "b.pdf".to_string()]
    );
}

#[test]
fn a_survey_lists_the_pdfs_with_sizes_and_finds_what_the_project_has() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    write(&root.join("have.pdf"), b"same");
    let src = dir.path().join("src");
    write(&src.join("a.pdf"), b"same");
    write(&src.join("b.pdf"), b"diff");
    write(&src.join("sub/c.pdf"), b"longer one");
    write(&src.join("skip.txt"), b"x");
    write(&src.join(".hidden.pdf"), b"x");
    let (notifier, rx) = wake();
    let survey = SourceSurvey::start(root.clone(), src.clone(), notifier);
    wait_for(&rx, "the survey", || {
        survey.snapshot().state == SurveyState::Done
    });
    let snap = survey.snapshot();
    let files: Vec<(String, u64)> = snap
        .files
        .iter()
        .map(|f| (f.rel.to_string_lossy().into_owned(), f.size))
        .collect();
    assert_eq!(
        files,
        vec![
            ("sub/c.pdf".to_string(), 10),
            ("a.pdf".to_string(), 4),
            ("b.pdf".to_string(), 4)
        ]
    );
    assert_eq!(snap.existing.len(), 1);
    assert_eq!(snap.existing.get(&1), Some(&PathBuf::from("have.pdf")));

    let (notifier, rx) = wake();
    let missing = SourceSurvey::start(root, dir.path().join("nope"), notifier);
    wait_for(&rx, "the failure", || {
        matches!(missing.snapshot().state, SurveyState::Failed(_))
    });
}

// ---------------------------------------------------------------------------------------------
// The trash and the file operations

#[test]
fn the_trash_takes_files_and_folders_and_gives_them_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    write(&root.join("a.pdf"), b"first");
    write(&root.join("dir/b.pdf"), b"b");

    let first = trash_at(&root, Path::new("a.pdf"), 100).unwrap();
    assert!(!root.join("a.pdf").exists());
    write(&root.join("a.pdf"), b"second");
    let second = trash_at(&root, Path::new("a.pdf"), 100).unwrap();
    assert_ne!(first.id, second.id);
    let folder = trash_at(&root, Path::new("dir"), 200).unwrap();
    assert!(folder.is_dir);
    assert!(!root.join("dir").exists());

    let listed = list_trash(&root).unwrap();
    assert_eq!(
        listed.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        vec![folder.id.clone(), second.id.clone(), first.id.clone()]
    );
    assert_eq!(listed[1].rel_path, PathBuf::from("a.pdf"));
    // The trash is hidden from the workspace scan.
    let index = scan(&root, &ScanLimits::default(), &AtomicBool::new(false)).unwrap();
    assert_eq!(index.file_count(), 0);

    assert_eq!(restore(&root, &first.id).unwrap(), PathBuf::from("a.pdf"));
    assert_eq!(fs::read(root.join("a.pdf")).unwrap(), b"first");
    assert_eq!(
        restore(&root, &second.id).unwrap(),
        PathBuf::from("a (2).pdf")
    );
    assert_eq!(fs::read(root.join("a (2).pdf")).unwrap(), b"second");
    assert_eq!(restore(&root, &folder.id).unwrap(), PathBuf::from("dir"));
    assert!(root.join("dir/b.pdf").is_file());
    assert!(list_trash(&root).unwrap().is_empty());
    assert!(matches!(
        restore(&root, &first.id),
        Err(ProjectError::Missing(_))
    ));
    assert!(matches!(
        restore(&root, "../x"),
        Err(ProjectError::Missing(_))
    ));

    trash_at(&root, Path::new("a.pdf"), 300).unwrap();
    empty_trash(&root).unwrap();
    assert!(list_trash(&root).unwrap().is_empty());
    assert!(!root.join("a.pdf").exists());
    assert!(matches!(
        trash_at(&root, Path::new("a.pdf"), 400),
        Err(ProjectError::Missing(_))
    ));
}

#[test]
fn folders_are_made_and_entries_renamed_only_inside_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let root = new_project(dir.path());
    write(&root.join("a.pdf"), b"a");
    write(&root.join("b.pdf"), b"b");

    assert_eq!(
        create_folder(&root, Path::new("new/inner")).unwrap(),
        root.join("new/inner")
    );
    assert!(root.join("new/inner").is_dir());
    assert!(matches!(
        create_folder(&root, Path::new("new")),
        Err(ProjectError::Exists(_))
    ));

    assert_eq!(
        project::rename(&root, Path::new("a.pdf"), Path::new("new/c.pdf")).unwrap(),
        root.join("new/c.pdf")
    );
    assert!(root.join("new/c.pdf").is_file() && !root.join("a.pdf").exists());
    assert!(matches!(
        project::rename(&root, Path::new("b.pdf"), Path::new("new/c.pdf")),
        Err(ProjectError::Exists(_))
    ));
    assert!(matches!(
        project::rename(&root, Path::new("gone.pdf"), Path::new("x.pdf")),
        Err(ProjectError::Missing(_))
    ));
    assert!(matches!(
        project::rename(&root, Path::new("new"), Path::new("new/inner/new")),
        Err(ProjectError::Protected(_))
    ));
    for bad in ["../b.pdf", "/tmp/b.pdf", "x/../../b.pdf"] {
        assert!(
            matches!(
                project::rename(&root, Path::new("b.pdf"), Path::new(bad)),
                Err(ProjectError::Outside(_))
            ),
            "{bad}"
        );
        assert!(matches!(
            create_folder(&root, Path::new(bad)),
            Err(ProjectError::Outside(_))
        ));
        assert!(matches!(
            trash_at(&root, Path::new(bad), 1),
            Err(ProjectError::Outside(_))
        ));
    }
    assert!(matches!(
        project::rename(&root, Path::new(".refdesk"), Path::new("data")),
        Err(ProjectError::Protected(_))
    ));
    assert!(root.join("b.pdf").is_file());
    assert!(!dir.path().join("b.pdf").exists());
}
