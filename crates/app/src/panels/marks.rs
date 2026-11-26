//! Bookmarks, one-letter marks and highlights (ETC-2, ETC-3, ETC-5): what `dispatch` does with
//! them, the per-frame upkeep, and the "Marks" view of the sidebar.
//!
//! The store is `refdesk_core::Annotations` (`annotations.json`), held in [`MarksApp`] — one for
//! the application. Only `dispatch` changes it (R-6), except that [`tick`] moves a record to the
//! new path of a moved file once the file's fingerprint is known. Changes are saved two seconds
//! after the last one, on a thread, and at exit.
//!
//! A document's record is found by its path and its content fingerprint (`session::Memory`
//! computes the fingerprints). Actions name a document by its path; `dispatch` adds the
//! fingerprint it knows.
//!
//! Places are page-space locations (R-5). A bookmark or mark is the place where the section path
//! line reads (a quarter down the view, `toc::reading_location`); going there puts that place
//! back at the same height, so "next" and "previous" step from where the reader is.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use egui::{
    Align, Key, Layout, Modifiers, Pos2, Rect, ScrollArea, Sense, TextEdit, TextStyle, Ui,
    UiBuilder, WidgetInfo, vec2,
};
use refdesk_core::store::loc_cmp;
use refdesk_core::{
    Annotations, DocAnnotations, HighlightColor, ItemKind, Location, Outline, PageLayout, PageText,
    Selection, Size, natural_cmp,
};

use crate::action::{Action, OpenTarget, dispatch as dispatch_action};
use crate::i18n::tr;
use crate::panels::breadcrumb::current_section;
use crate::panels::global_search::{TabSpot, file_name, pick_tab};
use crate::panels::search::line_job;
use crate::panels::sidebar::SidebarView;
use crate::panels::toc::{reading_location, scroll_target, triangle};
use crate::state::{
    AppState, DocId, DocStore, KeyGrab, StatusNotice, Tab, TabId, TabStore, WindowId,
};
use crate::views::copy_text::reading_place;
use crate::views::highlights::{self, DocView, HighlightEnv, MarkGrab};
use crate::views::links::page_label;
use crate::views::pdf_view::anchor_at;
use crate::views::selection::COPY_OPTIONS;

/// Seconds after the last change at which the store is written.
pub const SAVE_DELAY_SECS: f64 = 2.0;
/// Seconds the letter after "Set Mark" / "Go to Mark" is waited for.
pub const GRAB_SECS: f64 = 2.0;
/// Seconds a notice about marks stays in the status bar.
const NOTICE_SECS: f64 = 3.0;
/// Two places closer than this (in points, vertically, on one page) are the same place when
/// stepping to the next bookmark or highlight.
pub const STEP_EPS: f32 = 1.0;
/// How many characters of a highlight's text a row and its accessible name show.
const ROW_TEXT_CHARS: usize = 60;

// ---------------------------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------------------------

/// What can be done with bookmarks, marks and highlights. Carried by `Action::Marks`.
#[derive(Clone, Debug, PartialEq)]
pub enum MarksAction {
    /// Bookmark the place a PDF tab is reading (Ctrl+D), or show the bookmark that is there.
    AddBookmark(TabId),
    /// Move a PDF tab to the next (or previous) bookmark of its document.
    StepBookmark {
        /// The tab.
        tab: TabId,
        /// Next rather than previous.
        forward: bool,
    },
    /// Move a PDF tab to the next (or previous) highlight of its document.
    StepHighlight {
        /// The tab.
        tab: TabId,
        /// Next rather than previous.
        forward: bool,
    },
    /// Wait for the letter of a mark in a PDF tab (M sets, ' goes).
    StartMark {
        /// The tab.
        tab: TabId,
        /// Go to the mark rather than set it.
        go: bool,
    },
    /// The key after `StartMark`: a letter, or `None` when another key cancelled it.
    FinishMark {
        /// The tab that waited.
        tab: TabId,
        /// The mark's letter.
        letter: Option<char>,
    },
    /// Highlight the selected text of a PDF tab, in `color` or else the last colour used.
    Highlight {
        /// The tab.
        tab: TabId,
        /// The colour; `None` for the last one used.
        color: Option<HighlightColor>,
    },
    /// Copy the text and notes of every highlight of a PDF tab's document.
    CopyAllHighlights(TabId),
    /// Show the Marks view in the sidebar of the focused window.
    Show,
    /// Rename a bookmark.
    RenameBookmark {
        /// The document's path.
        doc: PathBuf,
        /// The bookmark.
        id: u64,
        /// The new name.
        name: String,
    },
    /// Remove a bookmark.
    RemoveBookmark {
        /// The document's path.
        doc: PathBuf,
        /// The bookmark.
        id: u64,
    },
    /// Change the colour of a highlight.
    SetColor {
        /// The document's path.
        doc: PathBuf,
        /// The highlight's group.
        group: u64,
        /// The new colour.
        color: HighlightColor,
    },
    /// Change the note of a highlight (empty removes it).
    SetNote {
        /// The document's path.
        doc: PathBuf,
        /// The highlight's group.
        group: u64,
        /// The note.
        note: String,
    },
    /// Remove a highlight with all its pages.
    RemoveHighlight {
        /// The document's path.
        doc: PathBuf,
        /// The highlight's group.
        group: u64,
    },
    /// Copy the text of a highlight.
    CopyHighlight {
        /// The document's path.
        doc: PathBuf,
        /// The highlight's group.
        group: u64,
    },
    /// Open the Marks view of the focused window with the note of a highlight being edited.
    EditNote {
        /// The document's path.
        doc: PathBuf,
        /// The highlight's group.
        group: u64,
    },
    /// Go to a place of a document: in a tab that shows it (the focused one, else one of
    /// `window`, else any), opening the document when no tab does. A long jump in the history.
    GoTo {
        /// The document's path.
        doc: PathBuf,
        /// The place.
        loc: Location,
        /// The window that asked.
        window: Option<WindowId>,
    },
}

// ---------------------------------------------------------------------------------------------
// The state
// ---------------------------------------------------------------------------------------------

/// A highlight whose page texts were fetched on a thread.
struct Pending {
    path: PathBuf,
    generation: Option<u32>,
    color: HighlightColor,
    pieces: Vec<(u32, Range<u32>)>,
    texts: Vec<(u32, Arc<PageText>)>,
}

/// Where the last jump to a mark left a view, to step on from the item rather than from the
/// reading line when the view could not put the item there (near the end of the document).
#[derive(Clone, Copy, Debug, PartialEq)]
struct LastJump {
    tab: TabId,
    anchor: Location,
    at: Location,
}

/// The marks of the application (`AppState::marks`).
pub struct MarksApp {
    /// Everything marked, as in `annotations.json`. Only `dispatch` changes it.
    pub store: Annotations,
    /// Counts changes of `store`; snapshots and rows are made again on a change.
    pub version: u64,
    /// The colour of the last highlight (yellow at first).
    pub last_color: HighlightColor,
    /// egui time of the last change not yet written.
    dirty_since: Option<f64>,
    /// When the wait for a mark letter began (egui time).
    grab_started: Option<f64>,
    pending: Arc<Mutex<Vec<Pending>>>,
    writer: Option<JoinHandle<()>>,
    last_jump: Option<LastJump>,
    /// Snapshots of the highlights of each open document, by path.
    views: HashMap<PathBuf, (u64, Arc<DocView>)>,
    /// The (path, fingerprint) of each open document, as of the last frame.
    keys: HashMap<DocId, (PathBuf, Option<String>)>,
    /// Documents already followed to their fingerprint.
    followed: HashSet<(PathBuf, String)>,
    /// How many times the store was written (tests).
    pub saves: u32,
}

impl Default for MarksApp {
    fn default() -> MarksApp {
        MarksApp {
            store: Annotations::default(),
            version: 0,
            last_color: HighlightColor::Yellow,
            dirty_since: None,
            grab_started: None,
            pending: Arc::default(),
            writer: None,
            last_jump: None,
            views: HashMap::new(),
            keys: HashMap::new(),
            followed: HashSet::new(),
            saves: 0,
        }
    }
}

impl MarksApp {
    /// The snapshot of the highlights of the document at `path`.
    fn view_for(&mut self, path: &Path, fingerprint: Option<&str>) -> Arc<DocView> {
        if let Some((version, view)) = self.views.get(path)
            && *version == self.version
        {
            return Arc::clone(view);
        }
        let highlights = self
            .store
            .doc(path, fingerprint)
            .map(|d| d.highlights().to_vec())
            .unwrap_or_default();
        let view = Arc::new(DocView::new(path.to_path_buf(), highlights));
        self.views
            .insert(path.to_path_buf(), (self.version, Arc::clone(&view)));
        view
    }

    /// Waits for a write in progress (tests).
    #[cfg(test)]
    pub fn wait_for_writer(&mut self) {
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }

    /// Whether a change waits to be written.
    #[cfg(test)]
    pub fn is_dirty(&self) -> bool {
        self.dirty_since.is_some()
    }
}

/// What the Marks view lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The document of the focused tab.
    #[default]
    ThisDocument,
    /// Every document with marks, grouped by document.
    AllDocuments,
}

/// A bookmark or a highlight, named so that it survives changes of the store.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ItemRef {
    /// The document's path, as in the store.
    pub doc: PathBuf,
    /// Bookmark or highlight.
    pub kind: ItemKind,
    /// The bookmark's id or the highlight's group.
    pub id: u64,
}

/// What a row being edited changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    /// The name of a bookmark.
    Name,
    /// The note of a highlight.
    Note,
}

/// A row of the Marks view that is being edited.
#[derive(Clone, Debug, PartialEq)]
pub struct Editing {
    /// The bookmark or highlight.
    pub item: ItemRef,
    /// Name or note.
    pub kind: EditKind,
    /// The text in the field.
    pub text: String,
    /// The field takes the keyboard (with its text selected) in the next frame.
    pub focus: bool,
}

/// One section of the Marks view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    /// Bookmarks, and the marks after them.
    Bookmarks,
    /// Highlights.
    Highlights,
}

/// One row of the Marks view. Indices point into `Annotations::docs` and into the bookmarks or
/// highlight pieces of that record; they hold while the store's version is the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A section's head: fold triangle, name and count.
    Section {
        /// Which section.
        section: Section,
        /// How many bookmarks or highlights it lists.
        count: usize,
    },
    /// The name of a document ("All documents" only).
    Doc {
        /// The record.
        doc: usize,
    },
    /// A bookmark.
    Bookmark {
        /// The record.
        doc: usize,
        /// Index into its bookmarks.
        index: usize,
    },
    /// A mark: lower case of the document, upper case a global mark that points into it.
    Mark {
        /// The record.
        doc: usize,
        /// The letter.
        letter: char,
    },
    /// A highlight, shown by its first piece.
    Highlight {
        /// The record.
        doc: usize,
        /// Index of the first piece into its highlights.
        index: usize,
    },
    /// The note of the highlight in the row above.
    Note {
        /// The record.
        doc: usize,
        /// Index of the highlight's first piece.
        index: usize,
    },
}

/// What the rows depend on besides the store.
#[derive(Clone, Debug, PartialEq)]
pub struct RowQuery {
    /// This document or all.
    pub scope: Scope,
    /// The record of the focused document, if it has one.
    pub current: Option<usize>,
    /// Text that names, texts or notes must contain (case does not count).
    pub filter: String,
    /// Only highlights of this colour.
    pub color: Option<HighlightColor>,
    /// The bookmark section is unfolded.
    pub bookmarks_open: bool,
    /// The highlight section is unfolded.
    pub highlights_open: bool,
}

/// The Marks view of one window. Not saved.
#[derive(Clone, Debug)]
pub struct MarksView {
    /// This document or all.
    pub scope: Scope,
    /// The filter text.
    pub filter: String,
    /// The colour chip that is on.
    pub color: Option<HighlightColor>,
    /// The bookmark section is unfolded.
    pub bookmarks_open: bool,
    /// The highlight section is unfolded.
    pub highlights_open: bool,
    /// The row being renamed or annotated.
    pub editing: Option<Editing>,
    /// A row to scroll into view in the next frame.
    pub reveal: Option<ItemRef>,
    offset: f32,
    view_h: f32,
    rows: Option<(u64, RowQuery, Arc<Vec<Row>>)>,
}

impl Default for MarksView {
    fn default() -> MarksView {
        MarksView {
            scope: Scope::ThisDocument,
            filter: String::new(),
            color: None,
            bookmarks_open: true,
            highlights_open: true,
            editing: None,
            reveal: None,
            offset: 0.0,
            view_h: 0.0,
            rows: None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------------------------

/// The name a new bookmark at `at` gets: the title of the section it is in, else "p. label".
pub fn default_bookmark_name(outline: &Outline, labels: &[String], at: Location) -> String {
    let title = current_section(outline, at)
        .and_then(|i| outline.items.get(i as usize))
        .map(|item| item.title.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty());
    title.unwrap_or_else(|| tr("marks.page_name").replace("{}", &page_label(at.page, labels)))
}

/// The index of the place to step to from `at` among `locs` (in document order): the first one
/// below it, or the last one above it. Places on the same page within [`STEP_EPS`] of `at` are
/// where the reader is and are passed over.
pub fn step_index(locs: &[Location], at: Location, forward: bool) -> Option<usize> {
    let below = |l: &Location| l.page > at.page || (l.page == at.page && l.y > at.y + STEP_EPS);
    let above = |l: &Location| l.page < at.page || (l.page == at.page && l.y < at.y - STEP_EPS);
    if forward {
        locs.iter().position(below)
    } else {
        locs.iter().rposition(above)
    }
}

/// The anchor (top-left place of the view) that shows `loc` on the reading line of a view laid
/// out as `layout` with `viewport`, keeping the view's horizontal position (`current` is its
/// anchor now). The view's scroll limits apply, so the anchor is the one the view will keep.
/// Without a layout (a tab that has not been drawn yet) the place goes a little below the top.
pub fn reading_anchor(
    layout: Option<&PageLayout>,
    viewport: Size,
    current: Location,
    loc: Location,
) -> Location {
    let Some(layout) = layout.filter(|l| loc.page < l.page_count()) else {
        return Location::new(loc.page, 0.0, (loc.y - 72.0).max(0.0));
    };
    let top = layout.content_pos(current);
    let line = layout.content_pos(reading_location(current, Some(layout), viewport.h));
    let target = layout.content_pos(loc);
    let want = refdesk_core::Point::new(top.x, target.y - (line.y - top.y));
    anchor_at(layout, layout.clamp_scroll(want, viewport))
}

/// The characters of `sel` on each of its pages: `None` when the text of one of its pages is not
/// there yet.
pub fn selection_pieces(
    sel: &Selection,
    texts: &dyn Fn(u32) -> Option<Arc<PageText>>,
) -> Option<Vec<(u32, Range<u32>)>> {
    let mut out = Vec::new();
    for page in sel.pages() {
        let text = texts(page)?;
        if let Some(range) = sel.range_on(page, text.chars.len() as u32)
            && range.start < range.end
        {
            out.push((page, range));
        }
    }
    Some(out)
}

/// The text "Copy All Highlights" puts on the clipboard: each highlight in document order with
/// its page label, its note on the next line, a blank line between them.
pub fn all_highlights_text(doc: &DocAnnotations, labels: &[String]) -> String {
    let mut parts = Vec::new();
    for head in doc.group_heads() {
        let mut part = tr("marks.copy_entry")
            .replace("{page}", &page_label(head.page, labels))
            .replace("{text}", &doc.group_text(head.group));
        if !head.note.is_empty() {
            part.push('\n');
            part.push_str(&tr("marks.copy_note").replace("{}", &head.note));
        }
        parts.push(part);
    }
    parts.join("\n\n")
}

/// The first `n` characters of `text` on one line, with "…" when it was longer.
pub fn shorten(text: &str, n: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= n {
        line
    } else {
        let mut cut: String = line.chars().take(n).collect();
        cut.push('…');
        cut
    }
}

/// The global marks (upper case) that point at the document of record `doc`.
fn global_letters(store: &Annotations, doc: &DocAnnotations) -> Vec<char> {
    store
        .global_marks()
        .iter()
        .filter(|(_, m)| {
            m.path == doc.path
                || matches!(
                    (m.fingerprint.as_deref(), doc.fingerprint.as_deref()),
                    (Some(a), Some(b)) if a == b
                )
        })
        .map(|(c, _)| *c)
        .collect()
}

/// The rows of the Marks view for `q`: the bookmark section (bookmarks, then the marks while no
/// filter or colour is set) and the highlight section (a highlight, then its note when it has
/// one). "All documents" puts each document's rows under its name, documents in the natural
/// order of their file names. A section's head counts what passes the filter.
pub fn build_rows(store: &Annotations, q: &RowQuery) -> Vec<Row> {
    let docs: Vec<usize> = match q.scope {
        Scope::ThisDocument => q.current.into_iter().collect(),
        Scope::AllDocuments => {
            let mut all: Vec<usize> = (0..store.docs().len())
                .filter(|i| !store.docs()[*i].is_empty())
                .collect();
            all.sort_by(|a, b| {
                let (pa, pb) = (&store.docs()[*a].path, &store.docs()[*b].path);
                natural_cmp(&file_name(pa), &file_name(pb)).then_with(|| pa.cmp(pb))
            });
            all
        }
    };
    let filter = q.filter.trim().to_lowercase();
    let matches = |text: &str| filter.is_empty() || text.to_lowercase().contains(&filter);
    let grouped = q.scope == Scope::AllDocuments;

    let mut bookmark_rows = Vec::new();
    let mut bookmark_count = 0;
    let mut highlight_rows = Vec::new();
    let mut highlight_count = 0;
    for &d in &docs {
        let doc = &store.docs()[d];
        // Bookmarks and marks.
        let mut rows = Vec::new();
        for (index, b) in doc.bookmarks().iter().enumerate() {
            if matches(&b.name) {
                rows.push(Row::Bookmark { doc: d, index });
            }
        }
        bookmark_count += rows.len();
        if filter.is_empty() && q.color.is_none() {
            for letter in doc.marks().keys() {
                rows.push(Row::Mark {
                    doc: d,
                    letter: *letter,
                });
            }
            for letter in global_letters(store, doc) {
                rows.push(Row::Mark { doc: d, letter });
            }
        }
        if !rows.is_empty() {
            if grouped {
                bookmark_rows.push(Row::Doc { doc: d });
            }
            bookmark_rows.extend(rows);
        }
        // Highlights: one row per group, at its first piece.
        let texts: HashMap<u64, String> = if filter.is_empty() {
            HashMap::new()
        } else {
            let mut t: HashMap<u64, String> = HashMap::new();
            for h in doc.highlights() {
                let entry = t.entry(h.group).or_default();
                entry.push_str(&h.text.to_lowercase());
                entry.push(' ');
            }
            t
        };
        let mut seen = HashSet::new();
        let mut rows = Vec::new();
        for (index, h) in doc.highlights().iter().enumerate() {
            if !seen.insert(h.group) {
                continue;
            }
            if q.color.is_some_and(|c| c != h.color) {
                continue;
            }
            let hit = filter.is_empty()
                || texts.get(&h.group).is_some_and(|t| t.contains(&filter))
                || h.note.to_lowercase().contains(&filter);
            if !hit {
                continue;
            }
            rows.push(Row::Highlight { doc: d, index });
            if !h.note.is_empty() {
                rows.push(Row::Note { doc: d, index });
            }
            highlight_count += 1;
        }
        if !rows.is_empty() {
            if grouped {
                highlight_rows.push(Row::Doc { doc: d });
            }
            highlight_rows.extend(rows);
        }
    }

    let mut out = vec![Row::Section {
        section: Section::Bookmarks,
        count: bookmark_count,
    }];
    if q.bookmarks_open {
        out.extend(bookmark_rows);
    }
    out.push(Row::Section {
        section: Section::Highlights,
        count: highlight_count,
    });
    if q.highlights_open {
        out.extend(highlight_rows);
    }
    out
}

/// Where the item of a row is (none for heads and notes).
pub fn row_loc(store: &Annotations, row: Row) -> Option<Location> {
    match row {
        Row::Bookmark { doc, index } => {
            store.docs().get(doc)?.bookmarks().get(index).map(|b| b.loc)
        }
        Row::Highlight { doc, index } | Row::Note { doc, index } => store
            .docs()
            .get(doc)?
            .highlights()
            .get(index)
            .map(|h| h.loc()),
        Row::Mark { doc, letter } => {
            if letter.is_ascii_uppercase() {
                store.global_mark(letter).map(|m| m.loc)
            } else {
                store.docs().get(doc)?.mark(letter)
            }
        }
        Row::Section { .. } | Row::Doc { .. } => None,
    }
}

/// The bookmark or highlight of a row.
pub fn row_item(store: &Annotations, row: Row) -> Option<ItemRef> {
    match row {
        Row::Bookmark { doc, index } => {
            let rec = store.docs().get(doc)?;
            Some(ItemRef {
                doc: rec.path.clone(),
                kind: ItemKind::Bookmark,
                id: rec.bookmarks().get(index)?.id,
            })
        }
        Row::Highlight { doc, index } | Row::Note { doc, index } => {
            let rec = store.docs().get(doc)?;
            Some(ItemRef {
                doc: rec.path.clone(),
                kind: ItemKind::Highlight,
                id: rec.highlights().get(index)?.group,
            })
        }
        _ => None,
    }
}

/// The rows to tint as nearest to the reader: in each section, the last bookmark or highlight of
/// record `doc` that starts at or above `at`.
pub fn nearest_rows(store: &Annotations, rows: &[Row], doc: usize, at: Location) -> Vec<usize> {
    let mut bookmark = None;
    let mut highlight = None;
    for (i, row) in rows.iter().enumerate() {
        let slot = match row {
            Row::Bookmark { doc: d, .. } if *d == doc => &mut bookmark,
            Row::Highlight { doc: d, .. } if *d == doc => &mut highlight,
            _ => continue,
        };
        if let Some(loc) = row_loc(store, *row)
            && loc_cmp(&loc, &at) != std::cmp::Ordering::Greater
        {
            *slot = Some(i);
        }
    }
    bookmark.into_iter().chain(highlight).collect()
}

/// The accessible name of a row, e.g. "Bookmark: 7.3 State management, p. 87" or
/// "Highlight (yellow): The Execution Management shall…, p. 91". `labels` gives the page labels
/// of a document.
pub fn row_label(store: &Annotations, row: Row, labels: &dyn Fn(&Path) -> Vec<String>) -> String {
    let page_of = |doc: &DocAnnotations, page: u32| page_label(page, &labels(&doc.path));
    match row {
        Row::Section { section, count } => {
            let key = match section {
                Section::Bookmarks => "marks.section_bookmarks",
                Section::Highlights => "marks.section_highlights",
            };
            tr(key).replace("{n}", &count.to_string())
        }
        Row::Doc { doc } => store
            .docs()
            .get(doc)
            .map(|d| file_name(&d.path))
            .unwrap_or_default(),
        Row::Bookmark { doc, index } => {
            let Some((rec, b)) = store
                .docs()
                .get(doc)
                .and_then(|r| Some((r, r.bookmarks().get(index)?)))
            else {
                return String::new();
            };
            tr("marks.a11y_bookmark")
                .replace("{name}", &b.name)
                .replace("{page}", &page_of(rec, b.loc.page))
        }
        Row::Mark { doc, letter } => {
            let page = row_loc(store, row)
                .zip(store.docs().get(doc))
                .map(|(loc, rec)| page_of(rec, loc.page))
                .unwrap_or_default();
            tr("marks.a11y_mark")
                .replace("{letter}", &letter.to_string())
                .replace("{page}", &page)
        }
        Row::Highlight { doc, index } => {
            let Some((rec, h)) = store
                .docs()
                .get(doc)
                .and_then(|r| Some((r, r.highlights().get(index)?)))
            else {
                return String::new();
            };
            tr("marks.a11y_highlight")
                .replace("{color}", highlights::color_name(h.color))
                .replace("{text}", &shorten(&h.text, ROW_TEXT_CHARS))
                .replace("{page}", &page_of(rec, h.page))
        }
        Row::Note { doc, index } => store
            .docs()
            .get(doc)
            .and_then(|r| r.highlights().get(index))
            .map(|h| tr("marks.a11y_note").replace("{}", &shorten(&h.note, ROW_TEXT_CHARS)))
            .unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------------------------
// What `dispatch` does
// ---------------------------------------------------------------------------------------------

/// The path and fingerprint of an open document.
fn key_of(state: &AppState, doc: DocId) -> Option<(PathBuf, Option<String>)> {
    let path = state.docs.get(doc)?.path().to_path_buf();
    let fingerprint = state.memory.fingerprint_of(&path).map(str::to_string);
    Some((path, fingerprint))
}

/// The fingerprint known for `path` (when a document there is open).
fn fingerprint_of(state: &AppState, path: &Path) -> Option<String> {
    state.memory.fingerprint_of(path).map(str::to_string)
}

/// The store changed: snapshots are made again and a save is due.
fn changed(state: &mut AppState, ctx: &egui::Context) {
    state.marks.version += 1;
    state.marks.dirty_since = Some(ctx.input(|i| i.time));
    ctx.request_repaint();
}

fn notice(state: &mut AppState, text: String, ctx: &egui::Context) {
    state.status_notice = Some(StatusNotice {
        text,
        until: ctx.input(|i| i.time) + NOTICE_SECS,
    });
    ctx.request_repaint();
}

/// Applies one action.
pub fn dispatch(state: &mut AppState, action: MarksAction, ctx: &egui::Context) {
    match action {
        MarksAction::AddBookmark(tab) => add_bookmark(state, tab, ctx),
        MarksAction::StepBookmark { tab, forward } => {
            step(state, tab, ItemKind::Bookmark, forward, ctx)
        }
        MarksAction::StepHighlight { tab, forward } => {
            step(state, tab, ItemKind::Highlight, forward, ctx)
        }
        MarksAction::StartMark { tab, go } => start_mark(state, tab, go, ctx),
        MarksAction::FinishMark { tab, letter } => finish_mark(state, tab, letter, ctx),
        MarksAction::Highlight { tab, color } => highlight_selection(state, tab, color, ctx),
        MarksAction::CopyAllHighlights(tab) => copy_all(state, tab, ctx),
        MarksAction::Show => {
            let w = state.focused_window_index();
            open_view(state, w);
            ctx.request_repaint();
        }
        MarksAction::RenameBookmark { doc, id, name } => {
            let (fp, now) = (fingerprint_of(state, &doc), state.memory.now());
            let name = name.trim();
            if !name.is_empty()
                && state
                    .marks
                    .store
                    .doc_mut(&doc, fp.as_deref(), now)
                    .rename_bookmark(id, name)
            {
                changed(state, ctx);
            }
        }
        MarksAction::RemoveBookmark { doc, id } => {
            let (fp, now) = (fingerprint_of(state, &doc), state.memory.now());
            if state
                .marks
                .store
                .doc_mut(&doc, fp.as_deref(), now)
                .remove_bookmark(id)
            {
                changed(state, ctx);
            }
        }
        MarksAction::SetColor { doc, group, color } => {
            let (fp, now) = (fingerprint_of(state, &doc), state.memory.now());
            if state
                .marks
                .store
                .doc_mut(&doc, fp.as_deref(), now)
                .set_highlight_color(group, color)
            {
                changed(state, ctx);
            }
        }
        MarksAction::SetNote { doc, group, note } => {
            let (fp, now) = (fingerprint_of(state, &doc), state.memory.now());
            if state
                .marks
                .store
                .doc_mut(&doc, fp.as_deref(), now)
                .set_highlight_note(group, &note)
            {
                changed(state, ctx);
            }
        }
        MarksAction::RemoveHighlight { doc, group } => {
            let (fp, now) = (fingerprint_of(state, &doc), state.memory.now());
            if state
                .marks
                .store
                .doc_mut(&doc, fp.as_deref(), now)
                .remove_highlight(group)
            {
                changed(state, ctx);
            }
        }
        MarksAction::CopyHighlight { doc, group } => {
            let fp = fingerprint_of(state, &doc);
            let text = state
                .marks
                .store
                .doc(&doc, fp.as_deref())
                .map(|d| d.group_text(group))
                .unwrap_or_default();
            if !text.is_empty() {
                ctx.copy_text(text.clone());
                notice(state, tr("status.copied_text").replace("{}", &text), ctx);
            }
        }
        MarksAction::EditNote { doc, group } => {
            let fp = fingerprint_of(state, &doc);
            let note = state
                .marks
                .store
                .doc(&doc, fp.as_deref())
                .and_then(|d| d.group(group).first().map(|h| h.note.clone()));
            let Some(note) = note else {
                return;
            };
            let w = state.focused_window_index();
            open_view(state, w);
            let item = ItemRef {
                doc,
                kind: ItemKind::Highlight,
                id: group,
            };
            if let Some(window) = state.windows.get_mut(w) {
                show_item(&mut window.marks, &item);
                window.marks.editing = Some(Editing {
                    item,
                    kind: EditKind::Note,
                    text: note,
                    focus: true,
                });
            }
            ctx.request_repaint();
        }
        MarksAction::GoTo { doc, loc, window } => go_to_place(state, &doc, loc, window, ctx),
    }
}

/// Opens the sidebar of window `w` on its Marks view.
fn open_view(state: &mut AppState, w: usize) {
    if let Some(window) = state.windows.get_mut(w) {
        window.toc_open = true;
        window.sidebar_view = SidebarView::Marks;
    }
}

/// Makes the row of `item` show in a Marks view: no filter hides it, its section is unfolded,
/// and the list scrolls to it.
fn show_item(view: &mut MarksView, item: &ItemRef) {
    view.filter.clear();
    view.color = None;
    match item.kind {
        ItemKind::Bookmark => view.bookmarks_open = true,
        ItemKind::Highlight => view.highlights_open = true,
    }
    view.reveal = Some(item.clone());
}

/// Ctrl+D: bookmarks the reading place of `tab`, named after its section, and opens the Marks
/// view with the new row being renamed. A bookmark at the same spot is shown instead.
fn add_bookmark(state: &mut AppState, tab: TabId, ctx: &egui::Context) {
    let Some(pdf) = state.tabs.pdf(tab) else {
        return;
    };
    let Some(info) = state.docs.get(pdf.doc).and_then(|d| d.info()) else {
        return;
    };
    let Some((path, fp)) = key_of(state, pdf.doc) else {
        return;
    };
    let at = reading_place(pdf);
    let now = state.memory.now();
    let doc = state.marks.store.doc_mut(&path, fp.as_deref(), now);
    let (id, new_name) = match doc.bookmark_near(at) {
        Some(id) => (id, None),
        None => {
            let name = default_bookmark_name(&info.outline, &info.page_labels, at);
            (doc.add_bookmark(&name, at, now), Some(name))
        }
    };
    let record_path = doc.path.clone();
    if new_name.is_some() {
        changed(state, ctx);
    }
    let Some(w) = state.window_of(tab) else {
        return;
    };
    open_view(state, w);
    let item = ItemRef {
        doc: record_path,
        kind: ItemKind::Bookmark,
        id,
    };
    let view = &mut state.windows[w].marks;
    show_item(view, &item);
    view.editing = new_name.map(|text| Editing {
        item,
        kind: EditKind::Name,
        text,
        focus: true,
    });
    ctx.request_repaint();
}

/// The place stepping starts from in `tab`: where the last jump put the item, when the view has
/// not moved since; before everything when stepping forward from the very top (what is above
/// the reading line there was never passed); else the reading place.
fn step_base(state: &AppState, tab: TabId, forward: bool) -> Option<Location> {
    let pdf = state.tabs.pdf(tab)?;
    let near = |a: Location, b: Location| {
        a.page == b.page && (a.x - b.x).abs() < 0.5 && (a.y - b.y).abs() < 0.5
    };
    Some(match state.marks.last_jump {
        Some(j) if j.tab == tab && near(j.anchor, pdf.view.anchor) => j.at,
        _ if forward && pdf.ui.scroll().y <= 0.5 => Location::new(0, 0.0, f32::MIN),
        _ => reading_place(pdf),
    })
}

/// "Next/Previous Bookmark" and "Next/Previous Highlight".
fn step(state: &mut AppState, tab: TabId, kind: ItemKind, forward: bool, ctx: &egui::Context) {
    let Some(pdf) = state.tabs.pdf(tab) else {
        return;
    };
    let Some((path, fp)) = key_of(state, pdf.doc) else {
        return;
    };
    let Some(base) = step_base(state, tab, forward) else {
        return;
    };
    let locs: Vec<Location> = state
        .marks
        .store
        .doc(&path, fp.as_deref())
        .map(|d| {
            d.items()
                .into_iter()
                .filter(|i| i.kind == kind)
                .map(|i| i.loc)
                .collect()
        })
        .unwrap_or_default();
    match step_index(&locs, base, forward) {
        Some(i) => jump_in_tab(state, tab, locs[i]),
        None => {
            let key = match kind {
                ItemKind::Bookmark => "marks.no_more_bookmarks",
                ItemKind::Highlight => "marks.no_more_highlights",
            };
            notice(state, tr(key).to_owned(), ctx);
        }
    }
    ctx.request_repaint();
}

/// Moves `tab` so that `loc` is on its reading line, as a long jump in its history (Back
/// returns), and marks the place for a moment.
fn jump_in_tab(state: &mut AppState, tab: TabId, loc: Location) {
    let pages = state
        .tabs
        .pdf(tab)
        .and_then(|p| state.docs.get(p.doc))
        .and_then(|d| d.info())
        .map(|i| i.page_sizes.len() as u32);
    let loc = match pages {
        Some(0) => return,
        Some(n) => Location {
            page: loc.page.min(n - 1),
            ..loc
        },
        None => loc,
    };
    let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) else {
        return;
    };
    let anchor = reading_anchor(pdf.ui.layout(), pdf.ui.viewport(), pdf.view.anchor, loc);
    let doc = pdf.view.history.current().doc.clone();
    let from = refdesk_core::Place {
        doc: doc.clone(),
        loc: pdf.view.anchor,
    };
    pdf.view
        .history
        .jump(from, refdesk_core::Place { doc, loc: anchor });
    pdf.view.anchor = anchor;
    pdf.ui.links.mark_arrival(loc);
    state.marks.last_jump = Some(LastJump {
        tab,
        anchor,
        at: loc,
    });
}

/// Goes to `loc` in the document at `path`: in a tab that shows it, or in a new one.
fn go_to_place(
    state: &mut AppState,
    path: &Path,
    loc: Location,
    window: Option<WindowId>,
    ctx: &egui::Context,
) {
    let tab = state.docs.find(path).and_then(|doc| {
        let mut spots = Vec::new();
        for w in &state.windows {
            for tab in w.dock.tabs() {
                if state.tabs.pdf(tab).is_some_and(|p| p.doc == doc) {
                    spots.push(TabSpot {
                        tab,
                        window: w.id,
                        visible: w.dock.focused_tab() == Some(tab),
                    });
                }
            }
        }
        pick_tab(&spots, state.focus, window)
    });
    let tab = match tab {
        Some(tab) => tab,
        None => {
            if let Some(w) = window.and_then(|id| state.window_index(id)) {
                state.follow_window_focus(w);
            }
            let target = OpenTarget {
                path: path.to_path_buf(),
                page: None,
            };
            dispatch_action(state, Action::OpenFiles(vec![target]), ctx);
            let opened = state.docs.find(path);
            let shows = |t: TabId| opened.is_some() && state.tabs.pdf(t).map(|p| p.doc) == opened;
            match state.focus.filter(|t| shows(*t)) {
                Some(tab) => tab,
                None => return,
            }
        }
    };
    if state.focus != Some(tab) {
        dispatch_action(state, Action::FocusTab(tab), ctx);
    }
    jump_in_tab(state, tab, loc);
    ctx.request_repaint();
}

/// M or ': waits for the letter of a mark in `tab`.
fn start_mark(state: &mut AppState, tab: TabId, go: bool, ctx: &egui::Context) {
    if state.tabs.pdf(tab).is_none() {
        return;
    }
    state.key_grab = Some(if go {
        KeyGrab::GoToMark(tab)
    } else {
        KeyGrab::SetMark(tab)
    });
    let now = ctx.input(|i| i.time);
    state.marks.grab_started = Some(now);
    state.status_notice = Some(StatusNotice {
        text: tr("marks.press_letter").to_owned(),
        until: now + GRAB_SECS,
    });
    ctx.request_repaint();
}

/// Ends the wait for a mark letter: clears `key_grab` and the prompt.
fn end_grab(state: &mut AppState) {
    state.key_grab = None;
    state.marks.grab_started = None;
    if state
        .status_notice
        .as_ref()
        .is_some_and(|n| n.text == tr("marks.press_letter"))
    {
        state.status_notice = None;
    }
}

/// The letter after M or ' (or `None` for another key).
fn finish_mark(state: &mut AppState, tab: TabId, letter: Option<char>, ctx: &egui::Context) {
    let go = match state.key_grab {
        Some(KeyGrab::SetMark(t)) if t == tab => false,
        Some(KeyGrab::GoToMark(t)) if t == tab => true,
        _ => return,
    };
    end_grab(state);
    let Some(letter) = letter else {
        ctx.request_repaint();
        return;
    };
    let Some(pdf) = state.tabs.pdf(tab) else {
        return;
    };
    let Some((path, fp)) = key_of(state, pdf.doc) else {
        return;
    };
    let name = tr("marks.mark_name").replace("{}", &letter.to_string());
    if !go {
        let at = reading_place(pdf);
        let now = state.memory.now();
        let set = if letter.is_ascii_uppercase() {
            state
                .marks
                .store
                .set_global_mark(letter, &path, fp.as_deref(), at)
        } else {
            state
                .marks
                .store
                .doc_mut(&path, fp.as_deref(), now)
                .set_mark(letter, at)
        };
        if set {
            changed(state, ctx);
            notice(state, name, ctx);
        }
        return;
    }
    let missing = tr("marks.no_mark").replace("{}", &letter.to_string());
    if letter.is_ascii_lowercase() {
        match state
            .marks
            .store
            .doc(&path, fp.as_deref())
            .and_then(|d| d.mark(letter))
        {
            Some(loc) => jump_in_tab(state, tab, loc),
            None => notice(state, missing, ctx),
        }
        ctx.request_repaint();
        return;
    }
    let Some(mark) = state.marks.store.global_mark(letter).cloned() else {
        notice(state, missing, ctx);
        return;
    };
    let same_doc = mark.path == path
        || matches!((mark.fingerprint.as_deref(), fp.as_deref()), (Some(a), Some(b)) if a == b);
    if same_doc {
        jump_in_tab(state, tab, mark.loc);
    } else {
        let window = state.window_of(tab).map(|w| state.windows[w].id);
        go_to_place(state, &mark.path, mark.loc, window, ctx);
    }
    ctx.request_repaint();
}

/// Highlights the selection of `tab` and clears it. When the text of a selected page is not
/// there yet, a thread waits for it and [`tick`] adds the highlight (R-1).
fn highlight_selection(
    state: &mut AppState,
    tab: TabId,
    color: Option<HighlightColor>,
    ctx: &egui::Context,
) {
    let color = color.unwrap_or(state.marks.last_color);
    let Some(pdf) = state.tabs.pdf(tab) else {
        return;
    };
    let Some(sel) = pdf.ui.selection.active() else {
        return;
    };
    let Some(doc) = state.docs.get(pdf.doc).cloned() else {
        return;
    };
    let Some((path, fp)) = key_of(state, pdf.doc) else {
        return;
    };
    state.marks.last_color = color;
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        pdf.ui.selection.clear();
    }
    let texts = |p: u32| doc.page_text(p);
    if let Some(pieces) = selection_pieces(&sel, &texts) {
        let now = state.memory.now();
        let added = state
            .marks
            .store
            .doc_mut(&path, fp.as_deref(), now)
            .add_highlight(color, &pieces, &texts, COPY_OPTIONS, now);
        if added.is_some() {
            changed(state, ctx);
        }
        return;
    }
    let generation = doc.info().map(|i| i.generation);
    let pending = Arc::clone(&state.marks.pending);
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("refdesk-highlight-text".into())
        .spawn(move || {
            let mut texts = Vec::new();
            for page in sel.pages() {
                match doc.page_text_wait(page) {
                    Ok(t) => texts.push((page, t)),
                    Err(e) => log::warn!("highlight: no text for page {page}: {e}"),
                }
            }
            let lookup = |p: u32| {
                texts
                    .iter()
                    .find(|(n, _)| *n == p)
                    .map(|(_, t)| Arc::clone(t))
            };
            let pieces = selection_pieces(&sel, &lookup).unwrap_or_default();
            pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Pending {
                    path,
                    generation,
                    color,
                    pieces,
                    texts,
                });
            ctx.request_repaint();
        });
    if let Err(e) = spawned {
        log::warn!("could not start the highlight thread: {e}");
    }
}

/// Copies every highlight of the document of `tab` with its page and note.
fn copy_all(state: &mut AppState, tab: TabId, ctx: &egui::Context) {
    let Some(pdf) = state.tabs.pdf(tab) else {
        return;
    };
    let labels = state
        .docs
        .get(pdf.doc)
        .and_then(|d| d.info())
        .map(|i| i.page_labels.clone())
        .unwrap_or_default();
    let Some((path, fp)) = key_of(state, pdf.doc) else {
        return;
    };
    let Some(doc) = state.marks.store.doc(&path, fp.as_deref()) else {
        return;
    };
    let count = doc.group_heads().len();
    if count == 0 {
        return;
    }
    let text = all_highlights_text(doc, &labels);
    ctx.copy_text(text);
    notice(
        state,
        tr("marks.copied_all").replace("{}", &count.to_string()),
        ctx,
    );
}

/// Whether the document of the focused PDF tab has bookmarks (`ItemKind::Bookmark`) or
/// highlights. For the availability of commands.
pub fn focused_has(state: &AppState, kind: ItemKind) -> bool {
    let Some((_, pdf)) = state.focused_pdf() else {
        return false;
    };
    let Some((path, fp)) = state
        .marks
        .keys
        .get(&pdf.doc)
        .cloned()
        .or_else(|| key_of(state, pdf.doc))
    else {
        return false;
    };
    state
        .marks
        .store
        .doc(&path, fp.as_deref())
        .is_some_and(|d| match kind {
            ItemKind::Bookmark => !d.bookmarks().is_empty(),
            ItemKind::Highlight => !d.highlights().is_empty(),
        })
}

// ---------------------------------------------------------------------------------------------
// Per frame, start and exit
// ---------------------------------------------------------------------------------------------

/// Reads `annotations.json` (when there is a place to read it from).
pub fn load_into(state: &mut AppState) {
    if let Some(paths) = &state.paths {
        state.marks.store = Annotations::load(&paths.annotations_file());
        state.marks.version += 1;
    }
}

/// Once per frame, in `logic()`: adds highlights whose page texts arrived, follows moved files
/// to their records, ends a wait for a mark letter that ran out (2 s), lost its tab or its focus,
/// or met a text field, hands every PDF tab the highlights of its document, and saves two
/// seconds after the last change.
pub fn tick(state: &mut AppState, ctx: &egui::Context) {
    let now = ctx.input(|i| i.time);
    take_pending(state, ctx);
    refresh_keys(state);
    follow_documents(state, ctx);
    check_grab(state, ctx, now);
    hand_out_views(state);
    save_when_due(state, ctx, now);
}

fn take_pending(state: &mut AppState, ctx: &egui::Context) {
    let done: Vec<Pending> = std::mem::take(
        &mut *state
            .marks
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    for p in done {
        // The document must still be open in the generation the selection was made in (R-7).
        let Some(doc) = state.docs.find(&p.path).and_then(|id| state.docs.get(id)) else {
            continue;
        };
        if doc.info().map(|i| i.generation) != p.generation {
            continue;
        }
        let fp = fingerprint_of(state, &p.path);
        let now = state.memory.now();
        let lookup = |n: u32| {
            p.texts
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, t)| Arc::clone(t))
        };
        let added = state
            .marks
            .store
            .doc_mut(&p.path, fp.as_deref(), now)
            .add_highlight(p.color, &p.pieces, &lookup, COPY_OPTIONS, now);
        if added.is_some() {
            changed(state, ctx);
        }
    }
}

/// Brings the (path, fingerprint) of every open document up to date. A key whose fingerprint
/// is known is kept as it is, so the fingerprint (looked up by a normalized path, which asks the
/// file system) is asked for only until it arrives.
fn refresh_keys(state: &mut AppState) {
    let mut keys = HashMap::new();
    for (id, doc) in state.docs.iter() {
        let path = doc.path();
        let key = match state.marks.keys.get(&id) {
            Some((p, Some(fp))) if p == path => (p.clone(), Some(fp.clone())),
            _ => (
                path.to_path_buf(),
                state.memory.fingerprint_of(path).map(str::to_string),
            ),
        };
        keys.insert(id, key);
    }
    state.marks.keys = keys;
}

fn follow_documents(state: &mut AppState, ctx: &egui::Context) {
    let open: Vec<(PathBuf, String)> = state
        .marks
        .keys
        .values()
        .filter_map(|(path, fp)| Some((path.clone(), fp.clone()?)))
        .collect();
    for key in open {
        if state.marks.followed.contains(&key) {
            continue;
        }
        if state.marks.store.follow(&key.0, &key.1) {
            changed(state, ctx);
        }
        state.marks.followed.insert(key);
    }
}

fn check_grab(state: &mut AppState, ctx: &egui::Context, now: f64) {
    let tab = match state.key_grab {
        Some(KeyGrab::SetMark(t) | KeyGrab::GoToMark(t)) => t,
        _ => {
            state.marks.grab_started = None;
            return;
        }
    };
    let started = *state.marks.grab_started.get_or_insert(now);
    let left = GRAB_SECS - (now - started);
    let lost = state.tabs.pdf(tab).is_none() || state.focus != Some(tab) || ctx.text_edit_focused();
    if lost || left <= 0.0 {
        end_grab(state);
        ctx.request_repaint();
    } else {
        ctx.request_repaint_after(Duration::from_secs_f64(left));
    }
}

fn hand_out_views(state: &mut AppState) {
    let tabs: Vec<(TabId, DocId)> = state
        .tabs
        .iter()
        .filter_map(|(id, t)| match t {
            Tab::Pdf(p) => Some((id, p.doc)),
            _ => None,
        })
        .collect();
    let mut open = HashSet::new();
    for (tab, doc) in tabs {
        let Some((path, fp)) = state.marks.keys.get(&doc).cloned() else {
            continue;
        };
        let view = state.marks.view_for(&path, fp.as_deref());
        open.insert(path);
        let grab = match state.key_grab {
            Some(KeyGrab::SetMark(t)) if t == tab => Some(MarkGrab::Set),
            Some(KeyGrab::GoToMark(t)) if t == tab => Some(MarkGrab::Go),
            _ => None,
        };
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
            pdf.ui.highlights.env = Some(HighlightEnv { doc: view, grab });
        }
    }
    state.marks.views.retain(|p, _| open.contains(p));
}

fn save_when_due(state: &mut AppState, ctx: &egui::Context, now: f64) {
    let Some(since) = state.marks.dirty_since else {
        return;
    };
    let Some(paths) = &state.paths else {
        // Nowhere to save: the change lives until exit.
        state.marks.dirty_since = None;
        return;
    };
    let left = SAVE_DELAY_SECS - (now - since);
    if left > 0.0 {
        ctx.request_repaint_after(Duration::from_secs_f64(left));
        return;
    }
    if state
        .marks
        .writer
        .as_ref()
        .is_some_and(|w| !w.is_finished())
    {
        // One write at a time, so an older one never lands after a newer one.
        ctx.request_repaint_after(Duration::from_millis(100));
        return;
    }
    let bytes = match state.marks.store.to_bytes() {
        Ok(b) => b,
        Err(e) => {
            log::warn!("cannot encode the marks: {e}");
            state.marks.dirty_since = None;
            return;
        }
    };
    state.marks.dirty_since = None;
    state.marks.saves += 1;
    let path = paths.annotations_file();
    match std::thread::Builder::new()
        .name("refdesk-marks-save".into())
        .spawn(move || {
            if let Err(e) = refdesk_core::atomic_write(&path, &bytes) {
                log::warn!("could not save {}: {e}", path.display());
            }
        }) {
        Ok(handle) => state.marks.writer = Some(handle),
        Err(e) => log::warn!("could not start the marks save: {e}"),
    }
}

/// Writes the store now when it changed (at exit).
pub fn save_now(state: &mut AppState) {
    if let Some(writer) = state.marks.writer.take() {
        let _ = writer.join();
    }
    let Some(paths) = &state.paths else {
        return;
    };
    if state.marks.dirty_since.take().is_none() {
        return;
    }
    let file = paths.annotations_file();
    match state.marks.store.save(&file) {
        Ok(()) => state.marks.saves += 1,
        Err(e) => log::warn!("could not save {}: {e}", file.display()),
    }
}

// ---------------------------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------------------------

/// What [`show_view`] reads besides its own state.
pub struct Env<'a> {
    /// The marks.
    pub marks: &'a MarksApp,
    /// The tabs.
    pub tabs: &'a TabStore,
    /// The open documents.
    pub docs: &'a DocStore,
    /// The focused tab of the window.
    pub focus_tab: Option<TabId>,
    /// The window.
    pub window: WindowId,
}

fn row_height(ui: &Ui) -> f32 {
    ui.text_style_height(&TextStyle::Body) + 6.0
}

/// What a click or a key on a row asked for.
enum RowEvent {
    Go(PathBuf, Location),
    Toggle(Section),
    Edit(Editing),
    Commit(Editing),
    Cancel,
    Queue(MarksAction),
}

/// Draws the Marks view into `ui` (the sidebar below its view switch).
pub fn show_view(ui: &mut Ui, view: &mut MarksView, env: Env<'_>, queue: &mut Vec<Action>) {
    let Env {
        marks,
        tabs,
        docs,
        focus_tab,
        window,
    } = env;
    let store = &marks.store;
    let current_pdf = focus_tab.and_then(|t| tabs.pdf(t));
    let current_key = current_pdf.and_then(|p| marks.keys.get(&p.doc));
    let current = current_key.and_then(|(path, fp)| store.position(path, fp.as_deref()));

    controls(ui, view);

    let query = RowQuery {
        scope: view.scope,
        current,
        filter: view.filter.clone(),
        color: view.color,
        bookmarks_open: view.bookmarks_open,
        highlights_open: view.highlights_open,
    };
    let rows = match &view.rows {
        Some((version, q, rows)) if *version == marks.version && *q == query => Arc::clone(rows),
        _ => {
            let rows = Arc::new(build_rows(store, &query));
            view.rows = Some((marks.version, query.clone(), Arc::clone(&rows)));
            rows
        }
    };
    if view.scope == Scope::ThisDocument && current_pdf.is_none() {
        ui.add_space(4.0);
        ui.label(egui::RichText::new(tr("marks.no_doc")).weak());
        return;
    }

    // Page labels of the open documents, by path.
    let mut labels: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for (_, d) in docs.iter() {
        if let Some(info) = d.info() {
            labels.insert(d.path().to_path_buf(), info.page_labels.clone());
        }
    }
    let labels_of = |p: &Path| labels.get(p).cloned().unwrap_or_default();
    let nearest = match (current, current_pdf) {
        (Some(doc), Some(pdf)) => nearest_rows(store, &rows, doc, reading_place(pdf)),
        _ => Vec::new(),
    };

    let row_h = row_height(ui);
    let reveal = view.reveal.take().and_then(|item| {
        rows.iter().position(|r| {
            row_item(store, *r).as_ref() == Some(&item) && !matches!(r, Row::Note { .. })
        })
    });
    let mut area = ScrollArea::vertical()
        .id_salt(("marks_rows", window))
        .auto_shrink([false, false]);
    if let Some(row) = reveal
        && let Some(offset) = scroll_target(row, row_h, view.offset, view.view_h)
    {
        area = area.vertical_scroll_offset(offset);
    }
    let mut events = Vec::new();
    let output = area.show_viewport(ui, |ui, viewport| {
        let width = ui.available_width();
        let (_, whole) = ui.allocate_space(vec2(width, row_h * rows.len() as f32));
        let first = ((viewport.min.y / row_h).floor().max(0.0)) as usize;
        let last = ((viewport.max.y / row_h).ceil() as usize).min(rows.len());
        for i in first..last {
            let rect = Rect::from_min_size(
                Pos2::new(whole.left(), whole.top() + i as f32 * row_h),
                vec2(width, row_h),
            );
            let cx = RowCx {
                store,
                window,
                labels_of: &labels_of,
                nearest: nearest.contains(&i),
                open: (view.bookmarks_open, view.highlights_open),
            };
            if let Some(event) = draw_row(ui, rect, rows[i], &cx, view.editing.as_mut()) {
                events.push(event);
            }
        }
    });
    view.offset = output.state.clamped_offset().y;
    view.view_h = output.inner_rect.height();
    for event in events {
        match event {
            RowEvent::Go(doc, loc) => queue.push(Action::Marks(MarksAction::GoTo {
                doc,
                loc,
                window: Some(window),
            })),
            RowEvent::Toggle(Section::Bookmarks) => view.bookmarks_open = !view.bookmarks_open,
            RowEvent::Toggle(Section::Highlights) => view.highlights_open = !view.highlights_open,
            RowEvent::Edit(editing) => view.editing = Some(editing),
            RowEvent::Commit(editing) => {
                view.editing = None;
                let action = match editing.kind {
                    EditKind::Name => MarksAction::RenameBookmark {
                        doc: editing.item.doc,
                        id: editing.item.id,
                        name: editing.text,
                    },
                    EditKind::Note => MarksAction::SetNote {
                        doc: editing.item.doc,
                        group: editing.item.id,
                        note: editing.text,
                    },
                };
                queue.push(Action::Marks(action));
            }
            RowEvent::Cancel => view.editing = None,
            RowEvent::Queue(action) => queue.push(Action::Marks(action)),
        }
    }
}

/// The scope toggle, the filter field and the colour chips.
fn controls(ui: &mut Ui, view: &mut MarksView) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        for (scope, key) in [
            (Scope::ThisDocument, "marks.scope_doc"),
            (Scope::AllDocuments, "marks.scope_all"),
        ] {
            if ui
                .add(egui::Button::selectable(view.scope == scope, tr(key)))
                .clicked()
            {
                view.scope = scope;
            }
        }
    });
    ui.add(
        TextEdit::singleline(&mut view.filter)
            .hint_text(tr("marks.filter"))
            .desired_width(f32::INFINITY),
    );
    ui.horizontal(|ui| {
        let size = ui.spacing().interact_size.y;
        for color in HighlightColor::ALL {
            let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
            let on = view.color == Some(color);
            let label = tr("marks.only_color").replace("{}", highlights::color_name(color));
            response
                .widget_info(|| WidgetInfo::selected(egui::Role::Button, true, on, label.clone()));
            let painter = ui.painter();
            painter.rect_filled(rect.shrink(3.0), 3.0, highlights::solid(color));
            if on || response.hovered() {
                let stroke = if on {
                    ui.visuals().selection.stroke
                } else {
                    ui.visuals().widgets.hovered.fg_stroke
                };
                painter.rect_stroke(rect.shrink(1.0), 3.0, stroke, egui::StrokeKind::Inside);
            }
            if response.on_hover_text(label).clicked() {
                view.color = if on { None } else { Some(color) };
            }
        }
    });
    ui.separator();
}

/// What [`draw_row`] reads.
struct RowCx<'a> {
    store: &'a Annotations,
    window: WindowId,
    labels_of: &'a dyn Fn(&Path) -> Vec<String>,
    /// The row is the nearest item above the reader.
    nearest: bool,
    /// The bookmark and highlight sections are unfolded.
    open: (bool, bool),
}

/// Draws one row; returns what the reader asked for on it.
fn draw_row(
    ui: &mut Ui,
    rect: Rect,
    row: Row,
    cx: &RowCx<'_>,
    editing: Option<&mut Editing>,
) -> Option<RowEvent> {
    let item = row_item(cx.store, row);
    if let Some(editing) = editing
        && item.as_ref() == Some(&editing.item)
        && !matches!(row, Row::Note { .. })
    {
        return edit_row(ui, rect, editing, cx.window);
    }
    let label = row_label(cx.store, row, cx.labels_of);
    let id = ui.make_persistent_id(("marks_row", cx.window, format!("{row:?}")));
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, label.clone()));
    let visuals = ui.visuals().clone();
    let painter = ui.painter_at(rect);
    let font = ui.style().text_styles[&TextStyle::Body].clone();
    let small = ui.style().text_styles[&TextStyle::Small].clone();
    if cx.nearest {
        painter.rect_filled(rect, 2.0, visuals.selection.bg_fill.gamma_multiply(0.35));
    } else if response.hovered() {
        painter.rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect.shrink(0.5),
            2.0,
            visuals.selection.stroke,
            egui::StrokeKind::Inside,
        );
    }
    let text_y = |h: f32| rect.center().y - h / 2.0;
    let page_text = |loc: Option<Location>, doc: usize| {
        let labels = cx
            .store
            .docs()
            .get(doc)
            .map(|d| (cx.labels_of)(&d.path))
            .unwrap_or_default();
        loc.map(|l| tr("marks.page_name").replace("{}", &page_label(l.page, &labels)))
            .unwrap_or_default()
    };
    // Right: the page label; left: the text.
    let right_text = |text: String, painter: &egui::Painter| -> f32 {
        if text.is_empty() {
            return rect.right();
        }
        let galley = painter.layout_no_wrap(text, small.clone(), visuals.weak_text_color());
        let x = rect.right() - 6.0 - galley.size().x;
        painter.galley(
            Pos2::new(x, text_y(galley.size().y)),
            galley,
            visuals.weak_text_color(),
        );
        x
    };
    let left_text = |painter: &egui::Painter, text: &str, x: f32, right: f32, weak: bool| {
        let color = if weak {
            visuals.weak_text_color()
        } else {
            visuals.text_color()
        };
        let galley = painter.layout_job(line_job(
            text,
            font.clone(),
            color,
            (right - x - 6.0).max(0.0),
        ));
        painter.galley(Pos2::new(x, text_y(galley.size().y)), galley, color);
    };
    let mut event = None;
    match row {
        Row::Section { section, .. } => {
            painter.rect_filled(rect, 0.0, visuals.faint_bg_color);
            let fold = Rect::from_min_size(rect.min, vec2(rect.height(), rect.height()));
            let open = match section {
                Section::Bookmarks => cx.open.0,
                Section::Highlights => cx.open.1,
            };
            painter.add(triangle(fold, open, visuals.strong_text_color()));
            left_text(&painter, &label, fold.right(), rect.right(), false);
            if response.clicked() {
                event = Some(RowEvent::Toggle(section));
            }
        }
        Row::Doc { .. } => {
            let galley = painter.layout_job(line_job(
                &label,
                font.clone(),
                visuals.strong_text_color(),
                rect.width() - 12.0,
            ));
            painter.galley(
                Pos2::new(rect.left() + 6.0, text_y(galley.size().y)),
                galley,
                visuals.strong_text_color(),
            );
        }
        Row::Bookmark { doc, index } => {
            let b = cx
                .store
                .docs()
                .get(doc)
                .and_then(|d| d.bookmarks().get(index));
            let name = b.map(|b| b.name.clone()).unwrap_or_default();
            let right = right_text(page_text(b.map(|b| b.loc), doc), &painter);
            left_text(&painter, &name, rect.left() + 12.0, right, false);
            if let (Some(b), Some(item)) = (b, item.clone()) {
                if response.clicked() {
                    event = Some(RowEvent::Go(item.doc.clone(), b.loc));
                }
                if response.has_focus() && ui.input(|i| i.key_pressed(Key::Delete)) {
                    event = Some(RowEvent::Queue(MarksAction::RemoveBookmark {
                        doc: item.doc.clone(),
                        id: item.id,
                    }));
                }
                response.context_menu(|ui| {
                    if ui.button(tr("marks.menu.rename")).clicked() {
                        event = Some(RowEvent::Edit(Editing {
                            item: item.clone(),
                            kind: EditKind::Name,
                            text: b.name.clone(),
                            focus: true,
                        }));
                        ui.close();
                    }
                    if ui.button(tr("marks.menu.remove")).clicked() {
                        event = Some(RowEvent::Queue(MarksAction::RemoveBookmark {
                            doc: item.doc.clone(),
                            id: item.id,
                        }));
                        ui.close();
                    }
                });
            }
        }
        Row::Mark { doc, .. } => {
            let loc = row_loc(cx.store, row);
            let galley = painter.layout_no_wrap(
                format!(
                    "{}  {}",
                    match row {
                        Row::Mark { letter, .. } => letter,
                        _ => ' ',
                    },
                    page_text(loc, doc)
                ),
                small.clone(),
                visuals.weak_text_color(),
            );
            painter.galley(
                Pos2::new(rect.left() + 18.0, text_y(galley.size().y)),
                galley,
                visuals.weak_text_color(),
            );
            let path = match row {
                Row::Mark { letter, .. } if letter.is_ascii_uppercase() => {
                    cx.store.global_mark(letter).map(|m| m.path.clone())
                }
                _ => cx.store.docs().get(doc).map(|d| d.path.clone()),
            };
            if let (Some(loc), Some(path)) = (loc, path)
                && response.clicked()
            {
                event = Some(RowEvent::Go(path, loc));
            }
        }
        Row::Highlight { doc, index } | Row::Note { doc, index } => {
            let rec = cx.store.docs().get(doc);
            let h = rec.and_then(|d| d.highlights().get(index))?;
            let stripe = Rect::from_min_size(
                Pos2::new(rect.left() + 4.0, rect.top() + 2.0),
                vec2(4.0, rect.height() - 4.0),
            );
            painter.rect_filled(stripe, 1.0, highlights::solid(h.color));
            if matches!(row, Row::Highlight { .. }) {
                let right = right_text(page_text(Some(h.loc()), doc), &painter);
                left_text(
                    &painter,
                    &shorten(&h.text, 200),
                    rect.left() + 14.0,
                    right,
                    false,
                );
            } else {
                left_text(
                    &painter,
                    &shorten(&h.note, 200),
                    rect.left() + 24.0,
                    rect.right(),
                    true,
                );
            }
            let item = item.clone()?;
            if response.clicked() {
                event = Some(RowEvent::Go(item.doc.clone(), h.loc()));
            }
            if response.has_focus() && ui.input(|i| i.key_pressed(Key::Delete)) {
                event = Some(RowEvent::Queue(MarksAction::RemoveHighlight {
                    doc: item.doc.clone(),
                    group: item.id,
                }));
            }
            response.context_menu(|ui| {
                if ui.button(tr("marks.menu.edit_note")).clicked() {
                    event = Some(RowEvent::Edit(Editing {
                        item: item.clone(),
                        kind: EditKind::Note,
                        text: h.note.clone(),
                        focus: true,
                    }));
                    ui.close();
                }
                ui.menu_button(tr("marks.menu.change_color"), |ui| {
                    for color in HighlightColor::ALL {
                        if ui.button(highlights::color_name(color)).clicked() {
                            event = Some(RowEvent::Queue(MarksAction::SetColor {
                                doc: item.doc.clone(),
                                group: item.id,
                                color,
                            }));
                            ui.close();
                        }
                    }
                });
                if ui.button(tr("marks.menu.copy_text")).clicked() {
                    event = Some(RowEvent::Queue(MarksAction::CopyHighlight {
                        doc: item.doc.clone(),
                        group: item.id,
                    }));
                    ui.close();
                }
                if ui.button(tr("marks.menu.remove")).clicked() {
                    event = Some(RowEvent::Queue(MarksAction::RemoveHighlight {
                        doc: item.doc.clone(),
                        group: item.id,
                    }));
                    ui.close();
                }
            });
        }
    }
    if response.clicked() {
        response.request_focus();
    }
    event
}

/// The text field of a row being renamed or annotated. Enter keeps the text, Esc the old one;
/// leaving the field keeps the text too.
fn edit_row(ui: &mut Ui, rect: Rect, editing: &mut Editing, window: WindowId) -> Option<RowEvent> {
    let id = ui.make_persistent_id(("marks_edit", window));
    let ctx = ui.ctx().clone();
    if editing.focus {
        editing.focus = false;
        let mut state = TextEdit::load_state(&ctx, id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::select_all_str(
                &editing.text,
            )));
        TextEdit::store_state(&ctx, id, state);
        ui.memory_mut(|m| m.request_focus(id));
    }
    let focused = ui.memory(|m| m.has_focus(id));
    let (enter, escape) = if focused {
        ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        })
    } else {
        (false, false)
    };
    let hint = match editing.kind {
        EditKind::Name => tr("marks.edit_name"),
        EditKind::Note => tr("marks.edit_note"),
    };
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(4.0, 1.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    let response = child.add(
        TextEdit::singleline(&mut editing.text)
            .id(id)
            .hint_text(hint)
            .desired_width(f32::INFINITY),
    );
    if enter {
        ui.memory_mut(|m| m.surrender_focus(id));
        return Some(RowEvent::Commit(editing.clone()));
    }
    if escape {
        ui.memory_mut(|m| m.surrender_focus(id));
        return Some(RowEvent::Cancel);
    }
    if response.lost_focus() {
        return Some(RowEvent::Commit(editing.clone()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use egui::{Event, Modifiers as Mods, Vec2};
    use refdesk_core::testutil::{PdfBuilder, TempPdf};
    use refdesk_core::{
        DocumentHandle, LayoutParams, OutlineItem, Paths, Point, TextPos, ViewState, ZoomMode,
    };

    use crate::action::dispatch_queued;
    use crate::commands::{self, MenuGroup};
    use crate::state::PdfTab;
    use crate::views::test_support::{Repaints, TIMEOUT, watch_repaints};
    use crate::windows::show_window;

    // --- pure logic --------------------------------------------------------------------------

    fn loc(page: u32, y: f32) -> Location {
        Location::new(page, 0.0, y)
    }

    fn outline() -> Outline {
        let item = |title: &str, depth: u16, page: u32, y: f32| OutlineItem {
            title: title.into(),
            depth,
            parent: None,
            target: Some(loc(page, y)),
            uri: None,
        };
        Outline::from_items(vec![
            item("1 Introduction", 0, 0, 60.0),
            item("7.3  State\nmanagement", 0, 3, 100.0),
        ])
    }

    #[test]
    fn a_new_bookmark_is_named_after_its_section_or_its_page() {
        let o = outline();
        assert_eq!(
            default_bookmark_name(&o, &[], loc(4, 10.0)),
            "7.3 State management"
        );
        assert_eq!(
            default_bookmark_name(&o, &[], loc(0, 300.0)),
            "1 Introduction"
        );
        let labels = vec!["i".to_string(), "ii".to_string(), "1".to_string()];
        let none = Outline::default();
        assert_eq!(
            default_bookmark_name(&none, &labels, loc(1, 0.0)),
            tr("marks.page_name").replace("{}", "ii")
        );
        assert_eq!(
            default_bookmark_name(&none, &[], loc(9, 0.0)),
            tr("marks.page_name").replace("{}", "10")
        );
    }

    #[test]
    fn stepping_finds_the_next_place_below_and_the_previous_above() {
        let locs = [loc(1, 100.0), loc(1, 300.0), loc(4, 0.0)];
        assert_eq!(step_index(&locs, loc(0, 500.0), true), Some(0));
        assert_eq!(
            step_index(&locs, loc(1, 100.0), true),
            Some(1),
            "here is passed over"
        );
        assert_eq!(step_index(&locs, loc(1, 100.5), true), Some(1));
        assert_eq!(step_index(&locs, loc(1, 300.0), false), Some(0));
        assert_eq!(step_index(&locs, loc(4, 0.0), true), None);
        assert_eq!(step_index(&locs, loc(1, 99.5), false), None);
        assert_eq!(step_index(&locs, loc(9, 0.0), false), Some(2));
        assert_eq!(step_index(&[], loc(0, 0.0), true), None);
    }

    fn layout(pages: usize) -> PageLayout {
        PageLayout::new(
            &vec![Size::new(612.0, 792.0); pages],
            LayoutParams {
                viewport: Size::new(800.0, 600.0),
                zoom: ZoomMode::Custom(1.0),
                gap: 8.0,
                margin: 12.0,
                fit_page: 0,
            },
        )
    }

    #[test]
    fn the_reading_anchor_puts_the_place_on_the_reading_line_within_the_scroll_limits() {
        let l = layout(10);
        let viewport = Size::new(800.0, 600.0);
        let target = loc(4, 300.0);
        let anchor = reading_anchor(Some(&l), viewport, loc(0, 0.0), target);
        let read = reading_location(anchor, Some(&l), viewport.h);
        assert_eq!(read.page, 4);
        assert!((read.y - 300.0).abs() < 0.5, "{read:?}");
        // Near the end the view stops at its last scroll position.
        let end = reading_anchor(Some(&l), viewport, loc(0, 0.0), loc(9, 780.0));
        let max = l.max_scroll(viewport);
        assert!((l.content_pos(end).y - max.y).abs() < 0.5);
        // Without a layout: a little above the place.
        assert_eq!(
            reading_anchor(None, viewport, loc(0, 0.0), loc(3, 200.0)),
            Location::new(3, 0.0, 128.0)
        );
    }

    fn line_page(text: &str) -> Arc<PageText> {
        let mut pt = PageText::default();
        for (i, ch) in text.chars().enumerate() {
            let x = 72.0 + i as f32 * 10.0;
            pt.chars.push(refdesk_core::TextChar {
                ch,
                quad: refdesk_core::Quad {
                    ul: refdesk_core::Point::new(x, 100.0),
                    ur: refdesk_core::Point::new(x + 10.0, 100.0),
                    ll: refdesk_core::Point::new(x, 112.0),
                    lr: refdesk_core::Point::new(x + 10.0, 112.0),
                },
                size: 10.0,
            });
        }
        let n = pt.chars.len() as u32;
        let bounds = refdesk_core::Rect::new(72.0, 100.0, 72.0 + n as f32 * 10.0, 112.0);
        pt.lines.push(refdesk_core::TextLine {
            chars: 0..n,
            bounds,
        });
        pt.blocks.push(refdesk_core::TextBlock {
            lines: 0..1,
            bounds,
        });
        Arc::new(pt)
    }

    fn sel(a: (u32, u32), b: (u32, u32)) -> Selection {
        Selection {
            anchor: TextPos { page: a.0, ch: a.1 },
            focus: TextPos { page: b.0, ch: b.1 },
        }
    }

    #[test]
    fn the_pieces_of_a_selection_need_the_text_of_every_page() {
        let texts = |p: u32| (p < 3).then(|| line_page("The Execution Management"));
        assert_eq!(
            selection_pieces(&sel((0, 4), (2, 3)), &texts),
            Some(vec![(0, 4..24), (1, 0..24), (2, 0..3)])
        );
        assert_eq!(
            selection_pieces(&sel((1, 9), (1, 4)), &texts),
            Some(vec![(1, 4..9)]),
            "a backwards selection"
        );
        let missing = |p: u32| (p != 1).then(|| line_page("abc"));
        assert_eq!(selection_pieces(&sel((0, 1), (2, 1)), &missing), None);
    }

    fn texts3(p: u32) -> Option<Arc<PageText>> {
        (p < 10_000).then(|| line_page("The Execution Management shall start"))
    }

    fn store_with(path: &str) -> Annotations {
        let mut store = Annotations::default();
        let doc = store.doc_mut(Path::new(path), None, 1);
        doc.add_bookmark("7.3 State management", loc(86, 100.0), 1);
        doc.add_bookmark("1 Introduction", loc(0, 60.0), 1);
        let g = doc
            .add_highlight(
                HighlightColor::Yellow,
                &[(90, 4..24)],
                &texts3,
                COPY_OPTIONS,
                1,
            )
            .expect("hl");
        doc.set_highlight_note(g, "check the states");
        doc.add_highlight(HighlightColor::Blue, &[(2, 0..3)], &texts3, COPY_OPTIONS, 1)
            .expect("hl");
        doc.set_mark('a', loc(11, 0.0));
        store
    }

    #[test]
    fn copying_all_highlights_lists_page_text_and_note_in_order() {
        let store = store_with("/d/spec.pdf");
        let doc = &store.docs()[0];
        let text = all_highlights_text(doc, &[]);
        let first = tr("marks.copy_entry")
            .replace("{page}", "3")
            .replace("{text}", "The");
        let second = format!(
            "{}\n{}",
            tr("marks.copy_entry")
                .replace("{page}", "91")
                .replace("{text}", "Execution Management"),
            tr("marks.copy_note").replace("{}", "check the states")
        );
        assert_eq!(text, format!("{first}\n\n{second}"));
    }

    fn query(scope: Scope, current: Option<usize>) -> RowQuery {
        RowQuery {
            scope,
            current,
            filter: String::new(),
            color: None,
            bookmarks_open: true,
            highlights_open: true,
        }
    }

    #[test]
    fn rows_follow_scope_filter_and_colour() {
        let store = store_with("/d/spec.pdf");
        let rows = build_rows(&store, &query(Scope::ThisDocument, Some(0)));
        assert_eq!(
            rows,
            vec![
                Row::Section {
                    section: Section::Bookmarks,
                    count: 2
                },
                Row::Bookmark { doc: 0, index: 0 },
                Row::Bookmark { doc: 0, index: 1 },
                Row::Mark {
                    doc: 0,
                    letter: 'a'
                },
                Row::Section {
                    section: Section::Highlights,
                    count: 2
                },
                Row::Highlight { doc: 0, index: 0 },
                Row::Highlight { doc: 0, index: 1 },
                Row::Note { doc: 0, index: 1 },
            ]
        );
        assert!(build_rows(&store, &query(Scope::ThisDocument, None)).len() == 2);

        let mut q = query(Scope::ThisDocument, Some(0));
        q.filter = "STATE".into();
        let rows = build_rows(&store, &q);
        // The bookmark by its name, the highlight by its note; no marks while filtering.
        assert_eq!(
            rows,
            vec![
                Row::Section {
                    section: Section::Bookmarks,
                    count: 1
                },
                Row::Bookmark { doc: 0, index: 1 },
                Row::Section {
                    section: Section::Highlights,
                    count: 1
                },
                Row::Highlight { doc: 0, index: 1 },
                Row::Note { doc: 0, index: 1 },
            ]
        );
        let mut q = query(Scope::ThisDocument, Some(0));
        q.color = Some(HighlightColor::Blue);
        let rows = build_rows(&store, &q);
        assert!(rows.contains(&Row::Highlight { doc: 0, index: 0 }));
        assert!(!rows.contains(&Row::Highlight { doc: 0, index: 1 }));
        assert!(!rows.iter().any(|r| matches!(r, Row::Mark { .. })));
    }

    #[test]
    fn all_documents_are_grouped_by_name_in_natural_order() {
        let mut store = Annotations::default();
        for (path, name) in [("/x/doc10.pdf", "ten"), ("/y/doc2.pdf", "two")] {
            store
                .doc_mut(Path::new(path), None, 1)
                .add_bookmark(name, loc(0, 0.0), 1);
        }
        store.doc_mut(Path::new("/z/empty.pdf"), None, 1);
        store.set_global_mark('Q', Path::new("/x/doc10.pdf"), None, loc(3, 0.0));
        let rows = build_rows(&store, &query(Scope::AllDocuments, None));
        assert_eq!(
            rows,
            vec![
                Row::Section {
                    section: Section::Bookmarks,
                    count: 2
                },
                Row::Doc { doc: 1 },
                Row::Bookmark { doc: 1, index: 0 },
                Row::Doc { doc: 0 },
                Row::Bookmark { doc: 0, index: 0 },
                Row::Mark {
                    doc: 0,
                    letter: 'Q'
                },
                Row::Section {
                    section: Section::Highlights,
                    count: 0
                },
            ]
        );
    }

    #[test]
    fn folded_sections_keep_their_heads_and_counts() {
        let store = store_with("/d/spec.pdf");
        let mut q = query(Scope::ThisDocument, Some(0));
        q.bookmarks_open = false;
        q.highlights_open = false;
        assert_eq!(
            build_rows(&store, &q),
            vec![
                Row::Section {
                    section: Section::Bookmarks,
                    count: 2
                },
                Row::Section {
                    section: Section::Highlights,
                    count: 2
                },
            ]
        );
    }

    #[test]
    fn the_nearest_row_is_the_last_item_above_the_reader() {
        let store = store_with("/d/spec.pdf");
        let rows = build_rows(&store, &query(Scope::ThisDocument, Some(0)));
        // Rows: 1 = bookmark p.1, 2 = bookmark p.87, 5 = highlight p.3, 6 = highlight p.91.
        assert_eq!(nearest_rows(&store, &rows, 0, loc(50, 0.0)), vec![1, 5]);
        assert_eq!(nearest_rows(&store, &rows, 0, loc(95, 0.0)), vec![2, 6]);
        assert_eq!(
            nearest_rows(&store, &rows, 0, loc(0, 10.0)),
            Vec::<usize>::new()
        );
        assert_eq!(
            nearest_rows(&store, &rows, 0, loc(0, 60.0)),
            vec![1],
            "at counts"
        );
    }

    #[test]
    fn rows_have_accessible_names() {
        let store = store_with("/d/spec.pdf");
        let none = |_: &Path| Vec::new();
        let label = |row| row_label(&store, row, &none);
        assert_eq!(
            label(Row::Bookmark { doc: 0, index: 1 }),
            tr("marks.a11y_bookmark")
                .replace("{name}", "7.3 State management")
                .replace("{page}", "87")
        );
        assert_eq!(
            label(Row::Highlight { doc: 0, index: 1 }),
            tr("marks.a11y_highlight")
                .replace("{color}", highlights::color_name(HighlightColor::Yellow))
                .replace("{text}", "Execution Management")
                .replace("{page}", "91")
        );
        let labels = |_: &Path| vec!["x".to_string(); 100];
        assert!(row_label(&store, Row::Bookmark { doc: 0, index: 1 }, &labels).contains('x'));
        assert!(
            label(Row::Mark {
                doc: 0,
                letter: 'a'
            })
            .contains("12")
        );
        assert!(label(Row::Note { doc: 0, index: 1 }).contains("check the states"));
        assert_eq!(shorten("a  b\nc", 10), "a b c");
        assert_eq!(shorten("abcdef", 3), "abc…");
    }

    #[test]
    fn thousands_of_rows_are_built_and_searched_quickly() {
        let mut store = Annotations::default();
        let doc = store.doc_mut(Path::new("/d/big.pdf"), None, 1);
        for p in 0..5000 {
            doc.add_highlight(
                HighlightColor::ALL[p as usize % 5],
                &[(p, 4..13)],
                &texts3,
                COPY_OPTIONS,
                1,
            );
        }
        let n = store.docs()[0].group_heads().len();
        assert!(n > 1000, "{n}");
        let started = Instant::now();
        let rows = build_rows(&store, &query(Scope::ThisDocument, Some(0)));
        let mut q = query(Scope::AllDocuments, None);
        q.filter = "execution".into();
        let filtered = build_rows(&store, &q);
        let near = nearest_rows(&store, &rows, 0, loc(2500, 0.0));
        let elapsed = started.elapsed();
        assert_eq!(rows.len(), n + 2);
        assert!(filtered.len() > 2);
        assert_eq!(near.len(), 1);
        assert!(elapsed.as_millis() < 200, "{elapsed:?}");
    }

    // --- the application ---------------------------------------------------------------------

    const SCREEN: Vec2 = Vec2::new(1000.0, 700.0);

    struct Rig {
        ctx: egui::Context,
        repaints: Repaints,
        state: AppState,
        time: f64,
        /// Seconds of egui time a frame takes. Tests about timing set it to 0 and move `time`.
        step: f64,
        copied: Vec<String>,
    }

    impl Rig {
        fn new() -> Rig {
            let ctx = egui::Context::default();
            let repaints = watch_repaints(&ctx);
            let mut state = AppState::new();
            state.memory.set_clock(Arc::new(|| 1_800_000_000));
            Rig {
                ctx,
                repaints,
                state,
                time: 1.0,
                step: 1.0 / 60.0,
                copied: Vec::new(),
            }
        }

        /// One frame like the app's: the upkeep of `logic()`, shortcuts, the window, dispatch.
        fn frame(&mut self, events: Vec<Event>) -> egui::FullOutput {
            self.time += self.step;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN)),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let state = &mut self.state;
            let mut queue = Vec::new();
            let output = self.ctx.run_ui(input, |ui| {
                crate::session::poll_fingerprints(state, ui.ctx());
                tick(state, ui.ctx());
                queue.extend(commands::handle_shortcuts(ui.ctx(), state));
                show_window(ui, state, 0, &mut queue);
            });
            for command in &output.platform_output.commands {
                if let egui::OutputCommand::CopyText(text) = command {
                    self.copied.push(text.clone());
                }
            }
            self.state.actions.append(&mut queue);
            dispatch_queued(&mut self.state, &self.ctx);
            output
        }

        fn run(&mut self, events: Vec<Event>) {
            self.frame(events).drop_without_applying_deltas();
        }

        fn frames(&mut self, n: usize) {
            for _ in 0..n {
                self.run(Vec::new());
            }
        }

        fn frames_until(&mut self, what: &str, mut check: impl FnMut(&Rig) -> bool) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.run(Vec::new());
                if check(self) {
                    return;
                }
                self.repaints.wait_one(what, deadline);
            }
        }

        fn open(&mut self, path: &Path) -> TabId {
            let target = OpenTarget {
                path: path.to_path_buf(),
                page: None,
            };
            self.act(Action::OpenFiles(vec![target]));
            let tab = self.state.focus.expect("focused");
            let doc = self.doc(tab);
            self.frames_until("the document info", |_| doc.info().is_some());
            self.act(Action::SetZoom {
                tab,
                zoom: ZoomMode::Custom(1.0),
            });
            self.frames_until("the layout", |rig| rig.pdf(tab).ui.layout().is_some());
            self.frames(2);
            tab
        }

        fn act(&mut self, action: Action) {
            dispatch_action(&mut self.state, action, &self.ctx);
        }

        fn marks(&mut self, action: MarksAction) {
            self.act(Action::Marks(action));
        }

        fn doc(&self, tab: TabId) -> DocumentHandle {
            let doc = self.pdf(tab).doc;
            self.state.docs.get(doc).expect("doc").clone()
        }

        fn pdf(&self, tab: TabId) -> &PdfTab {
            self.state.tabs.pdf(tab).expect("pdf tab")
        }

        fn view_mut(&mut self, tab: TabId) -> &mut ViewState {
            match self.state.tabs.get_mut(tab) {
                Some(Tab::Pdf(p)) => &mut p.view,
                _ => panic!("not a pdf tab"),
            }
        }

        /// Scrolls `tab` so that its reading line is at `loc`, and lets a frame settle it.
        fn read_at(&mut self, tab: TabId, at: Location) {
            let pdf = self.pdf(tab);
            let anchor = reading_anchor(pdf.ui.layout(), pdf.ui.viewport(), pdf.view.anchor, at);
            self.view_mut(tab).anchor = anchor;
            self.frames(2);
        }

        fn key(&mut self, key: Key, modifiers: Mods) {
            self.run(vec![
                Event::ModifiersChanged(modifiers),
                Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
            ]);
            self.run(vec![Event::ModifiersChanged(Mods::NONE)]);
        }

        fn record(&self, tab: TabId) -> Option<&DocAnnotations> {
            let path = self.doc(tab).path().to_path_buf();
            self.state.marks.store.doc(&path, None)
        }

        fn texts_of(&mut self, tab: TabId, pages: &[u32]) {
            let doc = self.doc(tab);
            for &p in pages {
                self.frames_until("the page text", |_| doc.page_text(p).is_some());
            }
        }

        /// The labels of the buttons in the accessibility tree of one frame.
        fn button_labels(&mut self) -> Vec<(String, Rect)> {
            self.ctx.enable_accesskit();
            let output = self.frame(Vec::new());
            let update = output.platform_output.accesskit_update.clone();
            output.drop_without_applying_deltas();
            update
                .map(|u| u.nodes)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(_, n)| {
                    let b = n.bounds()?;
                    Some((
                        n.label()?.to_owned(),
                        Rect::from_min_max(
                            Pos2::new(b.x0 as f32, b.y0 as f32),
                            Pos2::new(b.x1 as f32, b.y1 as f32),
                        ),
                    ))
                })
                .collect()
        }

        fn click(&mut self, at: Pos2) {
            self.run(vec![Event::PointerMoved(at)]);
            self.run(vec![Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Mods::NONE,
            }]);
            self.run(vec![Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Mods::NONE,
            }]);
        }
    }

    fn spec_doc(stem: &str) -> TempPdf {
        PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                p.text(72.0, 60.0, 14.0, "1 Introduction");
                p.text(72.0, 100.0, 12.0, "intro body one");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 60.0, 14.0, "2 Functional specification");
                p.text(72.0, 100.0, 12.0, "The Execution Management shall");
                p.text(72.0, 300.0, 14.0, "2.1 States");
                p.text(72.0, 360.0, 12.0, "state machine text");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "state body page two");
                p.text(72.0, 400.0, 14.0, "2.2 Errors");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "error body page three");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "annex body");
            })
            .outline_leaf("1 Introduction", 1, 60.0)
            .outline("2 Functional specification", 2, 60.0, |o| {
                o.child("2.1 States", 2, 300.0);
                o.child("2.2 Errors", 3, 400.0);
            })
            .write_temp(stem)
    }

    fn close(a: Location, b: Location) -> bool {
        a.page == b.page && (a.y - b.y).abs() < 1.5
    }

    // --- bookmarks ---------------------------------------------------------------------------

    #[test]
    fn adding_a_bookmark_names_it_after_the_section_and_opens_the_marks_view() {
        let pdf = spec_doc("wp74-add");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.read_at(tab, loc(1, 320.0));
        // Ctrl+D, through the shortcut table.
        rig.key(Key::D, Mods::CTRL);
        let rec = rig.record(tab).expect("a record");
        assert_eq!(rec.bookmarks().len(), 1);
        let b = &rec.bookmarks()[0];
        assert_eq!(b.name, "2.1 States");
        assert!(close(b.loc, loc(1, 320.0)), "{:?}", b.loc);
        let window = &rig.state.windows[0];
        assert!(window.toc_open);
        assert_eq!(window.sidebar_view, SidebarView::Marks);
        let editing = window
            .marks
            .editing
            .clone()
            .expect("the new row is renamed");
        assert_eq!(editing.kind, EditKind::Name);
        assert_eq!(editing.text, "2.1 States");
        assert_eq!(editing.item.id, b.id);

        // Typing replaces the selected name; Enter keeps it.
        rig.frames(2);
        rig.run(vec![Event::Text("Lifecycle".into())]);
        rig.key(Key::Enter, Mods::NONE);
        let rec = rig.record(tab).expect("a record");
        assert_eq!(rec.bookmarks()[0].name, "Lifecycle");
        assert!(rig.state.windows[0].marks.editing.is_none());
    }

    #[test]
    fn escape_keeps_the_default_name_and_a_second_bookmark_at_the_same_spot_is_not_added() {
        let pdf = spec_doc("wp74-same");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.read_at(tab, loc(2, 100.0));
        rig.marks(MarksAction::AddBookmark(tab));
        rig.frames(2);
        rig.key(Key::Escape, Mods::NONE);
        assert!(rig.state.windows[0].marks.editing.is_none());
        let rec = rig.record(tab).expect("record");
        assert_eq!(rec.bookmarks()[0].name, "2.1 States");
        // 30 points lower: the same spot.
        rig.read_at(tab, loc(2, 130.0));
        rig.marks(MarksAction::AddBookmark(tab));
        assert_eq!(rig.record(tab).map(|r| r.bookmarks().len()), Some(1));
        let window = &rig.state.windows[0];
        assert!(
            window.marks.editing.is_none(),
            "the existing row is only shown"
        );
        assert_eq!(
            window.marks.reveal.as_ref().map(|r| r.id),
            Some(rig.record(tab).expect("record").bookmarks()[0].id)
        );
    }

    #[test]
    fn a_page_without_a_section_names_the_bookmark_after_its_page() {
        let pdf = PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "plain");
            })
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "plain two");
            })
            .write_temp("wp74-plain");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.read_at(tab, loc(1, 200.0));
        rig.marks(MarksAction::AddBookmark(tab));
        assert_eq!(
            rig.record(tab).expect("record").bookmarks()[0].name,
            tr("marks.page_name").replace("{}", "2")
        );
    }

    #[test]
    fn bookmarks_are_renamed_removed_and_stepped_through_with_history() {
        let pdf = spec_doc("wp74-step");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        for at in [loc(1, 320.0), loc(3, 100.0), loc(0, 300.0)] {
            rig.read_at(tab, at);
            rig.marks(MarksAction::AddBookmark(tab));
        }
        let path = rig.doc(tab).path().to_path_buf();
        let ids: Vec<u64> = rig
            .record(tab)
            .expect("record")
            .bookmarks()
            .iter()
            .map(|b| b.id)
            .collect();
        assert_eq!(ids.len(), 3);
        rig.marks(MarksAction::RenameBookmark {
            doc: path.clone(),
            id: ids[1],
            name: "  States  ".into(),
        });
        assert_eq!(rig.record(tab).expect("r").bookmarks()[1].name, "States");

        // From the top: next goes to page 1, then page 2, then page 4; previous goes back.
        rig.read_at(tab, loc(0, 0.0));
        let start = rig.pdf(tab).view.anchor;
        let mut seen = Vec::new();
        for _ in 0..3 {
            rig.marks(MarksAction::StepBookmark { tab, forward: true });
            rig.frames(2);
            seen.push(reading_place(rig.pdf(tab)).page);
        }
        assert_eq!(seen, vec![0, 1, 3]);
        rig.marks(MarksAction::StepBookmark { tab, forward: true });
        assert_eq!(
            rig.state.status_notice.as_ref().map(|n| n.text.as_str()),
            Some(tr("marks.no_more_bookmarks"))
        );
        rig.marks(MarksAction::StepBookmark {
            tab,
            forward: false,
        });
        rig.frames(2);
        assert_eq!(reading_place(rig.pdf(tab)).page, 1);
        // Every step was a jump: Back goes the way it came, to the start in the end.
        for _ in 0..4 {
            rig.act(Action::Back(tab));
        }
        rig.frames(2);
        assert!(
            close(rig.pdf(tab).view.anchor, start),
            "{:?}",
            rig.pdf(tab).view.anchor
        );

        rig.marks(MarksAction::RemoveBookmark {
            doc: path,
            id: ids[0],
        });
        assert_eq!(rig.record(tab).expect("r").bookmarks().len(), 2);
    }

    // --- marks -------------------------------------------------------------------------------

    #[test]
    fn m_and_a_letter_set_a_mark_and_the_wait_ends() {
        let pdf = spec_doc("wp74-mark");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.read_at(tab, loc(2, 200.0));
        rig.key(Key::M, Mods::NONE);
        assert_eq!(rig.state.key_grab, Some(KeyGrab::SetMark(tab)));
        assert_eq!(
            rig.state.status_notice.as_ref().map(|n| n.text.as_str()),
            Some(tr("marks.press_letter"))
        );
        rig.key(Key::A, Mods::NONE);
        assert_eq!(rig.state.key_grab, None);
        let at = rig.record(tab).and_then(|r| r.mark('a')).expect("mark a");
        assert!(close(at, loc(2, 200.0)), "{at:?}");
        assert_eq!(
            rig.state.status_notice.as_ref().map(|n| n.text.clone()),
            Some(tr("marks.mark_name").replace("{}", "a"))
        );
    }

    #[test]
    fn going_to_a_mark_is_a_jump_and_back_returns() {
        let pdf = spec_doc("wp74-goto");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.read_at(tab, loc(3, 150.0));
        rig.key(Key::M, Mods::NONE);
        rig.key(Key::B, Mods::NONE);
        rig.read_at(tab, loc(0, 50.0));
        let before = rig.pdf(tab).view.anchor;
        rig.key(Key::Quote, Mods::NONE);
        assert_eq!(rig.state.key_grab, Some(KeyGrab::GoToMark(tab)));
        rig.key(Key::B, Mods::NONE);
        assert_eq!(rig.state.key_grab, None);
        rig.frames(2);
        assert!(close(reading_place(rig.pdf(tab)), loc(3, 150.0)));
        rig.act(Action::Back(tab));
        rig.frames(2);
        assert!(close(rig.pdf(tab).view.anchor, before));
    }

    #[test]
    fn a_global_mark_goes_to_its_document_even_when_it_was_closed() {
        let a = spec_doc("wp74-global-a");
        // Another content: a copy of A would be the same document by its fingerprint.
        let b = PdfBuilder::new()
            .page(612.0, 792.0, |p| {
                p.text(72.0, 100.0, 12.0, "another document");
            })
            .write_temp("wp74-global-b");
        let mut rig = Rig::new();
        let ta = rig.open(a.path());
        rig.read_at(ta, loc(2, 250.0));
        rig.key(Key::M, Mods::NONE);
        rig.key(Key::G, Mods::SHIFT);
        let mark = rig
            .state
            .marks
            .store
            .global_mark('G')
            .cloned()
            .expect("mark G");
        assert_eq!(mark.path, rig.doc(ta).path());
        let tb = rig.open(b.path());
        // Open: the tab of document A gets the focus and goes there.
        rig.key(Key::Quote, Mods::NONE);
        rig.key(Key::G, Mods::SHIFT);
        assert_eq!(rig.state.focus, Some(ta));
        rig.frames(2);
        assert!(close(reading_place(rig.pdf(ta)), loc(2, 250.0)));
        // Closed: it is opened again.
        rig.act(Action::CloseTab(ta));
        rig.act(Action::FocusTab(tb));
        rig.frames(2);
        rig.key(Key::Quote, Mods::NONE);
        rig.key(Key::G, Mods::SHIFT);
        let focus = rig.state.focus.expect("focus");
        assert_ne!(focus, tb);
        assert_eq!(rig.doc(focus).path(), mark.path);
        assert_eq!(rig.pdf(focus).view.anchor.page, 2);
    }

    #[test]
    fn the_wait_for_a_letter_ends_on_escape_another_key_time_and_a_closed_tab() {
        let pdf = spec_doc("wp74-cancel");
        let other = spec_doc("wp74-cancel-other");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.key(Key::M, Mods::NONE);
        rig.key(Key::Escape, Mods::NONE);
        assert_eq!(rig.state.key_grab, None);
        assert!(rig.state.status_notice.is_none(), "the prompt goes");

        rig.key(Key::M, Mods::NONE);
        rig.key(Key::Num1, Mods::NONE);
        assert_eq!(rig.state.key_grab, None);
        assert!(rig.record(tab).is_none_or(|r| r.marks().is_empty()));

        // Time runs out: two seconds of egui time, however many frames.
        rig.step = 0.0;
        rig.key(Key::M, Mods::NONE);
        rig.time += 1.9;
        rig.frames(3);
        assert!(rig.state.key_grab.is_some());
        rig.time += 0.2;
        rig.frames(1);
        assert_eq!(rig.state.key_grab, None);

        // The focus goes to another tab.
        let tab2 = rig.open(other.path());
        rig.act(Action::FocusTab(tab));
        rig.frames(1);
        rig.key(Key::M, Mods::NONE);
        rig.act(Action::FocusTab(tab2));
        rig.frames(1);
        assert_eq!(rig.state.key_grab, None);

        // The tab closes.
        rig.key(Key::M, Mods::NONE);
        assert!(rig.state.key_grab.is_some());
        rig.act(Action::CloseTab(tab2));
        rig.frames(1);
        assert_eq!(rig.state.key_grab, None);
    }

    #[test]
    fn a_missing_mark_is_announced() {
        let pdf = spec_doc("wp74-missing");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        let before = rig.pdf(tab).view.anchor;
        rig.key(Key::Quote, Mods::NONE);
        rig.key(Key::X, Mods::NONE);
        assert_eq!(
            rig.state.status_notice.as_ref().map(|n| n.text.clone()),
            Some(tr("marks.no_mark").replace("{}", "x"))
        );
        rig.key(Key::Quote, Mods::NONE);
        rig.key(Key::X, Mods::SHIFT);
        assert_eq!(
            rig.state.status_notice.as_ref().map(|n| n.text.clone()),
            Some(tr("marks.no_mark").replace("{}", "X"))
        );
        assert_eq!(rig.pdf(tab).view.anchor, before);
    }

    #[test]
    fn the_letter_after_m_does_not_run_a_command() {
        let pdf = spec_doc("wp74-noshortcut");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        // M again names mark m instead of starting over.
        rig.key(Key::M, Mods::NONE);
        rig.key(Key::M, Mods::NONE);
        assert_eq!(rig.state.key_grab, None);
        assert!(rig.record(tab).and_then(|r| r.mark('m')).is_some());
        // Ctrl+D while waiting cancels the wait and adds no bookmark.
        rig.key(Key::M, Mods::NONE);
        rig.key(Key::D, Mods::CTRL);
        assert_eq!(rig.state.key_grab, None);
        assert!(rig.record(tab).is_some_and(|r| r.bookmarks().is_empty()));
    }

    // --- highlights --------------------------------------------------------------------------

    /// The selection of `text` on `page` (its text must be there).
    fn find(rig: &Rig, tab: TabId, page: u32, text: &str) -> Selection {
        let pt = rig.doc(tab).page_text(page).expect("page text");
        let all: String = pt.chars.iter().map(|c| c.ch).collect();
        let at = all.find(text).expect("text on the page");
        let start = all[..at].chars().count() as u32;
        let end = start + text.chars().count() as u32;
        sel((page, start), (page, end))
    }

    fn select(rig: &mut Rig, tab: TabId, s: Selection) {
        if let Some(Tab::Pdf(p)) = rig.state.tabs.get_mut(tab) {
            p.ui.selection.set_for_test(s);
        }
    }

    #[test]
    fn highlighting_the_selection_records_it_and_clears_the_selection() {
        let pdf = spec_doc("wp74-hl");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.texts_of(tab, &[1]);
        let s = find(&rig, tab, 1, "Execution Management");
        select(&mut rig, tab, s);
        rig.key(Key::H, Mods::CTRL);
        assert!(!rig.pdf(tab).ui.has_selection());
        let rec = rig.record(tab).expect("record");
        let heads = rec.group_heads();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].text, "Execution Management");
        assert_eq!(heads[0].color, HighlightColor::Yellow);
        assert_eq!(heads[0].page, 1);

        // The menu's colour becomes the colour of the next Ctrl+H.
        let s = find(&rig, tab, 1, "2 Functional");
        select(&mut rig, tab, s);
        rig.marks(MarksAction::Highlight {
            tab,
            color: Some(HighlightColor::Green),
        });
        let s = find(&rig, tab, 1, "shall");
        select(&mut rig, tab, s);
        rig.key(Key::H, Mods::CTRL);
        let colors: Vec<HighlightColor> = rig
            .record(tab)
            .expect("record")
            .group_heads()
            .iter()
            .map(|h| h.color)
            .collect();
        assert_eq!(
            colors,
            vec![
                HighlightColor::Green,
                HighlightColor::Yellow,
                HighlightColor::Green
            ]
        );
        // The view gets the snapshot and draws it without panicking.
        rig.frames(2);
        let env = rig.pdf(tab).ui.highlights.env.clone().expect("env");
        assert_eq!(env.doc.highlights.len(), 3);
    }

    #[test]
    fn a_selection_over_pages_is_one_highlight_removed_together() {
        let pdf = spec_doc("wp74-pages");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        // Page 3 has never been shown: its text is fetched on a thread.
        select(&mut rig, tab, sel((1, 10), (3, 5)));
        rig.marks(MarksAction::Highlight { tab, color: None });
        assert!(!rig.pdf(tab).ui.has_selection());
        rig.frames_until("the highlight", |rig| {
            rig.record(tab).is_some_and(|r| !r.highlights().is_empty())
        });
        let rec = rig.record(tab).expect("record");
        let pages: Vec<u32> = rec.highlights().iter().map(|h| h.page).collect();
        assert_eq!(pages, vec![1, 2, 3]);
        let group = rec.highlights()[0].group;
        assert!(rec.highlights().iter().all(|h| h.group == group));
        let path = rig.doc(tab).path().to_path_buf();
        rig.marks(MarksAction::RemoveHighlight { doc: path, group });
        assert!(rig.record(tab).expect("record").highlights().is_empty());
    }

    #[test]
    fn colour_note_copy_and_stepping_work_on_a_highlight() {
        let pdf = spec_doc("wp74-edit");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.texts_of(tab, &[0, 1, 2]);
        let picks = [
            find(&rig, tab, 2, "state"),
            find(&rig, tab, 0, "intro"),
            find(&rig, tab, 1, "Execution"),
        ];
        for s in picks {
            select(&mut rig, tab, s);
            rig.marks(MarksAction::Highlight { tab, color: None });
        }
        let path = rig.doc(tab).path().to_path_buf();
        let group = rig.record(tab).expect("r").group_heads()[1].group;
        rig.marks(MarksAction::SetColor {
            doc: path.clone(),
            group,
            color: HighlightColor::Pink,
        });
        rig.marks(MarksAction::EditNote {
            doc: path.clone(),
            group,
        });
        let editing = rig.state.windows[0].marks.editing.clone().expect("editing");
        assert_eq!(editing.kind, EditKind::Note);
        rig.marks(MarksAction::SetNote {
            doc: path.clone(),
            group,
            note: "the states".into(),
        });
        let head = rig.record(tab).expect("r").group(group)[0].clone();
        assert_eq!(head.color, HighlightColor::Pink);
        assert_eq!(head.note, "the states");

        rig.marks(MarksAction::CopyHighlight {
            doc: path.clone(),
            group,
        });
        rig.marks(MarksAction::CopyAllHighlights(tab));
        rig.frames(1);
        assert_eq!(rig.copied.first().map(String::as_str), Some("Execution"));
        let all = rig.copied.get(1).cloned().unwrap_or_default();
        assert!(
            all.contains("the states") && all.contains("Execution"),
            "{all}"
        );

        rig.read_at(tab, loc(0, 0.0));
        let mut pages = Vec::new();
        for _ in 0..3 {
            rig.marks(MarksAction::StepHighlight { tab, forward: true });
            rig.frames(2);
            pages.push(reading_place(rig.pdf(tab)).page);
        }
        assert_eq!(pages, vec![0, 1, 2]);
    }

    #[test]
    fn same_colour_overlaps_merge_through_the_view() {
        let pdf = spec_doc("wp74-merge");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.texts_of(tab, &[1]);
        let s = find(&rig, tab, 1, "Execution");
        select(&mut rig, tab, s);
        rig.marks(MarksAction::Highlight { tab, color: None });
        let s = find(&rig, tab, 1, "tion Management");
        select(&mut rig, tab, s);
        rig.marks(MarksAction::Highlight { tab, color: None });
        let rec = rig.record(tab).expect("r");
        assert_eq!(rec.group_heads().len(), 1);
        assert_eq!(rec.highlights()[0].text, "Execution Management");
    }

    // --- the sidebar view --------------------------------------------------------------------

    #[test]
    fn the_marks_view_lists_its_rows_for_accessibility_and_a_click_goes_there() {
        let pdf = spec_doc("wp74-view");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.texts_of(tab, &[1]);
        rig.read_at(tab, loc(3, 100.0));
        rig.marks(MarksAction::AddBookmark(tab));
        rig.key(Key::Escape, Mods::NONE);
        let s = find(&rig, tab, 1, "Execution Management");
        select(&mut rig, tab, s);
        rig.marks(MarksAction::Highlight { tab, color: None });
        rig.read_at(tab, loc(0, 0.0));
        rig.frames(2);

        let labels = rig.button_labels();
        let names: Vec<&str> = labels.iter().map(|(l, _)| l.as_str()).collect();
        let bookmark = tr("marks.a11y_bookmark")
            .replace("{name}", "2.2 Errors")
            .replace("{page}", "4");
        let highlight = tr("marks.a11y_highlight")
            .replace("{color}", highlights::color_name(HighlightColor::Yellow))
            .replace("{text}", "Execution Management")
            .replace("{page}", "2");
        assert!(names.contains(&bookmark.as_str()), "{names:?}");
        assert!(names.contains(&highlight.as_str()), "{names:?}");
        assert!(names.contains(&tr("sidebar.marks")), "{names:?}");
        assert!(names.contains(&tr("marks.scope_all")), "{names:?}");
        let chip =
            tr("marks.only_color").replace("{}", highlights::color_name(HighlightColor::Pink));
        assert!(names.contains(&chip.as_str()), "{names:?}");

        let rect = labels
            .iter()
            .find(|(l, _)| *l == bookmark)
            .map(|(_, r)| *r)
            .expect("the bookmark row");
        rig.click(rect.center());
        rig.frames(2);
        assert!(close(reading_place(rig.pdf(tab)), loc(3, 100.0)));
        assert!(rig.pdf(tab).view.history.can_back());
    }

    #[test]
    fn pages_with_highlights_and_notes_draw_and_open_their_menu_without_panicking() {
        let pdf = spec_doc("wp74-paint");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.texts_of(tab, &[0]);
        select(&mut rig, tab, sel((0, 0), (0, 12)));
        rig.marks(MarksAction::Highlight { tab, color: None });
        let path = rig.doc(tab).path().to_path_buf();
        let group = rig.record(tab).expect("r").highlights()[0].group;
        rig.marks(MarksAction::SetNote {
            doc: path,
            group,
            note: "n".into(),
        });
        rig.read_at(tab, loc(0, 0.0));
        // A right click on the highlight: the menu knows which one it is about.
        let layout = rig.pdf(tab).ui.layout().cloned().expect("layout");
        let scroll = rig.pdf(tab).ui.scroll();
        let env = rig.pdf(tab).ui.highlights.env.clone().expect("env");
        let r = highlights::visible_rects(&env.doc, &layout, scroll, rig.pdf(tab).ui.viewport())[0];
        // The view starts below the menu bar and the section path line; find it by trying the
        // rectangle's centre with the offset of the view's top-left corner.
        let mut found = false;
        for dy in (0..120).step_by(4) {
            let at = Pos2::new(
                (r.rect.x0 + r.rect.x1) / 2.0 + rig_view_left(&rig),
                (r.rect.y0 + r.rect.y1) / 2.0 + dy as f32,
            );
            rig.run(vec![Event::PointerMoved(at)]);
            rig.run(vec![Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Secondary,
                pressed: true,
                modifiers: Mods::NONE,
            }]);
            rig.run(vec![Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Secondary,
                pressed: false,
                modifiers: Mods::NONE,
            }]);
            if rig.pdf(tab).ui.highlights.menu_target() == Some(group) {
                found = true;
                break;
            }
            rig.key(Key::Escape, Mods::NONE);
        }
        assert!(found, "a right click on the highlight finds it");
        rig.frames(3);
    }

    /// The left edge of the PDF view in the window (the sidebar is closed in these tests).
    fn rig_view_left(rig: &Rig) -> f32 {
        if rig.state.windows[0].toc_open {
            rig.state.windows[0].toc_width
        } else {
            0.0
        }
    }

    // --- saving ------------------------------------------------------------------------------

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let path = std::env::temp_dir().join(format!(
                "refdesk-wp74-{}-{name}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("dir");
            Dir(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn changes_are_saved_two_seconds_later_and_are_there_after_a_restart() {
        let dir = Dir::new("save");
        let pdf = spec_doc("wp74-save");
        let mut rig = Rig::new();
        rig.state.paths = Some(Paths::under(&dir.0));
        let tab = rig.open(pdf.path());
        rig.step = 0.0;
        rig.read_at(tab, loc(1, 320.0));
        rig.marks(MarksAction::AddBookmark(tab));
        let file = Paths::under(&dir.0).annotations_file();
        rig.time += 1.5;
        rig.frames(5);
        assert_eq!(rig.state.marks.saves, 0, "not yet");
        rig.time += 0.6;
        rig.frames(1);
        assert_eq!(rig.state.marks.saves, 1);
        rig.state.marks.wait_for_writer();
        assert!(file.exists());
        assert!(!rig.state.marks.is_dirty());

        let mut again = AppState::new();
        again.paths = Some(Paths::under(&dir.0));
        load_into(&mut again);
        let path = rig.doc(tab).path().to_path_buf();
        let rec = again.marks.store.doc(&path, None).expect("record");
        assert_eq!(rec.bookmarks()[0].name, "2.1 States");

        // A change just before exit is written by `save_now`.
        rig.marks(MarksAction::RenameBookmark {
            doc: path.clone(),
            id: rec.bookmarks()[0].id,
            name: "Renamed".into(),
        });
        save_now(&mut rig.state);
        let reread = Annotations::load(&file);
        assert_eq!(
            reread.doc(&path, None).expect("r").bookmarks()[0].name,
            "Renamed"
        );
    }

    #[test]
    fn without_a_save_path_marks_work_and_nothing_is_written() {
        let pdf = spec_doc("wp74-nopath");
        let mut rig = Rig::new();
        let tab = rig.open(pdf.path());
        rig.marks(MarksAction::AddBookmark(tab));
        rig.time += 3.0;
        rig.frames(2);
        assert_eq!(rig.state.marks.saves, 0);
        assert!(!rig.state.marks.is_dirty());
        save_now(&mut rig.state);
        assert_eq!(rig.record(tab).map(|r| r.bookmarks().len()), Some(1));
    }

    #[test]
    fn a_moved_file_keeps_its_marks() {
        let dir = Dir::new("moved");
        let old = dir.0.join("spec.pdf");
        std::fs::copy(spec_doc("wp74-moved").path(), &old).expect("copy");
        let mut rig = Rig::new();
        rig.state.paths = Some(Paths::under(&dir.0.join("state")));
        let tab = rig.open(&old);
        let path = rig.doc(tab).path().to_path_buf();
        rig.frames_until("the fingerprint", |rig| {
            rig.state.memory.fingerprint_of(&path).is_some()
        });
        rig.read_at(tab, loc(2, 100.0));
        rig.marks(MarksAction::AddBookmark(tab));
        rig.frames(1);
        save_now(&mut rig.state);

        let new = dir.0.join("renamed.pdf");
        std::fs::rename(&old, &new).expect("rename");
        let mut rig = Rig::new();
        rig.state.paths = Some(Paths::under(&dir.0.join("state")));
        load_into(&mut rig.state);
        let tab = rig.open(&new);
        let new_path = rig.doc(tab).path().to_path_buf();
        rig.frames_until("the record follows the file", |rig| {
            rig.state
                .marks
                .store
                .doc(&new_path, None)
                .is_some_and(|r| r.bookmarks().len() == 1)
        });
        assert_eq!(rig.state.marks.store.docs().len(), 1);
        assert_eq!(rig.state.marks.store.docs()[0].path, new_path);
        let env = rig.pdf(tab).ui.highlights.env.clone().expect("env");
        assert_eq!(env.doc.path, new_path);
    }

    // --- commands ----------------------------------------------------------------------------

    #[test]
    fn the_mark_commands_are_in_their_menus_with_their_shortcuts() {
        use egui::KeyboardShortcut;
        let expect = [
            (
                "edit.add_bookmark",
                MenuGroup::Edit,
                Some((Mods::CTRL, Key::D)),
            ),
            ("edit.next_bookmark", MenuGroup::Edit, None),
            ("edit.prev_bookmark", MenuGroup::Edit, None),
            ("edit.set_mark", MenuGroup::Edit, Some((Mods::NONE, Key::M))),
            (
                "edit.go_to_mark",
                MenuGroup::Edit,
                Some((Mods::NONE, Key::Quote)),
            ),
            (
                "edit.highlight",
                MenuGroup::Edit,
                Some((Mods::CTRL, Key::H)),
            ),
            ("edit.next_highlight", MenuGroup::Edit, None),
            ("edit.prev_highlight", MenuGroup::Edit, None),
            ("edit.copy_highlights", MenuGroup::Edit, None),
            ("view.show_marks", MenuGroup::View, None),
        ];
        for (id, menu, keys) in expect {
            let c = commands::command(id).expect(id);
            assert_eq!(c.menu, menu, "{id}");
            assert_eq!(
                c.shortcut,
                keys.map(|(m, k)| KeyboardShortcut::new(m, k)),
                "{id}"
            );
            assert!(commands::in_menu(menu).any(|x| x.id == id), "{id}");
            assert_ne!(tr(c.name_key), "???", "{id}");
        }
        // Without a document only the view command can run.
        let state = AppState::new();
        assert!(
            commands::command("edit.add_bookmark")
                .expect("c")
                .run(&state)
                .is_none()
        );
        assert_eq!(
            commands::command("view.show_marks").expect("c").run(&state),
            Some(Action::Marks(MarksAction::Show))
        );
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch_action(&mut state, Action::Marks(MarksAction::Show), &ctx);
        assert_eq!(state.windows[0].sidebar_view, SidebarView::Marks);
        assert!(state.windows[0].toc_open);
        let _ = Point::new(0.0, 0.0);
    }
}
