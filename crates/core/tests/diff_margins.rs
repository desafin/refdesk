//! Excluded margins in a comparison of real documents (`docs/SPEC.md` DIFF-11,
//! `docs/ARCHITECTURE.md` §4.9): headers that differ from page to page are only a change until
//! their edge of the page is left out.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use refdesk_core::diff::Margins;
use refdesk_core::testutil::{PdfBuilder, TempPdf};
use refdesk_core::{ChangeKind, DiffJob, DiffOptions, DiffResult, DocumentHandle, Prefs, Size};

const TIMEOUT: Duration = Duration::from_secs(60);

/// What a version of the test document looks like.
struct Version {
    /// Page size in pt.
    size: (f32, f32),
    /// Word that starts the running header of every page (the header differs on every page).
    header: &'static str,
    /// The same for the footer.
    footer: &'static str,
    /// The same for the column of line numbers at the left edge.
    numbers: &'static str,
    /// The changed word in the body of page 2 (index 1).
    body_word: &'static str,
}

const OLD: Version = Version {
    size: (612.0, 792.0),
    header: "Alpha",
    footer: "Draft",
    numbers: "ln",
    body_word: "quick",
};

/// Four pages. Each page has a header near the top (5 % of the page height), a footer near the
/// bottom (95 %), a line number column at the left edge (2 % of the width) and two body
/// paragraphs in the middle; all are placed as fractions of the page size.
fn document(v: &Version, stem: &str) -> TempPdf {
    let (w, h) = v.size;
    let mut pdf = PdfBuilder::new();
    let words = ["red", "green", "blue", "gold"];
    for (n, word) in words.iter().enumerate() {
        pdf = pdf.page(w, h, |p| {
            p.text(
                w * 0.12,
                h * 0.05,
                10.0,
                &format!("{} chapter {} title", v.header, word),
            );
            p.text(
                w * 0.12,
                h * 0.95,
                10.0,
                &format!("{} stamp {} here", v.footer, word),
            );
            p.text(w * 0.02, h * 0.4, 10.0, &format!("{}{}a", v.numbers, word));
            p.text(w * 0.02, h * 0.5, 10.0, &format!("{}{}b", v.numbers, word));
            let mid = if n == 1 { v.body_word } else { "steady" };
            p.text(
                w * 0.12,
                h * 0.4,
                10.0,
                &format!("The {mid} body text of page {n} stays here."),
            );
            p.text(
                w * 0.12,
                h * 0.5,
                10.0,
                &format!("A second paragraph of page {n} follows below."),
            );
        });
    }
    pdf.build_and_write(stem)
}

trait WriteTemp {
    fn build_and_write(&self, stem: &str) -> TempPdf;
}
impl WriteTemp for PdfBuilder {
    fn build_and_write(&self, stem: &str) -> TempPdf {
        self.write_temp(stem)
    }
}

fn open(file: &TempPdf) -> DocumentHandle {
    DocumentHandle::open(file.path(), Arc::new(|| {}))
}

fn compare(old: &Version, new: &Version, margins: Margins) -> Arc<DiffResult> {
    let (l, r) = (
        document(old, "margins-left"),
        document(new, "margins-right"),
    );
    let options = DiffOptions {
        margins,
        ..DiffOptions::default()
    };
    let job = DiffJob::start(open(&l), open(&r), options, Arc::new(|| {}));
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(result) = job.result() {
            return result.expect("comparison succeeds");
        }
        assert!(Instant::now() < deadline, "the comparison did not finish");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn only(top: f32, bottom: f32, left: f32, right: f32) -> Margins {
    Margins {
        top,
        bottom,
        left,
        right,
    }
}

fn new_version(size: (f32, f32)) -> Version {
    Version {
        size,
        header: "Beta",
        footer: "Final",
        numbers: "rr",
        body_word: "lazy",
    }
}

/// The lowest and highest point (as fractions of the page height) of the marks on `page`.
fn marked_rows(result: &DiffResult, page: usize) -> Vec<f32> {
    let h = result.right_page_sizes[page].h;
    result.right_marks[page]
        .iter()
        .map(|m| (m.rect.y0 + m.rect.y1) / 2.0 / h)
        .collect()
}

#[test]
fn without_margins_the_headers_footers_and_numbers_are_changes() {
    let result = compare(&OLD, &new_version(OLD.size), Margins::default());
    // Four headers, four footers, eight numbers and the body word: at least the headers show.
    assert!(
        result.changes.len() >= 9,
        "{} changes",
        result.changes.len()
    );
    assert!(
        result
            .changes
            .iter()
            .any(|c| c.right_text.contains("Beta chapter"))
    );
}

#[test]
fn a_top_margin_drops_the_headers_only() {
    let result = compare(&OLD, &new_version(OLD.size), only(0.1, 0.0, 0.0, 0.0));
    assert!(
        result
            .changes
            .iter()
            .all(|c| !c.right_text.contains("chapter") && !c.left_text.contains("chapter"))
    );
    assert!(
        result
            .changes
            .iter()
            .any(|c| c.right_text.contains("Final stamp")),
        "the footer is still compared"
    );
    // No mark in the top tenth of any page.
    for page in 0..4 {
        assert!(marked_rows(&result, page).iter().all(|&y| y > 0.1));
    }
}

#[test]
fn top_and_bottom_margins_drop_the_headers_and_footers() {
    let result = compare(&OLD, &new_version(OLD.size), only(0.1, 0.1, 0.0, 0.0));
    // The line numbers still differ: eight of them, left of the body.
    assert!(
        result
            .changes
            .iter()
            .all(|c| !c.right_text.contains("chapter") && !c.right_text.contains("stamp"))
    );
}

#[test]
fn margins_on_all_four_sides_leave_one_modified_paragraph() {
    let result = compare(&OLD, &new_version(OLD.size), only(0.1, 0.1, 0.1, 0.0));
    assert_eq!(result.changes.len(), 1, "{:?}", result.changes);
    let change = &result.changes[0];
    assert_eq!(change.kind, ChangeKind::Modified);
    assert!(change.left_text.contains("quick") && change.right_text.contains("lazy"));
    assert_eq!((result.added, result.removed, result.modified), (0, 0, 1));
    // Nothing is marked in an excluded band, on either side.
    for (marks, sizes) in [
        (&result.left_marks, &result.left_page_sizes),
        (&result.right_marks, &result.right_page_sizes),
    ] {
        for (page, page_marks) in marks.iter().enumerate() {
            let Size { w, h } = sizes[page];
            for m in page_marks {
                assert!(m.rect.y0 > 0.1 * h && m.rect.y1 < 0.9 * h, "{m:?}");
                assert!(m.rect.x0 > 0.1 * w, "{m:?}");
            }
        }
    }
}

#[test]
fn a_left_margin_alone_drops_the_line_numbers() {
    let without = compare(&OLD, &new_version(OLD.size), only(0.1, 0.1, 0.0, 0.0));
    let with = compare(&OLD, &new_version(OLD.size), only(0.1, 0.1, 0.1, 0.0));
    assert!(without.changes.len() > with.changes.len());
    assert_eq!(with.changes.len(), 1);
}

#[test]
fn documents_with_different_page_sizes_use_the_same_fractions() {
    let result = compare(&OLD, &new_version((400.0, 560.0)), only(0.1, 0.1, 0.1, 0.0));
    assert_eq!(result.changes.len(), 1, "{:?}", result.changes);
    assert_eq!(result.changes[0].kind, ChangeKind::Modified);
    assert_eq!(result.right_page_sizes[0], Size::new(400.0, 560.0));
    assert_eq!(result.left_page_sizes[0], Size::new(612.0, 792.0));
}

#[test]
fn out_of_range_margins_are_cut_down_before_they_are_used() {
    let wild = DiffOptions {
        margins: only(-1.0, 0.9, f32::NAN, 7.0),
        ..DiffOptions::default()
    };
    assert_eq!(wild.extract_options().margins, only(0.0, 0.4, 0.0, 0.4));
    // Two margins of 0.4 on opposite sides still leave the middle fifth of the page, and a
    // comparison with them runs to the end.
    let result = compare(&OLD, &new_version(OLD.size), only(0.9, 0.9, 0.9, 0.9));
    assert!(result.changes.len() <= 1);
}

#[test]
fn zero_margins_give_the_result_of_a_comparison_without_the_option() {
    let new = new_version(OLD.size);
    let a = compare(&OLD, &new, Margins::default());
    let b = compare(&OLD, &new, only(0.0, 0.0, 0.0, 0.0));
    assert_eq!(a.changes, b.changes);
    assert_eq!(a.left_marks, b.left_marks);
    assert_eq!(a.right_marks, b.right_marks);
    assert_eq!(a.sync, b.sync);
}

#[test]
fn the_margins_of_the_preferences_survive_a_save_and_old_files_still_load() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("prefs.json");
    let prefs = Prefs {
        diff_margins: only(0.06, 0.05, 0.0, 0.0),
        ..Prefs::default()
    };
    prefs.save(&file).unwrap();
    assert_eq!(Prefs::load(&file).diff_margins, only(0.06, 0.05, 0.0, 0.0));
    // A file written before the entry existed.
    std::fs::write(&file, br#"{"version": 1, "scroll_speed": 3.0}"#).unwrap();
    let old = Prefs::load(&file);
    assert_eq!(old.scroll_speed, Some(3.0));
    assert_eq!(old.diff_margins, Margins::default());
    // Damaged entries read as zero, values are cut down.
    std::fs::write(
        &file,
        br#"{"version": 1, "diff_margins": {"top": 0.9, "bottom": "x", "left": -2}}"#,
    )
    .unwrap();
    assert_eq!(Prefs::load(&file).diff_margins, only(0.4, 0.0, 0.0, 0.0));
    std::fs::write(&file, br#"{"version": 1, "diff_margins": 5}"#).unwrap();
    assert!(Prefs::load(&file).diff_margins.is_zero());
    // Nothing is written for zero margins.
    Prefs::default().save(&file).unwrap();
    assert!(
        !std::fs::read_to_string(&file)
            .unwrap()
            .contains("diff_margins")
    );
}
