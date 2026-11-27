//! Integration tests of dark page colours (VIEW-7) through the document engine, on PDFs made by
//! `testutil::PdfBuilder`. Waiting is done on the notifier, never by sleeping.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use refdesk_core::recolor::{DARK_BACKGROUND, PixelRect, recolor_dark};
use refdesk_core::testutil::{PdfBuilder, TempPdf};
use refdesk_core::{
    DocStatus, DocumentHandle, PageColors, Priority, Rect, RenderKey, RenderedImage, RenderedPage,
};

const TIMEOUT: Duration = Duration::from_secs(60);

struct Doc {
    handle: DocumentHandle,
    notes: Receiver<()>,
}

impl Doc {
    fn open(path: &Path) -> Doc {
        let (tx, notes) = mpsc::channel();
        let tx = Mutex::new(tx);
        let handle = DocumentHandle::open(
            path,
            Arc::new(move || {
                let _ = tx.lock().unwrap().send(());
            }),
        );
        let doc = Doc { handle, notes };
        doc.wait_for("ready", |h| {
            (h.status() != DocStatus::Loading).then_some(())
        });
        assert_eq!(doc.handle.status(), DocStatus::Ready);
        doc
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

    fn render(&self, page: u32, scale_milli: u32, colors: PageColors) -> RenderedPage {
        self.handle.set_page_colors(colors);
        let r = self
            .handle
            .render_wait(key(page, scale_milli))
            .expect("render");
        assert_eq!(r.colors, colors);
        r
    }
}

fn key(page: u32, scale_milli: u32) -> RenderKey {
    RenderKey {
        page,
        scale_milli,
        generation: 0,
    }
}

/// The RGB of pixel `(x, y)`.
fn px(rgb: &[u8], width: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * width + x) * 3) as usize;
    [rgb[i], rgb[i + 1], rgb[i + 2]]
}

fn lightness(p: [u8; 3]) -> u16 {
    u16::from(*p.iter().max().unwrap()) + u16::from(*p.iter().min().unwrap())
}

/// A 4×4 picture with a different colour in each quadrant.
fn quadrants() -> Vec<u8> {
    let mut rgb = Vec::new();
    for y in 0..4 {
        for x in 0..4 {
            let c: [u8; 3] = match (x < 2, y < 2) {
                (true, true) => [230, 40, 40],
                (false, true) => [40, 200, 60],
                (true, false) => [250, 240, 200],
                (false, false) => [20, 40, 160],
            };
            rgb.extend_from_slice(&c);
        }
    }
    rgb
}

/// Page 1: text on white. Page 2: a picture at (100, 100)-(300, 300) and text. Page 3: a white
/// picture covering the whole page (a "scan").
fn test_pdf() -> TempPdf {
    PdfBuilder::new()
        .page(300.0, 200.0, |p| {
            p.text(20.0, 60.0, 40.0, "HELLO");
        })
        .page(400.0, 400.0, |p| {
            p.image((100.0, 100.0, 300.0, 300.0), 4, 4, &quadrants());
            p.text(20.0, 50.0, 30.0, "FIGURE");
        })
        .page(200.0, 200.0, |p| {
            p.image((0.0, 0.0, 200.0, 200.0), 2, 2, &[255; 12]);
        })
        .write_temp("page-colors")
}

#[test]
fn a_dark_page_has_a_dark_background_and_light_text() {
    let pdf = test_pdf();
    let doc = Doc::open(pdf.path());
    let original = doc.render(0, 1000, PageColors::Original);
    let dark = doc.render(0, 1000, PageColors::Dark);
    assert_eq!((original.width, original.height), (dark.width, dark.height));
    let w = dark.width;
    // A corner is background.
    assert_eq!(px(&original.rgb, w, 2, 2), [255; 3]);
    assert_eq!(px(&dark.rgb, w, 2, 2), [DARK_BACKGROUND; 3]);
    // The darkest pixel of the original text is among the lightest of the dark page.
    let darkest = (0..original.rgb.len() / 3)
        .min_by_key(|i| original.rgb[i * 3])
        .unwrap();
    assert!(original.rgb[darkest * 3] < 60);
    assert!(
        dark.rgb[darkest * 3] > 200,
        "text pixel {}",
        dark.rgb[darkest * 3]
    );
    // No pixel of the dark page is lighter than the pressed range allows, or darker.
    assert!(
        dark.rgb
            .iter()
            .all(|v| (DARK_BACKGROUND..=0xe6).contains(v))
    );
}

#[test]
fn original_colours_render_as_before() {
    let pdf = test_pdf();
    let doc = Doc::open(pdf.path());
    let a = doc.render(0, 1000, PageColors::Original);
    assert_eq!(doc.handle.page_colors(), PageColors::Original);
    assert_eq!(px(&a.rgb, a.width, 2, 2), [255; 3]);
    // Rendering dark in between does not change the original rendering.
    doc.render(0, 1000, PageColors::Dark);
    let b = doc.render(0, 1000, PageColors::Original);
    assert_eq!(a, b);
}

#[test]
fn pictures_keep_their_colours_on_a_dark_page() {
    let pdf = test_pdf();
    let doc = Doc::open(pdf.path());
    let original = doc.render(1, 1000, PageColors::Original);
    let dark = doc.render(1, 1000, PageColors::Dark);
    let w = original.width;
    // Inside the picture (away from its edges): the same in both.
    for (x, y) in [(110, 110), (290, 110), (110, 290), (290, 290), (150, 250)] {
        let o = px(&original.rgb, w, x, y);
        assert_eq!(o, px(&dark.rgb, w, x, y), "({x}, {y})");
    }
    // The picture really is coloured there.
    assert_eq!(px(&original.rgb, w, 110, 110), [230, 40, 40]);
    // Outside: the background is dark.
    assert_eq!(px(&dark.rgb, w, 50, 350), [DARK_BACKGROUND; 3]);
    assert_eq!(px(&dark.rgb, w, 350, 200), [DARK_BACKGROUND; 3]);
}

#[test]
fn a_picture_covering_the_page_is_recoloured() {
    let pdf = test_pdf();
    let doc = Doc::open(pdf.path());
    let original = doc.render(2, 1000, PageColors::Original);
    let dark = doc.render(2, 1000, PageColors::Dark);
    assert_eq!(px(&original.rgb, original.width, 100, 100), [255; 3]);
    assert_eq!(px(&dark.rgb, dark.width, 100, 100), [DARK_BACKGROUND; 3]);
}

#[test]
fn a_dark_region_keeps_the_picture_part_and_recolours_the_rest() {
    let pdf = test_pdf();
    let doc = Doc::open(pdf.path());
    // Half over the picture's left edge (x = 100), at scale 2.
    let region = Rect::new(60.0, 120.0, 140.0, 160.0);
    let original: RenderedImage = doc
        .handle
        .render_region_wait(1, region, 2000, PageColors::Original)
        .unwrap();
    let dark = doc
        .handle
        .render_region_wait(1, region, 2000, PageColors::Dark)
        .unwrap();
    assert_eq!((dark.width, dark.height), (160, 80));
    let w = dark.width;
    // Left of x = 100 pt (pixel 80 in the region): recoloured background.
    assert_eq!(px(&original.rgb, w, 20, 40), [255; 3]);
    assert_eq!(px(&dark.rgb, w, 20, 40), [DARK_BACKGROUND; 3]);
    // Right of it: the picture, untouched.
    assert_eq!(px(&dark.rgb, w, 120, 40), px(&original.rgb, w, 120, 40));
    assert_eq!(px(&dark.rgb, w, 120, 40), [230, 40, 40]);
    // The region is the same as the cut of a whole dark page.
    let page = doc.render(1, 2000, PageColors::Dark);
    for (x, y) in [(20, 40), (120, 40), (79, 10), (81, 70)] {
        assert_eq!(
            px(&dark.rgb, w, x, y),
            px(&page.rgb, page.width, 120 + x, 240 + y),
            "({x}, {y})"
        );
    }
}

#[test]
fn polled_renders_carry_their_colours_and_new_colours_render_again() {
    let pdf = test_pdf();
    let doc = Doc::open(pdf.path());
    let k = key(0, 500);
    doc.handle.request_render(k, Priority::Visible);
    let first: RenderedPage = doc.wait_for("original render", |h| h.poll_rendered().pop());
    assert_eq!((first.key, first.colors), (k, PageColors::Original));
    // The same key in other colours is a new request, rendered again.
    doc.handle.set_page_colors(PageColors::Dark);
    doc.handle.request_render(k, Priority::Visible);
    let second = doc.wait_for("dark render", |h| h.poll_rendered().pop());
    assert_eq!((second.key, second.colors), (k, PageColors::Dark));
    assert_eq!(px(&second.rgb, second.width, 1, 1), [DARK_BACKGROUND; 3]);
}

/// Measures the conversion of one large page: A4 at 300 % zoom (4 device pixels per point).
/// Run with `cargo test -p refdesk-core --test page_colors -- --ignored --nocapture`.
#[test]
#[ignore = "timing report, not a check"]
fn timing_of_a_large_page() {
    let (w, h) = ((595.0f32 * 4.0) as u32, (842.0f32 * 4.0) as u32);
    let mut rgb: Vec<u8> = (0..w * h * 3).map(|i| (i % 251) as u8).collect();
    let keep = [PixelRect {
        x0: 200,
        y0: 300,
        x1: 1800,
        y1: 1500,
    }];
    let runs = 5;
    let started = Instant::now();
    for _ in 0..runs {
        recolor_dark(&mut rgb, w, h, 3, &keep);
    }
    let each = started.elapsed() / runs;
    println!(
        "recolor {w}x{h} ({:.1} Mpx), one picture kept: {each:?} per page",
        f64::from(w * h) / 1e6
    );
    assert!(lightness([rgb[0], rgb[1], rgb[2]]) <= 510);
}
