//! Checks the demo PDFs in `assets/demo/` (built by `assets/demo/build.py`): the README and the
//! screenshots rely on them having an outline three levels deep, page labels, and internal links
//! from requirement references to the requirements.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use refdesk_core::{
    CopyOptions, DestKind, Destination, DocStatus, DocumentHandle, DocumentInfo, Link, LinkTarget,
    PageText,
};

const TIMEOUT: Duration = Duration::from_secs(60);

fn demo(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/demo")
        .join(name)
}

struct Doc {
    handle: DocumentHandle,
    notes: mpsc::Receiver<()>,
}

impl Doc {
    fn open(name: &str) -> Doc {
        let path = demo(name);
        assert!(path.exists(), "{} is missing", path.display());
        let (tx, notes) = mpsc::channel();
        let tx = Mutex::new(tx);
        let handle = DocumentHandle::open(
            &path,
            Arc::new(move || {
                let _ = tx.lock().unwrap().send(());
            }),
        );
        Doc { handle, notes }
    }

    fn wait_for<T>(&self, what: &str, mut f: impl FnMut(&DocumentHandle) -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(v) = f(&self.handle) {
                return v;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.notes.recv_timeout(left) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {what}"),
                Err(RecvTimeoutError::Disconnected) => panic!("worker gone waiting for {what}"),
            }
        }
    }

    fn info(&self) -> Arc<DocumentInfo> {
        let status = self.wait_for("status", |h| {
            let s = h.status();
            (s != DocStatus::Loading).then_some(s)
        });
        assert_eq!(status, DocStatus::Ready);
        self.handle.info().expect("info when ready")
    }

    fn links(&self, page: u32) -> Arc<[Link]> {
        self.wait_for("links", |h| h.page_links(page))
    }

    fn text(&self, page: u32) -> String {
        let t: Arc<PageText> = self.handle.page_text_wait(page).unwrap();
        let n = u32::try_from(t.chars.len()).unwrap();
        t.text(
            0..n,
            CopyOptions {
                join_lines: true,
                dehyphenate: false,
            },
        )
    }
}

/// The index of the page labelled `label`.
fn page_of(info: &DocumentInfo, label: &str) -> u32 {
    let i = info
        .page_labels
        .iter()
        .position(|l| l == label)
        .unwrap_or_else(|| panic!("no page labelled {label}"));
    u32::try_from(i).unwrap()
}

fn check_spec(name: &str, min_pages: usize) -> (Doc, Arc<DocumentInfo>) {
    let doc = Doc::open(name);
    let info = doc.info();
    let pages = info.page_sizes.len();
    assert!((min_pages..=30).contains(&pages), "{name}: {pages} pages");
    // Front matter in roman numerals, the body from 1.
    assert_eq!(info.page_labels.len(), pages, "{name}: page labels");
    assert_eq!(info.page_labels[0], "i");
    assert_eq!(info.page_labels[1], "ii");
    let first = page_of(&info, "1");
    assert!(first >= 2, "{name}: body starts on page index {first}");
    assert!(
        info.title
            .as_deref()
            .unwrap_or("")
            .starts_with("Example Platform")
    );
    // Outline: chapters, sections, subsections, each pointing into the document.
    let items = &info.outline.items;
    assert!(!items.is_empty(), "{name}: no outline");
    assert!(
        items.iter().all(|i| i.target.is_some()),
        "{name}: outline item without target"
    );
    assert_eq!(items[0].title, "1 Introduction");
    assert_eq!(items[0].target.unwrap().page, first);
    (doc, info)
}

#[test]
fn process_manager_versions_have_outline_labels_and_links() {
    for name in ["process-manager-v1.0.pdf", "process-manager-v1.1.pdf"] {
        let (doc, info) = check_spec(name, 15);
        let items = &info.outline.items;
        let max_depth = items.iter().map(|i| i.depth).max().unwrap();
        assert_eq!(max_depth, 2, "{name}: outline depth");
        assert!(items.iter().any(|i| i.title == "4.1.2 Startup timeout"));

        // The table of contents on page ii links to the chapters, and each link lands where
        // the outline entry of the same heading does (not a page margin above it).
        let toc = doc.links(1);
        assert!(
            toc.len() > 20,
            "{name}: {} links on the contents page",
            toc.len()
        );
        let heading = items
            .iter()
            .find(|i| i.title == "2 Terms and acronyms")
            .and_then(|i| i.target)
            .unwrap();
        let landed = toc
            .iter()
            .find_map(|l| match &l.target {
                LinkTarget::Internal(Destination {
                    page,
                    kind: DestKind::Xyz { top: Some(top), .. },
                }) if *page == heading.page => Some(*top),
                _ => None,
            })
            .expect("contents link to chapter 2");
        assert!(
            (landed - heading.y).abs() < 10.0,
            "{name}: link lands at {landed}, heading at {}",
            heading.y
        );

        // Every requirement reference in the traceability table jumps to the page that holds
        // the requirement.
        let last = u32::try_from(info.page_sizes.len() - 1).unwrap();
        let req_page = items
            .iter()
            .find(|i| i.title == "4.1.2 Startup timeout")
            .and_then(|i| i.target)
            .unwrap()
            .page;
        assert!(doc.text(req_page).contains("[SWS_PM_00016]"));
        let trace_page = items.last().unwrap().target.unwrap().page;
        let mut internal = 0;
        for page in trace_page..=last {
            for link in doc.links(page).iter() {
                if let LinkTarget::Internal(dest) = &link.target {
                    assert!(dest.page <= last);
                    internal += 1;
                }
            }
        }
        assert!(
            internal >= 35,
            "{name}: {internal} links in the traceability table"
        );
    }
}

#[test]
fn versions_differ_where_the_change_history_says() {
    let old = Doc::open("process-manager-v1.0.pdf");
    let new = Doc::open("process-manager-v1.1.pdf");
    let (old_info, new_info) = (old.info(), new.info());
    let titles = |info: &DocumentInfo| -> Vec<String> {
        info.outline.items.iter().map(|i| i.title.clone()).collect()
    };
    assert!(!titles(&old_info).contains(&"6.3 Logical supervision".to_string()));
    assert!(titles(&new_info).contains(&"6.3 Logical supervision".to_string()));
    let all = |doc: &Doc, info: &DocumentInfo| -> String {
        (0..u32::try_from(info.page_sizes.len()).unwrap())
            .map(|p| doc.text(p))
            .collect()
    };
    let (a, b) = (all(&old, &old_info), all(&new, &new_info));
    assert!(a.contains("within 5 s after it was executed"));
    assert!(b.contains("startup.timeout_ms, default 3000 ms"));
    assert!(!a.contains("kInvalidTransition") && b.contains("kInvalidTransition"));
}

#[test]
fn watchdog_interface_has_outline_and_labels() {
    let (doc, info) = check_spec("watchdog-interface-v1.0.pdf", 5);
    assert!(
        info.outline
            .items
            .iter()
            .any(|i| i.title == "2.1 WatchdogClient")
    );
    assert!(doc.text(page_of(&info, "1")).contains("[EP-SWS-PM]"));
}
