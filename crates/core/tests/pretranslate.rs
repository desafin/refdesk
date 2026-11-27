//! Translating ahead of reading (WP-92, §4.16): sentence segments, the store, the job.
//! Nothing here talks to a real server: the backend is a scripted fake, the pages are made in
//! memory, files go to temporary directories and the job's clock is moved by hand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use refdesk_core::geom::{Point, Quad, Rect};
use refdesk_core::store::atomic::bad_path;
use refdesk_core::text::{PageText, TextBlock, TextChar, TextLine};
use refdesk_core::translate::pretranslate::{
    BLOCKED_BACKOFF_MAX, BLOCKED_BACKOFF_MIN, Clock, PRETRANSLATE_FORMAT_VERSION, blocked_backoff,
    identity_hash, list, remove, remove_all,
};
use refdesk_core::translate::segment::{MAX_SEGMENT_CHARS, SEGMENT_RULES_VERSION};
use refdesk_core::translate::{
    Assembled, Outcome, PageSource, PretranslateJob, PretranslateOptions, PretranslateOrder,
    PretranslateSettings, PretranslateSetup, PretranslateSnapshot, PretranslateStatus,
    PretranslateStore, Segment, SkipReason, StoredSegment, assemble, covering, pick_page, segments,
    sentence_around, skip_reason, store_path, text_hash,
};
use refdesk_core::{Notifier, TranslateBackend, TranslateError, TranslateRequest};

const WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------------------------

/// A page whose blocks are lists of lines (characters 10 pt wide, lines 16 pt apart).
fn page(blocks: &[&[&str]]) -> PageText {
    let mut pt = PageText::default();
    let mut y = 72.0;
    for lines in blocks {
        let first_line = pt.lines.len() as u32;
        for text in *lines {
            let start = pt.chars.len() as u32;
            for (i, ch) in text.chars().enumerate() {
                let x = 72.0 + i as f32 * 10.0;
                pt.chars.push(TextChar {
                    ch,
                    quad: Quad {
                        ul: Point::new(x, y),
                        ur: Point::new(x + 10.0, y),
                        ll: Point::new(x, y + 12.0),
                        lr: Point::new(x + 10.0, y + 12.0),
                    },
                    size: 12.0,
                });
            }
            let end = pt.chars.len() as u32;
            pt.lines.push(TextLine {
                chars: start..end,
                bounds: Rect::new(72.0, y, 72.0 + text.len() as f32 * 10.0, y + 12.0),
            });
            y += 16.0;
        }
        pt.blocks.push(TextBlock {
            lines: first_line..pt.lines.len() as u32,
            bounds: Rect::new(72.0, 72.0, 600.0, y),
        });
        y += 8.0;
    }
    pt
}

fn para(text: &str) -> PageText {
    page(&[&[text]])
}

/// The character range of the first occurrence of `word` in the page text.
fn find(pt: &PageText, word: &str) -> Range<u32> {
    let all: String = pt.chars.iter().map(|c| c.ch).collect();
    let byte = all
        .find(word)
        .unwrap_or_else(|| panic!("{word:?} not on the page"));
    let start = all[..byte].chars().count() as u32;
    start..start + word.chars().count() as u32
}

fn texts(segs: &[Segment]) -> Vec<&str> {
    segs.iter().map(|s| s.text.as_str()).collect()
}

fn raw(pt: &PageText, r: Range<u32>) -> String {
    pt.chars[r.start as usize..r.end as usize]
        .iter()
        .map(|c| c.ch)
        .collect()
}

/// Segments do not overlap, come in order, and start and end on the characters their text
/// starts and ends with.
fn check_ranges(pt: &PageText, segs: &[Segment]) {
    let mut last = 0;
    for s in segs {
        assert!(s.chars.start < s.chars.end, "{s:?}");
        assert!(s.chars.start >= last, "overlap at {s:?}");
        last = s.chars.end;
        let first = pt.chars[s.chars.start as usize].ch;
        let end = pt.chars[s.chars.end as usize - 1].ch;
        assert_eq!(s.text.chars().next(), Some(first), "{s:?}");
        assert_eq!(s.text.chars().next_back(), Some(end), "{s:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Segments
// ---------------------------------------------------------------------------------------------

#[test]
fn sentences_of_one_paragraph() {
    let pt = para("First one. The watchdog runs. Last one.");
    let segs = segments(&pt);
    assert_eq!(
        texts(&segs),
        ["First one.", "The watchdog runs.", "Last one."]
    );
    check_ranges(&pt, &segs);
    assert_eq!(raw(&pt, segs[1].chars.clone()), "The watchdog runs.");
    assert!(segs.iter().all(|s| s.skip.is_none()));
}

#[test]
fn sentence_across_lines_with_a_hyphenated_word() {
    let pt = page(&[&[
        "Intro text here. The watchdog super-",
        "vises the liveness of each",
        "process! Next sentence.",
    ]]);
    let segs = segments(&pt);
    assert_eq!(
        texts(&segs),
        [
            "Intro text here.",
            "The watchdog supervises the liveness of each process!",
            "Next sentence."
        ]
    );
    check_ranges(&pt, &segs);
    // The range runs over the hyphen and the line break.
    assert_eq!(
        segs[1].chars,
        find(&pt, "The").start..find(&pt, "process!").end
    );
}

#[test]
fn no_break_at_abbreviations_numbers_and_ellipses() {
    let pt = para(
        "Use a timer, e.g. the watchdog. See Fig. 3 for details. The value 3.14 and section \
         7.3.2 apply. Wait... Then it ends. Refer to No. 5 here. It is i.e. Same here. Ask Prof. \
         Dr. Smith of the Dept. Of Physics.",
    );
    assert_eq!(
        texts(&segments(&pt)),
        [
            "Use a timer, e.g. the watchdog.",
            "See Fig. 3 for details.",
            "The value 3.14 and section 7.3.2 apply.",
            "Wait... Then it ends.",
            "Refer to No. 5 here.",
            "It is i.e. Same here.",
            "Ask Prof. Dr. Smith of the Dept. Of Physics.",
        ]
    );
}

#[test]
fn semicolons_marks_and_closing_brackets_end_sentences() {
    let pt = para("Old one; New one? Yes it is! (See the figure.) After that.");
    assert_eq!(
        texts(&segments(&pt)),
        [
            "Old one;",
            "New one?",
            "Yes it is!",
            "(See the figure.)",
            "After that."
        ]
    );
}

#[test]
fn a_colon_ends_a_sentence_only_at_a_line_end() {
    let pt = page(&[&["The following rules apply:", "the first rule holds always."]]);
    assert_eq!(
        texts(&segments(&pt)),
        ["The following rules apply:", "the first rule holds always."]
    );
    let pt = para("Note the following: values are kept as they are.");
    assert_eq!(
        texts(&segments(&pt)),
        ["Note the following: values are kept as they are."]
    );
}

#[test]
fn headings_table_cells_and_list_items_are_whole() {
    let pt = page(&[
        &["7.3.2 Watchdog supervision"],
        &["1. Introduction"],
        &["a. Scope of the document"],
        &["Value"],
        &["2. The first item. It has two sentences."],
    ]);
    assert_eq!(
        texts(&segments(&pt)),
        [
            "7.3.2 Watchdog supervision",
            "1. Introduction",
            "a. Scope of the document",
            "Value",
            "2. The first item.",
            "It has two sentences."
        ]
    );
}

#[test]
fn a_block_end_always_ends_a_segment() {
    let pt = page(&[&["The watchdog checks"], &["each process in turn"]]);
    assert_eq!(
        texts(&segments(&pt)),
        ["The watchdog checks", "each process in turn"]
    );
}

#[test]
fn long_sentences_are_split_at_commas_or_spaces() {
    let clause = "the execution manager starts the process of the application";
    let sentence = format!(
        "{}.",
        (0..12)
            .map(|i| format!("{clause} {i}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let pt = para(&sentence);
    let segs = segments(&pt);
    assert!(segs.len() > 2, "{segs:?}");
    for s in &segs {
        assert!(
            s.text.chars().count() <= MAX_SEGMENT_CHARS,
            "{}",
            s.text.len()
        );
        assert!(!s.text.is_empty());
    }
    // Cut after a comma where there is one.
    assert!(segs[..segs.len() - 1].iter().all(|s| s.text.ends_with(',')));
    assert_eq!(texts(&segs).join(" "), sentence);
    check_ranges(&pt, &segs);

    // No comma: at a space, no word cut.
    let words = "word ".repeat(150);
    let pt = para(words.trim());
    let segs = segments(&pt);
    assert!(segs.len() >= 3);
    assert!(segs.iter().all(|s| s.text.split(' ').all(|w| w == "word")));
    assert!(
        segs.iter()
            .all(|s| s.text.chars().count() <= MAX_SEGMENT_CHARS)
    );
}

#[test]
fn an_empty_page_has_no_segments() {
    assert!(segments(&PageText::default()).is_empty());
    assert!(segments(&para("   ")).is_empty());
}

#[test]
fn segments_of_a_page_with_many_blocks_do_not_overlap() {
    let pt = page(&[
        &["4 Requirements"],
        &[
            "The process shall report its state. It shall do so within",
            "100 ms; otherwise the watchdog fires. See [SWS_EM_01004] for the de-",
            "tails, e.g. the timing.",
        ],
        &["Introduction . . . . . . . . . 12"],
        &["int main() { return 0; }"],
    ]);
    let segs = segments(&pt);
    check_ranges(&pt, &segs);
    assert_eq!(
        texts(&segs),
        [
            "4 Requirements",
            "The process shall report its state.",
            "It shall do so within 100 ms;",
            "otherwise the watchdog fires.",
            "See [SWS_EM_01004] for the details, e.g. the timing.",
            "Introduction . . . . . . . . . 12",
            "int main() { return 0; }",
        ]
    );
    assert_eq!(segs[5].skip, Some(SkipReason::Leaders));
    assert_eq!(segs[6].skip, Some(SkipReason::Code));
}

#[test]
fn skip_too_short() {
    assert_eq!(skip_reason("abc"), Some(SkipReason::TooShort));
    assert_eq!(skip_reason("12a"), Some(SkipReason::TooShort));
    assert_eq!(skip_reason("Yes."), None);
}

#[test]
fn skip_without_latin_letters() {
    assert_eq!(skip_reason("12.5 %"), Some(SkipReason::NoLatin));
    assert_eq!(skip_reason("3.2.1 — 4.0"), Some(SkipReason::NoLatin));
    assert_eq!(skip_reason("12 ms max"), None);
}

#[test]
fn skip_korean() {
    assert_eq!(
        skip_reason("이것은 한국어 문장입니다 ABC"),
        Some(SkipReason::Korean)
    );
    assert_eq!(
        skip_reason("The AUTOSAR 플랫폼 manages the execution of processes."),
        None
    );
}

#[test]
fn skip_table_of_contents_leaders() {
    assert_eq!(
        skip_reason("7.3 Watchdog . . . . . . . . 12"),
        Some(SkipReason::Leaders)
    );
    assert_eq!(skip_reason("Scope ........ iv"), Some(SkipReason::Leaders));
    assert_eq!(
        skip_reason("Scope \u{2026}\u{2026} 3"),
        Some(SkipReason::Leaders)
    );
    assert_eq!(skip_reason("See the list . . . and more of it."), None);
    assert_eq!(skip_reason("He paused.... Then he left the room."), None);
}

#[test]
fn skip_code() {
    assert_eq!(
        skip_reason("int main() { return 0; }"),
        Some(SkipReason::Code)
    );
    assert_eq!(skip_reason("std::vector<int> v;"), Some(SkipReason::Code));
    assert_eq!(skip_reason("x->next = y;"), Some(SkipReason::Code));
    assert_eq!(
        skip_reason("Call ara::exec::ExecutionClient::ReportExecutionState() to report the state."),
        None
    );
    assert_eq!(skip_reason("The function returns 0; then it stops."), None);
    assert_eq!(skip_reason("The state (see below) is kept."), None);
}

#[test]
fn skip_identifiers() {
    for id in [
        "SWS_EM_01004",
        "[SWS_EM_01004]",
        "ara::exec::ExecutionClient",
        "getValue",
        "IPv6",
        "E2E04",
        "SOME/IP",
        "ara.exec",
    ] {
        assert_eq!(skip_reason(id), Some(SkipReason::Identifier), "{id}");
    }
    for word in [
        "Introduction",
        "AUTOSAR",
        "Overview.",
        "Watchdog supervision",
    ] {
        assert_eq!(skip_reason(word), None, "{word}");
    }
}

#[test]
fn covering_takes_the_segments_a_range_touches() {
    let pt = para("First one. The watchdog runs. Last one.");
    let segs = segments(&pt);
    assert_eq!(segs[0].chars, 0..10);
    assert_eq!(segs[1].chars, 11..29);
    assert_eq!(segs[2].chars, 30..39);
    assert_eq!(covering(&segs, 0..5), 0..1);
    assert_eq!(covering(&segs, 5..15), 0..2);
    // Only the space between two segments.
    assert_eq!(covering(&segs, 10..11), 1..1);
    // Starting in the space, ending in the space.
    assert_eq!(covering(&segs, 10..20), 1..2);
    assert_eq!(covering(&segs, 11..30), 1..2);
    assert_eq!(covering(&segs, 11..31), 1..3);
    assert_eq!(covering(&segs, 0..39), 0..3);
    assert_eq!(covering(&segs, 35..100), 2..3);
    assert_eq!(covering(&segs, 39..39), 3..3);
    assert_eq!(covering(&segs, 12..12), 1..1);
    assert_eq!(covering(&[], 0..5), 0..0);
}

#[test]
fn sentence_around_gives_the_segment_of_the_word() {
    let pt = page(&[
        &["4 Requirements"],
        &[
            "The process shall report its state. It shall do so within",
            "100 ms; otherwise the watchdog super-",
            "vises it, e.g. by a timer. See Fig. 2 here.",
        ],
    ]);
    let segs = segments(&pt);
    for word in [
        "Requirements",
        "report",
        "within",
        "watchdog",
        "super",
        "timer",
        "Fig",
    ] {
        let r = find(&pt, word);
        let seg = &segs[covering(&segs, r.clone()).start];
        assert_eq!(sentence_around(&pt, r), seg.text, "{word}");
    }
}

// ---------------------------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "refdesk-pretranslate-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stored(chars: Range<u32>, text: &str, outcome: Outcome) -> StoredSegment {
    StoredSegment {
        chars,
        hash: text_hash(text),
        outcome,
    }
}

fn tr(t: &str) -> Outcome {
    Outcome::Translated(t.to_string())
}

#[test]
fn store_puts_and_finds() {
    let mut s = PretranslateStore::new("f00d", "m|ko", 3);
    assert!(!s.dirty());
    assert_eq!(s.get(0, 0..10, text_hash("First one.")), None);
    s.set_page(
        0,
        vec![
            stored(11..20, "Second.", Outcome::Skipped),
            stored(0..10, "First one.", tr("첫째.")),
            stored(21..30, "Third one.", Outcome::Failed),
        ],
    );
    assert!(s.dirty());
    assert_eq!(s.pages_done(), 1);
    assert!(s.page_done(0) && !s.page_done(1));
    assert_eq!(s.get(0, 0..10, text_hash("First one.")), Some("첫째."));
    // A different range or text does not match.
    assert_eq!(s.get(0, 0..9, text_hash("First one.")), None);
    assert_eq!(s.get(0, 0..10, text_hash("First one!")), None);
    // Skipped and failed are not translations, but are told apart.
    assert_eq!(s.get(0, 11..20, text_hash("Second.")), None);
    assert_eq!(
        s.outcome(0, 11..20, text_hash("Second.")),
        Some(&Outcome::Skipped)
    );
    assert_eq!(
        s.outcome(0, 21..30, text_hash("Third one.")),
        Some(&Outcome::Failed)
    );
    assert_eq!(s.counts(), (1, 1, 1));
    // Out of range pages are ignored.
    s.set_page(9, vec![]);
    assert_eq!(s.pages_done(), 1);
    s.set_page(1, vec![]);
    s.set_page(2, vec![]);
    assert!(s.is_complete());
    let rev = s.revision();
    s.mark_saved(rev);
    assert!(!s.dirty());
}

#[test]
fn store_round_trip() {
    let dir = TempDir::new("roundtrip");
    let path = store_path(dir.path(), "f00d", "m|ko");
    let mut s = PretranslateStore::new("f00d", "m|ko", 4);
    s.set_page(
        2,
        vec![
            stored(0..10, "First one.", tr("첫째 \"따옴표\".")),
            stored(11..20, "Second.", Outcome::Skipped),
            stored(21..30, "Third one.", Outcome::Failed),
        ],
    );
    s.set_page(0, vec![]);
    s.save(&path).unwrap();
    let back = PretranslateStore::load(&path).unwrap();
    assert_eq!(back.fingerprint(), "f00d");
    assert_eq!(back.identity(), "m|ko");
    assert_eq!(back.page_count(), 4);
    assert_eq!(back.pages_done(), 2);
    assert_eq!(back.page(2), s.page(2));
    assert_eq!(back.page(1), None);
    assert!(!back.dirty());
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(json["version"], PRETRANSLATE_FORMAT_VERSION);
    assert_eq!(json["rules"], SEGMENT_RULES_VERSION);
    assert_eq!(json["pages"][2][1][3], serde_json::Value::Null);
    assert_eq!(json["pages"][2][2][3], false);
    // open() takes it for the same document and identity only.
    assert_eq!(
        PretranslateStore::open(&path, "f00d", "m|ko", 4).pages_done(),
        2
    );
    assert_eq!(
        PretranslateStore::open(&path, "beef", "m|ko", 4).pages_done(),
        0
    );
    assert_eq!(
        PretranslateStore::open(&path, "f00d", "x|ko", 4).pages_done(),
        0
    );
    assert_eq!(
        PretranslateStore::open(&path, "f00d", "m|ko", 5).pages_done(),
        0
    );
}

#[test]
fn a_damaged_store_is_moved_away() {
    let dir = TempDir::new("damaged");
    let path = store_path(dir.path(), "f00d", "m|ko");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{ not json").unwrap();
    assert!(PretranslateStore::load(&path).is_none());
    assert!(!path.exists());
    assert_eq!(std::fs::read(bad_path(&path)).unwrap(), b"{ not json");
    assert_eq!(
        PretranslateStore::open(&path, "f00d", "m|ko", 2).pages_done(),
        0
    );
}

#[test]
fn a_store_of_other_splitting_rules_is_dropped() {
    let dir = TempDir::new("rules");
    let path = store_path(dir.path(), "f00d", "m|ko");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let json = serde_json::json!({
        "version": 1, "rules": SEGMENT_RULES_VERSION + 1, "fingerprint": "f00d",
        "identity": "m|ko", "page_count": 1, "pages": [[[0, 10, 1, "x"]]]
    });
    std::fs::write(&path, json.to_string()).unwrap();
    assert!(PretranslateStore::load(&path).is_none());
    // Not damaged: left in place to be replaced by the next save.
    assert!(path.exists());
    assert!(!bad_path(&path).exists());
    assert_eq!(
        PretranslateStore::open(&path, "f00d", "m|ko", 1).pages_done(),
        0
    );
}

#[test]
fn another_identity_is_another_file() {
    let dir = TempDir::new("identity");
    let a = store_path(dir.path(), "f00d", "translategemma:4b|ko");
    let b = store_path(dir.path(), "f00d", "qwen2.5:7b|ko");
    assert_ne!(a, b);
    assert_eq!(identity_hash("m|ko").len(), 16);
    assert_eq!(
        a,
        dir.path().join("translations").join(format!(
            "f00d.{}.json",
            identity_hash("translategemma:4b|ko")
        ))
    );
}

#[test]
fn list_and_remove_stores() {
    let dir = TempDir::new("list");
    assert!(list(dir.path()).is_empty());
    assert_eq!(remove_all(dir.path()).unwrap(), 0);
    let mut a = PretranslateStore::new("aaaa", "m|ko", 3);
    a.set_page(0, vec![stored(0..10, "First one.", tr("첫째."))]);
    let pa = store_path(dir.path(), "aaaa", "m|ko");
    a.save(&pa).unwrap();
    let b = PretranslateStore::new("bbbb", "n|ko", 7);
    let pb = store_path(dir.path(), "bbbb", "n|ko");
    b.save(&pb).unwrap();
    std::fs::write(dir.path().join("translations").join("junk.json"), "[").unwrap();

    let found = list(dir.path());
    assert_eq!(found.len(), 2, "{found:?}");
    let fa = found.iter().find(|f| f.fingerprint == "aaaa").unwrap();
    assert_eq!(
        (fa.identity.as_str(), fa.pages_done, fa.page_count),
        ("m|ko", 1, 3)
    );
    assert_eq!(fa.path, pa);
    assert_eq!(fa.bytes, std::fs::metadata(&pa).unwrap().len());
    let fb = found.iter().find(|f| f.fingerprint == "bbbb").unwrap();
    assert_eq!((fb.pages_done, fb.page_count), (0, 7));

    remove(&pa).unwrap();
    assert_eq!(list(dir.path()).len(), 1);
    assert_eq!(remove_all(dir.path()).unwrap(), 2);
    assert!(list(dir.path()).is_empty());
}

#[test]
fn settings_file() {
    let dir = TempDir::new("settings");
    let data = dir.path().join(".refdesk");
    assert_eq!(
        PretranslateSettings::load(&data),
        PretranslateSettings::default()
    );
    assert!(!PretranslateSettings::load(&data).enabled);
    let on = PretranslateSettings {
        enabled: true,
        paused: true,
    };
    on.save(&data).unwrap();
    assert_eq!(PretranslateSettings::load(&data), on);
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(PretranslateSettings::path(&data)).unwrap()).unwrap();
    assert_eq!(json["version"], PRETRANSLATE_FORMAT_VERSION);
    assert_eq!(json["enabled"], true);

    std::fs::write(PretranslateSettings::path(&data), "nope").unwrap();
    assert_eq!(
        PretranslateSettings::load(&data),
        PretranslateSettings::default()
    );
    assert!(bad_path(&PretranslateSettings::path(&data)).exists());
}

#[test]
fn assemble_joins_stored_segments() {
    let p0 = para("First one. The watchdog runs. Last one.");
    let p1 = para("12.5 % and more text here. Final words of it.");
    let segs0 = segments(&p0);
    let segs1 = segments(&p1);
    let mut s = PretranslateStore::new("f", "m|ko", 2);
    let put = |segs: &[Segment]| -> Vec<StoredSegment> {
        segs.iter()
            .map(|g| stored(g.chars.clone(), &g.text, tr(&format!("<{}>", g.text))))
            .collect()
    };
    s.set_page(0, put(&segs0));
    // From the middle of a sentence: the whole sentence.
    let r = find(&p0, "watch").start..find(&p0, "runs").end;
    assert_eq!(
        assemble(&s, &[(0, &p0, r)]),
        Ok(Assembled {
            source: "The watchdog runs.".into(),
            translation: "<The watchdog runs.>".into()
        })
    );
    // Page 1 is not done: it is named.
    let to_end = find(&p1, "Final").start..p1.chars.len() as u32;
    let across = [(0, &p0, 30..39), (1, &p1, to_end.clone())];
    assert_eq!(assemble(&s, &across), Err(vec![1]));
    // A skipped segment stands as it is; across pages.
    let mut on1 = put(&segs1);
    on1[0].outcome = Outcome::Skipped;
    s.set_page(1, on1);
    let all1 = 0..p1.chars.len() as u32;
    assert_eq!(
        assemble(&s, &[(0, &p0, 30..39), (1, &p1, all1)]),
        Ok(Assembled {
            source: "Last one. 12.5 % and more text here. Final words of it.".into(),
            translation: "<Last one.> 12.5 % and more text here. <Final words of it.>".into()
        })
    );
    // A failed segment, or one extracted differently, is missing.
    let mut failed = put(&segs1);
    failed[1].outcome = Outcome::Failed;
    s.set_page(1, failed);
    assert_eq!(assemble(&s, &[(1, &p1, to_end)]), Err(vec![1]));
    let other = para("First one. The watchdog walks. Last one.");
    let r = find(&other, "watchdog");
    assert_eq!(assemble(&s, &[(0, &other, r)]), Err(vec![0]));
    // Only white space selected.
    assert_eq!(assemble(&s, &[(0, &p0, 10..11)]), Err(vec![]));
}

// ---------------------------------------------------------------------------------------------
// The order
// ---------------------------------------------------------------------------------------------

#[test]
fn pick_page_goes_out_from_the_current_page_ahead_first() {
    let mut done = [false; 10];
    let mut order = Vec::new();
    while let Some(p) = pick_page(5, &[], 10, |p| done[p as usize]) {
        order.push(p);
        done[p as usize] = true;
    }
    assert_eq!(order, [5, 6, 4, 7, 8, 3, 9, 2, 1, 0]);
    // Urgent pages first, done ones passed over.
    let done = |p: u32| p == 2;
    assert_eq!(pick_page(5, &[2, 8], 10, done), Some(8));
    assert_eq!(pick_page(5, &[12], 10, done), Some(5));
    // A current page past the end counts as the last one.
    assert_eq!(pick_page(40, &[], 10, |_| false), Some(9));
    assert_eq!(pick_page(0, &[], 3, |_| true), None);
    assert_eq!(pick_page(0, &[], 0, |_| false), None);
}

#[test]
fn order_keeps_the_latest_urgent_pages_first() {
    let o = PretranslateOrder::new();
    assert_eq!(o.current(), 0);
    o.set_current(7);
    assert_eq!(o.current(), 7);
    for p in 0..10 {
        o.prioritize(p);
    }
    o.prioritize(5);
    let u = o.urgent();
    assert_eq!(u[0], 5);
    assert_eq!(u[1], 9);
    assert_eq!(u.len(), 8);
    assert_eq!(u.iter().filter(|p| **p == 5).count(), 1);
}

#[test]
fn backoff_grows_from_thirty_seconds_to_five_minutes() {
    assert_eq!(blocked_backoff(0), BLOCKED_BACKOFF_MIN);
    assert_eq!(blocked_backoff(0), Duration::from_secs(30));
    assert_eq!(blocked_backoff(1), Duration::from_secs(60));
    assert_eq!(blocked_backoff(2), Duration::from_secs(120));
    assert_eq!(blocked_backoff(3), Duration::from_secs(240));
    assert_eq!(blocked_backoff(4), BLOCKED_BACKOFF_MAX);
    assert_eq!(blocked_backoff(100), Duration::from_secs(300));
}

// ---------------------------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------------------------

/// Wakes waiting tests whenever the job or the fake backend does something.
#[derive(Default)]
struct Signal {
    n: Mutex<u64>,
    cv: Condvar,
}

impl Signal {
    fn bump(&self) {
        *self.n.lock().unwrap() += 1;
        self.cv.notify_all();
    }

    /// Waits until `cond` holds (looked at whenever something happened, and every 20 ms).
    fn wait(&self, what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + WAIT;
        let mut n = self.n.lock().unwrap();
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            n = self
                .cv
                .wait_timeout(n, Duration::from_millis(20))
                .unwrap()
                .0;
        }
    }
}

#[derive(Default)]
struct FakeState {
    /// Texts asked, in order (recorded when the call starts).
    calls: Vec<String>,
    /// Calls that may still finish; `None`: any number.
    permits: Option<usize>,
    /// (part of a text, error, how many more times).
    errors: Vec<(String, TranslateError, usize)>,
    /// Calls made while `foreground` was set.
    during_foreground: usize,
}

struct Fake {
    state: Mutex<FakeState>,
    cv: Condvar,
    signal: Arc<Signal>,
    clock: Option<Arc<ManualClock>>,
    foreground: Option<Arc<AtomicBool>>,
}

impl Fake {
    fn new(signal: &Arc<Signal>) -> Fake {
        Fake {
            state: Mutex::default(),
            cv: Condvar::new(),
            signal: Arc::clone(signal),
            clock: None,
            foreground: None,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    fn n_calls(&self) -> usize {
        self.state.lock().unwrap().calls.len()
    }

    fn set_permits(&self, permits: Option<usize>) {
        self.state.lock().unwrap().permits = permits;
        self.cv.notify_all();
    }

    fn fail(&self, part: &str, e: TranslateError, times: usize) {
        self.state
            .lock()
            .unwrap()
            .errors
            .push((part.to_string(), e, times));
    }
}

fn answer(text: &str) -> String {
    format!("KO[{text}]")
}

impl TranslateBackend for Fake {
    fn translate(
        &self,
        req: &TranslateRequest,
        cancel: &AtomicBool,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(), TranslateError> {
        let mut st = self.state.lock().unwrap();
        st.calls.push(req.text.clone());
        if self
            .foreground
            .as_ref()
            .is_some_and(|f| f.load(Ordering::SeqCst))
        {
            st.during_foreground += 1;
        }
        self.signal.bump();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(TranslateError::Cancelled);
            }
            match st.permits {
                Some(0) => {
                    st = self
                        .cv
                        .wait_timeout(st, Duration::from_millis(5))
                        .unwrap()
                        .0;
                }
                Some(n) => {
                    st.permits = Some(n - 1);
                    break;
                }
                None => break,
            }
        }
        if let Some(c) = &self.clock {
            c.advance(Duration::from_secs(1));
        }
        let failure = st
            .errors
            .iter_mut()
            .find(|(part, _, times)| *times > 0 && req.text.contains(part.as_str()));
        let result = match failure {
            Some((_, e, times)) => {
                *times -= 1;
                Err(e.clone())
            }
            None => {
                on_text(&answer(&req.text));
                Ok(())
            }
        };
        drop(st);
        self.signal.bump();
        result
    }

    fn warm_up(&self, _cancel: &AtomicBool) -> Result<(), TranslateError> {
        Ok(())
    }

    fn models(&self, _cancel: &AtomicBool) -> Result<Vec<String>, TranslateError> {
        Ok(Vec::new())
    }

    fn identity(&self) -> String {
        "fake|ko".to_string()
    }
}

/// A clock the test moves. Counts how often it is read.
#[derive(Default)]
struct ManualClock {
    now: Mutex<Duration>,
    reads: AtomicU64,
}

impl ManualClock {
    fn advance(&self, by: Duration) {
        *self.now.lock().unwrap() += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        self.reads.fetch_add(1, Ordering::SeqCst);
        *self.now.lock().unwrap()
    }
}

struct Pages(Vec<Arc<PageText>>);

impl PageSource for Pages {
    fn page_count(&self) -> Option<u32> {
        Some(self.0.len() as u32)
    }

    fn page_text(&self, page: u32) -> Option<Arc<PageText>> {
        self.0.get(page as usize).cloned()
    }
}

/// `n` pages with one sentence each that names the page.
fn numbered(n: u32) -> Pages {
    Pages(
        (0..n)
            .map(|i| Arc::new(para(&format!("This is page {i} of the test."))))
            .collect(),
    )
}

/// The page a numbered sentence names.
fn page_of(text: &str) -> u32 {
    text.split(' ').nth(3).unwrap().parse().unwrap()
}

struct Rig {
    fake: Arc<Fake>,
    signal: Arc<Signal>,
    clock: Arc<ManualClock>,
    order: Arc<PretranslateOrder>,
    busy: Arc<AtomicBool>,
    store: Arc<RwLock<PretranslateStore>>,
    path: PathBuf,
    _dir: TempDir,
}

impl Rig {
    fn new(
        name: &str,
        pages: u32,
        fake: impl FnOnce(&Arc<Signal>, &Arc<ManualClock>, &Arc<AtomicBool>) -> Fake,
    ) -> Rig {
        let dir = TempDir::new(name);
        let signal = Arc::new(Signal::default());
        let clock = Arc::new(ManualClock::default());
        let busy = Arc::new(AtomicBool::new(false));
        let fake = Arc::new(fake(&signal, &clock, &busy));
        let path = store_path(dir.path(), "f00d", "fake|ko");
        let store = Arc::new(RwLock::new(PretranslateStore::new(
            "f00d", "fake|ko", pages,
        )));
        Rig {
            fake,
            signal,
            clock,
            order: PretranslateOrder::new(),
            busy,
            store,
            path,
            _dir: dir,
        }
    }

    fn start(&self, source: impl PageSource + 'static) -> PretranslateJob {
        let signal = Arc::clone(&self.signal);
        let notifier: Notifier = Arc::new(move || signal.bump());
        PretranslateJob::start(PretranslateSetup {
            backend: self.fake.clone(),
            source: Arc::new(source),
            store: Arc::clone(&self.store),
            store_path: self.path.clone(),
            order: Arc::clone(&self.order),
            foreground_busy: Arc::clone(&self.busy),
            notifier,
            options: PretranslateOptions {
                save_interval: Duration::ZERO,
                clock: self.clock.clone(),
            },
        })
    }

    fn wait_status(
        &self,
        job: &PretranslateJob,
        what: &str,
        f: impl Fn(&PretranslateSnapshot) -> bool,
    ) {
        self.signal.wait(what, || f(&job.snapshot()));
    }
}

fn plain(signal: &Arc<Signal>, _: &Arc<ManualClock>, _: &Arc<AtomicBool>) -> Fake {
    Fake::new(signal)
}

fn finished(s: &PretranslateSnapshot) -> bool {
    s.status == PretranslateStatus::Finished
}

#[test]
fn job_goes_out_from_the_current_page() {
    let rig = Rig::new("order", 10, plain);
    rig.order.set_current(5);
    let job = rig.start(numbered(10));
    rig.wait_status(&job, "finished", finished);
    let pages: Vec<u32> = rig.fake.calls().iter().map(|t| page_of(t)).collect();
    assert_eq!(pages, [5, 6, 4, 7, 8, 3, 9, 2, 1, 0]);
    let st = rig.store.read().unwrap();
    assert!(st.is_complete());
    let segs = segments(&para("This is page 3 of the test."));
    assert_eq!(
        st.get(3, segs[0].chars.clone(), text_hash(&segs[0].text)),
        Some(answer("This is page 3 of the test.").as_str())
    );
}

#[test]
fn a_new_current_page_counts_from_the_next_page_on() {
    let rig = Rig::new("reorder", 10, plain);
    rig.fake.set_permits(Some(0));
    rig.order.set_current(5);
    let job = rig.start(numbered(10));
    rig.signal.wait("first call", || rig.fake.n_calls() == 1);
    // The page in progress goes on; the next one is picked around the new current page.
    rig.order.set_current(1);
    rig.fake.set_permits(None);
    rig.wait_status(&job, "finished", finished);
    let pages: Vec<u32> = rig.fake.calls().iter().map(|t| page_of(t)).collect();
    assert_eq!(pages, [5, 1, 2, 0, 3, 4, 6, 7, 8, 9]);
}

#[test]
fn urgent_pages_go_first() {
    let rig = Rig::new("urgent", 10, plain);
    rig.fake.set_permits(Some(0));
    let job = rig.start(numbered(10));
    rig.signal.wait("first call", || rig.fake.n_calls() == 1);
    rig.order.prioritize(8);
    rig.fake.set_permits(None);
    rig.wait_status(&job, "finished", finished);
    let pages: Vec<u32> = rig.fake.calls().iter().map(|t| page_of(t)).collect();
    assert_eq!(&pages[..3], [0, 8, 1]);
}

#[test]
fn every_page_is_saved_and_a_new_job_goes_on_from_there() {
    let rig = Rig::new("resume", 6, plain);
    rig.fake.set_permits(Some(3));
    let job = rig.start(numbered(6));
    // Three pages done and written; the fourth waits in the backend.
    rig.signal.wait("three pages on disk", || {
        PretranslateStore::load(&rig.path).is_some_and(|s| s.pages_done() == 3)
    });
    rig.signal.wait("fourth call", || rig.fake.n_calls() == 4);
    assert_eq!(job.snapshot().pages_done, 3);
    drop(job);
    rig.signal
        .wait("job gone", || Arc::strong_count(&rig.fake) == 1);

    let rig2 = Rig::new("resume2", 6, plain);
    let reopened = PretranslateStore::open(&rig.path, "f00d", "fake|ko", 6);
    assert_eq!(reopened.pages_done(), 3);
    *rig2.store.write().unwrap() = reopened;
    let job = Rig {
        path: rig.path.clone(),
        ..rig2
    };
    let j = job.start(numbered(6));
    job.wait_status(&j, "finished", finished);
    let pages: Vec<u32> = job.fake.calls().iter().map(|t| page_of(t)).collect();
    assert_eq!(pages, [3, 4, 5]);
    assert_eq!(PretranslateStore::load(&rig.path).unwrap().pages_done(), 6);
}

#[test]
fn pause_and_resume() {
    let rig = Rig::new("pause", 4, plain);
    rig.fake.set_permits(Some(0));
    let job = rig.start(numbered(4));
    rig.signal.wait("first call", || rig.fake.n_calls() == 1);
    job.set_paused(true);
    rig.fake.set_permits(None);
    rig.wait_status(&job, "paused", |s| s.status == PretranslateStatus::Paused);
    // The segment in flight finished and its page was written; nothing more was sent.
    assert_eq!(rig.fake.n_calls(), 1);
    assert_eq!(job.snapshot().pages_done, 1);
    assert_eq!(PretranslateStore::load(&rig.path).unwrap().pages_done(), 1);
    job.set_paused(false);
    rig.wait_status(&job, "finished", finished);
    assert_eq!(rig.fake.n_calls(), 4);
}

#[test]
fn the_backend_is_not_called_while_the_foreground_is_busy() {
    let rig = Rig::new("foreground", 3, |signal, _, busy| Fake {
        foreground: Some(Arc::clone(busy)),
        ..Fake::new(signal)
    });
    rig.busy.store(true, Ordering::SeqCst);
    let job = rig.start(numbered(3));
    rig.wait_status(&job, "waiting", |s| {
        s.status == PretranslateStatus::WaitingForeground
    });
    assert_eq!(rig.fake.n_calls(), 0);
    rig.busy.store(false, Ordering::SeqCst);
    rig.wait_status(&job, "finished", finished);
    assert_eq!(rig.fake.n_calls(), 3);

    // Raised in the middle: the next segment waits.
    let rig = Rig::new("foreground2", 3, |signal, _, busy| Fake {
        foreground: Some(Arc::clone(busy)),
        ..Fake::new(signal)
    });
    rig.fake.set_permits(Some(0));
    let job = rig.start(numbered(3));
    rig.signal.wait("first call", || rig.fake.n_calls() == 1);
    rig.busy.store(true, Ordering::SeqCst);
    rig.fake.set_permits(None);
    rig.wait_status(&job, "waiting", |s| {
        s.status == PretranslateStatus::WaitingForeground
    });
    assert_eq!(rig.fake.n_calls(), 1);
    rig.busy.store(false, Ordering::SeqCst);
    rig.wait_status(&job, "finished", finished);
    assert_eq!(rig.fake.state.lock().unwrap().during_foreground, 0);
}

#[test]
fn blocked_backs_off_and_tries_again() {
    let rig = Rig::new("blocked", 2, plain);
    rig.fake.fail(
        "page 0",
        TranslateError::Unreachable("connection refused".into()),
        2,
    );
    let job = rig.start(numbered(2));
    rig.wait_status(&job, "blocked", |s| {
        matches!(
            s.status,
            PretranslateStatus::Blocked(TranslateError::Unreachable(_))
        )
    });
    assert_eq!(rig.fake.n_calls(), 1);
    assert_eq!(job.snapshot().blocked_attempts, 1);
    // Not before 30 s have passed.
    rig.clock.advance(Duration::from_secs(28));
    let reads = rig.clock.reads.load(Ordering::SeqCst);
    job.nudge();
    rig.signal.wait("clock read again", || {
        job.nudge();
        rig.clock.reads.load(Ordering::SeqCst) > reads
    });
    assert_eq!(rig.fake.n_calls(), 1);
    rig.clock.advance(Duration::from_secs(2));
    job.nudge();
    rig.signal.wait("second try", || rig.fake.n_calls() == 2);
    // The second failure waits 60 s.
    rig.wait_status(&job, "blocked again", |s| s.blocked_attempts == 2);
    rig.clock.advance(Duration::from_secs(31));
    let reads = rig.clock.reads.load(Ordering::SeqCst);
    rig.signal.wait("clock read again", || {
        job.nudge();
        rig.clock.reads.load(Ordering::SeqCst) > reads
    });
    assert_eq!(rig.fake.n_calls(), 2);
    rig.clock.advance(Duration::from_secs(30));
    rig.signal.wait("third try", || {
        job.nudge();
        rig.fake.n_calls() >= 3
    });
    rig.wait_status(&job, "finished", finished);
    // Blocking is not a failure of the segment.
    let snap = job.snapshot();
    assert_eq!((snap.translated, snap.failed), (2, 0));
    assert_eq!(snap.blocked_attempts, 0);
}

#[test]
fn one_failing_segment_does_not_stop_the_rest() {
    let rig = Rig::new("failure", 3, plain);
    rig.fake.fail(
        "page 1",
        TranslateError::Http {
            status: 500,
            message: "oops".into(),
        },
        usize::MAX,
    );
    let job = rig.start(numbered(3));
    rig.wait_status(&job, "finished", finished);
    let calls: Vec<u32> = rig.fake.calls().iter().map(|t| page_of(t)).collect();
    // Three tries for page 1.
    assert_eq!(calls, [0, 1, 1, 1, 2]);
    let snap = job.snapshot();
    assert_eq!((snap.translated, snap.failed, snap.pages_done), (2, 1, 3));
    let st = rig.store.read().unwrap();
    let text = "This is page 1 of the test.";
    let seg = &segments(&para(text))[0];
    assert_eq!(
        st.outcome(1, seg.chars.clone(), text_hash(text)),
        Some(&Outcome::Failed)
    );
}

#[test]
fn a_repeated_text_is_sent_once() {
    let header = "AUTOSAR Specification of Execution Management.";
    let pages = Pages(
        (0..3)
            .map(|i| Arc::new(page(&[&[header], &[&format!("Body of page {i} here.")]])))
            .collect(),
    );
    let rig = Rig::new("repeat", 3, plain);
    let job = rig.start(pages);
    rig.wait_status(&job, "finished", finished);
    let calls = rig.fake.calls();
    assert_eq!(calls.iter().filter(|t| t.as_str() == header).count(), 1);
    assert_eq!(calls.len(), 4);
    let st = rig.store.read().unwrap();
    for p in 0..3 {
        let seg = &segments(&page(&[&[header]]))[0];
        assert_eq!(
            st.get(p, seg.chars.clone(), text_hash(header)),
            Some(answer(header).as_str())
        );
    }
    assert_eq!(job.snapshot().translated, 6);
}

#[test]
fn skipped_segments_are_not_sent_and_counted() {
    let pages = Pages(vec![Arc::new(page(&[
        &["SWS_EM_01004"],
        &["The process reports its state."],
        &["Introduction . . . . . . 12"],
    ]))]);
    let rig = Rig::new("skips", 1, plain);
    let job = rig.start(pages);
    rig.wait_status(&job, "finished", finished);
    assert_eq!(rig.fake.calls(), ["The process reports its state."]);
    let snap = job.snapshot();
    assert_eq!((snap.translated, snap.skipped), (1, 2));
}

#[test]
fn dropping_the_job_ends_its_thread() {
    let rig = Rig::new("drop", 3, plain);
    rig.fake.set_permits(Some(0));
    let job = rig.start(numbered(3));
    rig.signal.wait("first call", || rig.fake.n_calls() == 1);
    drop(job);
    // The thread held the backend; it lets go when it ends.
    rig.signal
        .wait("thread ended", || Arc::strong_count(&rig.fake) == 1);
    assert_eq!(rig.fake.n_calls(), 1);
    assert_eq!(rig.store.read().unwrap().pages_done(), 0);
}

#[test]
fn stop_after_page_finishes_the_page_first() {
    let rig = Rig::new("stop", 4, plain);
    let two = Pages(
        (0..4)
            .map(|i| {
                Arc::new(para(&format!(
                    "This is page {i} of the test. And a second one here."
                )))
            })
            .collect(),
    );
    rig.fake.set_permits(Some(0));
    let job = rig.start(two);
    rig.signal.wait("first call", || rig.fake.n_calls() == 1);
    job.stop_after_page();
    rig.fake.set_permits(None);
    rig.wait_status(&job, "stopped", |s| s.status == PretranslateStatus::Stopped);
    assert_eq!(rig.fake.n_calls(), 2);
    assert_eq!(job.snapshot().pages_done, 1);
    assert_eq!(PretranslateStore::load(&rig.path).unwrap().pages_done(), 1);
    assert!(job.snapshot().ended());
}

#[test]
fn snapshot_counts_rate_and_time_left() {
    let rig = Rig::new("snapshot", 4, |signal, clock, _| Fake {
        clock: Some(Arc::clone(clock)),
        ..Fake::new(signal)
    });
    let pages = Pages(
        (0..4)
            .map(|i| {
                Arc::new(page(&[
                    &[&format!(
                        "This is page {i} of the test. Another sentence {i} here."
                    )],
                    &["12.5"],
                ]))
            })
            .collect(),
    );
    rig.fake.set_permits(Some(5));
    let job = rig.start(pages);
    rig.wait_status(&job, "two pages", |s| s.pages_done == 2);
    rig.signal.wait("fifth call", || rig.fake.n_calls() == 6);
    let snap = job.snapshot();
    assert_eq!(snap.page_count, 4);
    assert_eq!(snap.pages_done, 2);
    assert_eq!(snap.current_page, Some(2));
    assert_eq!(snap.status, PretranslateStatus::Running);
    assert_eq!(snap.translated, 5);
    assert_eq!(snap.skipped, 2);
    // One segment per second (the fake moves the clock a second per call).
    let rate = snap.rate.unwrap();
    assert!((rate - 1.0).abs() < 1e-3, "{rate}");
    // Two pages left at two segments a page.
    assert_eq!(snap.remaining, Some(Duration::from_secs(4)));
    rig.fake.set_permits(None);
    rig.wait_status(&job, "finished", finished);
    let snap = job.snapshot();
    assert_eq!((snap.translated, snap.skipped, snap.pages_done), (8, 4, 4));
    assert_eq!(snap.current_page, None);
    assert_eq!(snap.remaining, Some(Duration::ZERO));
}

#[test]
fn a_document_that_cannot_be_read() {
    struct Broken;
    impl PageSource for Broken {
        fn page_count(&self) -> Option<u32> {
            None
        }
        fn page_text(&self, _: u32) -> Option<Arc<PageText>> {
            None
        }
    }
    let rig = Rig::new("broken", 0, plain);
    let job = rig.start(Broken);
    rig.wait_status(&job, "no document", |s| {
        s.status == PretranslateStatus::NoDocument
    });
    assert!(job.snapshot().ended());

    // Pages without text are done with nothing in them; a page count that differs from the
    // store starts the store over.
    let rig = Rig::new("empty", 5, plain);
    let job = rig.start(Pages(
        (0..2).map(|_| Arc::new(PageText::default())).collect(),
    ));
    rig.wait_status(&job, "finished", finished);
    assert_eq!(rig.store.read().unwrap().page_count(), 2);
    assert_eq!(rig.fake.n_calls(), 0);
}

// ---------------------------------------------------------------------------------------------
// Measurements (run by hand: cargo test -p refdesk-core --release --test pretranslate -- --ignored --nocapture)
// ---------------------------------------------------------------------------------------------

/// How long a big store takes to write and read.
#[test]
#[ignore = "measurement"]
fn measure_store_size_and_speed() {
    let dir = TempDir::new("measure");
    let path = store_path(dir.path(), "f00d", "m|ko");
    let korean =
        "실행 관리자는 애플리케이션의 프로세스를 시작하고 상태를 감시하며, 필요하면 다시 시작한다.";
    for (pages, per_page) in [(1000u32, 15u32), (3000, 15), (5000, 20)] {
        let mut s = PretranslateStore::new("f00d", "m|ko", pages);
        for p in 0..pages {
            let segs = (0..per_page)
                .map(|i| StoredSegment {
                    chars: i * 100..i * 100 + 90,
                    hash: u64::from(p) << 32 | u64::from(i),
                    outcome: Outcome::Translated(format!("{korean} {p}.{i}")),
                })
                .collect();
            s.set_page(p, segs);
        }
        let t = Instant::now();
        let bytes = s.to_bytes().unwrap();
        let encode = t.elapsed();
        let t = Instant::now();
        s.save(&path).unwrap();
        let save = t.elapsed();
        let t = Instant::now();
        let back = PretranslateStore::load(&path).unwrap();
        let load = t.elapsed();
        let t = Instant::now();
        let listed = list(dir.path());
        let listing = t.elapsed();
        assert_eq!(back.pages_done(), pages);
        assert_eq!(listed[0].pages_done, pages);
        println!(
            "{pages} pages x {per_page} segments: {:.1} MB, encode {encode:?}, save {save:?}, load {load:?}, list {listing:?}",
            bytes.len() as f64 / 1e6
        );
    }
}

/// The first pages of a real document through the real Ollama of this machine. Run by hand:
/// `REFDESK_PRETRANSLATE_PDF=file.pdf cargo test -p refdesk-core --release --test pretranslate
/// -- --ignored --nocapture real_ollama`. `REFDESK_PRETRANSLATE_PAGES` (default 20) limits the
/// pages.
#[test]
#[ignore = "needs a running Ollama and a PDF"]
fn measure_real_ollama() {
    use refdesk_core::store::TranslateConfig;
    use refdesk_core::{DocumentHandle, OllamaBackend};

    struct FirstPages(DocumentHandle, u32);
    impl PageSource for FirstPages {
        fn page_count(&self) -> Option<u32> {
            self.0.page_count().map(|n| n.min(self.1))
        }
        fn page_text(&self, page: u32) -> Option<Arc<PageText>> {
            PageSource::page_text(&self.0, page)
        }
    }

    let Some(pdf) = std::env::var_os("REFDESK_PRETRANSLATE_PDF") else {
        println!("set REFDESK_PRETRANSLATE_PDF");
        return;
    };
    let limit: u32 = std::env::var("REFDESK_PRETRANSLATE_PAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let doc = DocumentHandle::open(Path::new(&pdf), Arc::new(|| {}));
    let source = FirstPages(doc, limit);
    let pages = source.page_count().expect("document opens");
    let split_only = std::env::var_os("REFDESK_PRETRANSLATE_SPLIT_ONLY").is_some();

    // Splitting alone.
    let mut by_reason: std::collections::BTreeMap<String, usize> = Default::default();
    let mut total = 0;
    let mut chars = 0;
    for p in 0..pages {
        let text = source.page_text(p).expect("page text");
        for s in segments(&text) {
            total += 1;
            chars += s.text.chars().count();
            *by_reason.entry(format!("{:?}", s.skip)).or_default() += 1;
            if p < 3 || std::env::var("REFDESK_PRETRANSLATE_SHOW").is_ok_and(|v| v == p.to_string())
            {
                println!("  p{p} {:?} {}", s.skip, s.text);
            }
        }
    }
    println!(
        "{pages} pages: {total} segments ({:.1} a page, {:.0} characters each), {by_reason:?}",
        total as f64 / f64::from(pages),
        chars as f64 / total.max(1) as f64
    );
    if split_only {
        return;
    }

    let dir = TempDir::new("ollama");
    let backend = Arc::new(OllamaBackend::new(&TranslateConfig::default()));
    let path = store_path(dir.path(), "real", &backend.identity());
    let signal = Arc::new(Signal::default());
    let s2 = Arc::clone(&signal);
    let started = Instant::now();
    let job = PretranslateJob::start(PretranslateSetup {
        backend: backend.clone(),
        source: Arc::new(source),
        store: Arc::new(RwLock::new(PretranslateStore::new(
            "real",
            &backend.identity(),
            pages,
        ))),
        store_path: path.clone(),
        order: PretranslateOrder::new(),
        foreground_busy: Arc::new(AtomicBool::new(false)),
        notifier: Arc::new(move || s2.bump()),
        options: PretranslateOptions::default(),
    });
    let deadline = Instant::now() + Duration::from_secs(3600);
    let mut last_done = 0;
    let mut n = signal.n.lock().unwrap();
    loop {
        let snap = job.snapshot();
        if snap.pages_done != last_done {
            last_done = snap.pages_done;
            println!(
                "  {:>6.1}s page {} done ({}/{}), {} translated, {} skipped, {:?}/s, left {:?}",
                started.elapsed().as_secs_f64(),
                snap.current_page.map_or(-1, i64::from),
                snap.pages_done,
                snap.page_count,
                snap.translated,
                snap.skipped,
                snap.rate,
                snap.remaining
            );
        }
        if snap.ended() || matches!(snap.status, PretranslateStatus::Blocked(_)) {
            println!("status {:?}", snap.status);
            break;
        }
        assert!(Instant::now() < deadline);
        n = signal
            .cv
            .wait_timeout(n, Duration::from_millis(200))
            .unwrap()
            .0;
    }
    drop(n);
    let elapsed = started.elapsed();
    let snap = job.snapshot();
    drop(job);
    let store = PretranslateStore::load(&path).expect("saved");
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "done: {} pages in {elapsed:.1?} ({:.1} s a page), {} translated ({:.2}/s overall), {} skipped ({:.0}%), {} failed, file {} bytes",
        store.pages_done(),
        elapsed.as_secs_f64() / f64::from(pages.max(1)),
        snap.translated,
        snap.translated as f64 / elapsed.as_secs_f64(),
        snap.skipped,
        100.0 * snap.skipped as f64 / (snap.translated + snap.skipped).max(1) as f64,
        snap.failed,
        size
    );
    for p in 0..2.min(pages) {
        for s in store.page(p).unwrap_or(&[]).iter().take(6) {
            println!("  p{p} {:?}", s.outcome);
        }
    }
}
