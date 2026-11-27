//! Integration tests of diff token extraction (`docs/ARCHITECTURE.md` §4.9, `docs/SPEC.md` §8.5
//! steps 1–3) on documents laid out by `testutil::flow_document` and read through the engine.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use refdesk_core::diff::{DocTokens, ExtractOptions, PageSource, extract, noise};
use refdesk_core::testutil::{FlowOptions, flow_document};
use refdesk_core::{DocStatus, DocumentHandle};
use xxhash_rust::xxh3::xxh3_64;

/// Deadline for anything the engine does. Far above what a slow machine needs.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Opens `path`, waits until it is ready and reads the text of every page.
fn page_sources(path: &Path) -> Vec<PageSource> {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let handle = DocumentHandle::open(
        path,
        Arc::new(move || {
            let _ = tx.lock().unwrap().send(());
        }),
    );
    let deadline = Instant::now() + TIMEOUT;
    while handle.status() == DocStatus::Loading {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => panic!("timed out opening {}", path.display()),
            Err(RecvTimeoutError::Disconnected) => panic!("worker gone"),
        }
    }
    assert_eq!(handle.status(), DocStatus::Ready);
    let info = handle.info().expect("info when ready");
    info.page_sizes
        .iter()
        .enumerate()
        .map(|(page, &size)| PageSource {
            text: handle.page_text_wait(page as u32).unwrap(),
            size,
        })
        .collect()
}

/// Lays `paragraphs` out with `opts` and extracts the result with default options.
fn extract_flow(stem: &str, paragraphs: &[String], opts: &FlowOptions) -> (usize, DocTokens) {
    let pdf = flow_document(paragraphs, opts).write_temp(stem);
    let pages = page_sources(pdf.path());
    let tokens = extract(&pages, &ExtractOptions::default());
    (pages.len(), tokens)
}

/// Header with a version string and footer with the page number.
fn with_header_footer(version: &'static str) -> FlowOptions {
    FlowOptions::default()
        .header(move |_| format!("Refdesk Interface Specification {version}"))
        .footer(|n| format!("Page {n}"))
}

/// `n` distinct one-sentence paragraphs of 20 to 90 lowercase words, so that the page breaks
/// fall inside paragraphs and their continuation lines start with a lowercase letter.
fn paragraphs(n: usize) -> Vec<String> {
    const WORDS: &[&str] = &[
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
    ];
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    (0..n)
        .map(|i| {
            let len = 20 + (next() % 71) as usize;
            let mut words = vec!["Paragraph".to_owned(), format!("tag{i}")];
            words
                .extend((0..len).map(|_| WORDS[(next() % WORDS.len() as u64) as usize].to_owned()));
            format!("{}.", words.join(" "))
        })
        .collect()
}

/// The paragraph key `extract` must produce for an input paragraph (ASCII, no hyphenation).
fn expected_key(paragraph: &str) -> u64 {
    xxh3_64(
        paragraph
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .as_bytes(),
    )
}

fn keys(tokens: &DocTokens) -> Vec<u64> {
    tokens.paragraphs.iter().map(|p| p.key).collect()
}

#[test]
fn header_and_footer_are_removed_and_paragraphs_match_the_input() {
    let input = paragraphs(30);
    let (page_count, tokens) = extract_flow("thirty", &input, &with_header_footer("v1.2"));
    assert!(page_count >= 5, "only {page_count} pages");

    assert_eq!(tokens.removed_lines.len(), page_count * 2);
    for r in &tokens.removed_lines {
        assert!(
            r.text == "Refdesk Interface Specification v1.2"
                || r.text == format!("Page {}", r.page + 1),
            "{r:?}"
        );
    }
    assert_eq!(tokens.paragraphs.len(), 30);
    let expected: Vec<u64> = input.iter().map(|p| expected_key(p)).collect();
    assert_eq!(keys(&tokens), expected);

    // Some paragraphs run over a page break and were joined back.
    let spanning = tokens
        .paragraphs
        .iter()
        .filter(|p| p.start.page != p.end.page)
        .count();
    assert!(spanning > 0, "no paragraph crosses a page");

    // Every word points at its own characters on the page.
    for w in &tokens.words {
        assert_eq!(tokens.word_text(w), w.key);
        assert!(w.tail.is_none());
    }
    let word_count: usize = input.iter().map(|p| p.split_whitespace().count()).sum();
    assert_eq!(tokens.words.len(), word_count);
}

#[test]
fn inserted_paragraph_leaves_the_other_keys_in_order() {
    let original = paragraphs(30);
    let mut changed = original.clone();
    changed.insert(
        1,
        "Inserted paragraph that pushes everything after it a few lines down and onto later \
         pages so that page numbers no longer line up between the two versions of the text."
            .to_owned(),
    );
    let opts = with_header_footer("v1.2");
    let (_, before) = extract_flow("before", &original, &opts);
    let (_, after) = extract_flow("after", &changed, &opts);

    let mut after_keys = keys(&after);
    assert_eq!(after_keys.len(), 31);
    assert_eq!(after_keys[1], expected_key(&changed[1]));
    after_keys.remove(1);
    assert_eq!(after_keys, keys(&before));
}

#[test]
fn version_string_in_the_header_does_not_change_the_keys() {
    let input = paragraphs(30);
    let (pages_a, a) = extract_flow("v12", &input, &with_header_footer("v1.2"));
    let (pages_b, b) = extract_flow("v20", &input, &with_header_footer("v2.0-rc1"));
    assert_eq!(pages_a, pages_b);
    assert_eq!(a.removed_lines.len(), pages_a * 2);
    assert_eq!(b.removed_lines.len(), pages_b * 2);
    assert_eq!(keys(&a), keys(&b));
}

/// Extraction of a large real document, for the WP-60 report. Run with
/// `cargo test -p refdesk-core --release --test diff_extract -- --ignored --nocapture`.
/// The path is taken from `REFDESK_BENCH_PDF`; the test does nothing without it.
#[test]
#[ignore = "needs a large PDF outside the repository"]
fn large_document_summary() {
    let Ok(path) = std::env::var("REFDESK_BENCH_PDF") else {
        eprintln!("REFDESK_BENCH_PDF not set");
        return;
    };
    let read_start = Instant::now();
    let pages = page_sources(Path::new(&path));
    let read = read_start.elapsed();
    let start = Instant::now();
    let tokens = extract(&pages, &ExtractOptions::default());
    let took = start.elapsed();
    println!("pages: {}", pages.len());
    println!("page text: {read:?}, extract: {took:?}");
    println!(
        "words: {}, paragraphs: {}",
        tokens.words.len(),
        tokens.paragraphs.len()
    );
    println!("removed lines: {}", tokens.removed_lines.len());
    let mut shapes: std::collections::HashMap<String, usize> = Default::default();
    for r in &tokens.removed_lines {
        *shapes.entry(noise::line_shape(&r.text)).or_default() += 1;
    }
    let mut shapes: Vec<_> = shapes.into_iter().collect();
    shapes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (shape, n) in shapes.iter().take(5) {
        println!("  {n:5}  {shape}");
    }
    let spanning = tokens
        .paragraphs
        .iter()
        .filter(|p| p.start.page != p.end.page)
        .count();
    let tails = tokens.words.iter().filter(|w| w.tail.is_some()).count();
    println!("paragraphs over a page break: {spanning}, words with a tail: {tails}");
}
