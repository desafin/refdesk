//! Integration tests of the document search (`docs/ARCHITECTURE.md` §4.8) on PDFs made by
//! `testutil::PdfBuilder`. Waiting is done on conditions with generous deadlines, never by
//! sleeping to fit the timing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use refdesk_core::testutil::{PdfBuilder, TempPdf};
use refdesk_core::{
    DocStatus, DocumentHandle, Notifier, SearchError, SearchJob, SearchOptions, SearchQuery,
    SearchSnapshot,
};

/// Deadline for anything the worker or the search does.
const TIMEOUT: Duration = Duration::from_secs(60);

fn query(text: &str) -> SearchQuery {
    SearchQuery {
        text: text.to_string(),
        ..SearchQuery::default()
    }
}

/// An open document and the channel its search notifier sends on.
struct Fixture {
    doc: DocumentHandle,
    notes: Receiver<()>,
    notify: Notifier,
    calls: Arc<AtomicUsize>,
}

impl Fixture {
    fn open(path: &Path) -> Fixture {
        let doc = DocumentHandle::open(path, Arc::new(|| {}));
        let (tx, notes) = mpsc::channel();
        let tx = Mutex::new(tx);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let notify: Notifier = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            let _ = tx.lock().unwrap().send(());
        });
        Fixture {
            doc,
            notes,
            notify,
            calls,
        }
    }

    /// Opens `pdf` and waits until the document is ready.
    fn ready(pdf: &PdfBuilder, stem: &str) -> (Fixture, TempPdf) {
        let file = pdf.write_temp(stem);
        let fx = Fixture::open(file.path());
        wait_until("the document to open", || {
            fx.doc.status() != DocStatus::Loading
        });
        assert_eq!(fx.doc.status(), DocStatus::Ready);
        (fx, file)
    }

    fn start(&self, q: SearchQuery, start_page: u32) -> SearchJob {
        SearchJob::start(self.doc.clone(), q, start_page, Arc::clone(&self.notify)).unwrap()
    }

    /// Waits until the snapshot says the search is finished.
    fn finish(&self, job: &SearchJob) -> Arc<SearchSnapshot> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let s = job.snapshot();
            if s.finished {
                return s;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.notes.recv_timeout(left) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => panic!("search did not finish"),
                Err(RecvTimeoutError::Disconnected) => panic!("notifier gone"),
            }
        }
    }
}

/// Waits (without a fixed sleep) until `cond` holds.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 2.0
}

#[test]
fn matches_come_in_document_order_with_the_right_pages_and_rects() {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "alpha beta alpha");
        })
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "gamma only");
        })
        .page(612.0, 792.0, |p| {
            p.text(144.0, 300.0, 12.0, "x Alpha");
        });
    let (fx, _file) = Fixture::ready(&pdf, "search-order");
    let job = fx.start(query("alpha"), 0);
    let s = fx.finish(&job);

    let pages: Vec<u32> = s.matches.iter().map(|m| m.page).collect();
    assert_eq!(pages, [0, 0, 2]);
    assert_eq!((s.pages_done, s.pages_total), (3, 3));
    assert!(!s.truncated);

    let first = &s.matches[0];
    assert_eq!(first.rects.len(), 1);
    let r = first.rects[0];
    assert!(close(r.x0, 72.0), "{r:?}");
    assert!(r.y0 < 100.0 && r.y1 > 100.0 && r.y1 - r.y0 < 20.0, "{r:?}");
    assert_eq!(first.matched, "alpha");
    assert_eq!(first.before, "");
    // The text after the match goes on into the next page (the context is reading order).
    assert_eq!(first.after, " beta alpha gamma only");

    let second = &s.matches[1].rects[0];
    assert!(second.x0 > r.x1, "{second:?} should be right of {r:?}");
    assert!(first.chars.end <= s.matches[1].chars.start);

    let third = &s.matches[2];
    // The case is ignored when matching; the context shows the text as it is written.
    assert_eq!(third.matched, "Alpha");
    assert_eq!(third.before, "x ");
    let r = third.rects[0];
    assert!(r.x0 > 144.0 && r.x0 < 170.0, "{r:?}");
    assert!(r.y0 < 300.0 && r.y1 > 300.0, "{r:?}");
}

#[test]
fn case_sensitive_whole_word_and_regex_options() {
    let pdf = PdfBuilder::new().page(612.0, 792.0, |p| {
        p.text(72.0, 100.0, 12.0, "Page pages page. REQ_12 and REQ_7x");
    });
    let (fx, _file) = Fixture::ready(&pdf, "search-options");
    let run = |q: SearchQuery| -> Vec<String> {
        let job = fx.start(q, 0);
        fx.finish(&job)
            .matches
            .iter()
            .map(|m| m.matched.clone())
            .collect()
    };
    assert_eq!(run(query("page")).len(), 3);
    let cs = SearchQuery {
        case_sensitive: true,
        ..query("page")
    };
    assert_eq!(run(cs), ["page", "page"]);
    let ww = SearchQuery {
        whole_word: true,
        ..query("page")
    };
    assert_eq!(run(ww).len(), 2); // "Page" and "page.", not "pages"
    let re = SearchQuery {
        regex: true,
        case_sensitive: true,
        ..query(r"\bREQ_\d+")
    };
    assert_eq!(run(re), ["REQ_12", "REQ_7"]);
}

#[test]
fn start_page_in_the_middle_still_gives_document_order_and_progress() {
    let mut pdf = PdfBuilder::new();
    for n in 0..5 {
        pdf = pdf.page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, &format!("needle on page {n}"));
        });
    }
    let file = pdf.write_temp("search-start");
    // The notifier hands control to the test after every page and waits for it to come back,
    // so each snapshot below is the state right after one page.
    let (note_tx, note_rx) = mpsc::channel::<()>();
    let (ack_tx, ack_rx) = mpsc::channel::<()>();
    let note_tx = Mutex::new(note_tx);
    let ack_rx = Mutex::new(ack_rx);
    let notify: Notifier = Arc::new(move || {
        let _ = note_tx.lock().unwrap().send(());
        let _ = ack_rx.lock().unwrap().recv_timeout(TIMEOUT);
    });
    let fx = Fixture::open(file.path());
    wait_until("document", || fx.doc.status() == DocStatus::Ready);
    let job = SearchJob::start_with(
        fx.doc.clone(),
        query("needle"),
        3,
        notify,
        SearchOptions {
            publish_interval: Duration::ZERO,
            ..SearchOptions::default()
        },
    )
    .unwrap();

    let mut seen: Vec<(u32, Vec<u32>)> = Vec::new();
    loop {
        note_rx.recv_timeout(TIMEOUT).expect("notification");
        let s = job.snapshot();
        seen.push((s.pages_done, s.matches.iter().map(|m| m.page).collect()));
        let finished = s.finished;
        ack_tx.send(()).unwrap();
        if finished {
            break;
        }
    }
    // Pages are processed 3, 4, 0, 1, 2; the matches are always listed by page.
    assert_eq!(
        seen,
        [
            (1, vec![3]),
            (2, vec![3, 4]),
            (3, vec![0, 3, 4]),
            (4, vec![0, 1, 3, 4]),
            (5, vec![0, 1, 2, 3, 4]),
            (5, vec![0, 1, 2, 3, 4]), // the final notification
        ]
    );
    assert_eq!(job.snapshot().pages_total, 5);
}

#[test]
fn start_page_past_the_end_counts_as_the_first_page() {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "needle");
        })
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "needle");
        });
    let (fx, _file) = Fixture::ready(&pdf, "search-past-end");
    let job = fx.start(query("needle"), 99);
    let s = fx.finish(&job);
    assert_eq!(s.matches.len(), 2);
    assert_eq!(s.pages_done, 2);
}

#[test]
fn phrase_over_a_page_break_is_found_once_on_the_first_page() {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "intro");
            p.text(72.0, 700.0, 12.0, "the state");
        })
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "machine is described here");
        });
    let (fx, _file) = Fixture::ready(&pdf, "search-page-break");
    let job = fx.start(query("state machine"), 0);
    let s = fx.finish(&job);
    assert_eq!(s.matches.len(), 1);
    let m = &s.matches[0];
    assert_eq!(m.page, 0);
    assert_eq!(m.matched, "state machine");
    assert_eq!(m.rects.len(), 1);
    assert!(m.rects[0].y0 < 700.0 && m.rects[0].y1 > 700.0);
    let cont = m.cont.as_ref().expect("continuation on the next page");
    assert_eq!(cont.page, 1);
    assert_eq!(cont.chars, 0..7);
    assert_eq!(cont.rects.len(), 1);
    assert!(close(cont.rects[0].x0, 72.0));
    assert!(cont.rects[0].y0 < 100.0 && cont.rects[0].y1 > 100.0);
    assert_eq!(m.after, " is described here");

    // Starting on the second page finds it just the same.
    let job = fx.start(query("state machine"), 1);
    assert_eq!(fx.finish(&job).matches.len(), 1);
}

#[test]
fn hyphenated_word_in_a_document_is_found() {
    let pdf = PdfBuilder::new().page(612.0, 792.0, |p| {
        p.text(72.0, 100.0, 12.0, "the speci-");
        p.text(72.0, 116.0, 12.0, "fication is long");
    });
    let (fx, _file) = Fixture::ready(&pdf, "search-hyphen");
    let job = fx.start(query("specification"), 0);
    let s = fx.finish(&job);
    assert_eq!(s.matches.len(), 1);
    assert_eq!(s.matches[0].rects.len(), 2);
    assert!(s.matches[0].rects[0].y1 < s.matches[0].rects[1].y1);
}

#[test]
fn no_match_finishes_with_an_empty_list() {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "something");
        })
        .page(612.0, 792.0, |_| {});
    let (fx, _file) = Fixture::ready(&pdf, "search-none");
    let job = fx.start(query("absent"), 0);
    let s = fx.finish(&job);
    assert!(s.finished && s.matches.is_empty() && !s.truncated);
    assert_eq!((s.pages_done, s.pages_total), (2, 2));
}

#[test]
fn empty_query_and_bad_regex_do_not_start() {
    let (fx, _file) = Fixture::ready(
        &PdfBuilder::new().page(612.0, 792.0, |_| {}),
        "search-errors",
    );
    let start = |q: SearchQuery| SearchJob::start(fx.doc.clone(), q, 0, Arc::clone(&fx.notify));
    assert!(matches!(start(query("")), Err(SearchError::Empty)));
    assert!(matches!(start(query("   ")), Err(SearchError::Empty)));
    let bad = SearchQuery {
        regex: true,
        ..query("(open")
    };
    match start(bad) {
        Err(SearchError::InvalidRegex(msg)) => assert!(!msg.is_empty()),
        other => panic!("expected an invalid regex error, got {other:?}"),
    }
    // Nothing was started, so nothing notifies.
    assert_eq!(fx.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn stops_at_the_match_limit() {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "hit hit hit hit");
        })
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "hit");
        });
    let (fx, _file) = Fixture::ready(&pdf, "search-limit");
    let limited = |max_matches| {
        SearchJob::start_with(
            fx.doc.clone(),
            query("hit"),
            0,
            Arc::clone(&fx.notify),
            SearchOptions {
                max_matches,
                ..SearchOptions::default()
            },
        )
        .unwrap()
    };
    // The limit falls inside the first page.
    let job = limited(2);
    let s = fx.finish(&job);
    assert!(s.truncated);
    assert_eq!(s.matches.len(), 2);
    assert_eq!(s.pages_done, 1);
    // Exactly as many matches as the limit is not a truncation.
    let job = limited(5);
    let s = fx.finish(&job);
    assert!(!s.truncated);
    assert_eq!(s.matches.len(), 5);
}

#[test]
fn dropping_the_job_ends_the_thread() {
    let mut pdf = PdfBuilder::new();
    for _ in 0..20 {
        pdf = pdf.page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "needle");
        });
    }
    let file = pdf.write_temp("search-drop");
    let fx = Fixture::open(file.path());
    wait_until("document", || fx.doc.status() == DocStatus::Ready);

    // The first notification parks the search thread until the test lets it go, so the search
    // is certainly still running when the job is dropped.
    let token = Arc::new(());
    let held = Arc::clone(&token);
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let entered_tx: Mutex<Sender<()>> = Mutex::new(entered_tx);
    let go_rx = Mutex::new(go_rx);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let notify: Notifier = Arc::new(move || {
        let _keep = &held;
        if c.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = entered_tx.lock().unwrap().send(());
            let _ = go_rx.lock().unwrap().recv_timeout(TIMEOUT);
        }
    });
    let job = SearchJob::start(fx.doc.clone(), query("needle"), 0, notify).unwrap();
    entered_rx
        .recv_timeout(TIMEOUT)
        .expect("first notification");
    assert!(
        Arc::strong_count(&token) > 1,
        "the thread holds the notifier"
    );
    let before = job.snapshot().pages_done;
    drop(job); // must not block
    go_tx.send(()).unwrap();
    wait_until("the search thread to end", || {
        Arc::strong_count(&token) == 1
    });
    // It stopped early and did not notify again after the drop.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(before < 20);
}

#[test]
fn the_notifier_is_called_for_matches_and_at_the_end() {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "needle");
        })
        .page(612.0, 792.0, |_| {})
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "needle");
        });
    let (fx, _file) = Fixture::ready(&pdf, "search-notify");
    let job = fx.start(query("needle"), 0);
    fx.finish(&job);
    wait_until("the final notification", || {
        fx.calls.load(Ordering::SeqCst) >= 3
    });
    // Two pages with matches and the end; the empty page does not notify.
    assert_eq!(fx.calls.load(Ordering::SeqCst), 3);

    let job = fx.start(query("absent"), 0);
    fx.finish(&job);
    wait_until("a notification for the empty result", || {
        fx.calls.load(Ordering::SeqCst) >= 4
    });
}

#[test]
fn a_document_that_cannot_be_opened_finishes_without_panic() {
    let path = std::env::temp_dir().join("refdesk-search-test-no-such-file.pdf");
    let fx = Fixture::open(&path);
    let job = fx.start(query("anything"), 0);
    let s = fx.finish(&job);
    assert!(s.finished);
    assert!(s.matches.is_empty());
    assert_eq!(s.pages_total, 0);
}

#[test]
fn searching_while_the_document_is_still_opening_works() {
    let pdf = PdfBuilder::new().page(612.0, 792.0, |p| {
        p.text(72.0, 100.0, 12.0, "needle");
    });
    let file = pdf.write_temp("search-opening");
    let fx = Fixture::open(file.path());
    // No wait for readiness: the search waits for the document itself.
    let job = fx.start(query("needle"), 0);
    let s = fx.finish(&job);
    assert_eq!(s.matches.len(), 1);
}

/// Times a search of a large real document. Run with
/// `cargo test -p refdesk-core --test search -- --ignored --nocapture`.
#[test]
#[ignore = "needs the 660-page reference PDF; prints timings"]
fn large_document_timing() {
    let default = "/home/oscar/orca/projects/pdfToolCreation/references/viewers/tdf/benches/example_dictionary.pdf";
    let path = std::env::var("REFDESK_BIG_PDF").unwrap_or_else(|_| default.to_string());
    if !Path::new(&path).exists() {
        eprintln!("skipped: {path} does not exist");
        return;
    }
    let fx = Fixture::open(Path::new(&path));
    wait_until("document", || fx.doc.status() == DocStatus::Ready);
    for word in ["the", "dictionary", "specification"] {
        // First run extracts the text; the second shows the cost of searching alone.
        for round in ["cold", "warm"] {
            let t = Instant::now();
            let job = fx.start(query(word), 0);
            let s = fx.finish(&job);
            eprintln!(
                "{word:>14} {round}: {:>7} matches, {} pages, {:?}",
                s.matches.len(),
                s.pages_done,
                t.elapsed()
            );
        }
    }
}
