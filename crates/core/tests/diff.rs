//! Integration tests of document comparison (`docs/ARCHITECTURE.md` §4.9, `docs/SPEC.md` §8.5
//! steps 4–7): document pairs laid out by `testutil::flow_document`, opened through the engine
//! and compared with `DiffJob`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use refdesk_core::diff::{DiffInput, PageSource, diff_tokens};
use refdesk_core::testutil::{FlowOptions, PdfBuilder, TempPdf, flow_document};
use refdesk_core::{
    ChangeKind, DiffJob, DiffLimits, DiffOptions, DiffResult, DocStatus, DocumentHandle, Location,
    MarkKind, Notifier, PageText, Rect, Side,
};

/// Deadline for anything the engine or a comparison does. Far above what a slow machine needs.
const TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Waits (without a fixed sleep) until `cond` holds.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A notifier that counts its calls and sends a message for each.
fn counting_notifier() -> (Notifier, mpsc::Receiver<()>, Arc<AtomicUsize>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let notify: Notifier = Arc::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
        let _ = tx.lock().unwrap().send(());
    });
    (notify, rx, calls)
}

fn open(path: &Path) -> DocumentHandle {
    DocumentHandle::open(path, Arc::new(|| {}))
}

/// Waits for the result of `job`, driven by its notifier.
fn finish(job: &DiffJob, notes: &mpsc::Receiver<()>) -> Result<Arc<DiffResult>, String> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(r) = job.result() {
            return r;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match notes.recv_timeout(left) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => panic!("comparison did not finish"),
            Err(RecvTimeoutError::Disconnected) => panic!("notifier gone"),
        }
    }
}

/// An open document pair with the page texts of both sides, for checking marks.
struct Pair {
    _files: (TempPdf, TempPdf),
    left: DocumentHandle,
    right: DocumentHandle,
    result: Arc<DiffResult>,
}

impl Pair {
    fn page_text(&self, side: Side, page: u32) -> Arc<PageText> {
        let doc = match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        };
        doc.page_text_wait(page).unwrap()
    }

    /// The non-space characters under the marks of `kinds` on `side`, page by page, as one
    /// string with the words of different marks separated by single spaces.
    fn marked_text(&self, side: Side, kinds: &[MarkKind]) -> String {
        let pages = match side {
            Side::Left => &self.result.left_marks,
            Side::Right => &self.result.right_marks,
        };
        let mut words: Vec<String> = Vec::new();
        for (p, marks) in pages.iter().enumerate() {
            let rects: Vec<Rect> = marks
                .iter()
                .filter(|m| kinds.contains(&m.kind))
                .map(|m| m.rect)
                .collect();
            if rects.is_empty() {
                continue;
            }
            let pt = self.page_text(side, p as u32);
            let mut cur = String::new();
            for line in &pt.lines {
                for c in &pt.chars[line.chars.start as usize..line.chars.end as usize] {
                    let b = c.quad.bounds();
                    let (cx, cy) = ((b.x0 + b.x1) / 2.0, (b.y0 + b.y1) / 2.0);
                    let inside = rects
                        .iter()
                        .any(|r| cx >= r.x0 && cx <= r.x1 && cy >= r.y0 && cy <= r.y1);
                    if inside && !c.ch.is_whitespace() {
                        cur.push(c.ch);
                    } else if !cur.is_empty() {
                        words.push(std::mem::take(&mut cur));
                    }
                }
                // A line end ends a word.
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
        }
        words.join(" ")
    }
}

/// Lays out both paragraph lists, compares them through `DiffJob` and checks the result's
/// invariants.
fn compare_with(
    left: &[String],
    right: &[String],
    lopts: &FlowOptions,
    ropts: &FlowOptions,
) -> Pair {
    let lf = flow_document(left, lopts).write_temp("diff-left");
    let rf = flow_document(right, ropts).write_temp("diff-right");
    compare_files(lf, rf)
}

fn compare(left: &[String], right: &[String]) -> Pair {
    compare_with(
        left,
        right,
        &FlowOptions::default(),
        &FlowOptions::default(),
    )
}

fn compare_files(lf: TempPdf, rf: TempPdf) -> Pair {
    let left = open(lf.path());
    let right = open(rf.path());
    let (notify, notes, _) = counting_notifier();
    let job = DiffJob::start(left.clone(), right.clone(), DiffOptions::default(), notify);
    let result = finish(&job, &notes).expect("comparison succeeds");
    assert_eq!(job.progress(), 1.0);
    check_invariants(&result);
    Pair {
        _files: (lf, rf),
        left,
        right,
        result,
    }
}

/// The stacked y of `loc` (pages one below the other).
fn stacked(sizes: &[refdesk_core::Size], loc: Location) -> f64 {
    let above: f64 = sizes[..loc.page as usize]
        .iter()
        .map(|s| f64::from(s.h))
        .sum();
    above + f64::from(loc.y)
}

/// The properties every result must have.
fn check_invariants(res: &DiffResult) {
    for (pages, sizes) in [
        (&res.left_marks, &res.left_page_sizes),
        (&res.right_marks, &res.right_page_sizes),
    ] {
        assert_eq!(pages.len(), sizes.len());
        for (p, marks) in pages.iter().enumerate() {
            let s = sizes[p];
            for m in marks {
                let r = m.rect;
                assert!(r.x1 > r.x0 && r.y1 > r.y0, "empty mark {m:?}");
                assert!(
                    r.x0 >= 0.0 && r.y0 >= 0.0 && r.x1 <= s.w && r.y1 <= s.h,
                    "mark outside page {p}: {m:?}"
                );
                assert!((m.change as usize) < res.changes.len(), "{m:?}");
            }
        }
    }
    for i in 0..res.changes.len() {
        let has = res
            .left_marks
            .iter()
            .chain(&res.right_marks)
            .flatten()
            .any(|m| m.change as usize == i);
        assert!(has, "change {i} has no mark");
    }
    let n = |k| res.changes.iter().filter(|c| c.kind == k).count() as u32;
    assert_eq!(res.added, n(ChangeKind::Added));
    assert_eq!(res.removed, n(ChangeKind::Removed));
    assert_eq!(res.modified, n(ChangeKind::Modified));

    for w in res.sync.windows(2) {
        assert!(
            stacked(&res.left_page_sizes, w[1].0) > stacked(&res.left_page_sizes, w[0].0),
            "left not increasing: {w:?}"
        );
        assert!(
            stacked(&res.right_page_sizes, w[1].1) > stacked(&res.right_page_sizes, w[0].1),
            "right not increasing: {w:?}"
        );
    }
    let origin = Location::new(0, 0.0, 0.0);
    assert_eq!(res.sync.first(), Some(&(origin, origin)));
    let end = |sizes: &[refdesk_core::Size]| {
        Location::new(sizes.len() as u32 - 1, 0.0, sizes.last().unwrap().h)
    };
    assert_eq!(
        res.sync.last(),
        Some(&(end(&res.left_page_sizes), end(&res.right_page_sizes)))
    );
}

fn kinds(res: &DiffResult) -> Vec<ChangeKind> {
    res.changes.iter().map(|c| c.kind).collect()
}

/// Deterministic pseudo-random numbers.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const VOCABULARY: &[&str] = &[
    "the",
    "engine",
    "renders",
    "visible",
    "pages",
    "before",
    "background",
    "work",
    "and",
    "keeps",
    "extracted",
    "text",
    "while",
    "document",
    "stays",
    "open",
    "worker",
    "thread",
    "owns",
    "each",
    "request",
    "carries",
    "generation",
    "number",
    "results",
    "from",
    "older",
    "generations",
    "are",
    "dropped",
    "layout",
    "places",
    "vertically",
    "with",
    "small",
    "gap",
    "between",
    "of",
];

const OTHER_VOCABULARY: &[&str] = &[
    "a", "river", "crosses", "valley", "where", "farmers", "grow", "wheat", "barley", "under",
    "mild", "summer", "sun", "cattle", "graze", "on", "hills", "near", "old", "stone", "bridges",
    "built", "by", "monks", "the", "of",
];

/// `n` distinct one-sentence paragraphs of 20 to 90 lowercase words from `words`, so that page
/// breaks fall inside paragraphs and continuation lines start with a lowercase letter.
fn paragraphs_from(n: usize, seed: u64, tag: &str, words: &[&str]) -> Vec<String> {
    let mut rng = Rng(seed);
    (0..n)
        .map(|i| {
            let len = 20 + (rng.next() % 71) as usize;
            let mut out = vec!["Paragraph".to_owned(), format!("{tag}{i}")];
            out.extend(
                (0..len).map(|_| words[(rng.next() % words.len() as u64) as usize].to_owned()),
            );
            format!("{}.", out.join(" "))
        })
        .collect()
}

fn paragraphs(n: usize) -> Vec<String> {
    paragraphs_from(n, 0x9E37_79B9_7F4A_7C15, "tag", VOCABULARY)
}

/// A paragraph about something else, sharing only words such as "the" and "of".
fn foreign_paragraph(words: usize) -> String {
    paragraphs_from(1, 42, "foreign", OTHER_VOCABULARY)[0]
        .split_whitespace()
        .take(words)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Header with a version string and footer with the page number.
fn with_header_footer(version: &'static str) -> FlowOptions {
    FlowOptions::default()
        .header(move |_| format!("Refdesk Interface Specification {version}"))
        .footer(|n| format!("Page {n}"))
}

/// The non-space characters of `text`, words separated by single spaces.
fn words_of(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Reads every page of `doc` (waiting until it is open).
fn page_sources(doc: &DocumentHandle) -> Vec<PageSource> {
    wait_until("the document to open", || {
        doc.status() != DocStatus::Loading
    });
    let info = doc.info().expect("info when ready");
    info.page_sizes
        .iter()
        .enumerate()
        .map(|(page, &size)| PageSource {
            text: doc.page_text_wait(page as u32).unwrap(),
            size,
        })
        .collect()
}

fn diff_input(doc: &DocumentHandle) -> DiffInput {
    DiffInput::from_pages(
        &page_sources(doc),
        &DiffOptions::default().extract_options(),
    )
}

/// The top left of the first character of paragraph `p`.
fn paragraph_start(input: &DiffInput, p: usize) -> Location {
    let pos = input.tokens.paragraphs[p].start;
    let b = input.tokens.pages[pos.page as usize].chars[pos.ch as usize]
        .quad
        .bounds();
    Location::new(pos.page, b.x0, b.y0)
}

fn close(a: Location, b: Location) -> bool {
    a.page == b.page && (a.y - b.y).abs() < 0.01
}

// ---------------------------------------------------------------------------------------------
// Correctness
// ---------------------------------------------------------------------------------------------

#[test]
fn identical_documents_have_no_changes_and_no_marks() {
    let input = paragraphs(30);
    let pair = compare(&input, &input);
    let res = &pair.result;
    assert!(res.changes.is_empty(), "{:?}", res.changes);
    assert!(
        res.left_marks
            .iter()
            .chain(&res.right_marks)
            .all(Vec::is_empty)
    );
    assert!(!res.approximate);
    // Starts and ends of all 30 paragraph pairs, plus the document start and end.
    assert!(res.sync.len() >= 30, "{}", res.sync.len());
}

#[test]
fn paragraph_added_near_the_front_is_one_added_change_at_its_place() {
    let original = paragraphs(30);
    let mut changed = original.clone();
    let inserted = "Inserted paragraph that pushes everything after it a few lines down and onto \
                    later pages so that page numbers no longer line up between the versions."
        .to_owned();
    changed.insert(1, inserted.clone());
    let pair = compare(&original, &changed);
    let res = &pair.result;
    assert_eq!(kinds(res), [ChangeKind::Added]);
    let c = &res.changes[0];
    assert!(c.left.is_none());
    assert_eq!(c.right_text, inserted);

    // The marks cover exactly the inserted paragraph, on the first page of the right document.
    assert!(res.left_marks.iter().all(Vec::is_empty));
    assert_eq!(
        pair.marked_text(Side::Right, &[MarkKind::Added]),
        words_of(&inserted)
    );
    assert!(!res.right_marks[0].is_empty());
    assert!(res.right_marks[1..].iter().all(Vec::is_empty));

    // The anchors: the end of paragraph 0 on the left, the inserted paragraph on the right.
    let l = diff_input(&pair.left);
    let r = diff_input(&pair.right);
    assert!(close(c.anchor.1, paragraph_start(&r, 1)));
    let end0 = l.tokens.paragraphs[0].end;
    assert_eq!(c.anchor.0.page, end0.page);
    assert!(c.anchor.0.y > paragraph_start(&l, 0).y);
    assert!(c.anchor.0.y < paragraph_start(&l, 1).y);
}

#[test]
fn removed_paragraph_is_one_removed_change() {
    let original = paragraphs(30);
    let mut changed = original.clone();
    let gone = changed.remove(12);
    let pair = compare(&original, &changed);
    let res = &pair.result;
    assert_eq!(kinds(res), [ChangeKind::Removed]);
    assert!(res.right_marks.iter().all(Vec::is_empty));
    assert_eq!(
        pair.marked_text(Side::Left, &[MarkKind::Removed]),
        words_of(&gone)
    );
    assert!(res.changes[0].right.is_none());
    assert!(gone.starts_with(res.changes[0].left_text.trim_end_matches('…')));
}

#[test]
fn changed_word_is_one_modification_marked_on_that_word_and_paragraph() {
    let original = paragraphs(30);
    let mut changed = original.clone();
    let target = 7;
    let mut words: Vec<&str> = original[target].split_whitespace().collect();
    let middle = words.len() / 2;
    let old_word = words[middle].to_owned();
    words[middle] = "replacement";
    changed[target] = words.join(" ");

    let pair = compare(&original, &changed);
    let res = &pair.result;
    assert_eq!(kinds(res), [ChangeKind::Modified]);
    assert_eq!(
        pair.marked_text(Side::Left, &[MarkKind::ModifiedWord]),
        old_word
    );
    assert_eq!(
        pair.marked_text(Side::Right, &[MarkKind::ModifiedWord]),
        "replacement"
    );
    assert_eq!(
        pair.marked_text(Side::Left, &[MarkKind::ModifiedBlock]),
        words_of(&original[target])
    );
    assert_eq!(
        pair.marked_text(Side::Right, &[MarkKind::ModifiedBlock]),
        words_of(&changed[target])
    );
    let c = &res.changes[0];
    assert!(original[target].starts_with(c.left_text.trim_end_matches('…')));
    assert!(changed[target].starts_with(c.right_text.trim_end_matches('…')));
}

#[test]
fn added_word_is_a_modification_without_word_marks_on_the_left() {
    let original = paragraphs(30);
    let mut changed = original.clone();
    let mut words: Vec<&str> = original[4].split_whitespace().collect();
    words.insert(6, "additionally");
    changed[4] = words.join(" ");
    let pair = compare(&original, &changed);
    assert_eq!(kinds(&pair.result), [ChangeKind::Modified]);
    assert_eq!(pair.marked_text(Side::Left, &[MarkKind::ModifiedWord]), "");
    assert_eq!(
        pair.marked_text(Side::Right, &[MarkKind::ModifiedWord]),
        "additionally"
    );
}

#[test]
fn three_consecutive_added_paragraphs_are_one_change() {
    let original = paragraphs(30);
    let mut changed = original.clone();
    let new = paragraphs_from(3, 7, "new", VOCABULARY);
    for (k, p) in new.iter().enumerate() {
        changed.insert(10 + k, p.clone());
    }
    let pair = compare(&original, &changed);
    assert_eq!(kinds(&pair.result), [ChangeKind::Added]);
    assert_eq!(
        pair.marked_text(Side::Right, &[MarkKind::Added]),
        words_of(&new.join(" "))
    );
}

#[test]
fn three_separate_changes_come_in_document_order() {
    let original = paragraphs(40);
    let mut changed = original.clone();
    changed[30] = changed[30].replacen("Paragraph", "Section", 1);
    changed.remove(20);
    changed.insert(5, "A brand new short paragraph.".to_owned());
    let pair = compare(&original, &changed);
    let res = &pair.result;
    assert_eq!(
        kinds(res),
        [ChangeKind::Added, ChangeKind::Removed, ChangeKind::Modified]
    );
    let r = &res.right_page_sizes;
    let ys: Vec<f64> = res.changes.iter().map(|c| stacked(r, c.anchor.1)).collect();
    assert!(ys.windows(2).all(|w| w[0] < w[1]), "{ys:?}");
}

#[test]
fn changes_at_the_very_start_and_end() {
    let original = paragraphs(20);
    let mut changed = original.clone();
    changed.insert(0, "Preface added before everything else.".to_owned());
    changed.pop();
    let pair = compare(&original, &changed);
    let res = &pair.result;
    assert_eq!(kinds(res), [ChangeKind::Added, ChangeKind::Removed]);
    // Nothing comes before the addition: its left anchor is the document start.
    assert_eq!(res.changes[0].anchor.0, Location::new(0, 0.0, 0.0));
    assert_eq!(
        pair.marked_text(Side::Left, &[MarkKind::Removed]),
        words_of(&original[19])
    );
}

#[test]
fn one_empty_document_and_two_empty_documents() {
    let input = paragraphs(10);
    let pair = compare(&[], &input);
    assert_eq!(kinds(&pair.result), [ChangeKind::Added]);
    assert_eq!(
        pair.marked_text(Side::Right, &[MarkKind::Added]),
        words_of(&input.join(" "))
    );
    let pair = compare(&input, &[]);
    assert_eq!(kinds(&pair.result), [ChangeKind::Removed]);
    let pair = compare(&[], &[]);
    assert!(pair.result.changes.is_empty());
    assert_eq!(pair.result.sync.len(), 2);
}

// ---------------------------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------------------------

#[test]
fn different_paragraph_boundaries_alone_are_no_change() {
    let original = paragraphs(20);
    let mut changed = original.clone();
    // Paragraph 5 becomes two paragraphs; 9 and 10 become one.
    let words: Vec<&str> = original[5].split_whitespace().collect();
    let (a, b) = words.split_at(words.len() / 2);
    changed[5] = a.join(" ");
    changed.insert(6, b.join(" "));
    let joined = format!("{} {}", changed[10], changed[11]);
    changed[10] = joined;
    changed.remove(11);
    let pair = compare(&original, &changed);
    assert!(pair.result.changes.is_empty(), "{:?}", pair.result.changes);
}

#[test]
fn different_line_breaks_and_page_breaks_alone_are_no_change() {
    let input = paragraphs(30);
    let narrow = FlowOptions {
        page_width: 500.0,
        font_size: 10.0,
        line_height: 13.0,
        ..FlowOptions::default()
    };
    let pair = compare_with(&input, &input, &FlowOptions::default(), &narrow);
    assert_ne!(
        pair.result.left_page_sizes.len(),
        pair.result.right_page_sizes.len()
    );
    assert!(pair.result.changes.is_empty(), "{:?}", pair.result.changes);
}

#[test]
fn hyphenated_line_ends_alone_are_no_change() {
    // "speci-" / "fication" and "well-" / "known" broken at line ends on the left, whole on the
    // right.
    let left = PdfBuilder::new().page(612.0, 792.0, |p| {
        p.text(72.0, 100.0, 12.0, "Intro line ends here.");
        p.text(72.0, 140.0, 12.0, "The speci-");
        p.text(72.0, 156.0, 12.0, "fication is a well-");
        p.text(72.0, 172.0, 12.0, "known document.");
    });
    let right = PdfBuilder::new().page(612.0, 792.0, |p| {
        p.text(72.0, 100.0, 12.0, "Intro line ends here.");
        p.text(72.0, 140.0, 12.0, "The specification is a well-known");
        p.text(72.0, 156.0, 12.0, "document.");
    });
    let pair = compare_files(left.write_temp("hyph-l"), right.write_temp("hyph-r"));
    assert!(pair.result.changes.is_empty(), "{:?}", pair.result.changes);
}

#[test]
fn version_string_in_the_header_and_page_numbers_are_no_change() {
    let input = paragraphs(30);
    let pair = compare_with(
        &input,
        &input,
        &with_header_footer("v1.2"),
        &with_header_footer("v2.0-rc1"),
    );
    assert!(pair.result.changes.is_empty(), "{:?}", pair.result.changes);
}

#[test]
fn unrelated_paragraphs_are_removed_and_added_not_modified() {
    let original = paragraphs(20);
    let mut changed = original.clone();
    changed[8] = foreign_paragraph(40);
    let pair = compare(&original, &changed);
    assert_eq!(
        kinds(&pair.result),
        [ChangeKind::Removed, ChangeKind::Added]
    );
}

#[test]
fn unrelated_documents_finish_within_the_time_limits() {
    let left = paragraphs_from(150, 1, "left", VOCABULARY);
    let right = paragraphs_from(150, 2, "right", OTHER_VOCABULARY);
    let words: usize = left.iter().map(|p| p.split_whitespace().count()).sum();
    assert!(words > 5000, "{words}");
    let start = Instant::now();
    let pair = compare(&left, &right);
    let res = &pair.result;
    assert!(!res.changes.is_empty());
    // Everything on the left is gone and everything on the right is new.
    assert!(res.changes.iter().all(|c| c.kind != ChangeKind::Modified));
    let limits = DiffLimits::default();
    let budget = limits.paragraph_timeout + limits.total_word_timeout + TIMEOUT;
    assert!(start.elapsed() < budget);
}

// ---------------------------------------------------------------------------------------------
// Synchronisation and location mapping
// ---------------------------------------------------------------------------------------------

#[test]
fn equal_paragraph_starts_map_onto_each_other() {
    let original = paragraphs(30);
    let mut changed = original.clone();
    changed.insert(
        3,
        "Something new in between the third and fourth paragraph.".to_owned(),
    );
    let pair = compare(&original, &changed);
    let l = diff_input(&pair.left);
    let r = diff_input(&pair.right);
    for p in [0, 2, 3, 10, 29] {
        let rp = if p >= 3 { p + 1 } else { p };
        let a = paragraph_start(&l, p);
        let b = paragraph_start(&r, rp);
        let mapped = pair.result.map_location(Side::Left, a);
        assert!(close(mapped, b), "paragraph {p}: {mapped:?} vs {b:?}");
        let back = pair.result.map_location(Side::Right, b);
        assert!(close(back, a), "paragraph {p}: {back:?} vs {a:?}");
    }
}

#[test]
fn comparing_a_document_with_itself_maps_every_location_to_itself() {
    let input = paragraphs(30);
    let pair = compare(&input, &input);
    let pages = pair.result.left_page_sizes.len() as u32;
    for page in 0..pages {
        for y in [0.0, 13.7, 100.0, 400.25, 792.0] {
            let loc = Location::new(page, 50.0, y);
            assert_eq!(pair.result.map_location(Side::Left, loc), loc);
            assert_eq!(pair.result.map_location(Side::Right, loc), loc);
        }
    }
}

#[test]
fn a_page_of_new_paragraphs_moves_later_locations_one_page_on() {
    // One-line paragraphs take 16 pt plus an 8 pt gap; 27 of them fill a page exactly
    // (baselines 84, 108, …, 708 with the bottom margin at 720).
    let line = |i: usize| format!("Short paragraph number {i} of the document.");
    let original: Vec<String> = (0..120).map(line).collect();
    let mut changed: Vec<String> = (0..27)
        .map(|i| format!("Inserted line {i} here."))
        .collect();
    changed.extend(original.iter().cloned());
    let pair = compare(&original, &changed);
    let res = &pair.result;
    assert_eq!(kinds(res), [ChangeKind::Added]);
    assert_eq!(res.right_page_sizes.len(), res.left_page_sizes.len() + 1);
    for loc in [
        Location::new(1, 72.0, 90.0),
        Location::new(2, 72.0, 300.0),
        Location::new(3, 72.0, 500.0),
    ] {
        let m = res.map_location(Side::Left, loc);
        assert_eq!(m.page, loc.page + 1, "{loc:?} → {m:?}");
        assert!((m.y - loc.y).abs() < 0.01, "{loc:?} → {m:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------------------------

#[test]
fn progress_never_goes_down_reaches_one_and_the_notifier_is_called() {
    let input = paragraphs(60);
    let lf = flow_document(&input, &FlowOptions::default()).write_temp("prog-l");
    let rf = flow_document(&input[1..], &FlowOptions::default()).write_temp("prog-r");
    let (notify, _notes, calls) = counting_notifier();
    // Not waiting for the documents to open: the job must wait itself.
    let job = DiffJob::start(
        open(lf.path()),
        open(rf.path()),
        DiffOptions::default(),
        notify,
    );
    let mut seen = vec![job.progress()];
    wait_until("the comparison to finish", || {
        seen.push(job.progress());
        job.result().is_some()
    });
    seen.push(job.progress());
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
    assert!(seen.iter().all(|p| (0.0..=1.0).contains(p)));
    assert_eq!(job.progress(), 1.0);
    let res = job.result().unwrap().unwrap();
    assert_eq!(kinds(&res), [ChangeKind::Removed]);
    // Progress steps and the end.
    wait_until("the final notification", || {
        calls.load(Ordering::SeqCst) >= 2
    });
}

#[test]
fn dropping_the_job_ends_the_thread() {
    let input = paragraphs(80);
    let lf = flow_document(&input, &FlowOptions::default()).write_temp("drop-l");
    let rf = flow_document(&input, &FlowOptions::default()).write_temp("drop-r");

    // The first notification parks the thread until the test lets it go, so the comparison is
    // certainly still running when the job is dropped.
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
    let job = DiffJob::start(
        open(lf.path()),
        open(rf.path()),
        DiffOptions::default(),
        notify,
    );
    entered_rx
        .recv_timeout(TIMEOUT)
        .expect("first notification");
    assert!(
        Arc::strong_count(&token) > 1,
        "the thread holds the notifier"
    );
    let before = job.progress();
    let started = Instant::now();
    drop(job); // must not block
    assert!(started.elapsed() < Duration::from_secs(1));
    go_tx.send(()).unwrap();
    wait_until("the comparison thread to end", || {
        Arc::strong_count(&token) == 1
    });
    assert!(before < 0.5, "{before}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no notification after the drop"
    );
}

#[test]
fn a_missing_file_ends_with_an_error() {
    let input = paragraphs(3);
    let ok = flow_document(&input, &FlowOptions::default()).write_temp("present");
    let missing = std::env::temp_dir().join("refdesk-diff-test-no-such-file.pdf");
    for (l, r) in [
        (missing.as_path(), ok.path()),
        (ok.path(), missing.as_path()),
    ] {
        let (notify, notes, _) = counting_notifier();
        let job = DiffJob::start(open(l), open(r), DiffOptions::default(), notify);
        let err = finish(&job, &notes).expect_err("missing file");
        assert!(err.contains("refdesk-diff-test-no-such-file"), "{err}");
        assert_eq!(job.progress(), 1.0);
    }
}

// ---------------------------------------------------------------------------------------------
// Measurements for the WP-61 report
// ---------------------------------------------------------------------------------------------

/// A large real document compared with itself. Run with
/// `REFDESK_BENCH_PDF=… cargo test -p refdesk-core --release --test diff -- --ignored --nocapture`.
#[test]
#[ignore = "needs a large PDF outside the repository"]
fn large_document_against_itself() {
    let Ok(path) = std::env::var("REFDESK_BENCH_PDF") else {
        eprintln!("REFDESK_BENCH_PDF not set");
        return;
    };
    let left = open(Path::new(&path));
    let right = open(Path::new(&path));
    let (notify, notes, _) = counting_notifier();
    let start = Instant::now();
    let job = DiffJob::start(left.clone(), right.clone(), DiffOptions::default(), notify);
    let res = finish(&job, &notes).unwrap();
    let total = start.elapsed();
    check_invariants(&res);
    println!(
        "{} pages: DiffJob {total:?}, changes {}, sync points {}, approximate {}",
        res.left_page_sizes.len(),
        res.changes.len(),
        res.sync.len(),
        res.approximate
    );
    // The comparison step alone, on texts the engines already hold.
    let (l, r) = (diff_input(&left), diff_input(&right));
    let start = Instant::now();
    let again = diff_tokens(&l, &r, &DiffLimits::default());
    println!(
        "diff_tokens alone: {:?} ({} paragraphs, {} words)",
        start.elapsed(),
        l.tokens.paragraphs.len(),
        l.tokens.words.len()
    );
    assert_eq!(again.changes.len(), 0);
    assert_eq!(res.changes.len(), 0);
}

/// A generated pair of 3000 paragraphs with 30 changes.
#[test]
#[ignore = "slow in debug builds; run in release for the report"]
fn large_generated_pair_with_thirty_changes() {
    let original = paragraphs(3000);
    let mut changed = original.clone();
    let mut expected = Vec::new();
    // Ten of each kind, spread over the document, applied from the back so indices hold.
    for k in (0..30).rev() {
        let at = 50 + k * 98;
        match k % 3 {
            0 => {
                changed.insert(
                    at,
                    format!("Freshly added paragraph number {k} with new text."),
                );
                expected.push(ChangeKind::Added);
            }
            1 => {
                changed.remove(at);
                expected.push(ChangeKind::Removed);
            }
            _ => {
                changed[at] = changed[at].replacen("Paragraph", "Passage", 1);
                expected.push(ChangeKind::Modified);
            }
        }
    }
    expected.reverse();
    let lf = flow_document(&original, &with_header_footer("v1")).write_temp("big-l");
    let rf = flow_document(&changed, &with_header_footer("v2")).write_temp("big-r");
    let (left, right) = (open(lf.path()), open(rf.path()));
    let (notify, notes, _) = counting_notifier();
    let start = Instant::now();
    let job = DiffJob::start(left.clone(), right.clone(), DiffOptions::default(), notify);
    let res = finish(&job, &notes).unwrap();
    let total = start.elapsed();
    check_invariants(&res);
    let (l, r) = (diff_input(&left), diff_input(&right));
    let start = Instant::now();
    let _ = diff_tokens(&l, &r, &DiffLimits::default());
    println!(
        "{} / {} pages: DiffJob {total:?}, diff_tokens alone {:?}, changes {} \
         (added {}, removed {}, modified {}), approximate {}",
        res.left_page_sizes.len(),
        res.right_page_sizes.len(),
        start.elapsed(),
        res.changes.len(),
        res.added,
        res.removed,
        res.modified,
        res.approximate
    );
    assert_eq!(kinds(&res), expected);
}
