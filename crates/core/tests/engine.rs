//! Integration tests of the document engine (`docs/ARCHITECTURE.md` §4.2) on PDFs made by
//! `testutil::PdfBuilder`. Waiting is done on the notifier with generous deadlines, never by
//! sleeping. (Rule R-2 covers `src/` only; the password and page label tests use `mupdf`
//! directly to prepare their input files.)

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use refdesk_core::testutil::{PdfBuilder, TempPdf};
use refdesk_core::{
    DestKind, Destination, DocStatus, DocumentHandle, DocumentInfo, EngineError, EngineOptions,
    LinkTarget, Location, Priority, Rect, RenderKey, RenderedPage,
};

/// Deadline for anything the worker does. Far above what a slow machine needs.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Options under which no pending request expires during a test.
fn no_expiry() -> EngineOptions {
    EngineOptions {
        render_expiry: Duration::from_secs(3600),
    }
}

/// A handle plus the channel its notifier sends on.
struct Doc {
    handle: DocumentHandle,
    notes: Receiver<()>,
    count: Arc<AtomicUsize>,
}

impl Doc {
    fn open(path: &Path) -> Doc {
        Doc::open_with(path, EngineOptions::default())
    }

    fn open_with(path: &Path, options: EngineOptions) -> Doc {
        let (tx, notes) = mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        // `Sender` is not `Sync`; the notifier must be.
        let tx = Mutex::new(tx);
        let handle = DocumentHandle::open_with(
            path,
            Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
                let _ = tx.lock().unwrap().send(());
            }),
            options,
        );
        Doc {
            handle,
            notes,
            count,
        }
    }

    /// Waits until `f` returns `Some`, re-checking after every notification.
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

    fn wait_status(&self) -> DocStatus {
        self.wait_for("status", |h| {
            let s = h.status();
            (s != DocStatus::Loading).then_some(s)
        })
    }

    fn ready(&self) -> Arc<DocumentInfo> {
        assert_eq!(self.wait_status(), DocStatus::Ready);
        self.handle.info().expect("info when ready")
    }

    /// Collects polled renders until `done` says the list is complete.
    fn collect_until(&self, mut done: impl FnMut(&[RenderedPage]) -> bool) -> Vec<RenderedPage> {
        let mut got = Vec::new();
        self.wait_for("renders", |h| {
            got.extend(h.poll_rendered());
            done(&got).then_some(())
        });
        got
    }
}

fn key(page: u32, scale_milli: u32, generation: u32) -> RenderKey {
    RenderKey {
        page,
        scale_milli,
        generation,
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.5
}

fn assert_rect(r: Rect, x0: f32, y0: f32, x1: f32, y1: f32) {
    assert!(
        close(r.x0, x0) && close(r.y0, y0) && close(r.x1, x1) && close(r.y1, y1),
        "{r:?} != ({x0}, {y0}, {x1}, {y1})"
    );
}

fn all_white(rgb: &[u8]) -> bool {
    rgb.iter().all(|&b| b == 255)
}

/// A temporary file with arbitrary bytes, deleted on drop.
struct TempFile(PathBuf);

impl TempFile {
    fn new(name: &str, bytes: &[u8]) -> TempFile {
        let path =
            std::env::temp_dir().join(format!("refdesk-engine-test-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        TempFile(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------------------------

#[test]
fn opens_with_page_sizes_and_outline() {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "1 Introduction");
        })
        .page(300.0, 400.0, |p| {
            p.text(20.0, 300.0, 12.0, "1.1 Scope");
        })
        .outline("1 Introduction", 1, 100.0, |o| {
            o.child("1.1 Scope", 2, 300.0);
        })
        .outline_leaf("2 Usage", 2, 50.0)
        .write_temp("open");
    let doc = Doc::open(pdf.path());
    let info = doc.ready();
    assert_eq!(info.path, pdf.path());
    assert_eq!(info.generation, 0);
    assert_eq!(info.page_sizes.len(), 2);
    assert!(close(info.page_sizes[0].w, 612.0) && close(info.page_sizes[0].h, 792.0));
    assert!(close(info.page_sizes[1].w, 300.0) && close(info.page_sizes[1].h, 400.0));
    assert!(info.page_labels.is_empty());
    assert_eq!(info.title, None);

    let items = &info.outline.items;
    let summary: Vec<(&str, u16, Option<u32>)> = items
        .iter()
        .map(|i| (i.title.as_str(), i.depth, i.parent))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("1 Introduction", 0, None),
            ("1.1 Scope", 1, Some(0)),
            ("2 Usage", 0, None)
        ]
    );
    let targets: Vec<Location> = items.iter().map(|i| i.target.unwrap()).collect();
    for (t, (page, y)) in targets.iter().zip([(0, 100.0), (1, 300.0), (1, 50.0)]) {
        assert_eq!(t.page, page);
        assert!(close(t.x, 0.0) && close(t.y, y), "{t:?}");
    }
    assert!(items.iter().all(|i| i.uri.is_none()));
}

#[test]
fn missing_file_fails() {
    let path = std::env::temp_dir().join("refdesk-engine-test-does-not-exist.pdf");
    let doc = Doc::open(&path);
    assert!(matches!(doc.wait_status(), DocStatus::Failed(_)));
    assert!(doc.handle.info().is_none());
    assert_eq!(
        doc.handle.render_wait(key(0, 1000, 0)),
        Err(EngineError::NotReady)
    );
}

#[test]
fn garbage_bytes_fail() {
    let junk: Vec<u8> = (0..4096u32).map(|i| (i * 7919 % 251) as u8).collect();
    let file = TempFile::new("garbage.pdf", &junk);
    let doc = Doc::open(&file.0);
    assert!(matches!(doc.wait_status(), DocStatus::Failed(_)));
    assert_eq!(doc.handle.page_text_wait(0), Err(EngineError::NotReady));
}

#[test]
fn damaged_files_do_not_panic() {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "first page");
            p.link_uri((72.0, 90.0, 200.0, 104.0), "https://example.com");
        })
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "second page");
        })
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "third page");
        })
        .outline_leaf("t", 2, 10.0)
        .build();
    // Truncated in the middle, and overwritten in the middle.
    let mut smashed = pdf.clone();
    let mid = smashed.len() / 2;
    for b in &mut smashed[mid - 150..mid + 150] {
        *b = b'x';
    }
    let truncated = pdf[..pdf.len() / 2].to_vec();
    for (name, bytes) in [("smashed.pdf", smashed), ("truncated.pdf", truncated)] {
        let file = TempFile::new(name, &bytes);
        let doc = Doc::open(&file.0);
        match doc.wait_status() {
            DocStatus::Ready => {
                let n = doc.handle.info().unwrap().page_sizes.len() as u32;
                for page in 0..n {
                    // Each call may succeed or fail; it must answer.
                    let _ = doc.handle.render_wait(key(page, 1000, 0));
                    let _ = doc.handle.page_text_wait(page);
                    let _ =
                        doc.handle
                            .render_region_wait(page, Rect::new(0.0, 0.0, 50.0, 50.0), 1000);
                    // Non-blocking extraction runs before blocking jobs, so after the next
                    // blocking call the links have either arrived or failed.
                    let _ = doc.handle.page_links(page);
                    let _ = doc.handle.page_text_wait(page);
                    let _ = doc.handle.page_links(page);
                }
            }
            DocStatus::Failed(msg) => assert!(!msg.is_empty()),
            other => panic!("{name}: unexpected status {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

fn text_and_blank() -> TempPdf {
    PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 24.0, "HELLO WORLD");
        })
        .page(400.0, 300.0, |_| {})
        .write_temp("render")
}

#[test]
fn render_wait_size_and_content() {
    let pdf = text_and_blank();
    let doc = Doc::open(pdf.path());
    doc.ready();
    let page = doc.handle.render_wait(key(0, 1500, 0)).unwrap();
    assert_eq!(page.key, key(0, 1500, 0));
    assert!((page.width as i64 - 918).abs() <= 1, "width {}", page.width);
    assert!(
        (page.height as i64 - 1188).abs() <= 1,
        "height {}",
        page.height
    );
    assert_eq!(page.rgb.len(), (page.width * page.height * 3) as usize);
    assert!(!all_white(&page.rgb));

    let blank = doc.handle.render_wait(key(1, 2000, 0)).unwrap();
    assert!((blank.width as i64 - 800).abs() <= 1 && (blank.height as i64 - 600).abs() <= 1);
    assert_eq!(blank.rgb.len(), (blank.width * blank.height * 3) as usize);
    assert!(all_white(&blank.rgb));

    // render_wait answers directly, not through poll_rendered.
    assert!(doc.handle.poll_rendered().is_empty());
    assert!(matches!(
        doc.handle.render_wait(key(2, 1000, 0)),
        Err(EngineError::PageOutOfRange { page: 2, count: 2 })
    ));
}

#[test]
fn render_region_size_content_and_clipping() {
    let pdf = text_and_blank();
    let doc = Doc::open(pdf.path());
    doc.ready();
    // The text baseline is at y = 100 with 24 pt glyphs.
    let text = doc
        .handle
        .render_region_wait(0, Rect::new(70.0, 76.0, 200.0, 106.0), 2000)
        .unwrap();
    assert_eq!((text.width, text.height), (260, 60));
    assert_eq!(text.rgb.len(), (text.width * text.height * 3) as usize);
    assert!(!all_white(&text.rgb));

    let empty = doc
        .handle
        .render_region_wait(0, Rect::new(300.0, 500.0, 400.0, 600.0), 1000)
        .unwrap();
    assert_eq!((empty.width, empty.height), (100, 100));
    assert!(all_white(&empty.rgb));

    // Reaches past the right and bottom edges: clipped to 512..612 x 692..792.
    let clipped = doc
        .handle
        .render_region_wait(0, Rect::new(512.0, 692.0, 700.0, 900.0), 1000)
        .unwrap();
    assert_eq!((clipped.width, clipped.height), (100, 100));

    assert!(matches!(
        doc.handle
            .render_region_wait(0, Rect::new(700.0, 0.0, 800.0, 100.0), 1000),
        Err(EngineError::Failed(_))
    ));
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

fn pages_with_text(n: usize) -> TempPdf {
    let mut b = PdfBuilder::new();
    for i in 0..n {
        b = b.page(612.0, 792.0, |p| {
            for line in 0..40 {
                let y = 72.0 + line as f32 * 16.0;
                p.text(
                    72.0,
                    y,
                    12.0,
                    &format!("page {i} line {line} some words to draw"),
                );
            }
        });
    }
    b.write_temp("pages")
}

#[test]
fn duplicate_requests_give_one_result() {
    let pdf = pages_with_text(2);
    let doc = Doc::open_with(pdf.path(), no_expiry());
    doc.ready();
    let a = key(0, 1000, 0);
    let b = key(1, 1000, 0);
    for _ in 0..5 {
        doc.handle.request_render(a, Priority::Visible);
    }
    // Served after every Visible request, so any duplicate of `a` would come before it.
    doc.handle.request_render(b, Priority::Background);
    let got = doc.collect_until(|g| g.iter().any(|r| r.key == b));
    assert_eq!(got.iter().filter(|r| r.key == a).count(), 1);
    // Nothing else is on the way: a blocking job queued now runs after any leftover render.
    doc.handle.render_wait(key(1, 500, 0)).unwrap();
    assert!(doc.handle.poll_rendered().is_empty());
}

#[test]
fn visible_request_overtakes_background_ones() {
    let n = 20u32;
    let pdf = pages_with_text(2 * n as usize + 1);
    let doc = Doc::open_with(pdf.path(), no_expiry());
    doc.ready();
    let visible = key(2 * n, 1000, 0);
    for page in 0..n {
        doc.handle
            .request_render(key(page, 1000, 0), Priority::Background);
    }
    doc.handle.request_render(visible, Priority::Visible);
    // More recent Background requests: by recency alone these would all go first.
    for page in n..2 * n {
        doc.handle
            .request_render(key(page, 1000, 0), Priority::Background);
    }
    let got = doc.collect_until(|g| g.len() == 2 * n as usize + 1);
    let pos = got.iter().position(|r| r.key == visible).unwrap();
    assert!(pos < n as usize / 2, "visible page came at position {pos}");
}

#[test]
fn pending_requests_that_are_not_renewed_are_dropped() {
    let pdf = pages_with_text(4);
    // Every pending request is older than zero when the worker looks at it.
    let doc = Doc::open_with(
        pdf.path(),
        EngineOptions {
            render_expiry: Duration::ZERO,
        },
    );
    doc.ready();
    for page in 0..4 {
        doc.handle
            .request_render(key(page, 1000, 0), Priority::Visible);
    }
    // A blocking job is served only after the worker has looked at the render queue.
    doc.handle.render_wait(key(0, 500, 0)).unwrap();
    assert!(doc.handle.poll_rendered().is_empty());
}

#[test]
fn notifier_is_called_for_every_result() {
    let pdf = pages_with_text(3);
    let doc = Doc::open_with(pdf.path(), no_expiry());
    doc.ready();
    let before = doc.count.load(Ordering::SeqCst);
    for page in 0..3 {
        doc.handle
            .request_render(key(page, 1000, 0), Priority::Visible);
    }
    let got = doc.collect_until(|g| g.len() == 3);
    assert_eq!(got.len(), 3);
    assert!(doc.count.load(Ordering::SeqCst) - before >= 3);
    // Non-blocking text and links also notify when they arrive.
    let before = doc.count.load(Ordering::SeqCst);
    doc.wait_for("text", |h| h.page_text(1));
    doc.wait_for("links", |h| h.page_links(1));
    assert!(doc.count.load(Ordering::SeqCst) - before >= 2);
}

#[test]
fn worker_stops_when_the_last_handle_is_dropped() {
    let pdf = pages_with_text(1);
    let doc = Doc::open(pdf.path());
    doc.ready();
    let Doc { handle, notes, .. } = doc;
    let clone = handle.clone();
    drop(handle);
    // One handle is left: the worker still answers.
    assert!(clone.render_wait(key(0, 500, 0)).is_ok());
    drop(clone);
    // The notifier (and with it the sender) is freed once the worker has let go of it.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match notes.recv_timeout(left) {
            Ok(()) => {}
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("worker did not stop"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------------------------

#[test]
fn text_order_count_and_positions() {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "Hello World");
            p.text(72.0, 130.0, 12.0, "Second line");
        })
        .write_temp("text");
    let doc = Doc::open(pdf.path());
    doc.ready();
    let text = doc.handle.page_text_wait(0).unwrap();
    let s: String = text.chars.iter().map(|c| c.ch).collect();
    assert_eq!(s, "Hello WorldSecond line");
    assert_eq!(text.chars.len(), "Hello World".len() + "Second line".len());

    for c in &text.chars {
        let b = c.quad.bounds();
        assert!(
            b.x0 >= 0.0 && b.y0 >= 0.0 && b.x1 <= 612.0 && b.y1 <= 792.0,
            "{c:?}"
        );
        assert!(close(c.size, 12.0));
    }
    // Page space: y grows downwards and the first glyph sits on the baseline at y = 100.
    let first = text.chars[0].quad.bounds();
    assert!(close(first.x0, 72.0), "{first:?}");
    assert!(
        first.y0 < 100.0 && first.y1 > 98.0 && first.y1 < 104.0,
        "{first:?}"
    );

    assert_eq!(text.lines.len(), 2);
    assert_eq!(text.lines[0].chars, 0..11);
    assert_eq!(text.lines[1].chars, 11..22);
    assert!(text.lines[0].bounds.y0 < text.lines[1].bounds.y0);
    let covered: u32 = text
        .blocks
        .iter()
        .map(|b| b.lines.end - b.lines.start)
        .sum();
    assert_eq!(covered, 2);

    // The non-blocking call now answers from the cache.
    assert_eq!(doc.handle.page_text(0).as_deref(), Some(&*text));
    assert!(matches!(
        doc.handle.page_text_wait(1),
        Err(EngineError::PageOutOfRange { page: 1, count: 1 })
    ));
}

#[test]
fn page_text_without_blocking() {
    let pdf = pages_with_text(2);
    let doc = Doc::open(pdf.path());
    doc.ready();
    assert!(doc.handle.page_text(1).is_none());
    let text = doc.wait_for("text", |h| h.page_text(1));
    assert!(
        text.chars
            .iter()
            .map(|c| c.ch)
            .collect::<String>()
            .starts_with("page 1 line 0")
    );
    assert!(doc.handle.page_text(5).is_none());
}

// ---------------------------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------------------------

#[test]
fn link_destination_and_rectangles() {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "see the next page");
            p.link_to((72.0, 90.0, 200.0, 104.0), 2, Some((72.0, 100.0)));
            p.link_uri((72.0, 200.0, 220.0, 214.0), "https://example.com/spec");
            p.link_to((72.0, 300.0, 150.0, 314.0), 2, None);
        })
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "Target");
        })
        .write_temp("links");
    let doc = Doc::open(pdf.path());
    doc.ready();
    let links = doc.wait_for("links", |h| h.page_links(0));
    assert_eq!(links.len(), 3);

    assert_eq!(links[0].rects.len(), 1);
    assert_rect(links[0].rects[0], 72.0, 90.0, 200.0, 104.0);
    match links[0].target {
        LinkTarget::Internal(Destination {
            page: 1,
            kind:
                DestKind::Xyz {
                    left: Some(left),
                    top: Some(top),
                },
        }) => assert!(close(left, 72.0) && close(top, 100.0), "{left}, {top}"),
        ref other => panic!("unexpected target {other:?}"),
    }

    assert_rect(links[1].rects[0], 72.0, 200.0, 220.0, 214.0);
    assert_eq!(
        links[1].target,
        LinkTarget::External("https://example.com/spec".to_string())
    );

    assert_eq!(
        links[2].target,
        LinkTarget::Internal(Destination {
            page: 1,
            kind: DestKind::Fit
        })
    );
    // The second page has none.
    assert!(doc.wait_for("links", |h| h.page_links(1)).is_empty());
}

#[test]
fn link_over_two_lines_is_one_link() {
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 110.0, 12.0, "see section 4.2 on the next");
            p.text(72.0, 124.0, 12.0, "page for details");
            p.link_to((400.0, 100.0, 540.0, 112.0), 2, Some((72.0, 300.0)));
            p.link_to((72.0, 114.0, 150.0, 126.0), 2, Some((72.0, 300.0)));
        })
        .page(612.0, 792.0, |_| {})
        .write_temp("twoline");
    let doc = Doc::open(pdf.path());
    doc.ready();
    let links = doc.wait_for("links", |h| h.page_links(0));
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(links[0].rects.len(), 2);
    assert_rect(links[0].rects[0], 400.0, 100.0, 540.0, 112.0);
    assert_rect(links[0].rects[1], 72.0, 114.0, 150.0, 126.0);
}

// ---------------------------------------------------------------------------------------------
// Reload
// ---------------------------------------------------------------------------------------------

#[test]
fn reload_bumps_generation_and_drops_old_results() {
    let one = |word: &str, pages: usize| {
        let mut b = PdfBuilder::new();
        for _ in 0..pages {
            b = b.page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, word);
            });
        }
        b.build()
    };
    let pdf = PdfBuilder::new()
        .page(612.0, 792.0, |_| {})
        .write_temp("reload");
    std::fs::write(pdf.path(), one("Old", 2)).unwrap();
    let doc = Doc::open_with(pdf.path(), no_expiry());
    let info = doc.ready();
    assert_eq!(info.page_sizes.len(), 2);
    let old_text = doc.handle.page_text_wait(0).unwrap();
    assert_eq!(
        old_text.chars.iter().map(|c| c.ch).collect::<String>(),
        "Old"
    );

    // Old-generation requests: they may be rendered before the reload is handled, but their
    // results must never be delivered.
    for page in 0..2 {
        doc.handle
            .request_render(key(page, 1000, 0), Priority::Background);
    }
    std::fs::write(pdf.path(), one("New", 3)).unwrap();
    doc.handle.reload();
    let info = doc.wait_for("generation 1", |h| h.info().filter(|i| i.generation == 1));
    assert_eq!(doc.handle.status(), DocStatus::Ready);
    assert_eq!(info.page_sizes.len(), 3);

    // Requests with the old key are ignored from now on.
    doc.handle
        .request_render(key(0, 1000, 0), Priority::Visible);
    doc.handle
        .request_render(key(2, 1000, 1), Priority::Visible);
    let got = doc.collect_until(|g| g.iter().any(|r| r.key == key(2, 1000, 1)));
    assert!(
        got.iter().all(|r| r.key.generation == 1),
        "{:?}",
        got.iter().map(|r| r.key).collect::<Vec<_>>()
    );
    doc.handle.render_wait(key(0, 500, 1)).unwrap();
    assert!(
        doc.handle
            .poll_rendered()
            .iter()
            .all(|r| r.key.generation == 1)
    );
    assert!(doc.handle.render_wait(key(0, 500, 0)).is_err());

    // Text was extracted anew.
    let new_text = doc.handle.page_text_wait(0).unwrap();
    assert_eq!(
        new_text.chars.iter().map(|c| c.ch).collect::<String>(),
        "New"
    );
}

// ---------------------------------------------------------------------------------------------
// Password and page labels (input prepared with mupdf directly)
// ---------------------------------------------------------------------------------------------

#[test]
fn encrypted_document_needs_password() {
    use mupdf::pdf::{Encryption, PdfDocument, PdfWriteOptions};
    let src = PdfBuilder::new()
        .page(612.0, 792.0, |p| {
            p.text(72.0, 100.0, 12.0, "secret text");
        })
        .build();
    let file = TempFile::new("encrypted.pdf", b"");
    let mut opts = PdfWriteOptions::default();
    opts.set_encryption(Encryption::Aes256)
        .set_user_password("open sesame")
        .set_owner_password("owner");
    PdfDocument::from_bytes(&src)
        .unwrap()
        .save_with_options(file.0.to_str().unwrap(), opts)
        .unwrap();

    let doc = Doc::open(&file.0);
    assert_eq!(doc.wait_status(), DocStatus::NeedsPassword);
    assert!(doc.handle.info().is_none());
    assert_eq!(doc.handle.page_text_wait(0), Err(EngineError::NotReady));

    let n = doc.count.load(Ordering::SeqCst);
    doc.handle.authenticate("wrong");
    doc.wait_for("answer to the wrong password", |_| {
        (doc.count.load(Ordering::SeqCst) > n).then_some(())
    });
    assert_eq!(doc.handle.status(), DocStatus::NeedsPassword);

    doc.handle.authenticate("open sesame");
    let info = doc.wait_for("ready", |h| h.info());
    assert_eq!(doc.handle.status(), DocStatus::Ready);
    assert_eq!(info.page_sizes.len(), 1);
    let text = doc.handle.page_text_wait(0).unwrap();
    assert_eq!(
        text.chars.iter().map(|c| c.ch).collect::<String>(),
        "secret text"
    );
}

#[test]
fn page_labels_are_read() {
    use mupdf::pdf::PdfDocument;
    use mupdf::{PageLabelRule, PageLabelStyle};
    let src = PdfBuilder::new()
        .page(612.0, 792.0, |_| {})
        .page(612.0, 792.0, |_| {})
        .page(612.0, 792.0, |_| {})
        .build();
    let mut pdf = PdfDocument::from_bytes(&src).unwrap();
    pdf.set_page_label_rule(PageLabelRule::new(0, PageLabelStyle::LowerRoman))
        .unwrap();
    let mut appendix = PageLabelRule::new(2, PageLabelStyle::Decimal);
    appendix.prefix = "A-".to_string();
    pdf.set_page_label_rule(appendix).unwrap();
    let file = TempFile::new("labels.pdf", b"");
    pdf.save(file.0.to_str().unwrap()).unwrap();

    let doc = Doc::open(&file.0);
    let info = doc.ready();
    assert_eq!(info.page_labels, vec!["i", "ii", "A-1"]);
}
