//! Tests of the workspace scan, its background runner and the watcher
//! (`docs/ARCHITECTURE.md` §4.13). Everything happens in temporary directories. Waiting is done
//! on conditions with generous deadlines; the only fixed waits are the ones that check that
//! something does *not* happen.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use refdesk_core::workspace::scan;
use refdesk_core::{
    EntryKind, Notifier, ScanLimits, WorkspaceIndex, WorkspaceScan, WorkspaceWatcher, natural_cmp,
};

/// Deadline for anything a background thread does.
const TIMEOUT: Duration = Duration::from_secs(60);

fn touch(path: &Path) {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(path, b"").unwrap();
}

fn touch_all(root: &Path, names: &[&str]) {
    for name in names {
        touch(&root.join(name));
    }
}

fn run_scan(root: &Path) -> WorkspaceIndex {
    scan(root, &ScanLimits::default(), &AtomicBool::new(false)).unwrap()
}

/// The entries as `dir/` and `dir/file.pdf` paths in index order.
fn listing(index: &WorkspaceIndex) -> Vec<String> {
    index
        .entries
        .iter()
        .map(|e| {
            let path = e.rel_path.to_string_lossy().into_owned();
            if e.kind == EntryKind::Dir {
                format!("{path}/")
            } else {
                path
            }
        })
        .collect()
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

// ---------------------------------------------------------------------------------------------
// scan

#[test]
fn scan_lists_folders_first_then_files_in_natural_order_before_their_children() {
    let dir = tempfile::tempdir().unwrap();
    touch_all(
        dir.path(),
        &[
            "a10.pdf",
            "a2.pdf",
            "A1.pdf",
            "Zeta/b.pdf",
            "alpha/x.pdf",
            "alpha/sub/deep.pdf",
            "alpha/sub/Deep2.pdf",
        ],
    );
    let index = run_scan(dir.path());
    assert_eq!(
        listing(&index),
        [
            "alpha/",
            "alpha/sub/",
            "alpha/sub/deep.pdf",
            "alpha/sub/Deep2.pdf",
            "alpha/x.pdf",
            "Zeta/",
            "Zeta/b.pdf",
            "A1.pdf",
            "a2.pdf",
            "a10.pdf",
        ]
    );
    assert!(!index.truncated);
    assert_eq!(index.root, dir.path());

    let e = &index.entries;
    assert_eq!(
        (e[0].name.as_str(), e[0].parent, e[0].depth),
        ("alpha", None, 0)
    );
    assert_eq!(
        (e[1].name.as_str(), e[1].parent, e[1].depth),
        ("sub", Some(0), 1)
    );
    assert_eq!((e[2].parent, e[2].depth), (Some(1), 2));
    assert_eq!(
        (e[4].name.as_str(), e[4].parent, e[4].depth),
        ("x.pdf", Some(0), 1)
    );
    assert_eq!(e[6].rel_path, Path::new("Zeta/b.pdf"));
    assert_eq!(e[9].kind, EntryKind::Pdf);
    assert_eq!(e[9].parent, None);
}

#[test]
fn scan_takes_pdf_by_extension_in_any_case_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    touch_all(
        dir.path(),
        &[
            "lower.pdf",
            "upper.PDF",
            "mixed.Pdf",
            "notes.txt",
            "README",
            "x.pdf.bak",
            "pdf",
            "archive.pdf.zip",
        ],
    );
    // A folder whose name ends in .pdf is a folder, not a file.
    touch(&dir.path().join("book.pdf/inner.pdf"));
    let index = run_scan(dir.path());
    assert_eq!(
        listing(&index),
        [
            "book.pdf/",
            "book.pdf/inner.pdf",
            "lower.pdf",
            "mixed.Pdf",
            "upper.PDF"
        ]
    );
}

#[test]
fn scan_leaves_out_folders_without_pdf_but_keeps_the_path_to_a_deep_one() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("empty")).unwrap();
    fs::create_dir_all(dir.path().join("deep/empty/er/est")).unwrap();
    touch(&dir.path().join("texts/a.txt"));
    touch(&dir.path().join("texts/more/b.txt"));
    touch(&dir.path().join("p/q/r/s/only.pdf"));
    touch(&dir.path().join("p/q/r/s/t/none.txt"));
    let index = run_scan(dir.path());
    assert_eq!(
        listing(&index),
        ["p/", "p/q/", "p/q/r/", "p/q/r/s/", "p/q/r/s/only.pdf"]
    );
    assert_eq!(index.dir_count(), 4);
    assert_eq!(index.file_count(), 1);
}

#[test]
fn scan_of_a_folder_with_nothing_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let index = run_scan(dir.path());
    assert!(index.entries.is_empty());
    assert!(!index.truncated);
    assert!(index.children(None).is_empty());
}

#[test]
fn scan_skips_hidden_files_and_folders_with_everything_in_them() {
    let dir = tempfile::tempdir().unwrap();
    touch_all(
        dir.path(),
        &[
            ".hidden.pdf",
            ".git/inside.pdf",
            ".cache/deeper/x.pdf",
            "shown/.secret.pdf",
            "shown/.dot/y.pdf",
            "shown/visible.pdf",
        ],
    );
    let index = run_scan(dir.path());
    assert_eq!(listing(&index), ["shown/", "shown/visible.pdf"]);
}

#[cfg(unix)]
#[test]
fn scan_follows_links_to_pdf_files_and_folders_but_not_loops_or_broken_links() {
    use std::os::unix::fs::symlink;

    let outside = tempfile::tempdir().unwrap();
    touch(&outside.path().join("real.pdf"));
    touch(&outside.path().join("plain.txt"));
    touch(&outside.path().join("lib/book.pdf"));

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    touch(&root.join("a/doc.pdf"));
    // A link to a PDF, named like a PDF, and one named otherwise.
    symlink(outside.path().join("real.pdf"), root.join("alias.pdf")).unwrap();
    symlink(outside.path().join("real.pdf"), root.join("latest")).unwrap();
    // A link to a file that is not a PDF.
    symlink(outside.path().join("plain.txt"), root.join("text-link")).unwrap();
    // A link to a folder somewhere else is followed.
    symlink(outside.path().join("lib"), root.join("lib-link")).unwrap();
    // Links back to ancestors, and to the root itself, are not.
    symlink(root, root.join("a/up-to-root")).unwrap();
    symlink(root.join("a"), root.join("a/self")).unwrap();
    // A link to a folder that was already walked is not followed again.
    symlink(root.join("a"), root.join("z-again")).unwrap();
    // Broken links, to a file and to a folder.
    symlink(root.join("nothing.pdf"), root.join("broken.pdf")).unwrap();
    symlink(root.join("nowhere"), root.join("broken-dir")).unwrap();

    let index = run_scan(root);
    assert_eq!(
        listing(&index),
        [
            "a/",
            "a/doc.pdf",
            "lib-link/",
            "lib-link/book.pdf",
            "alias.pdf",
            "latest",
        ]
    );
}

#[cfg(unix)]
#[test]
fn scan_ends_on_a_pair_of_folders_that_link_to_each_other() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    touch(&root.join("one/a.pdf"));
    touch(&root.join("two/b.pdf"));
    symlink(root.join("two"), root.join("one/to-two")).unwrap();
    symlink(root.join("one"), root.join("two/to-one")).unwrap();
    let index = run_scan(root);
    // `one` is walked first and follows the link into `two`; the link back to `one` is refused.
    // When `two` itself comes up, its link to `one` is refused as well.
    assert_eq!(
        listing(&index),
        [
            "one/",
            "one/to-two/",
            "one/to-two/b.pdf",
            "one/a.pdf",
            "two/",
            "two/b.pdf"
        ]
    );
}

#[cfg(unix)]
#[test]
fn scan_goes_on_when_a_folder_cannot_be_read() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    touch_all(
        dir.path(),
        &["a/ok.pdf", "locked/hidden-from-us.pdf", "z.pdf"],
    );
    let locked = dir.path().join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    // Running as root, the folder stays readable and there is nothing to test.
    let readable = fs::read_dir(&locked).is_ok();
    let result = scan(dir.path(), &ScanLimits::default(), &AtomicBool::new(false));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        eprintln!("skipped: permissions are not enforced for this user");
        return;
    }
    assert_eq!(listing(&result.unwrap()), ["a/", "a/ok.pdf", "z.pdf"]);
}

#[test]
fn scan_of_a_missing_root_or_a_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let limits = ScanLimits::default();
    let cancel = AtomicBool::new(false);
    let missing = scan(&dir.path().join("nope"), &limits, &cancel).unwrap_err();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    touch(&dir.path().join("file.pdf"));
    assert!(scan(&dir.path().join("file.pdf"), &limits, &cancel).is_err());
}

#[test]
fn scan_stops_at_the_entry_limit_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..10 {
        touch(&dir.path().join(format!("f{i:02}.pdf")));
    }
    let cancel = AtomicBool::new(false);
    let limits = |max_entries| ScanLimits {
        max_entries,
        ..ScanLimits::default()
    };

    let cut = scan(dir.path(), &limits(3), &cancel).unwrap();
    assert!(cut.truncated);
    assert_eq!(listing(&cut), ["f00.pdf", "f01.pdf", "f02.pdf"]);

    let exact = scan(dir.path(), &limits(10), &cancel).unwrap();
    assert!(!exact.truncated);
    assert_eq!(exact.entries.len(), 10);
}

#[test]
fn entry_limit_applies_across_folders_and_leaves_a_consistent_tree() {
    let dir = tempfile::tempdir().unwrap();
    for d in ["a", "b", "c"] {
        for i in 0..4 {
            touch(&dir.path().join(format!("{d}/{i}.pdf")));
        }
    }
    let limits = ScanLimits {
        max_entries: 7,
        ..ScanLimits::default()
    };
    let cut = scan(dir.path(), &limits, &AtomicBool::new(false)).unwrap();
    assert!(cut.truncated);
    assert!(cut.entries.len() <= 7);
    assert_eq!(cut.entries.len(), 7);
    for e in &cut.entries {
        if let Some(p) = e.parent {
            assert_eq!(cut.entries[p as usize].kind, EntryKind::Dir);
        }
    }
}

#[test]
fn scan_does_not_go_below_the_depth_limit_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    touch_all(
        dir.path(),
        &["top.pdf", "a/x.pdf", "a/b/y.pdf", "a/b/c/z.pdf"],
    );
    let cancel = AtomicBool::new(false);
    let limits = |max_depth| ScanLimits {
        max_depth,
        ..ScanLimits::default()
    };

    let top_only = scan(dir.path(), &limits(0), &cancel).unwrap();
    assert!(top_only.truncated);
    assert_eq!(listing(&top_only), ["top.pdf"]);

    let one = scan(dir.path(), &limits(1), &cancel).unwrap();
    assert!(one.truncated);
    assert_eq!(listing(&one), ["a/", "a/x.pdf", "top.pdf"]);

    let all = scan(dir.path(), &limits(3), &cancel).unwrap();
    assert!(!all.truncated);
    assert_eq!(all.entries.len(), 7);
    assert_eq!(all.entries.iter().map(|e| e.depth).max(), Some(3));

    assert!(
        !run_scan(dir.path()).truncated,
        "the default depth is enough here"
    );
}

#[test]
fn scan_with_the_cancel_flag_set_is_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("a/b.pdf"));
    let cancel = AtomicBool::new(true);
    let err = scan(dir.path(), &ScanLimits::default(), &cancel).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn index_helpers_answer_for_children_ancestors_paths_and_files() {
    let dir = tempfile::tempdir().unwrap();
    touch_all(
        dir.path(),
        &["m/n/o.pdf", "m/p.pdf", "m/n/o2.pdf", "q.pdf", "r/s.pdf"],
    );
    let index = run_scan(dir.path());
    // 0 m/  1 m/n/  2 m/n/o.pdf  3 m/n/o2.pdf  4 m/p.pdf  5 r/  6 r/s.pdf  7 q.pdf
    assert_eq!(
        listing(&index),
        [
            "m/",
            "m/n/",
            "m/n/o.pdf",
            "m/n/o2.pdf",
            "m/p.pdf",
            "r/",
            "r/s.pdf",
            "q.pdf"
        ]
    );

    assert_eq!(index.children(None), [0, 5, 7]);
    assert_eq!(index.children(Some(0)), [1, 4]);
    assert_eq!(index.children(Some(1)), [2, 3]);
    assert!(index.children(Some(2)).is_empty());
    assert!(index.children(Some(999)).is_empty());
    assert_eq!(index.children_of(Some(0)), [1, 4]);
    assert!(index.has_children(0));
    assert!(!index.has_children(2));
    assert!(!index.has_children(999));

    assert_eq!(index.ancestors(2), [1, 0]);
    assert_eq!(index.ancestors(1), [0]);
    assert!(index.ancestors(0).is_empty());
    assert!(index.ancestors(7).is_empty());
    assert!(index.ancestors(999).is_empty());

    assert_eq!(index.abs_path(2), dir.path().join("m/n/o.pdf"));
    assert_eq!(index.abs_path(999), dir.path());

    assert_eq!(index.files().collect::<Vec<_>>(), [2, 3, 4, 6, 7]);
    assert_eq!(index.file_count(), 5);
    assert_eq!(index.dir_count(), 3);
    assert_eq!(index.entry(2).map(|e| e.name.as_str()), Some("o.pdf"));
    assert!(index.entry(999).is_none());
}

#[test]
fn find_takes_absolute_and_relative_paths_and_misses_what_is_not_there() {
    let dir = tempfile::tempdir().unwrap();
    touch_all(dir.path(), &["m/n/o.pdf", "q.pdf"]);
    let index = run_scan(dir.path());
    let root = dir.path();

    assert_eq!(index.find(&root.join("m/n/o.pdf")), Some(2));
    assert_eq!(index.find(Path::new("m/n/o.pdf")), Some(2));
    assert_eq!(index.find(Path::new("./m/../m/n/o.pdf")), Some(2));
    assert_eq!(index.find(&root.join("m/./n/../n/o.pdf")), Some(2));
    assert_eq!(
        index.find(&root.join("m")),
        Some(0),
        "folders are found too"
    );
    assert_eq!(index.find(Path::new("q.pdf")), Some(3));

    assert_eq!(index.find(&root.join("m/n/none.pdf")), None);
    assert_eq!(index.find(Path::new("none.pdf")), None);
    assert_eq!(index.find(root), None, "the root is not an entry");
    assert_eq!(index.find(Path::new("")), None);
    assert_eq!(index.find(Path::new(".")), None);
    assert_eq!(index.find(Path::new("../q.pdf")), None);
    assert_eq!(index.find(Path::new("/somewhere/else/q.pdf")), None);
    assert_eq!(index.find(&root.join("../q.pdf")), None);
}

#[cfg(unix)]
#[test]
fn find_works_with_a_root_given_through_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let real = fs::canonicalize(dir.path()).unwrap();
    touch(&real.join("t/doc.pdf"));
    let holder = tempfile::tempdir().unwrap();
    let via = holder.path().join("via");
    std::os::unix::fs::symlink(&real, &via).unwrap();

    let index = run_scan(&via);
    assert_eq!(index.root, via);
    assert_eq!(index.abs_path(1), via.join("t/doc.pdf"));
    assert_eq!(index.find(&via.join("t/doc.pdf")), Some(1));
    assert_eq!(index.find(&real.join("t/doc.pdf")), Some(1));
}

#[test]
fn natural_cmp_orders_numbers_by_value_and_ignores_case() {
    let lt = |a: &str, b: &str| {
        assert_eq!(natural_cmp(a, b), Ordering::Less, "{a:?} < {b:?}");
        assert_eq!(natural_cmp(b, a), Ordering::Greater, "{b:?} > {a:?}");
    };
    lt("doc2", "doc10");
    lt("doc10", "doc10a");
    lt("a2.pdf", "a10.pdf");
    lt("a", "B");
    lt("B", "c");
    lt("abc", "abd");
    lt("ab", "abc");
    lt("", "a");
    lt("file9", "file09x");
    lt("a1", "a01");
    lt("v1.2", "v1.10");
    lt("1", "a");
    lt("x007", "x8");
    lt("한글2", "한글10");
    assert_eq!(natural_cmp("ABC", "abc"), Ordering::Equal);
    assert_eq!(natural_cmp("", ""), Ordering::Equal);
    assert_eq!(natural_cmp("a12b", "A12B"), Ordering::Equal);
    // Huge runs of digits do not overflow.
    let big = "9".repeat(60);
    assert_eq!(
        natural_cmp(&format!("n{big}"), &format!("n{big}0")),
        Ordering::Less
    );
}

#[test]
#[ignore = "measures the scan of 50,000 files; run with --ignored --nocapture"]
fn measure_scan_and_find_of_fifty_thousand_files() {
    let dir = tempfile::tempdir().unwrap();
    for a in 0..50 {
        for b in 0..10 {
            let sub = dir.path().join(format!("area{a:02}/part{b:02}"));
            fs::create_dir_all(&sub).unwrap();
            for c in 0..100 {
                fs::write(sub.join(format!("doc{c:03}.pdf")), b"").unwrap();
            }
        }
    }

    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let index = scan(dir.path(), &ScanLimits::default(), &cancel).unwrap();
    let scan_time = started.elapsed();
    assert_eq!(index.file_count(), 50_000);
    assert_eq!(index.dir_count(), 550);
    assert!(!index.truncated);

    let started = Instant::now();
    let mut found = 0;
    for a in 0..50 {
        for b in 0..10 {
            for c in (0..100).step_by(10) {
                let path = dir
                    .path()
                    .join(format!("area{a:02}/part{b:02}/doc{c:03}.pdf"));
                found += usize::from(index.find(&path).is_some());
            }
        }
    }
    let find_time = started.elapsed();
    assert_eq!(found, 5_000);

    let started = Instant::now();
    let mut seen = 0;
    for a in 0..index.dir_count() as u32 {
        seen += index.children(Some(a)).len();
    }
    seen += index.children(None).len();
    let children_time = started.elapsed();
    assert!(seen > 0);

    let started = Instant::now();
    let files = index.files().count();
    let files_time = started.elapsed();
    assert_eq!(files, 50_000);

    eprintln!(
        "scan of {} entries: {scan_time:?}; 5,000 find calls: {find_time:?} ({:?} each); \
         children() of every folder: {children_time:?}; files(): {files_time:?}",
        index.entries.len(),
        find_time / 5_000,
    );
}

// ---------------------------------------------------------------------------------------------
// WorkspaceScan

/// A notifier that counts its calls.
fn counting_notifier() -> (Notifier, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let notifier: Notifier = Arc::new(move || {
        counter.fetch_add(1, AtomicOrdering::SeqCst);
    });
    (notifier, calls)
}

#[test]
fn background_scan_publishes_an_index_and_calls_the_notifier() {
    let dir = tempfile::tempdir().unwrap();
    touch_all(dir.path(), &["a/b.pdf", "c.pdf"]);
    let (notifier, calls) = counting_notifier();
    let job = WorkspaceScan::start(dir.path().to_path_buf(), ScanLimits::default(), notifier);
    assert_eq!(job.root(), dir.path());

    wait_until("the first scan", || calls.load(AtomicOrdering::SeqCst) >= 1);
    let index = job.index().expect("an index after the notifier was called");
    assert_eq!(listing(&index), ["a/", "a/b.pdf", "c.pdf"]);
    assert!(!job.scanning());
    assert_eq!(job.error(), None);
}

#[test]
fn background_scan_of_a_missing_folder_reports_the_error_and_has_no_index() {
    let dir = tempfile::tempdir().unwrap();
    let (notifier, calls) = counting_notifier();
    let job = WorkspaceScan::start(dir.path().join("gone"), ScanLimits::default(), notifier);
    wait_until("the failed scan", || {
        calls.load(AtomicOrdering::SeqCst) >= 1
    });
    assert!(job.index().is_none());
    assert!(job.error().is_some_and(|e| !e.is_empty()));
    assert!(!job.scanning());
}

#[test]
fn rescan_finds_what_was_added_and_the_old_index_stays_until_then() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("one.pdf"));
    let (notifier, calls) = counting_notifier();
    let job = WorkspaceScan::start(dir.path().to_path_buf(), ScanLimits::default(), notifier);
    wait_until("the first scan", || calls.load(AtomicOrdering::SeqCst) >= 1);
    let before = job.index().unwrap();
    assert_eq!(listing(&before), ["one.pdf"]);

    touch(&dir.path().join("two.pdf"));
    let calls_before = calls.load(AtomicOrdering::SeqCst);
    job.rescan();
    // While the scan runs there is always an index, and it is the old or the new one.
    wait_until("the rescan", || {
        let now = job.index().expect("an index while rescanning");
        assert!(now.entries.len() == 1 || now.entries.len() == 2);
        calls.load(AtomicOrdering::SeqCst) > calls_before && !job.scanning()
    });
    assert_eq!(listing(&job.index().unwrap()), ["one.pdf", "two.pdf"]);
    assert_eq!(
        listing(&before),
        ["one.pdf"],
        "a handed-out index does not change"
    );
}

#[test]
fn a_failed_rescan_keeps_the_last_good_index_and_a_good_one_clears_the_error() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("ws");
    touch(&root.join("a.pdf"));
    let (notifier, calls) = counting_notifier();
    let job = WorkspaceScan::start(root.clone(), ScanLimits::default(), notifier);
    wait_until("the first scan", || calls.load(AtomicOrdering::SeqCst) >= 1);
    assert_eq!(job.error(), None);

    let moved = parent.path().join("moved");
    fs::rename(&root, &moved).unwrap();
    job.rescan();
    wait_until("the failed rescan", || {
        calls.load(AtomicOrdering::SeqCst) >= 2 && !job.scanning()
    });
    assert!(job.error().is_some());
    assert_eq!(listing(&job.index().unwrap()), ["a.pdf"]);

    fs::rename(&moved, &root).unwrap();
    job.rescan();
    wait_until("the good rescan", || {
        calls.load(AtomicOrdering::SeqCst) >= 3 && !job.scanning()
    });
    assert_eq!(job.error(), None);
}

#[test]
fn dropping_the_scan_ends_its_thread_without_waiting_for_it() {
    let dir = tempfile::tempdir().unwrap();
    for a in 0..20 {
        for b in 0..20 {
            touch(&dir.path().join(format!("d{a}/e{b}/f.pdf")));
        }
    }
    let notifier: Notifier = Arc::new(|| {});
    let job = WorkspaceScan::start(
        dir.path().to_path_buf(),
        ScanLimits::default(),
        Arc::clone(&notifier),
    );
    let started = Instant::now();
    drop(job);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the drop must not wait for the scan"
    );
    // The thread holds a copy of the notifier until it ends.
    wait_until("the scan thread to end", || {
        Arc::strong_count(&notifier) == 1
    });
}

#[test]
fn a_dropped_scan_does_not_call_the_notifier_afterwards() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("a.pdf"));
    let (notifier, calls) = counting_notifier();
    let watcher = Arc::clone(&notifier);
    let job = WorkspaceScan::start(dir.path().to_path_buf(), ScanLimits::default(), notifier);
    wait_until("the first scan", || calls.load(AtomicOrdering::SeqCst) >= 1);
    drop(job);
    wait_until("the thread to end", || Arc::strong_count(&watcher) == 1);
    let seen = calls.load(AtomicOrdering::SeqCst);
    assert_eq!(seen, 1);
}

// ---------------------------------------------------------------------------------------------
// WorkspaceWatcher

/// A short window so the tests are quick; the default is half a second.
const WINDOW: Duration = Duration::from_millis(150);

/// How long to wait to see that nothing comes.
const QUIET: Duration = Duration::from_millis(1200);

struct Watch {
    // Dropped last by field order; the watcher is what is under test.
    _watcher: WorkspaceWatcher,
    calls: Receiver<()>,
}

impl Watch {
    fn start(root: &Path) -> Watch {
        Watch::start_with_window(root, WINDOW)
    }

    fn start_with_window(root: &Path, window: Duration) -> Watch {
        let (tx, calls) = mpsc::channel();
        let tx = Mutex::new(tx);
        let watcher = WorkspaceWatcher::start_with_window(root, window, move || {
            let _ = tx.lock().unwrap().send(());
        })
        .expect("watching a temporary folder");
        Watch {
            _watcher: watcher,
            calls,
        }
    }

    /// Waits for a call.
    fn expect_call(&self, what: &str) {
        match self.calls.recv_timeout(TIMEOUT) {
            Ok(()) => {}
            Err(e) => panic!("no change was reported for {what}: {e:?}"),
        }
    }

    /// Checks that no call comes for a while.
    fn expect_quiet(&self, what: &str) {
        match self.calls.recv_timeout(QUIET) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) => panic!("a change was reported for {what}"),
            Err(e) => panic!("the watcher ended: {e:?}"),
        }
    }

    /// Takes the calls that are there now and the ones that follow until things go quiet.
    fn drain(&self) -> usize {
        let mut n = 0;
        while self.calls.recv_timeout(QUIET).is_ok() {
            n += 1;
        }
        n
    }
}

#[test]
fn watcher_reports_a_new_pdf() {
    let dir = tempfile::tempdir().unwrap();
    let watch = Watch::start(dir.path());
    touch(&dir.path().join("new.pdf"));
    watch.expect_call("a new PDF");
}

#[test]
fn watcher_reports_a_removed_pdf() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("old.PDF"));
    let watch = Watch::start(dir.path());
    fs::remove_file(dir.path().join("old.PDF")).unwrap();
    watch.expect_call("a removed PDF");
}

#[test]
fn watcher_reports_a_renamed_pdf_and_a_pdf_renamed_to_something_else() {
    let dir = tempfile::tempdir().unwrap();
    touch_all(dir.path(), &["a.pdf", "b.pdf"]);
    let watch = Watch::start(dir.path());
    fs::rename(dir.path().join("a.pdf"), dir.path().join("a2.pdf")).unwrap();
    watch.expect_call("a renamed PDF");
    assert_eq!(watch.drain(), 0, "one rename, one report");
    fs::rename(dir.path().join("b.pdf"), dir.path().join("b.bak")).unwrap();
    watch.expect_call("a PDF that stopped being one");
}

#[test]
fn watcher_reports_a_pdf_created_in_a_new_subfolder() {
    let dir = tempfile::tempdir().unwrap();
    let watch = Watch::start(dir.path());
    touch(&dir.path().join("fresh/deeper/inside.pdf"));
    watch.expect_call("a PDF in a new folder");
    // The folder is watched from now on: what is added later is seen too.
    watch.drain();
    touch(&dir.path().join("fresh/deeper/second.pdf"));
    watch.expect_call("a second PDF in the new folder");
}

#[test]
fn watcher_reports_folders_created_removed_and_renamed() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("keep")).unwrap();
    let watch = Watch::start(dir.path());

    fs::create_dir(dir.path().join("made")).unwrap();
    watch.expect_call("a new folder");
    watch.drain();

    fs::rename(dir.path().join("keep"), dir.path().join("kept")).unwrap();
    watch.expect_call("a renamed folder");
    watch.drain();

    fs::remove_dir(dir.path().join("made")).unwrap();
    watch.expect_call("a removed folder");
}

#[test]
fn watcher_reports_a_folder_moved_out_of_the_workspace() {
    let outside = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("leaving/x.pdf"));
    let watch = Watch::start(dir.path());
    fs::rename(dir.path().join("leaving"), outside.path().join("left")).unwrap();
    watch.expect_call("a folder that was moved away");
}

#[test]
fn watcher_ignores_other_files_content_changes_and_hidden_things() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("exists.pdf"));
    touch(&dir.path().join("notes.txt"));
    fs::create_dir(dir.path().join(".hidden")).unwrap();
    let watch = Watch::start(dir.path());

    touch(&dir.path().join("only.txt"));
    watch.expect_quiet("a new text file");
    fs::write(dir.path().join("notes.txt"), b"changed").unwrap();
    watch.expect_quiet("a text file that changed");
    fs::rename(dir.path().join("notes.txt"), dir.path().join("moved.txt")).unwrap();
    watch.expect_quiet("a text file that was renamed");
    fs::remove_file(dir.path().join("moved.txt")).unwrap();
    watch.expect_quiet("a text file that was removed");
    fs::write(dir.path().join("exists.pdf"), b"%PDF new contents").unwrap();
    watch.expect_quiet("a PDF whose content changed");
    touch(&dir.path().join(".hidden.pdf"));
    watch.expect_quiet("a hidden PDF");
    touch(&dir.path().join(".hidden/inside.pdf"));
    watch.expect_quiet("a PDF in a hidden folder");
    fs::create_dir(dir.path().join(".dotdir")).unwrap();
    watch.expect_quiet("a hidden folder");

    // The watcher is alive: a PDF is reported, once.
    touch(&dir.path().join("visible.pdf"));
    watch.expect_call("a PDF after all of that");
    assert_eq!(
        watch.drain(),
        0,
        "the ignored changes did not add a report of their own"
    );
}

#[test]
fn watcher_joins_a_burst_of_changes_into_one_or_two_reports() {
    let dir = tempfile::tempdir().unwrap();
    let watch = Watch::start_with_window(dir.path(), Duration::from_millis(400));
    for i in 0..10 {
        touch(&dir.path().join(format!("burst{i}.pdf")));
    }
    watch.expect_call("the burst");
    let more = watch.drain();
    assert!(more <= 1, "ten files made {} reports", more + 1);
}

#[test]
fn watcher_cannot_watch_a_missing_folder() {
    let dir = tempfile::tempdir().unwrap();
    let result = WorkspaceWatcher::start(&dir.path().join("gone"), || {});
    assert!(result.is_err());
}

#[test]
fn dropping_the_watcher_ends_the_watch() {
    let dir = tempfile::tempdir().unwrap();
    let marker = Arc::new(());
    let held = Arc::clone(&marker);
    let (tx, calls) = mpsc::channel();
    let tx = Mutex::new(tx);
    let watcher = WorkspaceWatcher::start_with_window(dir.path(), WINDOW, move || {
        let _keep = &held;
        let _ = tx.lock().unwrap().send(());
    })
    .unwrap();
    drop(watcher);
    // The thread owns the callback; it is gone once the thread ended.
    wait_until("the watch thread to end", || {
        Arc::strong_count(&marker) == 1
    });
    touch(&dir.path().join("late.pdf"));
    assert!(matches!(
        calls.recv_timeout(Duration::from_millis(600)),
        Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout)
    ));
}

#[test]
fn the_default_window_is_half_a_second() {
    assert_eq!(
        refdesk_core::workspace::WATCH_WINDOW,
        Duration::from_millis(500)
    );
}
