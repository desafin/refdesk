//! `annotations.json`: bookmarks, marks and highlights (ETC-2, ETC-3, ETC-5).

use std::fs;
use std::path::Path;
use std::sync::Arc;

use refdesk_core::store::bad_path;
use refdesk_core::{
    Annotations, CopyOptions, HighlightColor, ItemKind, Location, PageText, Point, Quad, Rect,
    TextBlock, TextChar, TextLine,
};

const CW: f32 = 10.0;
const LINE_H: f32 = 12.0;
const PITCH: f32 = 20.0;

/// A page whose lines start at (72, 100), one line every 20 pt, each character 10 pt wide.
fn page(lines: &[&str]) -> Arc<PageText> {
    let mut pt = PageText::default();
    let first_line = 0;
    for (n, text) in lines.iter().enumerate() {
        let y = 100.0 + n as f32 * PITCH;
        let start = pt.chars.len() as u32;
        for (i, ch) in text.chars().enumerate() {
            let x = 72.0 + i as f32 * CW;
            pt.chars.push(TextChar {
                ch,
                quad: Quad {
                    ul: Point::new(x, y),
                    ur: Point::new(x + CW, y),
                    ll: Point::new(x, y + LINE_H),
                    lr: Point::new(x + CW, y + LINE_H),
                },
                size: 10.0,
            });
        }
        let end = pt.chars.len() as u32;
        pt.lines.push(TextLine {
            chars: start..end,
            bounds: Rect::new(72.0, y, 72.0 + text.chars().count() as f32 * CW, y + LINE_H),
        });
    }
    let n = pt.lines.len() as u32;
    let bounds = pt
        .lines
        .iter()
        .map(|l| l.bounds)
        .reduce(|a, b| a.union(&b))
        .unwrap_or_default();
    pt.blocks.push(TextBlock {
        lines: first_line..n,
        bounds,
    });
    Arc::new(pt)
}

/// Three pages of the same two lines.
fn texts(p: u32) -> Option<Arc<PageText>> {
    (p < 3).then(|| page(&["The Execution Management shall", "start the processes."]))
}

const OPTS: CopyOptions = CopyOptions {
    join_lines: true,
    dehyphenate: true,
};

fn loc(page: u32, y: f32) -> Location {
    Location::new(page, 0.0, y)
}

fn a_pdf() -> &'static Path {
    Path::new("/docs/a.pdf")
}

// --- bookmarks -------------------------------------------------------------------------------

#[test]
fn bookmarks_are_added_renamed_and_removed_and_listed_in_document_order() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 10);
    let b = doc.add_bookmark("later", loc(5, 300.0), 10);
    let a = doc.add_bookmark("earlier", loc(1, 50.0), 11);
    let c = doc.add_bookmark("same page, higher", loc(5, 100.0), 12);
    assert!(a != b && b != c && a != c, "ids are unique");
    let names: Vec<&str> = doc.bookmarks().iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["earlier", "same page, higher", "later"]);

    assert!(doc.rename_bookmark(b, "renamed"));
    assert_eq!(doc.bookmark(b).map(|b| b.name.as_str()), Some("renamed"));
    assert!(!doc.rename_bookmark(999, "x"));
    assert!(doc.remove_bookmark(a));
    assert!(!doc.remove_bookmark(a));
    assert_eq!(doc.bookmarks().len(), 2);
}

#[test]
fn a_bookmark_on_the_same_page_within_forty_points_is_the_same_spot() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 10);
    let id = doc.add_bookmark("x", loc(3, 200.0), 10);
    assert_eq!(doc.bookmark_near(loc(3, 230.0)), Some(id));
    assert_eq!(doc.bookmark_near(loc(3, 160.0)), Some(id));
    assert_eq!(doc.bookmark_near(loc(3, 241.0)), None);
    assert_eq!(doc.bookmark_near(loc(4, 200.0)), None);
}

// --- marks -----------------------------------------------------------------------------------

#[test]
fn marks_take_lower_case_letters_and_are_replaced() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 10);
    assert!(doc.set_mark('a', loc(2, 10.0)));
    assert!(doc.set_mark('a', loc(7, 20.0)));
    assert!(!doc.set_mark('A', loc(1, 0.0)));
    assert!(!doc.set_mark('1', loc(1, 0.0)));
    assert_eq!(doc.mark('a'), Some(loc(7, 20.0)));
    assert_eq!(doc.mark('b'), None);
    assert_eq!(doc.marks().len(), 1);
}

#[test]
fn global_marks_take_upper_case_letters_and_point_into_a_document() {
    let mut store = Annotations::default();
    assert!(store.set_global_mark('A', a_pdf(), Some("fp"), loc(4, 40.0)));
    assert!(!store.set_global_mark('a', a_pdf(), None, loc(4, 40.0)));
    let mark = store.global_mark('A').expect("mark A");
    assert_eq!(mark.path, a_pdf());
    assert_eq!(mark.fingerprint.as_deref(), Some("fp"));
    assert_eq!(mark.loc, loc(4, 40.0));
    assert!(store.global_mark('B').is_none());
}

// --- highlights ------------------------------------------------------------------------------

#[test]
fn a_highlight_gets_its_rectangles_and_text_from_the_page_text() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 10);
    // "Execution Management" is characters 4..24 of the first line.
    let group = doc
        .add_highlight(HighlightColor::Green, &[(1, 4..24)], &texts, OPTS, 10)
        .expect("a highlight");
    let pieces = doc.group(group);
    assert_eq!(pieces.len(), 1);
    let h = pieces[0];
    assert_eq!(h.page, 1);
    assert_eq!(h.text, "Execution Management");
    assert_eq!(h.color, HighlightColor::Green);
    assert_eq!(h.rects.len(), 1);
    assert!((h.rects[0].x0 - 112.0).abs() < 0.01);
    assert!((h.rects[0].y0 - 100.0).abs() < 0.01);
}

#[test]
fn a_highlight_over_several_pages_is_one_piece_per_page_removed_as_a_group() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 10);
    let group = doc
        .add_highlight(
            HighlightColor::Yellow,
            &[(0, 30..50), (1, 0..50), (2, 0..10)],
            &texts,
            OPTS,
            10,
        )
        .expect("a highlight");
    let pieces = doc.group(group);
    assert_eq!(pieces.iter().map(|h| h.page).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(doc.group_heads().len(), 1);
    assert_eq!(doc.highlights_on(1..2).len(), 1);
    assert_eq!(doc.highlights_on(0..3).len(), 3);
    assert!(doc.highlights_on(3..9).is_empty());
    assert!(doc.remove_highlight(group));
    assert!(doc.highlights().is_empty());
    assert!(!doc.remove_highlight(group));
}

#[test]
fn nothing_is_highlighted_for_empty_ranges_or_pages_without_text() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 10);
    assert!(
        doc.add_highlight(HighlightColor::Yellow, &[(0, 5..5)], &texts, OPTS, 1)
            .is_none()
    );
    assert!(
        doc.add_highlight(HighlightColor::Yellow, &[(9, 0..5)], &texts, OPTS, 1)
            .is_none()
    );
    assert!(doc.highlights().is_empty());
}

#[test]
fn colour_and_note_change_for_the_whole_group() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 10);
    let group = doc
        .add_highlight(
            HighlightColor::Yellow,
            &[(0, 30..50), (1, 0..10)],
            &texts,
            OPTS,
            10,
        )
        .expect("a highlight");
    assert!(doc.set_highlight_color(group, HighlightColor::Pink));
    assert!(doc.set_highlight_note(group, "  check this  "));
    for h in doc.group(group) {
        assert_eq!(h.color, HighlightColor::Pink);
        assert_eq!(h.note, "check this");
    }
    assert!(!doc.set_highlight_note(999, "x"));
    assert!(!doc.set_highlight_color(999, HighlightColor::Blue));
}

#[test]
fn overlapping_highlights_of_the_same_colour_merge_and_of_another_colour_stay() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 10);
    let first = doc
        .add_highlight(HighlightColor::Yellow, &[(0, 4..13)], &texts, OPTS, 5)
        .expect("first");
    doc.set_highlight_note(first, "note one");
    let blue = doc
        .add_highlight(HighlightColor::Blue, &[(0, 0..3)], &texts, OPTS, 6)
        .expect("blue");
    // "Execution Management" overlaps "Execution": one yellow highlight covering both.
    let merged = doc
        .add_highlight(HighlightColor::Yellow, &[(0, 10..24)], &texts, OPTS, 7)
        .expect("merged");
    assert!(doc.group(first).is_empty(), "the old group is gone");
    let pieces = doc.group(merged);
    assert_eq!(pieces.len(), 1);
    assert_eq!(pieces[0].chars, 4..24);
    assert_eq!(pieces[0].text, "Execution Management");
    assert_eq!(pieces[0].note, "note one", "the note is kept");
    assert_eq!(pieces[0].created, 5, "the earlier time is kept");
    // A different colour over the same characters is a separate highlight.
    let green = doc
        .add_highlight(HighlightColor::Green, &[(0, 4..24)], &texts, OPTS, 8)
        .expect("green");
    assert_eq!(doc.group(blue).len(), 1);
    assert_eq!(doc.group_heads().len(), 3);
    assert_ne!(green, merged);
}

#[test]
fn a_merge_keeps_the_pages_of_the_old_highlight_that_the_new_one_does_not_reach() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 10);
    doc.add_highlight(
        HighlightColor::Yellow,
        &[(0, 40..50), (1, 0..10)],
        &texts,
        OPTS,
        5,
    )
    .expect("first");
    let merged = doc
        .add_highlight(HighlightColor::Yellow, &[(0, 20..45)], &texts, OPTS, 6)
        .expect("merged");
    let pages: Vec<(u32, std::ops::Range<u32>)> = doc
        .group(merged)
        .iter()
        .map(|h| (h.page, h.chars.clone()))
        .collect();
    assert_eq!(pages, [(0, 20..50), (1, 0..10)]);
}

#[test]
fn items_are_bookmarks_and_highlights_in_document_order() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 10);
    let late = doc.add_bookmark("late", loc(2, 500.0), 1);
    let group = doc
        .add_highlight(HighlightColor::Yellow, &[(1, 0..5)], &texts, OPTS, 1)
        .expect("hl");
    let early = doc.add_bookmark("early", loc(0, 10.0), 1);
    let items: Vec<(ItemKind, u64)> = doc.items().iter().map(|i| (i.kind, i.id)).collect();
    assert_eq!(
        items,
        [
            (ItemKind::Bookmark, early),
            (ItemKind::Highlight, group),
            (ItemKind::Bookmark, late),
        ]
    );
}

#[test]
fn the_text_of_a_group_joins_its_pages() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 10);
    let group = doc
        .add_highlight(
            HighlightColor::Yellow,
            &[(0, 30..50), (1, 0..3)],
            &texts,
            OPTS,
            1,
        )
        .expect("hl");
    assert_eq!(doc.group_text(group), "start the processes. The");
}

// --- finding a document ----------------------------------------------------------------------

#[test]
fn a_moved_file_is_found_by_its_fingerprint_and_the_record_takes_the_new_path() {
    let mut store = Annotations::default();
    store
        .doc_mut(Path::new("/old/spec.pdf"), Some("fp1"), 10)
        .add_bookmark("x", loc(1, 0.0), 10);
    store.set_global_mark('S', Path::new("/old/spec.pdf"), Some("fp1"), loc(2, 0.0));
    let new = Path::new("/new/renamed.pdf");
    assert!(store.doc(new, None).is_none(), "not by path alone");
    assert_eq!(
        store.doc(new, Some("fp1")).map(|d| d.bookmarks().len()),
        Some(1)
    );
    assert!(store.follow(new, "fp1"));
    assert!(!store.follow(new, "fp1"), "nothing more to change");
    let doc = store.doc(new, None).expect("found by the new path");
    assert_eq!(doc.path, new);
    assert_eq!(store.docs().len(), 1);
    assert_eq!(store.global_mark('S').map(|m| m.path.as_path()), Some(new));
}

#[test]
fn the_same_path_with_another_fingerprint_is_the_same_document() {
    let mut store = Annotations::default();
    store
        .doc_mut(a_pdf(), Some("v1"), 10)
        .add_bookmark("x", loc(1, 0.0), 10);
    assert!(store.doc(a_pdf(), Some("v2")).is_some());
    assert!(store.follow(a_pdf(), "v2"));
    assert_eq!(
        store
            .doc(a_pdf(), None)
            .and_then(|d| d.fingerprint.as_deref()),
        Some("v2")
    );
}

#[test]
fn the_most_recently_used_record_wins_and_on_a_tie_the_path() {
    let mut store = Annotations::default();
    store
        .doc_mut(Path::new("/x/a.pdf"), Some("fp"), 10)
        .add_bookmark("by path", loc(1, 0.0), 10);
    // A copy of the same content elsewhere, used later (`doc_mut` would have adopted the first
    // record, so the second one is made without a fingerprint and given it afterwards).
    let copy = store.doc_mut(Path::new("/y/copy.pdf"), None, 20);
    copy.fingerprint = Some("fp".into());
    copy.add_bookmark("by content, newer", loc(1, 0.0), 20);
    let found = store.doc(Path::new("/x/a.pdf"), Some("fp")).expect("found");
    assert_eq!(found.bookmarks()[0].name, "by content, newer");

    let mut store = Annotations::default();
    store
        .doc_mut(Path::new("/x/a.pdf"), None, 10)
        .add_bookmark("by path", loc(1, 0.0), 10);
    let other = store.doc_mut(Path::new("/y/b.pdf"), None, 10);
    other.fingerprint = Some("fp".into());
    other.add_bookmark("by content", loc(1, 0.0), 10);
    let found = store.doc(Path::new("/x/a.pdf"), Some("fp")).expect("found");
    assert_eq!(found.bookmarks()[0].name, "by path");
    // Without a fingerprint, nothing matches by content.
    assert!(store.doc(Path::new("/z/c.pdf"), None).is_none());
}

// --- the file --------------------------------------------------------------------------------

fn filled() -> Annotations {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), Some("fp"), 100);
    doc.add_bookmark("7.3 State management", loc(86, 120.5), 100);
    doc.set_mark('a', loc(11, 30.0));
    let Some(group) = doc.add_highlight(
        HighlightColor::Orange,
        &[(0, 4..24), (1, 0..3)],
        &texts,
        OPTS,
        100,
    ) else {
        panic!("no highlight");
    };
    doc.set_highlight_note(group, "read again");
    store.set_global_mark('Q', a_pdf(), Some("fp"), loc(3, 3.0));
    store
}

#[test]
fn the_file_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("annotations.json");
    let store = filled();
    store.save(&path).unwrap();
    let loaded = Annotations::load(&path);
    assert_eq!(loaded, store);
    assert!(!bad_path(&path).exists());
    // New ids after loading do not collide with the stored ones.
    let mut loaded = loaded;
    let doc = loaded.doc_mut(a_pdf(), None, 200);
    let id = doc.add_bookmark("new", loc(1, 1.0), 200);
    let ids: Vec<u64> = doc.bookmarks().iter().map(|b| b.id).collect();
    assert_eq!(ids.iter().filter(|i| **i == id).count(), 1);
    assert!(doc.highlights().iter().all(|h| h.id != id && h.group != id));
}

#[test]
fn records_without_anything_are_not_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("annotations.json");
    let mut store = Annotations::default();
    store.doc_mut(Path::new("/docs/empty.pdf"), None, 1);
    store.save(&path).unwrap();
    assert!(Annotations::load(&path).docs().is_empty());
}

#[test]
fn a_missing_or_empty_file_gives_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("annotations.json");
    assert_eq!(Annotations::load(&path), Annotations::default());
    fs::write(&path, "  \n").unwrap();
    assert_eq!(Annotations::load(&path), Annotations::default());
    assert!(!bad_path(&path).exists());
}

#[test]
fn a_damaged_file_gives_nothing_and_is_kept_as_bad() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("annotations.json");
    fs::write(&path, b"{ not json").unwrap();
    assert_eq!(Annotations::load(&path), Annotations::default());
    assert_eq!(fs::read(bad_path(&path)).unwrap(), b"{ not json");
    assert!(!path.exists());
}

#[test]
fn one_damaged_entry_is_dropped_alone_and_unknown_fields_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("annotations.json");
    let text = r#"{
      "version": 9, "from_the_future": [1, 2],
      "documents": [
        {"path": "/docs/a.pdf", "fingerprint": "fp", "last_used": 5, "mystery": true,
         "bookmarks": [
            {"id": 1, "name": "good", "loc": {"page": 2, "x": 0, "y": 10}, "extra": 1},
            {"id": 2, "name": "no place"},
            {"name": "no id", "loc": {"page": 3, "x": 0, "y": 0}}
         ],
         "marks": {"a": {"page": 1, "x": 0, "y": 5}, "ab": {"page": 1, "x": 0, "y": 5},
                   "b": "nowhere"},
         "highlights": [
            {"id": 7, "group": 7, "color": "purple", "page": 0, "chars": {"start": 0, "end": 3},
             "rects": [{"x0": 1, "y0": 2, "x1": 3, "y1": 4}]},
            {"id": 8, "group": 8, "page": 0, "chars": {"start": 9, "end": 3},
             "rects": []},
            {"id": 9, "page": "one"}
         ]},
        {"no_path": true},
        "junk"
      ],
      "global_marks": {"A": {"path": "/docs/a.pdf", "loc": {"page": 1, "x": 0, "y": 0}},
                       "b": {"path": "/docs/a.pdf", "loc": {"page": 1, "x": 0, "y": 0}}}
    }"#;
    fs::write(&path, text).unwrap();
    let store = Annotations::load(&path);
    assert_eq!(store.docs().len(), 1);
    let doc = store.doc(a_pdf(), None).expect("the document");
    let names: Vec<&str> = doc.bookmarks().iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["good", "no id"]);
    assert!(
        doc.bookmarks().iter().all(|b| b.id != 0),
        "ids are filled in"
    );
    assert_eq!(doc.marks().keys().copied().collect::<Vec<_>>(), ['a']);
    assert_eq!(doc.highlights().len(), 1);
    assert_eq!(
        doc.highlights()[0].color,
        HighlightColor::Yellow,
        "an unknown colour reads as yellow"
    );
    assert!(store.global_mark('A').is_some());
    assert_eq!(store.global_marks().len(), 1);
    // The original is kept for the reader; the file itself is replaced at the next save.
    assert_eq!(fs::read_to_string(bad_path(&path)).unwrap(), text);
}

#[test]
fn listing_thousands_of_highlights_by_page_is_quick() {
    let mut store = Annotations::default();
    let doc = store.doc_mut(a_pdf(), None, 1);
    let many = |p: u32| (p < 2000).then(|| page(&["The Execution Management shall"]));
    for p in 0..2000 {
        doc.add_highlight(HighlightColor::Yellow, &[(p, 4..13)], &many, OPTS, 1);
    }
    assert_eq!(doc.highlights().len(), 2000);
    let started = std::time::Instant::now();
    for p in 0..2000 {
        assert_eq!(
            doc.highlights_on(p..p + 2).len(),
            if p == 1999 { 1 } else { 2 }
        );
    }
    assert!(
        started.elapsed().as_millis() < 50,
        "{:?}",
        started.elapsed()
    );
}
