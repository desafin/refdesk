//! The search of a PDF view: the search bar above it, the highlights over its pages and the
//! result list beside the window (FIND-1 ~ FIND-6, `docs/ARCHITECTURE.md` §4.8).
//!
//! The search itself is `refdesk_core::SearchJob`, which runs on its own thread. This module
//! decides what the screen shows of it, and everything that decides something is a plain function
//! or a method of [`SearchState`] that does not touch egui, so it is tested without a window
//! (rule R-10): which match is the current one, where the next and the previous one are, how the
//! list wraps around, where the view has to scroll to show a match, which rectangles of which
//! pages are highlighted, how the result list is laid out, and when a typed query starts a search.
//! [`show_bar`] and [`show_results_panel`] only draw those results and turn input into calls.
//!
//! The state of a search belongs to a view ([`crate::views::pdf_view::PdfViewUiState::search`]),
//! so every tab searches on its own. It is not saved.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{
    Color32, Frame, Id, Key, Modifiers, Panel, Pos2, RichText, ScrollArea, Sense, Stroke, TextEdit,
    TextStyle, Ui, vec2,
};
use refdesk_core::{
    DocumentHandle, Location, Notifier, Outline, PageLayout, Place, Point, Rect as PageRect,
    SearchError, SearchJob, SearchMatch, SearchQuery, SearchSnapshot, Size, selection_text,
};

use crate::i18n::tr;
use crate::state::{AppState, DocStore, PdfTab, Tab, TabId, TabStore, WindowId};
use crate::views::links::page_label;
use crate::views::pdf_view::anchor_at;
use crate::views::selection::COPY_OPTIONS;

/// A typed query starts a search this long after the last keystroke.
pub const INPUT_DELAY_SECS: f64 = 0.15;
/// The longest selected text that "Find" takes over into the input field, in characters.
pub const MAX_SELECTION_CHARS: usize = 200;
/// How long the "continued from the start" notice stays, in seconds.
pub const WRAP_NOTICE_SECS: f64 = 1.5;
/// The width of the result list that has not been resized.
pub const DEFAULT_RESULTS_WIDTH: f32 = 320.0;
/// The narrowest and the widest the result list can be dragged to.
pub const RESULTS_WIDTH_RANGE: (f32, f32) = (180.0, 800.0);
/// A match counts as comfortably in view when it is at least this fraction of the viewport
/// height away from the top and the bottom edge.
const REVEAL_MARGIN: f32 = 0.1;
/// The same for the left and right edge, in logical pixels.
const REVEAL_MARGIN_X: f32 = 16.0;
/// The text before a match is cut to this many characters in the result list, so that the match
/// itself stays in view in a narrow panel.
const BEFORE_CHARS: usize = 24;
/// The matched text is cut to this many characters in the result list.
const MATCH_CHARS: usize = 80;

// ---------------------------------------------------------------------------------------------
// Pure logic: the selected text as a query
// ---------------------------------------------------------------------------------------------

/// The query a selected text makes. `raw` is the text as selected, one `\n` per line end.
///
/// `strict` (Ctrl+F) takes a selection of one line of at most [`MAX_SELECTION_CHARS`] characters
/// and nothing else, so a selected paragraph does not become a search for the whole paragraph.
/// Not strict (the context menu entry, where the user asked for this text) joins the lines with
/// blanks and cuts the text at [`MAX_SELECTION_CHARS`].
pub fn selection_query(raw: &str, strict: bool) -> Option<String> {
    if strict {
        let text = raw.trim();
        let ok = !text.is_empty()
            && !text.contains(['\n', '\r'])
            && text.chars().count() <= MAX_SELECTION_CHARS;
        return ok.then(|| text.to_owned());
    }
    let joined = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() {
        return None;
    }
    Some(joined.chars().take(MAX_SELECTION_CHARS).collect())
}

// ---------------------------------------------------------------------------------------------
// Pure logic: which match is the current one
// ---------------------------------------------------------------------------------------------

/// Where a match starts: its page and the top-left corner of its first rectangle.
pub fn match_start(m: &SearchMatch) -> Location {
    let (x, y) = m.rects.first().map_or((0.0, 0.0), |r| (r.x0, r.y0));
    Location::new(m.page, x, y)
}

/// A match that does not move when matches are added before it: the page and the first
/// character. The list of matches is in document order, so the key finds it again by bisection.
pub type MatchKey = (u32, u32);

/// The key of a match.
pub fn match_key(m: &SearchMatch) -> MatchKey {
    (m.page, m.chars.start)
}

/// The index of the match with this key, if it is there.
pub fn find_key(matches: &[SearchMatch], key: MatchKey) -> Option<usize> {
    let i = matches.partition_point(|m| match_key(m) < key);
    matches.get(i).filter(|m| match_key(m) == key).map(|_| i)
}

/// Whether a match starts at `from` or after it (page first, then the height of its first line).
fn starts_at_or_after(m: &SearchMatch, from: Location) -> bool {
    m.page > from.page || (m.page == from.page && match_start(m).y >= from.y)
}

/// The first match at or after `from`. When there is none: the first match of the document if the
/// search `finished` (the search wraps around), else `None` (a later page may still have one).
pub fn next_from(matches: &[SearchMatch], from: Location, finished: bool) -> Option<usize> {
    let first = matches.partition_point(|m| m.page < from.page);
    matches[first..]
        .iter()
        .position(|m| starts_at_or_after(m, from))
        .map(|i| i + first)
        .or_else(|| (finished && !matches.is_empty()).then_some(0))
}

/// The last match before `from`. When there is none: the last match of the document if the search
/// `finished`, else `None`.
pub fn prev_from(matches: &[SearchMatch], from: Location, finished: bool) -> Option<usize> {
    let end = matches.partition_point(|m| m.page <= from.page);
    matches[..end]
        .iter()
        .rposition(|m| !starts_at_or_after(m, from))
        .or_else(|| (finished && !matches.is_empty()).then(|| matches.len() - 1))
}

/// The result of stepping from one match to the next or the previous.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    /// The match stepped to.
    pub index: usize,
    /// The step went past the end (or the start) of the document and wrapped around.
    pub wrapped: bool,
}

/// The match after (or before) match `current` of `len` matches. Past the last match it wraps to
/// the first one, and the other way round, once the search has `finished`; before that there is
/// nothing to step to, because the matches that come later are not known yet.
pub fn step_index(len: usize, current: usize, forward: bool, finished: bool) -> Option<Step> {
    if len == 0 || current >= len {
        return None;
    }
    let (index, wrapped) = if forward {
        if current + 1 < len {
            (current + 1, false)
        } else if finished {
            (0, true)
        } else {
            return None;
        }
    } else if current > 0 {
        (current - 1, false)
    } else if finished {
        (len - 1, true)
    } else {
        return None;
    };
    Some(Step { index, wrapped })
}

// ---------------------------------------------------------------------------------------------
// Pure logic: highlights
// ---------------------------------------------------------------------------------------------

/// Which matches lie on which page: built once per snapshot, so that drawing a frame looks at
/// the matches of the visible pages only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageIndex {
    /// For each page the range of the matches that start on it (they are in document order).
    starts: Vec<Range<u32>>,
    /// For each page the matches that start on the page before and run on to it.
    conts: BTreeMap<u32, Vec<u32>>,
}

impl PageIndex {
    /// The index of `matches`, which must be in document order. `pages` is the page count of the
    /// document; pages past it that matches mention are covered as well.
    pub fn build(matches: &[SearchMatch], pages: u32) -> PageIndex {
        let last = matches.last().map_or(0, |m| m.page + 1);
        let mut starts = vec![0..0; pages.max(last) as usize];
        let mut conts: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (i, m) in matches.iter().enumerate() {
            let i = i as u32;
            if let Some(range) = starts.get_mut(m.page as usize) {
                if range.start == range.end {
                    *range = i..i + 1;
                } else {
                    range.end = i + 1;
                }
            }
            if let Some(cont) = &m.cont {
                conts.entry(cont.page).or_default().push(i);
            }
        }
        PageIndex { starts, conts }
    }

    /// The matches that start on `page`, as indices into the match list.
    pub fn starting_on(&self, page: u32) -> Range<usize> {
        self.starts
            .get(page as usize)
            .map_or(0..0, |r| r.start as usize..r.end as usize)
    }

    /// The matches that start on an earlier page and continue on `page`.
    pub fn continuing_on(&self, page: u32) -> &[u32] {
        self.conts.get(&page).map_or(&[], Vec::as_slice)
    }
}

/// One rectangle to tint, in viewport coordinates (the top-left corner of the view is the
/// origin, logical pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MatchRect {
    /// The area.
    pub rect: PageRect,
    /// It belongs to the current match.
    pub current: bool,
}

/// The rectangles of the matches on the pages that overlap the viewport. A match that runs over a
/// page boundary has its rectangles on both pages.
pub fn match_rects(
    index: &PageIndex,
    matches: &[SearchMatch],
    current: Option<usize>,
    layout: &PageLayout,
    scroll: Point,
    viewport: Size,
) -> Vec<MatchRect> {
    let scale = layout.scale();
    let mut out = Vec::new();
    for page in layout.visible_pages(scroll, viewport) {
        let origin = layout.page_rect(page);
        let mut add = |rects: &[PageRect], is_current: bool| {
            for r in rects {
                out.push(MatchRect {
                    rect: PageRect::new(
                        origin.x0 + r.x0 * scale - scroll.x,
                        origin.y0 + r.y0 * scale - scroll.y,
                        origin.x0 + r.x1 * scale - scroll.x,
                        origin.y0 + r.y1 * scale - scroll.y,
                    ),
                    current: is_current,
                });
            }
        };
        for i in index.starting_on(page) {
            if let Some(m) = matches.get(i) {
                add(&m.rects, current == Some(i));
            }
        }
        for &i in index.continuing_on(page) {
            let i = i as usize;
            if let Some(cont) = matches.get(i).and_then(|m| m.cont.as_ref()) {
                add(&cont.rects, current == Some(i));
            }
        }
    }
    out
}

/// The scroll offset that shows the first line of a match, or `None` when it is comfortably in
/// view already (then the view stays where it is). A match that is not is put a third of the
/// viewport height below the top edge (and a third of its width from the left edge when it is
/// off to the side). `rect` is in the page space of `page`.
pub fn reveal_scroll(
    layout: &PageLayout,
    scroll: Point,
    viewport: Size,
    page: u32,
    rect: PageRect,
) -> Option<Point> {
    if page >= layout.page_count() {
        return None;
    }
    let scale = layout.scale();
    let origin = layout.page_rect(page);
    let (top, bottom) = (origin.y0 + rect.y0 * scale, origin.y0 + rect.y1 * scale);
    let (left, right) = (origin.x0 + rect.x0 * scale, origin.x0 + rect.x1 * scale);
    let margin_y = viewport.h * REVEAL_MARGIN;
    let in_view_y = top >= scroll.y + margin_y && bottom <= scroll.y + viewport.h - margin_y;
    let can_scroll_x = layout.max_scroll(viewport).x > 0.0;
    let in_view_x = !can_scroll_x
        || (left >= scroll.x + REVEAL_MARGIN_X && right <= scroll.x + viewport.w - REVEAL_MARGIN_X);
    if in_view_y && in_view_x {
        return None;
    }
    let y = if in_view_y {
        scroll.y
    } else {
        top - viewport.h / 3.0
    };
    let x = if in_view_x {
        scroll.x
    } else {
        left - viewport.w / 3.0
    };
    let to = layout.clamp_scroll(Point::new(x, y), viewport);
    ((to.x - scroll.x).abs() > 0.5 || (to.y - scroll.y).abs() > 0.5).then_some(to)
}

// ---------------------------------------------------------------------------------------------
// Pure logic: the count text and the input delay
// ---------------------------------------------------------------------------------------------

/// The text beside the input field: "3 / 42", "3 / 100000+" when the search stopped at its limit,
/// "– / 7… 40%" while it still runs (`searching` is the percent of the pages done), and "No
/// match" when the finished search found nothing. `current` is the index of the current match.
pub fn count_text(
    current: Option<usize>,
    snapshot: &SearchSnapshot,
    searching: Option<u32>,
) -> String {
    let found = snapshot.matches.len();
    let n = current.map_or_else(|| "–".to_owned(), |i| (i + 1).to_string());
    match searching {
        Some(percent) => tr("find.count_searching")
            .replace("{n}", &n)
            .replace("{total}", &found.to_string())
            .replace("{percent}", &percent.to_string()),
        None if found == 0 => tr("find.none").to_owned(),
        None => {
            let total = if snapshot.truncated {
                format!("{found}+")
            } else {
                found.to_string()
            };
            tr("find.count")
                .replace("{n}", &n)
                .replace("{total}", &total)
        }
    }
}

/// How long until a query edited at `edited` (seconds) is due to be searched; `None` when it is
/// due at `now`.
pub fn input_delay_left(edited: f64, now: f64) -> Option<Duration> {
    let left = INPUT_DELAY_SECS - (now - edited);
    (left > 0.0).then(|| Duration::from_secs_f64(left))
}

// ---------------------------------------------------------------------------------------------
// Pure logic: the result list
// ---------------------------------------------------------------------------------------------

/// One line of the result list. Both kinds are one line high.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListRow {
    /// The head of the matches of one page.
    Page {
        /// The zero-based page.
        page: u32,
        /// The first match on the page (an index into the match list).
        first: u32,
        /// How many matches start on the page.
        count: u32,
        /// The section of the outline the first match is in (an index into `Outline::items`).
        section: Option<u32>,
    },
    /// One match (an index into the match list).
    Match(u32),
}

/// Where a row sorts: page, then the head before its matches, then the matches in order.
pub type RowKey = (u32, u8, u32);

/// The key of a row.
pub fn row_key(row: &ListRow, matches: &[SearchMatch]) -> RowKey {
    match row {
        ListRow::Page { page, .. } => (*page, 0, 0),
        ListRow::Match(i) => matches
            .get(*i as usize)
            .map_or((u32::MAX, 1, 0), |m| (m.page, 1, m.chars.start)),
    }
}

/// The rows of the list: a head for every page that has matches, each followed by the matches that
/// start on it. The section of a head is the one the first match of its page is in.
pub fn list_rows(matches: &[SearchMatch], outline: &Outline) -> Vec<ListRow> {
    let mut rows = Vec::with_capacity(matches.len() + matches.len() / 4 + 1);
    let mut i = 0;
    while i < matches.len() {
        let page = matches[i].page;
        let end = i + matches[i..].iter().take_while(|m| m.page == page).count();
        rows.push(ListRow::Page {
            page,
            first: i as u32,
            count: (end - i) as u32,
            section: outline.current(match_start(&matches[i])),
        });
        rows.extend((i..end).map(|j| ListRow::Match(j as u32)));
        i = end;
    }
    rows
}

/// The row that shows match `index`.
pub fn row_of_match(rows: &[ListRow], matches: &[SearchMatch], index: usize) -> Option<usize> {
    let key = matches.get(index).map(match_key)?;
    let key = (key.0, 1, key.1);
    let at = rows.partition_point(|r| row_key(r, matches) < key);
    rows.get(at)
        .filter(|r| row_key(r, matches) == key)
        .map(|_| at)
}

/// The row with this key.
pub fn find_row(rows: &[ListRow], matches: &[SearchMatch], key: RowKey) -> Option<usize> {
    let at = rows.partition_point(|r| row_key(r, matches) < key);
    rows.get(at)
        .filter(|r| row_key(r, matches) == key)
        .map(|_| at)
}

/// The scroll offset that keeps the row at the top of a list in place when rows are added above
/// it: `top` is the row that was at the top (its key and how far it was scrolled out) before the
/// rows changed. `None` when that row is gone.
pub fn keep_top_offset(
    top: (RowKey, f32),
    rows: &[ListRow],
    matches: &[SearchMatch],
    stride: f32,
) -> Option<f32> {
    let row = find_row(rows, matches, top.0)?;
    Some(row as f32 * stride + top.1)
}

/// "12 — Section title", or "12" without a section; `page` is zero-based and shown by its label.
pub fn page_head_text(page: u32, labels: &[String], section: Option<&str>) -> String {
    let label = page_label(page, labels);
    match section.filter(|s| !s.trim().is_empty()) {
        Some(title) => tr("link.page_title")
            .replace("{page}", &label)
            .replace("{title}", title),
        None => tr("link.page").replace("{page}", &label),
    }
}

/// The three parts of a match line: the text before, the match, the text after, each on one
/// line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowText {
    /// The end of the text before the match.
    pub before: String,
    /// The matched text.
    pub matched: String,
    /// The text after the match.
    pub after: String,
}

fn one_line(text: &str) -> String {
    text.replace(char::is_control, " ")
}

/// The line of a match in the result list.
pub fn row_text(m: &SearchMatch) -> RowText {
    let before = one_line(&m.before);
    let count = before.chars().count();
    let before = if count > BEFORE_CHARS {
        let cut = before
            .char_indices()
            .nth(count - BEFORE_CHARS)
            .map_or(0, |(i, _)| i);
        format!("…{}", &before[cut..])
    } else {
        before
    };
    let matched = one_line(&m.matched);
    let matched = if matched.chars().count() > MATCH_CHARS {
        let cut = matched
            .char_indices()
            .nth(MATCH_CHARS)
            .map_or(matched.len(), |(i, _)| i);
        format!("{}…", &matched[..cut])
    } else {
        matched
    };
    RowText {
        before,
        matched,
        after: one_line(&m.after),
    }
}

// ---------------------------------------------------------------------------------------------
// The state of one search
// ---------------------------------------------------------------------------------------------

/// Which way a step wrapped around.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrap {
    /// Forward past the last match: continued at the first.
    ToStart,
    /// Backward past the first match: continued at the last.
    ToEnd,
}

/// When the typed query is searched.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Due {
    /// At once: an option was toggled, the document was reloaded, a search was asked for.
    Now,
    /// [`INPUT_DELAY_SECS`] after this time (seconds): the text was edited.
    After(f64),
}

/// What [`SearchState::tick`] found out in a frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tick {
    /// The first current match of a new search was chosen: the view shows it ([`reveal_current`]).
    pub reveal: bool,
    /// A frame is needed after this long (a typed query becomes due).
    pub repaint_after: Option<Duration>,
}

/// The search of one view: the query as typed, the running job, what it found, which match is the
/// current one. Not saved.
#[derive(Clone, Debug, Default)]
pub struct SearchState {
    /// The search bar is shown and the matches are highlighted.
    pub open: bool,
    /// The result list is shown beside the window.
    pub list_open: bool,
    /// The query as typed and the three options. It is remembered when the bar is closed.
    pub query: SearchQuery,
    /// The input field takes the keyboard focus and selects all its text in the next frame.
    pub focus_input: bool,
    /// The first move away from where the search began was written to the history already.
    pub departed: bool,
    /// How many searches were started (tests).
    pub starts: u32,
    job: Option<Arc<SearchJob>>,
    /// The query the job runs.
    running: Option<SearchQuery>,
    /// The generation of the document the job runs on.
    generation: Option<u32>,
    /// Why the query cannot be searched (an invalid regular expression).
    error: Option<String>,
    due: Option<Due>,
    /// What is shown: the matches, in document order.
    snapshot: Arc<SearchSnapshot>,
    /// `snapshot` is the one of the current job (not what the job before it left).
    live: bool,
    /// Percent of the pages searched while the job runs.
    percent: Option<u32>,
    index: PageIndex,
    /// Counts the snapshots shown; the rows of the list are made again when it changes.
    version: u64,
    current: Option<MatchKey>,
    current_idx: Option<usize>,
    /// Where the current match starts, to search on from there when the query changes.
    current_pos: Option<Location>,
    /// The first match at or after this place becomes the current one as soon as it is known.
    pick: Option<Location>,
    wrap_notice: Option<(Wrap, f64)>,
    /// The current match changed: the list scrolls to it (unless the pointer is on the list).
    pub list_follow: bool,
    rows: Option<(u64, Option<u32>, Arc<Vec<ListRow>>)>,
    list_scroll: ListScroll,
}

/// The scroll position of the result list, as of the last frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ListScroll {
    offset: f32,
    view_h: f32,
    /// The row at the top and how far it is scrolled out; `None` at the very top.
    top: Option<(RowKey, f32)>,
}

impl SearchState {
    /// The text in the input field is not empty.
    pub fn has_query(&self) -> bool {
        !self.query.text.trim().is_empty()
    }

    /// The matches shown, in document order.
    pub fn snapshot(&self) -> &Arc<SearchSnapshot> {
        &self.snapshot
    }

    /// The index of the current match in [`SearchState::snapshot`].
    #[cfg(test)]
    pub fn current_index(&self) -> Option<usize> {
        self.current_idx
    }

    /// The current match.
    pub fn current_match(&self) -> Option<&SearchMatch> {
        self.snapshot.matches.get(self.current_idx?)
    }

    /// Why the query cannot be searched, if it cannot.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// A search runs for the query: its job is not finished.
    #[cfg(test)]
    pub fn is_searching(&self) -> bool {
        self.percent.is_some()
    }

    /// The finished search found nothing.
    pub fn no_match(&self) -> bool {
        self.live
            && self.running.is_some()
            && self.percent.is_none()
            && self.snapshot.matches.is_empty()
    }

    /// The text of the field changed at `now` (seconds): the search starts after the input delay.
    pub fn edited(&mut self, now: f64) {
        self.due = Some(Due::After(now));
    }

    /// An option changed: the search starts at once.
    pub fn options_changed(&mut self) {
        self.due = Some(Due::Now);
    }

    /// Drops the job and everything it found.
    fn clear_results(&mut self) {
        self.job = None;
        self.running = None;
        self.due = None;
        self.snapshot = Arc::default();
        self.index = PageIndex::default();
        self.version += 1;
        self.live = false;
        self.percent = None;
        self.current = None;
        self.current_idx = None;
        self.current_pos = None;
        self.pick = None;
        self.list_follow = false;
    }

    /// Shows the bar. With `text` (the selected text) the field is filled in and searched at once;
    /// else a remembered query is searched again and an empty field just waits. Called again
    /// while the bar is open it selects the text of the field instead. Returns true when a search
    /// has to be started now.
    pub fn open(&mut self, text: Option<String>) -> bool {
        let was_open = self.open;
        self.open = true;
        self.focus_input = true;
        match text {
            Some(text) => {
                self.query.text = text;
                self.current = None;
                self.current_idx = None;
                self.current_pos = None;
                self.departed = false;
                true
            }
            None if was_open => false,
            None => {
                self.current = None;
                self.current_idx = None;
                self.current_pos = None;
                self.departed = false;
                self.has_query()
            }
        }
    }

    /// Hides the bar and the highlights and cancels the search. The query stays for the next time.
    pub fn close(&mut self) {
        self.open = false;
        self.focus_input = false;
        self.error = None;
        self.wrap_notice = None;
        self.clear_results();
    }

    /// Starts searching for the query from the current match (else from `anchor`, the place the
    /// view is at). The job before it is dropped, which cancels it. An empty query or an invalid
    /// regular expression starts nothing and clears the results.
    pub fn start(
        &mut self,
        doc: &DocumentHandle,
        notifier: &Notifier,
        anchor: Location,
        generation: Option<u32>,
    ) {
        self.due = None;
        self.error = None;
        self.generation = generation;
        let from = self.current_pos.unwrap_or(anchor);
        match SearchJob::start(doc.clone(), self.query.clone(), from.page, notifier.clone()) {
            Ok(job) => {
                self.starts += 1;
                self.job = Some(Arc::new(job));
                self.running = Some(self.query.clone());
                self.live = false;
                self.percent = Some(0);
                self.pick = Some(from);
            }
            Err(SearchError::Empty) => self.clear_results(),
            Err(SearchError::InvalidRegex(message)) => {
                self.clear_results();
                self.error = Some(message);
            }
            Err(SearchError::Thread(message)) => {
                log::error!("search: {message}");
                self.clear_results();
                self.error = Some(message);
            }
        }
    }

    /// One frame of the search: starts the search that is due, takes over what the job found, and
    /// chooses the first current match. `generation` is that of the document as of now: a reloaded
    /// document is searched again (R-7). `anchor` is where the view is.
    pub fn tick(
        &mut self,
        doc: &DocumentHandle,
        notifier: &Notifier,
        generation: Option<u32>,
        anchor: Location,
        now: f64,
    ) -> Tick {
        let mut out = Tick::default();
        if self.running.is_some() {
            match (self.generation, generation) {
                (Some(old), Some(new)) if old != new => self.due = Some(Due::Now),
                (None, Some(_)) => self.generation = generation,
                _ => {}
            }
        }
        match self.due {
            Some(Due::Now) => self.start(doc, notifier, anchor, generation),
            Some(Due::After(edited)) => match input_delay_left(edited, now) {
                None => self.start(doc, notifier, anchor, generation),
                left => out.repaint_after = left,
            },
            None => {}
        }

        let Some(job) = &self.job else {
            return out;
        };
        let live = job.snapshot();
        let total = live.pages_total.max(1);
        self.percent = (!live.finished).then(|| (live.pages_done * 100 / total).min(100));
        // What the job before this one left stays until the new job has something to show, so
        // that typing does not make the highlights blink.
        let fresh = live.pages_done > 0 || live.finished;
        if fresh && !(self.live && Arc::ptr_eq(&live, &self.snapshot)) {
            self.index = PageIndex::build(&live.matches, live.pages_total);
            self.snapshot = live;
            self.version += 1;
            self.live = true;
            self.current_idx = self
                .current
                .and_then(|k| find_key(&self.snapshot.matches, k));
            if self.current_idx.is_none() {
                self.current = None;
            }
        }
        if self.live
            && let Some(from) = self.pick
        {
            let finished = self.snapshot.finished;
            match next_from(&self.snapshot.matches, from, finished) {
                Some(i) => {
                    self.pick = None;
                    self.set_current(i);
                    out.reveal = true;
                }
                None if finished => self.pick = None,
                None => {}
            }
        }
        out
    }

    /// Makes match `index` the current one.
    pub fn set_current(&mut self, index: usize) {
        if let Some(m) = self.snapshot.matches.get(index) {
            self.current = Some(match_key(m));
            self.current_pos = Some(match_start(m));
            self.current_idx = Some(index);
            self.list_follow = true;
        }
    }

    /// Steps to the next or the previous match from the current one (from `anchor` when there is
    /// none). Returns the new index. A step that wraps around is announced for a moment.
    pub fn step(&mut self, forward: bool, anchor: Location, now: f64) -> Option<usize> {
        let snapshot = Arc::clone(&self.snapshot);
        let matches = &snapshot.matches;
        // The matches of an older query are not complete for this one.
        let finished = self.live && snapshot.finished;
        let (index, wrapped) = match self.current_idx {
            Some(i) => {
                let s = step_index(matches.len(), i, forward, finished)?;
                (s.index, s.wrapped)
            }
            None => {
                let from = self.current_pos.unwrap_or(anchor);
                let i = if forward {
                    next_from(matches, from, finished)?
                } else {
                    prev_from(matches, from, finished)?
                };
                (i, false)
            }
        };
        if wrapped {
            let which = if forward { Wrap::ToStart } else { Wrap::ToEnd };
            self.wrap_notice = Some((which, now));
        }
        self.pick = None;
        self.set_current(index);
        Some(index)
    }

    /// The wrap notice to show at `now` and how long it stays.
    pub fn wrap_notice(&self, now: f64) -> Option<(Wrap, Duration)> {
        let (which, since) = self.wrap_notice?;
        let left = WRAP_NOTICE_SECS - (now - since);
        (left > 0.0).then(|| (which, Duration::from_secs_f64(left)))
    }

    /// The rectangles to highlight for the pages of the viewport. Nothing while the bar is closed.
    pub fn highlights(&self, layout: &PageLayout, scroll: Point, viewport: Size) -> Vec<MatchRect> {
        if !self.open {
            return Vec::new();
        }
        match_rects(
            &self.index,
            &self.snapshot.matches,
            self.current_idx,
            layout,
            scroll,
            viewport,
        )
    }

    /// The rows of the result list, made again only when the matches or the outline changed.
    pub fn rows(&mut self, outline: &Outline, generation: u32) -> Arc<Vec<ListRow>> {
        if let Some((version, g, rows)) = &self.rows
            && *version == self.version
            && *g == Some(generation)
        {
            return Arc::clone(rows);
        }
        let rows = Arc::new(list_rows(&self.snapshot.matches, outline));
        self.rows = Some((self.version, Some(generation), Arc::clone(&rows)));
        rows
    }
}

// ---------------------------------------------------------------------------------------------
// Moving the view
// ---------------------------------------------------------------------------------------------

/// Which moves of the view go into the history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// The first move away from where the search began: one long jump, so that Back returns
    /// there. The moves after it (next, previous) add nothing.
    FirstDeparture,
    /// Every move that changes the place: a click in the result list.
    Always,
}

/// Moves the view of `pdf` to `to`, and records the move as the history rule says.
fn move_view(pdf: &mut PdfTab, to: Location, record: Record) {
    let first = !pdf.ui.search.departed;
    if record == Record::Always || first {
        let doc = pdf.view.history.current().doc.clone();
        let from = Place {
            doc: doc.clone(),
            loc: pdf.view.anchor,
        };
        pdf.view.history.jump(from, Place { doc, loc: to });
        pdf.ui.search.departed = true;
    }
    pdf.view.anchor = to;
}

/// Shows the current match of `pdf`: scrolls the view so that the match is a third of the
/// viewport height below its top edge, unless it is comfortably in view already. Returns true when
/// the view moved.
pub fn reveal_current(pdf: &mut PdfTab, record: Record) -> bool {
    let Some(m) = pdf.ui.search.current_match() else {
        return false;
    };
    let Some(rect) = m.rects.first().copied() else {
        return false;
    };
    let page = m.page;
    let to = match pdf.ui.layout() {
        Some(layout) if page < layout.page_count() => {
            let scroll = pdf.ui.scroll();
            let viewport = pdf.ui.viewport();
            let Some(to) = reveal_scroll(layout, scroll, viewport, page, rect) else {
                return false;
            };
            anchor_at(layout, to)
        }
        // Not laid out yet: the top of the match's line is all that is known.
        _ => Location::new(page, 0.0, (rect.y0 - 24.0).max(0.0)),
    };
    move_view(pdf, to, record);
    true
}

/// Makes match `index` the current one and shows it. A click in the result list: a move that
/// changes the place is a long jump in the history.
pub fn go_to_result(pdf: &mut PdfTab, index: usize) -> bool {
    if index >= pdf.ui.search.snapshot().matches.len() {
        return false;
    }
    pdf.ui.search.set_current(index);
    // Keeps the list from scrolling under the pointer that clicked it.
    pdf.ui.search.list_follow = false;
    reveal_current(pdf, Record::Always);
    true
}

/// Next or previous match (F3, Shift+F3, Enter, Shift+Enter): moves the current match and shows
/// it. Returns true when the current match changed.
pub fn step_tab(pdf: &mut PdfTab, forward: bool, now: f64) -> bool {
    let anchor = pdf.view.anchor;
    if pdf.ui.search.step(forward, anchor, now).is_none() {
        return false;
    }
    reveal_current(pdf, Record::FirstDeparture);
    true
}

/// Opens the search bar of `pdf` (Ctrl+F): focus goes to the input field. A selected text takes
/// the place of the query and is searched at once (see [`selection_query`] for `strict`).
pub fn find_in(pdf: &mut PdfTab, doc: &DocumentHandle, notifier: &Notifier, strict: bool) {
    let selected = pdf.ui.selection.active().and_then(|sel| {
        let raw = if strict {
            selection_text(
                &sel,
                |p| doc.page_text(p),
                refdesk_core::CopyOptions {
                    join_lines: false,
                    dehyphenate: false,
                },
            )
        } else {
            selection_text(&sel, |p| doc.page_text(p), COPY_OPTIONS)
        };
        selection_query(&raw, strict)
    });
    let anchor = pdf.view.anchor;
    let generation = doc.info().map(|i| i.generation);
    let search = &mut pdf.ui.search;
    if search.open(selected) {
        search.start(doc, notifier, anchor, generation);
    }
}

/// F3 and Shift+F3 for the tab `tab`: steps through the matches. Without a search it opens the
/// bar like Ctrl+F does.
pub fn step(state: &mut AppState, tab: TabId, forward: bool, ctx: &egui::Context) {
    let AppState { docs, tabs, .. } = state;
    let Some(Tab::Pdf(pdf)) = tabs.get_mut(tab) else {
        return;
    };
    let Some(doc) = docs.get(pdf.doc) else {
        return;
    };
    if pdf.ui.search.open && pdf.ui.search.has_query() {
        let now = ctx.input(|i| i.time);
        step_tab(pdf, forward, now);
    } else {
        find_in(pdf, doc, &crate::action::notifier(ctx), true);
    }
}

/// Ctrl+F (`from_menu` false) and "Search for This Text" (`from_menu` true) for the tab `tab`.
pub fn find(state: &mut AppState, tab: TabId, from_menu: bool, ctx: &egui::Context) {
    let AppState { docs, tabs, .. } = state;
    if let Some(Tab::Pdf(pdf)) = tabs.get_mut(tab)
        && let Some(doc) = docs.get(pdf.doc)
    {
        find_in(pdf, doc, &crate::action::notifier(ctx), !from_menu);
    }
}

// ---------------------------------------------------------------------------------------------
// Drawing: the search bar
// ---------------------------------------------------------------------------------------------

fn input_id(tab: TabId) -> Id {
    Id::unique(("find_input", tab))
}

/// Draws the search bar above the view of a PDF tab, when it is open, and handles its keys.
/// Typing, the options and Enter start and move the search; the view is moved from here (the
/// bar is drawn before the view, which picks the new position up in the same frame).
pub fn show_bar(ui: &mut Ui, pdf: &mut PdfTab, doc: &DocumentHandle, tab: TabId, focused: bool) {
    if !pdf.ui.search.open {
        return;
    }
    let ctx = ui.ctx().clone();
    let now = ui.input(|i| i.time);
    let generation = doc.info().map(|i| i.generation);
    let tick = pdf.ui.search.tick(
        doc,
        &crate::action::notifier(&ctx),
        generation,
        pdf.view.anchor,
        now,
    );
    if tick.reveal {
        reveal_current(pdf, Record::FirstDeparture);
    }
    if let Some(left) = tick.repaint_after {
        ctx.request_repaint_after(left);
    }

    let id = input_id(tab);
    let has_focus = ui.memory(|m| m.has_focus(id));
    // The keys are the bar's while the field has the keyboard, and while this view has it and no
    // other text field does.
    if has_focus || (focused && !ctx.text_edit_focused()) {
        handle_keys(ui, pdf, id, has_focus, now);
        if !pdf.ui.search.open {
            ui.memory_mut(|m| m.surrender_focus(id));
            return;
        }
    }

    let search = &mut pdf.ui.search;
    if std::mem::take(&mut search.focus_input) {
        let mut state = TextEdit::load_state(&ctx, id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::select_all_str(
                &search.query.text,
            )));
        TextEdit::store_state(&ctx, id, state);
        ui.memory_mut(|m| m.request_focus(id));
    }

    let mut list_toggled = false;
    let mut close = false;
    let mut edited = false;
    let mut options_changed = false;
    let mut step_dir = None;
    let no_match = search.no_match();
    let error = search.error().map(str::to_owned);
    let dark = ui.visuals().dark_mode;
    Frame::NONE
        .fill(ui.visuals().panel_fill)
        .inner_margin(4.0)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let mut field = TextEdit::singleline(&mut search.query.text)
                    .id(id)
                    .hint_text(tr("find.hint"))
                    .desired_width(240.0);
                if no_match || error.is_some() {
                    field = field.background_color(if dark {
                        Color32::from_rgb(92, 38, 38)
                    } else {
                        Color32::from_rgb(255, 208, 208)
                    });
                }
                if ui.add(field).changed() {
                    edited = true;
                }
                let count = count_text(search.current_idx, &search.snapshot, search.percent);
                if error.is_none() && search.has_query() {
                    let text = RichText::new(count);
                    ui.label(if no_match {
                        text.color(ui.visuals().error_fg_color)
                    } else {
                        text
                    });
                }
                if ui.button("↑").on_hover_text(tr("find.prev")).clicked() {
                    step_dir = Some(false);
                }
                if ui.button("↓").on_hover_text(tr("find.next")).clicked() {
                    step_dir = Some(true);
                }
                let toggles = [
                    ("Aa", &mut search.query.case_sensitive, "find.case"),
                    (
                        tr("find.word_label"),
                        &mut search.query.whole_word,
                        "find.word",
                    ),
                    (".*", &mut search.query.regex, "find.regex"),
                ];
                for (label, flag, hint) in toggles {
                    let on = *flag;
                    if ui
                        .selectable_label(on, label)
                        .on_hover_text(tr(hint))
                        .clicked()
                    {
                        *flag = !on;
                        options_changed = true;
                    }
                }
                if ui
                    .selectable_label(search.list_open, tr("find.list"))
                    .on_hover_text(tr("find.list_hint"))
                    .clicked()
                {
                    list_toggled = true;
                }
                if let Some((which, left)) = search.wrap_notice(now) {
                    ui.label(
                        RichText::new(match which {
                            Wrap::ToStart => tr("find.wrapped_start"),
                            Wrap::ToEnd => tr("find.wrapped_end"),
                        })
                        .color(ui.visuals().warn_fg_color),
                    );
                    ui.ctx().request_repaint_after(left);
                }
                if ui.button("×").on_hover_text(tr("find.close")).clicked() {
                    close = true;
                }
            });
            if let Some(message) = &error {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    tr("find.regex_error").replace("{}", message),
                );
            }
        });

    if edited {
        pdf.ui.search.edited(now);
        // The search starts when the delay is over; wake up then.
        ctx.request_repaint_after(Duration::from_secs_f64(INPUT_DELAY_SECS));
    }
    if options_changed {
        pdf.ui.search.options_changed();
        ctx.request_repaint();
    }
    if list_toggled {
        pdf.ui.search.list_open = !pdf.ui.search.list_open;
    }
    if let Some(forward) = step_dir {
        step_tab(pdf, forward, now);
    }
    if close {
        pdf.ui.search.close();
        ui.memory_mut(|m| m.surrender_focus(id));
    }
}

/// The keys of the search bar: Esc closes it, Enter and Shift+Enter step. While the field has the
/// keyboard F3, Shift+F3 step too and Ctrl+F selects the text of the field (the shortcut table
/// does not see keys while a text field has the keyboard).
fn handle_keys(ui: &mut Ui, pdf: &mut PdfTab, id: Id, in_field: bool, now: f64) {
    let ctx = ui.ctx().clone();
    // Shift is compared loosely, so the variant with Shift is asked first.
    let (escape, prev, next, find) = ui.input_mut(|i| {
        let escape = i.consume_key(Modifiers::NONE, Key::Escape);
        let prev = i.consume_key(Modifiers::SHIFT, Key::Enter)
            || (in_field && i.consume_key(Modifiers::SHIFT, Key::F3));
        let next = i.consume_key(Modifiers::NONE, Key::Enter)
            || (in_field && i.consume_key(Modifiers::NONE, Key::F3));
        let find = in_field && i.consume_key(Modifiers::CTRL, Key::F);
        (escape, prev, next, find)
    });
    if escape {
        pdf.ui.search.close();
        ctx.memory_mut(|m| m.surrender_focus(id));
    } else if prev {
        step_tab(pdf, false, now);
    } else if next {
        step_tab(pdf, true, now);
    } else if find {
        pdf.ui.search.focus_input = true;
        ctx.request_repaint();
    }
}

// ---------------------------------------------------------------------------------------------
// Drawing: the result list
// ---------------------------------------------------------------------------------------------

/// Draws the result list in a panel at the right edge of window `window`, when the focused tab
/// has one open (FIND-4).
pub fn show_results_panel(
    ui: &mut Ui,
    tabs: &mut TabStore,
    docs: &DocStore,
    window: WindowId,
    focus: Option<TabId>,
) {
    let Some(Tab::Pdf(pdf)) = focus.and_then(|id| tabs.get_mut(id)) else {
        return;
    };
    if !(pdf.ui.search.open && pdf.ui.search.list_open) {
        return;
    }
    let Some(doc) = docs.get(pdf.doc) else {
        return;
    };
    Panel::right(Id::unique(("search_results", window)))
        .default_size(DEFAULT_RESULTS_WIDTH)
        .size_range(RESULTS_WIDTH_RANGE.0..=RESULTS_WIDTH_RANGE.1)
        .show(ui, |ui| show_results(ui, pdf, doc));
}

/// What a click on a row asks for.
enum RowEvent {
    /// Go to this match.
    Go(usize),
}

fn show_results(ui: &mut Ui, pdf: &mut PdfTab, doc: &DocumentHandle) {
    let info = doc.info();
    let search = &mut pdf.ui.search;
    let total = search.snapshot.matches.len();
    ui.label(RichText::new(format!("{} ({total})", tr("find.results_title"))).strong());
    ui.separator();
    let Some(info) = info else {
        ui.label(RichText::new(tr("doc.loading")).weak());
        return;
    };
    let rows = search.rows(&info.outline, info.generation);
    if rows.is_empty() {
        let text = if search.no_match() {
            tr("find.none")
        } else if search.has_query() {
            ""
        } else {
            tr("find.results_empty")
        };
        ui.label(RichText::new(text).weak());
        return;
    }

    let snapshot = Arc::clone(&search.snapshot);
    let matches = &snapshot.matches;
    let row_h = ui.text_style_height(&TextStyle::Body) + 4.0;
    let stride = row_h + ui.spacing().item_spacing.y;
    let mut area = ScrollArea::vertical().auto_shrink([false, false]);
    let scroll = search.list_scroll;
    let in_panel = ui.rect_contains_pointer(ui.max_rect());
    // Rows that arrive above the one at the top must not make the list jump: the row that was at
    // the top stays there.
    let kept = scroll
        .top
        .and_then(|top| keep_top_offset(top, &rows, matches, stride));
    let follow = (search.list_follow && !in_panel)
        .then(|| {
            let row = row_of_match(&rows, matches, search.current_idx?)?;
            crate::panels::toc::scroll_target(
                row,
                stride,
                kept.unwrap_or(scroll.offset),
                scroll.view_h,
            )
        })
        .flatten();
    search.list_follow = false;
    if let Some(offset) = follow.or(kept.filter(|o| (o - scroll.offset).abs() > 0.5)) {
        area = area.vertical_scroll_offset(offset);
    }

    let current = search.current_idx;
    let mut events = Vec::new();
    let output = area.show_rows(ui, row_h, rows.len(), |ui, range| {
        for row in &rows[range] {
            show_row(
                ui,
                row_h,
                row,
                matches,
                current,
                &info.outline,
                &info.page_labels,
                &mut events,
            );
        }
    });
    let offset = output.state.clamped_offset().y;
    let top_row = (offset / stride).floor() as usize;
    search.list_scroll = ListScroll {
        offset,
        view_h: output.inner_rect.height(),
        top: (offset > 0.5)
            .then(|| {
                let row = rows.get(top_row)?;
                Some((row_key(row, matches), offset - top_row as f32 * stride))
            })
            .flatten(),
    };

    for event in events {
        match event {
            RowEvent::Go(index) => {
                go_to_result(pdf, index);
            }
        }
    }
}

/// Draws one row of the list and reports its click.
#[allow(clippy::too_many_arguments)]
fn show_row(
    ui: &mut Ui,
    row_h: f32,
    row: &ListRow,
    matches: &[SearchMatch],
    current: Option<usize>,
    outline: &Outline,
    labels: &[String],
    events: &mut Vec<RowEvent>,
) {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
    let visuals = ui.visuals().clone();
    let font = ui.style().text_styles[&TextStyle::Body].clone();
    let width = (rect.width() - 8.0).max(0.0);
    match *row {
        ListRow::Page {
            page,
            first,
            count,
            section,
        } => {
            let title = section
                .and_then(|i| outline.items.get(i as usize))
                .map(|item| item.title.as_str());
            let head = page_head_text(page, labels, title);
            let text = format!(
                "{head}  ({})",
                tr("find.match_count").replace("{n}", &count.to_string())
            );
            let job = line_job(&text, font, visuals.strong_text_color(), width);
            let galley = ui.painter().layout_job(job);
            ui.painter().rect_filled(rect, 2.0, visuals.faint_bg_color);
            let pos = Pos2::new(rect.left() + 4.0, rect.center().y - galley.size().y / 2.0);
            ui.painter()
                .galley(pos, galley, visuals.strong_text_color());
            if response.clicked() {
                events.push(RowEvent::Go(first as usize));
            }
        }
        ListRow::Match(index) => {
            let Some(m) = matches.get(index as usize) else {
                return;
            };
            let is_current = current == Some(index as usize);
            if is_current {
                ui.painter()
                    .rect_filled(rect, 2.0, visuals.selection.bg_fill);
            } else if response.hovered() {
                ui.painter()
                    .rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
            }
            let color = if is_current {
                visuals.selection.stroke.color
            } else {
                visuals.text_color()
            };
            let text = row_text(m);
            let job = match_job(
                &text,
                font,
                color,
                visuals.strong_text_color(),
                visuals.warn_fg_color.gamma_multiply(0.45),
                width,
            );
            let galley = ui.painter().layout_job(job);
            let pos = Pos2::new(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0);
            ui.painter().galley(pos, galley, color);
            if response.clicked() {
                events.push(RowEvent::Go(index as usize));
            }
        }
    }
}

fn wrapping(width: f32) -> TextWrapping {
    TextWrapping {
        max_width: width,
        max_rows: 1,
        break_anywhere: true,
        ..Default::default()
    }
}

/// A one-line text, shortened with "…" past `width`.
fn line_job(text: &str, font: egui::FontId, color: Color32, width: f32) -> LayoutJob {
    let mut job = LayoutJob {
        wrap: wrapping(width),
        ..Default::default()
    };
    job.append(&one_line(text), 0.0, TextFormat::simple(font, color));
    job
}

/// The line of a match: the text before, the match marked, the text after.
fn match_job(
    text: &RowText,
    font: egui::FontId,
    color: Color32,
    strong: Color32,
    highlight: Color32,
    width: f32,
) -> LayoutJob {
    let mut job = LayoutJob {
        wrap: wrapping(width),
        ..Default::default()
    };
    job.append(&text.before, 0.0, TextFormat::simple(font.clone(), color));
    job.append(
        &text.matched,
        0.0,
        TextFormat {
            background: highlight,
            ..TextFormat::simple(font.clone(), strong)
        },
    );
    job.append(&text.after, 0.0, TextFormat::simple(font, color));
    job
}

/// The colors of the highlights over the pages: all matches, and the current one with its border.
pub fn highlight_colors() -> (Color32, Color32, Stroke) {
    (
        Color32::from_rgba_unmultiplied(255, 214, 0, 96),
        Color32::from_rgba_unmultiplied(255, 120, 0, 150),
        Stroke::new(1.5, Color32::from_rgb(200, 80, 0)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use egui::{Event, Rect, Vec2};
    use refdesk_core::testutil::{PdfBuilder, TempPdf};
    use refdesk_core::{
        LayoutParams, MatchPart, OutlineNode, Selection, TextPos, ZoomMode, geom::Size,
    };

    use crate::action::{Action, OpenTarget, dispatch, dispatch_queued};
    use crate::commands::{self, MenuGroup};
    use crate::i18n::{current_lang, lookup};
    use crate::views::test_support::{Repaints, watch_repaints};
    use crate::windows::show_window;

    // --- builders of matches without a document --------------------------------------------

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> PageRect {
        PageRect::new(x0, y0, x1, y1)
    }

    /// A match on `page` whose first line is at height `y`; `start` is its first character.
    fn m(page: u32, start: u32, y: f32) -> SearchMatch {
        SearchMatch {
            page,
            chars: start..start + 3,
            rects: vec![rect(72.0, y, 100.0, y + 12.0)],
            cont: None,
            before: "before ".into(),
            matched: "abc".into(),
            after: " after".into(),
        }
    }

    fn at(page: u32, y: f32) -> Location {
        Location::new(page, 0.0, y)
    }

    fn snapshot(matches: Vec<SearchMatch>, finished: bool) -> SearchSnapshot {
        SearchSnapshot {
            matches,
            pages_done: 3,
            pages_total: 10,
            finished,
            truncated: false,
        }
    }

    fn template(key: &str) -> &'static str {
        lookup(current_lang(), key).expect("key exists")
    }

    // --- selected text as a query ------------------------------------------------------------

    #[test]
    fn ctrl_f_takes_a_selection_of_one_short_line_only() {
        assert_eq!(
            selection_query("  SWS_EM_01004 ", true).as_deref(),
            Some("SWS_EM_01004")
        );
        assert_eq!(selection_query("two\nlines", true), None);
        assert_eq!(selection_query("   ", true), None);
        let long = "x".repeat(MAX_SELECTION_CHARS + 1);
        assert_eq!(selection_query(&long, true), None);
        let limit = "x".repeat(MAX_SELECTION_CHARS);
        assert_eq!(
            selection_query(&limit, true).as_deref(),
            Some(limit.as_str())
        );
    }

    #[test]
    fn the_menu_entry_joins_lines_and_cuts_long_text() {
        assert_eq!(
            selection_query("two\nlines  here", false).as_deref(),
            Some("two lines here")
        );
        let long = "x".repeat(MAX_SELECTION_CHARS * 2);
        assert_eq!(
            selection_query(&long, false).map(|s| s.chars().count()),
            Some(MAX_SELECTION_CHARS)
        );
        assert_eq!(selection_query(" \n ", false), None);
    }

    // --- the current match -------------------------------------------------------------------

    #[test]
    fn the_first_match_at_or_after_a_place_is_found() {
        let ms = vec![
            m(1, 0, 100.0),
            m(3, 0, 100.0),
            m(3, 50, 400.0),
            m(6, 0, 100.0),
        ];
        assert_eq!(next_from(&ms, at(0, 0.0), false), Some(0));
        assert_eq!(next_from(&ms, at(1, 100.0), false), Some(0));
        assert_eq!(next_from(&ms, at(1, 100.5), false), Some(1));
        assert_eq!(next_from(&ms, at(3, 200.0), false), Some(2));
        assert_eq!(next_from(&ms, at(4, 0.0), false), Some(3));
        // Past the last match: nothing yet, or the first one once the search is complete.
        assert_eq!(next_from(&ms, at(7, 0.0), false), None);
        assert_eq!(next_from(&ms, at(7, 0.0), true), Some(0));
        assert_eq!(next_from(&[], at(0, 0.0), true), None);
    }

    #[test]
    fn the_previous_match_is_the_last_one_before_a_place() {
        let ms = vec![
            m(1, 0, 100.0),
            m(3, 0, 100.0),
            m(3, 50, 400.0),
            m(6, 0, 100.0),
        ];
        assert_eq!(prev_from(&ms, at(6, 0.0), false), Some(2));
        assert_eq!(prev_from(&ms, at(3, 100.0), false), Some(0));
        assert_eq!(prev_from(&ms, at(1, 100.0), false), None);
        assert_eq!(prev_from(&ms, at(1, 100.0), true), Some(3));
        assert_eq!(prev_from(&[], at(0, 0.0), true), None);
    }

    #[test]
    fn stepping_wraps_around_only_when_the_search_is_complete() {
        assert_eq!(
            step_index(3, 0, true, false),
            Some(Step {
                index: 1,
                wrapped: false
            })
        );
        assert_eq!(
            step_index(3, 2, true, true),
            Some(Step {
                index: 0,
                wrapped: true
            })
        );
        assert_eq!(
            step_index(3, 0, false, true),
            Some(Step {
                index: 2,
                wrapped: true
            })
        );
        assert_eq!(
            step_index(3, 1, false, true),
            Some(Step {
                index: 0,
                wrapped: false
            })
        );
        // Later matches may still come: the end is not the end yet.
        assert_eq!(step_index(3, 2, true, false), None);
        assert_eq!(step_index(3, 0, false, false), None);
        assert_eq!(step_index(0, 0, true, true), None);
        // A single match steps to itself, wrapping.
        assert_eq!(
            step_index(1, 0, true, true),
            Some(Step {
                index: 0,
                wrapped: true
            })
        );
    }

    #[test]
    fn a_match_is_found_again_by_its_key_when_matches_come_in_before_it() {
        let later = vec![m(3, 0, 100.0), m(5, 20, 100.0)];
        let key = match_key(&later[1]);
        assert_eq!(find_key(&later, key), Some(1));
        let more = vec![
            m(1, 0, 100.0),
            m(2, 0, 100.0),
            m(3, 0, 100.0),
            m(5, 20, 100.0),
        ];
        assert_eq!(find_key(&more, key), Some(3));
        assert_eq!(find_key(&more, (4, 0)), None);
    }

    // --- scrolling to a match ----------------------------------------------------------------

    fn layout(zoom: f32) -> PageLayout {
        let sizes = vec![Size::new(595.0, 842.0); 10];
        PageLayout::new(
            &sizes,
            LayoutParams {
                viewport: Size::new(800.0, 600.0),
                zoom: ZoomMode::Custom(zoom),
                gap: 8.0,
                margin: 12.0,
                fit_page: 0,
            },
        )
    }

    const VIEWPORT: Size = Size { w: 800.0, h: 600.0 };

    #[test]
    fn a_match_outside_the_view_goes_a_third_below_the_top_edge() {
        let layout = layout(1.0);
        let r = rect(72.0, 300.0, 100.0, 312.0);
        let to = reveal_scroll(&layout, Point::new(0.0, 0.0), VIEWPORT, 5, r).expect("moves");
        let top = layout.page_rect(5).y0 + 300.0 * layout.scale();
        assert!((top - to.y - VIEWPORT.h / 3.0).abs() < 0.01, "{to:?} {top}");
    }

    #[test]
    fn a_match_that_is_comfortably_in_view_does_not_move_the_view() {
        let layout = layout(1.0);
        let scroll = Point::new(0.0, 0.0);
        // Page 0, a line at 200 pt: 12 + 267 px, inside the view with margins.
        assert_eq!(
            reveal_scroll(
                &layout,
                scroll,
                VIEWPORT,
                0,
                rect(72.0, 200.0, 100.0, 212.0)
            ),
            None
        );
        // Five pixels below the top edge of the view: visible, but not comfortably.
        let scroll = Point::new(0.0, 600.0);
        let y_pt = (605.0 - layout.page_rect(0).y0) / layout.scale();
        let near_edge = rect(72.0, y_pt, 100.0, y_pt + 9.0);
        let to = reveal_scroll(&layout, scroll, VIEWPORT, 0, near_edge).expect("moves");
        assert!((to.y - (605.0 - VIEWPORT.h / 3.0)).abs() < 0.01, "{to:?}");
        // A match that is just past the bottom edge.
        let below = rect(
            72.0,
            y_pt + 600.0 / layout.scale(),
            100.0,
            y_pt + 612.0 / layout.scale(),
        );
        assert!(reveal_scroll(&layout, scroll, VIEWPORT, 0, below).is_some());
        // A page that does not exist is nothing to show.
        assert_eq!(
            reveal_scroll(&layout, scroll, VIEWPORT, 99, near_edge),
            None
        );
    }

    #[test]
    fn a_match_off_to_the_side_scrolls_sideways_too() {
        let layout = layout(3.0);
        assert!(layout.max_scroll(VIEWPORT).x > 0.0);
        let r = rect(300.0, 100.0, 360.0, 112.0);
        let to = reveal_scroll(&layout, Point::new(0.0, 0.0), VIEWPORT, 0, r).expect("moves");
        let left = layout.page_rect(0).x0 + 300.0 * layout.scale();
        assert!((left - to.x - VIEWPORT.w / 3.0).abs() < 0.01);
    }

    // --- highlights --------------------------------------------------------------------------

    #[test]
    fn the_index_lists_the_matches_of_each_page_and_the_ones_that_run_on() {
        let mut across = m(2, 40, 700.0);
        across.cont = Some(MatchPart {
            page: 3,
            chars: 0..4,
            rects: vec![rect(72.0, 50.0, 90.0, 62.0)],
        });
        let ms = vec![m(0, 0, 100.0), m(2, 0, 100.0), across, m(3, 0, 300.0)];
        let index = PageIndex::build(&ms, 5);
        assert_eq!(index.starting_on(0), 0..1);
        assert_eq!(index.starting_on(1), 0..0);
        assert_eq!(index.starting_on(2), 1..3);
        assert_eq!(index.starting_on(3), 3..4);
        assert_eq!(index.starting_on(4), 0..0);
        assert_eq!(index.starting_on(99), 0..0);
        assert_eq!(index.continuing_on(3), &[2]);
        assert!(index.continuing_on(2).is_empty());
    }

    #[test]
    fn only_the_rectangles_of_the_visible_pages_are_made_and_a_split_match_is_on_both_pages() {
        let layout = layout(1.0);
        let mut across = m(2, 40, 780.0);
        across.cont = Some(MatchPart {
            page: 3,
            chars: 0..4,
            rects: vec![rect(72.0, 50.0, 90.0, 62.0)],
        });
        let ms = vec![
            m(0, 0, 100.0),
            m(2, 0, 100.0),
            across,
            m(3, 0, 300.0),
            m(8, 0, 100.0),
        ];
        let index = PageIndex::build(&ms, 10);

        // Scrolled so that the bottom of page 2 and the top of page 3 are in view.
        let boundary = layout.page_rect(3).y0;
        let scroll = Point::new(0.0, boundary - 300.0);
        let rects = match_rects(&index, &ms, Some(2), &layout, scroll, VIEWPORT);
        // Page 2: matches 1 and 2, page 3: the continuation of 2 and match 3. Nothing of 0 or 4.
        assert_eq!(rects.len(), 4, "{rects:?}");
        assert_eq!(
            rects.iter().filter(|r| r.current).count(),
            2,
            "both halves of the current match"
        );

        // The rectangle of match 1 lies where its text is: page space → content → viewport.
        let s = layout.scale();
        let o = layout.page_rect(2);
        let expect = rect(
            o.x0 + 72.0 * s - scroll.x,
            o.y0 + 100.0 * s - scroll.y,
            o.x0 + 100.0 * s - scroll.x,
            o.y0 + 112.0 * s - scroll.y,
        );
        let found = rects
            .iter()
            .find(|r| (r.rect.y0 - expect.y0).abs() < 0.01 && !r.current)
            .expect("rectangle of match 1");
        assert_eq!(found.rect, expect);
        // The continuation is drawn on page 3, at its own place.
        let o3 = layout.page_rect(3);
        let cont_y = o3.y0 + 50.0 * s - scroll.y;
        assert!(
            rects
                .iter()
                .any(|r| r.current && (r.rect.y0 - cont_y).abs() < 0.01)
        );
    }

    #[test]
    fn nothing_is_highlighted_while_the_bar_is_closed() {
        let layout = layout(1.0);
        let ms = vec![m(0, 0, 100.0)];
        let mut state = SearchState {
            snapshot: Arc::new(snapshot(ms.clone(), true)),
            index: PageIndex::build(&ms, 10),
            ..SearchState::default()
        };
        let scroll = Point::new(0.0, 0.0);
        assert!(state.highlights(&layout, scroll, VIEWPORT).is_empty());
        state.open = true;
        assert_eq!(state.highlights(&layout, scroll, VIEWPORT).len(), 1);
        state.close();
        assert!(state.highlights(&layout, scroll, VIEWPORT).is_empty());
    }

    // --- the count text and the input delay --------------------------------------------------

    #[test]
    fn the_count_text_covers_searching_done_nothing_and_the_limit() {
        let ms: Vec<SearchMatch> = (0..42).map(|i| m(i / 5, i, 100.0)).collect();
        let done = snapshot(ms.clone(), true);
        assert_eq!(count_text(Some(2), &done, None), "3 / 42");
        assert_eq!(count_text(None, &done, None), "– / 42");

        let running = snapshot(ms, false);
        assert_eq!(
            count_text(Some(2), &running, Some(37)),
            template("find.count_searching")
                .replace("{n}", "3")
                .replace("{total}", "42")
                .replace("{percent}", "37")
        );
        assert!(count_text(Some(2), &running, Some(37)).contains("37%"));

        let none = snapshot(Vec::new(), true);
        assert_eq!(count_text(None, &none, None), template("find.none"));
        // Nothing yet, but still looking: not "no match".
        let looking = snapshot(Vec::new(), false);
        assert_ne!(count_text(None, &looking, Some(5)), template("find.none"));

        let many: Vec<SearchMatch> = (0..refdesk_core::search::MAX_MATCHES as u32)
            .map(|i| m(i / 50, i % 50, 100.0))
            .collect();
        let capped = SearchSnapshot {
            truncated: true,
            ..snapshot(many, true)
        };
        assert_eq!(count_text(Some(0), &capped, None), "1 / 100000+");
    }

    #[test]
    fn a_typed_query_is_due_after_the_input_delay() {
        assert!(input_delay_left(1.0, 1.05).is_some());
        let left = input_delay_left(1.0, 1.05).expect("not yet");
        assert!((left.as_secs_f64() - 0.10).abs() < 1e-6);
        assert!(input_delay_left(1.0, 1.16).is_none());
        assert!(input_delay_left(1.0, 2.0).is_none());
    }

    // --- the result list ---------------------------------------------------------------------

    fn node(title: &str, page: u32, y: f32) -> OutlineNode {
        OutlineNode {
            title: title.to_string(),
            target: Some(Location::new(page, 0.0, y)),
            uri: None,
            children: Vec::new(),
        }
    }

    #[test]
    fn the_list_groups_matches_by_page_with_counts_and_sections() {
        let outline = Outline::from_tree(vec![node("1 Intro", 0, 50.0), node("2 Design", 3, 50.0)]);
        let ms = vec![
            m(1, 0, 100.0),
            m(1, 20, 200.0),
            m(3, 0, 300.0),
            m(7, 5, 100.0),
        ];
        let rows = list_rows(&ms, &outline);
        assert_eq!(
            rows,
            vec![
                ListRow::Page {
                    page: 1,
                    first: 0,
                    count: 2,
                    section: Some(0)
                },
                ListRow::Match(0),
                ListRow::Match(1),
                ListRow::Page {
                    page: 3,
                    first: 2,
                    count: 1,
                    section: Some(1)
                },
                ListRow::Match(2),
                ListRow::Page {
                    page: 7,
                    first: 3,
                    count: 1,
                    section: Some(1)
                },
                ListRow::Match(3),
            ]
        );
        // The row of the current match.
        assert_eq!(row_of_match(&rows, &ms, 0), Some(1));
        assert_eq!(row_of_match(&rows, &ms, 2), Some(4));
        assert_eq!(row_of_match(&rows, &ms, 3), Some(6));
        assert_eq!(row_of_match(&rows, &ms, 9), None);
        assert!(list_rows(&[], &outline).is_empty());
    }

    #[test]
    fn a_page_head_names_the_page_and_the_section() {
        let labels = vec!["i".to_string(), "ii".to_string()];
        assert_eq!(
            page_head_text(11, &[], Some("2 Design")),
            template("link.page_title")
                .replace("{page}", "12")
                .replace("{title}", "2 Design")
        );
        assert_eq!(
            page_head_text(11, &[], None),
            template("link.page").replace("{page}", "12")
        );
        // Without an outline there is no section; a blank title counts as none.
        assert_eq!(
            page_head_text(1, &labels, Some("  ")),
            template("link.page").replace("{page}", "ii")
        );
        let head = page_head_text(11, &[], Some("2 Design"));
        assert!(head.contains("12") && head.contains("2 Design"));
    }

    #[test]
    fn a_line_has_the_text_before_the_match_and_after_it() {
        let mut one = m(0, 0, 100.0);
        one.before = "the quick brown fox jumps over the lazy dog and ".into();
        one.matched = "needle".into();
        one.after = " in\na\thaystack".into();
        let t = row_text(&one);
        assert!(t.before.starts_with('…') && t.before.ends_with("dog and "));
        assert_eq!(t.before.chars().count(), BEFORE_CHARS + 1);
        assert_eq!(t.matched, "needle");
        assert_eq!(t.after, " in a haystack");
        // Short text before the match is kept whole.
        assert_eq!(row_text(&m(0, 0, 1.0)).before, "before ");
    }

    #[test]
    fn rows_that_arrive_above_the_top_row_keep_it_where_it_was() {
        let outline = Outline::default();
        let late: Vec<SearchMatch> = (0..5).map(|i| m(5, i * 10, 100.0 + i as f32)).collect();
        let rows = list_rows(&late, &outline);
        // The row at the top is match 2, scrolled out by 7 px.
        let top = (row_key(&rows[3], &late), 7.0);
        let stride = 20.0;
        assert_eq!(
            keep_top_offset(top, &rows, &late, stride),
            Some(3.0 * stride + 7.0)
        );

        // Matches of earlier pages arrive (the search wrapped around).
        let mut all: Vec<SearchMatch> = (0..4).map(|i| m(1, i * 10, 100.0 + i as f32)).collect();
        all.extend(late.iter().cloned());
        let rows2 = list_rows(&all, &outline);
        let offset = keep_top_offset(top, &rows2, &all, stride).expect("still there");
        let row = (offset / stride).floor() as usize;
        assert_eq!(row_key(&rows2[row], &all), top.0);
        // A row that is gone cannot be kept.
        assert_eq!(keep_top_offset(top, &rows2[..2], &all[..1], stride), None);
    }

    #[test]
    fn twenty_thousand_matches_make_their_rows_quickly() {
        let outline = Outline::from_tree(
            (0..400)
                .map(|i| node(&format!("{i} Section"), i * 2, 50.0))
                .collect(),
        );
        let ms: Vec<SearchMatch> = (0..20_000u32)
            .map(|i| m(i / 25, i % 25 * 10, 50.0 + (i % 25) as f32 * 20.0))
            .collect();
        let t = Instant::now();
        let rows = list_rows(&ms, &outline);
        let rows_took = t.elapsed();
        let t = Instant::now();
        let texts: usize = ms.iter().map(|x| row_text(x).matched.len()).sum();
        let texts_took = t.elapsed();
        let t = Instant::now();
        let index = PageIndex::build(&ms, 800);
        let index_took = t.elapsed();
        eprintln!(
            "WP-53 timing for {} matches: list_rows {rows_took:?} ({} rows), row_text of all {texts_took:?}, \
             page index {index_took:?}",
            ms.len(),
            rows.len()
        );
        assert_eq!(rows.len(), 20_000 + 800);
        assert!(texts > 0 && index.starting_on(799).len() == 25);
        // Generous: the point is that nothing is quadratic.
        let limit = Duration::from_secs(1);
        assert!(rows_took < limit && texts_took < limit && index_took < limit);
    }

    // --- commands ----------------------------------------------------------------------------

    #[test]
    fn the_find_commands_are_in_the_edit_menu_with_their_keys() {
        let edit: Vec<&str> = commands::in_menu(MenuGroup::Edit).map(|c| c.id).collect();
        for id in ["edit.find", "edit.find_next", "edit.find_prev"] {
            assert!(edit.contains(&id), "{id} is in the Edit menu");
        }
        let key = |id: &str| commands::command(id).and_then(|c| c.shortcut).expect(id);
        assert_eq!(
            key("edit.find"),
            egui::KeyboardShortcut::new(Modifiers::CTRL, Key::F)
        );
        assert_eq!(
            key("edit.find_next"),
            egui::KeyboardShortcut::new(Modifiers::NONE, Key::F3)
        );
        assert_eq!(
            key("edit.find_prev"),
            egui::KeyboardShortcut::new(Modifiers::SHIFT, Key::F3)
        );
        for entry in commands::SELECTION_MENU {
            assert!(lookup(current_lang(), entry.name_key).is_some());
        }
        assert!(
            commands::SELECTION_MENU
                .iter()
                .any(|e| e.name_key == "cmd.edit.search_selection")
        );
    }

    // --- with a document ---------------------------------------------------------------------

    /// 8 pages with a heading; "a needle here" at the given (page, y) places.
    fn needle_doc(stem: &str, places: &[(usize, f32)]) -> TempPdf {
        let mut b = PdfBuilder::new();
        for page in 0..8usize {
            b = b.page(595.0, 842.0, |p| {
                p.text(72.0, 60.0, 12.0, &format!("heading {page}"));
                for (at_page, y) in places {
                    if *at_page == page {
                        p.text(72.0, *y, 12.0, "a needle here");
                    }
                }
            });
        }
        b.write_temp(stem)
    }

    /// What a search job tells through its notifier, as a channel. A `Context` swallows repaint
    /// requests that come while one is pending and no frame ran, which would make a wait on
    /// them hang when the test does not run frames.
    struct Pump {
        rx: std::sync::mpsc::Receiver<()>,
        notifier: Notifier,
    }

    impl Pump {
        fn new() -> Pump {
            let (tx, rx) = std::sync::mpsc::channel();
            // `Sender` is not `Sync`; the notifier must be.
            let tx = std::sync::Mutex::new(tx);
            let notifier: Notifier = Arc::new(move || {
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send(());
                }
            });
            Pump { rx, notifier }
        }

        /// Calls `check` until it holds, after each notification. Panics after the deadline.
        fn wait_until(&self, what: &str, mut check: impl FnMut() -> bool) {
            let deadline = Instant::now() + crate::views::test_support::TIMEOUT;
            while !check() {
                let left = deadline.saturating_duration_since(Instant::now());
                if self.rx.recv_timeout(left).is_err() {
                    panic!("timed out waiting for {what}");
                }
            }
        }
    }

    struct Rig {
        ctx: egui::Context,
        pump: Pump,
        repaints: Repaints,
        state: AppState,
        time: f64,
    }

    impl Rig {
        fn new() -> Rig {
            let ctx = egui::Context::default();
            let repaints = watch_repaints(&ctx);
            Rig {
                ctx,
                pump: Pump::new(),
                repaints,
                state: AppState::new(),
                time: 0.0,
            }
        }

        /// Opens `pdf`, waits for its pages and runs frames until the view is laid out.
        fn open(&mut self, pdf: &TempPdf) -> TabId {
            let target = OpenTarget {
                path: pdf.path().to_path_buf(),
                page: None,
            };
            dispatch(&mut self.state, Action::OpenFiles(vec![target]), &self.ctx);
            let tab = self.state.focus.expect("focused");
            let doc = self.doc(tab);
            self.repaints
                .wait_until("the document info", || doc.info().is_some());
            self.frames_until("the layout", |rig| rig.pdf(tab).ui.layout().is_some());
            // 100 %: pages are taller than the view, so matches lie off screen.
            dispatch(
                &mut self.state,
                Action::SetZoom {
                    tab,
                    zoom: ZoomMode::Custom(1.0),
                },
                &self.ctx,
            );
            self.frames(2);
            tab
        }

        fn doc(&self, tab: TabId) -> DocumentHandle {
            let doc = self.state.tabs.pdf(tab).expect("pdf tab").doc;
            self.state.docs.get(doc).expect("doc").clone()
        }

        fn pdf(&self, tab: TabId) -> &PdfTab {
            self.state.tabs.pdf(tab).expect("pdf tab")
        }

        fn search(&self, tab: TabId) -> &SearchState {
            &self.pdf(tab).ui.search
        }

        /// One frame of the window: shortcuts, the window, then the actions it queued.
        fn frame(&mut self, events: Vec<Event>) {
            self.time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1000.0, 700.0),
                )),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let state = &mut self.state;
            let mut queue = Vec::new();
            self.ctx
                .run_ui(input, |ui| {
                    queue.extend(commands::handle_shortcuts(ui.ctx(), state));
                    show_window(ui, state, 0, &mut queue);
                })
                .drop_without_applying_deltas();
            self.state.actions.append(&mut queue);
            dispatch_queued(&mut self.state, &self.ctx);
        }

        fn frames(&mut self, n: usize) {
            for _ in 0..n {
                self.frame(Vec::new());
            }
        }

        fn frames_until(&mut self, what: &str, mut check: impl FnMut(&Rig) -> bool) {
            let deadline = Instant::now() + crate::views::test_support::TIMEOUT;
            loop {
                self.frame(Vec::new());
                if check(self) {
                    return;
                }
                self.repaints.wait_one(what, deadline);
            }
        }

        fn key(&mut self, key: Key, modifiers: Modifiers) {
            self.frame(vec![
                Event::ModifiersChanged(modifiers),
                Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
            ]);
            self.frame(vec![Event::ModifiersChanged(Modifiers::NONE)]);
        }

        fn type_text(&mut self, text: &str) {
            self.frame(vec![Event::Text(text.to_owned())]);
        }

        /// Opens the bar with Ctrl+F and waits until its field has the keyboard.
        fn open_bar(&mut self, tab: TabId) {
            self.key(Key::F, Modifiers::CTRL);
            self.frames(2);
            assert!(self.search(tab).open, "Ctrl+F opened the bar");
            assert!(self.ctx.text_edit_focused(), "the field has the keyboard");
        }

        /// Types a query and runs frames until the search finished and chose a current match.
        fn search_for(&mut self, tab: TabId, text: &str) {
            self.type_text(text);
            self.frames_until("the search to finish", |rig| {
                let s = rig.search(tab);
                s.live && s.snapshot().finished && !s.is_searching()
            });
        }
    }

    fn generation_of(doc: &DocumentHandle) -> u32 {
        doc.info()
            .map(|i| i.generation)
            .expect("the document is open")
    }

    fn state_on(rig: &Rig) -> (SearchState, Notifier) {
        (SearchState::default(), rig.pump.notifier.clone())
    }

    /// Ticks until the job has finished and its matches are shown.
    fn finish(
        st: &mut SearchState,
        rig: &Rig,
        doc: &DocumentHandle,
        notifier: &Notifier,
        anchor: Location,
        now: f64,
    ) {
        rig.pump.wait_until("the search to finish", || {
            st.tick(doc, notifier, doc.info().map(|i| i.generation), anchor, now);
            st.live && st.snapshot().finished
        });
    }

    #[test]
    fn typing_starts_the_search_and_the_results_come_into_the_tab() {
        let pdf = needle_doc(
            "wp53-typing",
            &[(1, 100.0), (3, 300.0), (3, 500.0), (6, 100.0)],
        );
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.open_bar(tab);
        assert_eq!(rig.search(tab).starts, 0, "an empty field searches nothing");
        rig.search_for(tab, "needle");
        let s = rig.search(tab);
        assert_eq!(s.query.text, "needle");
        assert_eq!(s.starts, 1);
        assert_eq!(s.snapshot().matches.len(), 4);
        assert!(s.current_index().is_some(), "a current match was chosen");
        assert_eq!(
            s.snapshot()
                .matches
                .iter()
                .map(|x| x.page)
                .collect::<Vec<_>>(),
            vec![1, 3, 3, 6]
        );
    }

    #[test]
    fn a_new_query_cancels_the_old_search_and_replaces_its_results() {
        let pdf = needle_doc("wp53-replace", &[(1, 100.0), (3, 300.0)]);
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        let doc = rig.doc(tab);
        let (mut st, notifier) = state_on(&rig);
        st.query.text = "needle".into();
        st.start(&doc, &notifier, at(0, 0.0), Some(generation_of(&doc)));
        let first_job = Arc::downgrade(st.job.as_ref().expect("job"));
        finish(&mut st, &rig, &doc, &notifier, at(0, 0.0), 0.0);
        assert_eq!(st.snapshot().matches.len(), 2);

        st.query.text = "heading".into();
        st.start(&doc, &notifier, at(0, 0.0), Some(generation_of(&doc)));
        assert!(
            first_job.upgrade().is_none(),
            "the first job was dropped, which cancels it"
        );
        assert_eq!(st.starts, 2);
        // Until the new job has something, the old matches stay on screen.
        assert!(!st.live);
        assert_eq!(st.snapshot().matches.len(), 2);
        rig.pump.wait_until("the second search", || {
            st.tick(&doc, &notifier, Some(generation_of(&doc)), at(0, 0.0), 0.0);
            st.live && st.snapshot().finished
        });
        assert_eq!(st.snapshot().matches.len(), 8, "one heading per page");
        assert!(
            st.snapshot()
                .matches
                .iter()
                .all(|x| x.matched.to_lowercase() == "heading")
        );
    }

    #[test]
    fn typing_within_the_delay_starts_one_search() {
        let pdf = needle_doc("wp53-delay", &[(1, 100.0)]);
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        let doc = rig.doc(tab);
        let (mut st, notifier) = state_on(&rig);
        let generation = Some(generation_of(&doc));
        st.query.text = "n".into();
        st.edited(10.00);
        assert!(
            st.tick(&doc, &notifier, generation, at(0, 0.0), 10.05)
                .repaint_after
                .is_some()
        );
        st.query.text = "ne".into();
        st.edited(10.05);
        st.tick(&doc, &notifier, generation, at(0, 0.0), 10.10);
        st.query.text = "nee".into();
        st.edited(10.10);
        let wait = st
            .tick(&doc, &notifier, generation, at(0, 0.0), 10.20)
            .repaint_after;
        assert_eq!(st.starts, 0, "still within the delay of the last keystroke");
        assert!(wait.is_some_and(|w| w.as_secs_f64() <= INPUT_DELAY_SECS));
        st.tick(&doc, &notifier, generation, at(0, 0.0), 10.26);
        assert_eq!(st.starts, 1);
        assert_eq!(st.running.as_ref().map(|q| q.text.as_str()), Some("nee"));
        // Nothing more is due.
        st.tick(&doc, &notifier, generation, at(0, 0.0), 11.0);
        assert_eq!(st.starts, 1);
    }

    #[test]
    fn the_first_match_after_the_viewed_page_becomes_the_current_one() {
        let pdf = needle_doc("wp53-current", &[(1, 100.0), (3, 300.0), (6, 100.0)]);
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        let doc = rig.doc(tab);
        let (mut st, notifier) = state_on(&rig);
        st.query.text = "needle".into();
        // Reading page 5 (index 4): the search begins there and goes on to the end.
        let anchor = at(4, 0.0);
        st.start(&doc, &notifier, anchor, Some(generation_of(&doc)));
        let mut revealed = false;
        rig.pump.wait_until("the current match", || {
            revealed |= st
                .tick(&doc, &notifier, Some(generation_of(&doc)), anchor, 0.0)
                .reveal;
            st.current_index().is_some()
        });
        assert!(revealed, "the view is asked to show it");
        assert_eq!(st.current_match().map(|x| x.page), Some(6));
        finish(&mut st, &rig, &doc, &notifier, anchor, 0.0);
        assert_eq!(
            st.current_match().map(|x| x.page),
            Some(6),
            "it stays when matches before it arrive"
        );
        assert_eq!(st.snapshot().matches.len(), 3);
        assert_eq!(st.current_index(), Some(2));

        // Next wraps to the first match of the document and says so; previous wraps back.
        assert_eq!(st.step(true, anchor, 5.0), Some(0));
        assert_eq!(st.wrap_notice(5.5).map(|n| n.0), Some(Wrap::ToStart));
        assert_eq!(st.wrap_notice(5.0 + WRAP_NOTICE_SECS + 0.1), None);
        assert_eq!(st.step(true, anchor, 6.0), Some(1));
        assert_eq!(st.step(false, anchor, 7.0), Some(0));
        assert_eq!(st.step(false, anchor, 8.0), Some(2));
        assert_eq!(st.wrap_notice(8.1).map(|n| n.0), Some(Wrap::ToEnd));
        assert_eq!(st.current_match().map(|x| x.page), Some(6));
        assert!(st.list_follow, "the list scrolls to the new current match");
    }

    #[test]
    fn a_match_off_screen_is_shown_a_third_down_and_one_in_view_does_not_move_the_view() {
        let pdf = needle_doc("wp53-reveal", &[(0, 150.0), (4, 300.0)]);
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        let start = rig.pdf(tab).view.anchor;
        rig.open_bar(tab);
        rig.search_for(tab, "needle");
        rig.frames(2);
        // The first match is on page 0, in view: the view stayed.
        assert_eq!(rig.search(tab).current_match().map(|x| x.page), Some(0));
        assert_eq!(rig.pdf(tab).view.anchor, start, "the view stayed");
        assert_eq!(
            rig.pdf(tab).view.history.len(),
            1,
            "no move, no history entry"
        );

        // Next goes to page 4, which is far below: its line is a third down from the top.
        rig.key(Key::Enter, Modifiers::NONE);
        rig.frames(2);
        let pdf_tab = rig.pdf(tab);
        let current = rig.search(tab).current_match().expect("current");
        assert_eq!(current.page, 4);
        let layout = pdf_tab.ui.layout().expect("layout");
        let line = layout.page_rect(4).y0 + current.rects[0].y0 * layout.scale();
        let on_screen = line - pdf_tab.ui.scroll().y;
        let third = pdf_tab.ui.viewport().h / 3.0;
        assert!(
            (on_screen - third).abs() < 1.5,
            "line at {on_screen}, a third is {third}"
        );

        // Another look at the same match (F3 on a one-match search wraps to it): in view, so the
        // view does not move.
        let before = rig.pdf(tab).ui.scroll();
        let Some(Tab::Pdf(pdf_tab)) = rig.state.tabs.get_mut(tab) else {
            panic!("pdf tab");
        };
        assert!(!reveal_current(pdf_tab, Record::FirstDeparture));
        assert_eq!(rig.pdf(tab).ui.scroll(), before);
    }

    #[test]
    fn many_steps_make_one_history_entry_and_a_click_in_the_list_makes_another() {
        let places = [
            (2, 100.0),
            (3, 100.0),
            (4, 100.0),
            (5, 100.0),
            (6, 100.0),
            (7, 100.0),
        ];
        let pdf = needle_doc("wp53-history", &places);
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        let start = rig.pdf(tab).view.anchor;
        rig.open_bar(tab);
        rig.search_for(tab, "needle");
        rig.frames(2);
        // Searching moved the view to the first match (page 2): one entry.
        assert_eq!(rig.search(tab).current_match().map(|x| x.page), Some(2));
        assert_eq!(rig.pdf(tab).view.history.len(), 2);
        for _ in 0..5 {
            rig.key(Key::Enter, Modifiers::NONE);
            rig.frames(2);
        }
        assert_eq!(rig.search(tab).current_match().map(|x| x.page), Some(7));
        assert_eq!(rig.pdf(tab).view.history.len(), 2, "stepping adds nothing");
        // One step back is where the search began.
        dispatch(&mut rig.state, Action::Back(tab), &rig.ctx);
        rig.frames(2);
        let back = rig.pdf(tab).view.anchor;
        assert_eq!((back.page, back.y.round()), (start.page, start.y.round()));
        assert!(!rig.pdf(tab).view.history.can_back());

        // A click on a row of the list goes far away: a long jump with its own entry.
        dispatch(&mut rig.state, Action::Forward(tab), &rig.ctx);
        rig.frames(2);
        let len = rig.pdf(tab).view.history.len();
        let Some(Tab::Pdf(pdf_tab)) = rig.state.tabs.get_mut(tab) else {
            panic!("pdf tab");
        };
        assert!(go_to_result(pdf_tab, 0));
        assert_eq!(rig.pdf(tab).view.history.len(), len + 1);
        assert_eq!(rig.search(tab).current_index(), Some(0));
        // The match is a third down from the top edge: the top edge is above its page.
        assert!((1..=2).contains(&rig.pdf(tab).view.anchor.page));
        // Going back from there returns to the place the click left.
        rig.frames(2);
        dispatch(&mut rig.state, Action::Back(tab), &rig.ctx);
        assert!(
            rig.pdf(tab).view.anchor.page >= 6,
            "{:?}",
            rig.pdf(tab).view.anchor
        );
    }

    #[test]
    fn the_options_change_the_results_and_a_bad_regex_is_an_error() {
        let pdf = {
            let mut b = PdfBuilder::new();
            b = b.page(595.0, 842.0, |p| {
                p.text(72.0, 100.0, 12.0, "Foo foo FOO food");
            });
            b.write_temp("wp53-options")
        };
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        let doc = rig.doc(tab);
        let (mut st, notifier) = state_on(&rig);
        let count = |st: &mut SearchState, rig: &Rig, doc: &DocumentHandle, notifier: &Notifier| {
            st.start(doc, notifier, at(0, 0.0), Some(generation_of(doc)));
            rig.pump.wait_until("the search", || {
                st.tick(doc, notifier, Some(generation_of(doc)), at(0, 0.0), 0.0);
                st.live && st.snapshot().finished
            });
            st.snapshot().matches.len()
        };
        st.query.text = "foo".into();
        assert_eq!(
            count(&mut st, &rig, &doc, &notifier),
            4,
            "ignoring case, inside words"
        );
        st.query.case_sensitive = true;
        assert_eq!(
            count(&mut st, &rig, &doc, &notifier),
            2,
            "foo and the one in food"
        );
        st.query.case_sensitive = false;
        st.query.whole_word = true;
        assert_eq!(
            count(&mut st, &rig, &doc, &notifier),
            3,
            "not the one in food"
        );
        st.query.whole_word = false;
        st.query.regex = true;
        st.query.text = "fo+d".into();
        assert_eq!(count(&mut st, &rig, &doc, &notifier), 1);

        // A regular expression that does not compile: an error, and the old results are gone.
        st.query.text = "(".into();
        st.start(&doc, &notifier, at(0, 0.0), Some(generation_of(&doc)));
        assert!(st.error().is_some());
        assert!(st.snapshot().matches.is_empty());
        assert!(st.current_index().is_none());
        assert!(st.job.is_none(), "nothing runs");
        assert!(!st.no_match(), "an error is not \"no match\"");
        // Fixing it searches again and clears the error.
        st.query.text = "fo+".into();
        assert_eq!(count(&mut st, &rig, &doc, &notifier), 4);
        assert!(st.error().is_none());
    }

    #[test]
    fn no_match_is_told_once_the_search_is_over() {
        let pdf = needle_doc("wp53-nomatch", &[(1, 100.0)]);
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.open_bar(tab);
        rig.search_for(tab, "zzzz");
        let s = rig.search(tab);
        assert!(s.no_match());
        assert!(s.snapshot().matches.is_empty() && s.current_index().is_none());
        assert_eq!(count_text(None, s.snapshot(), None), template("find.none"));
    }

    fn select(pdf: &mut PdfTab, from: u32, to: u32, page: u32) {
        pdf.ui.selection.set_for_test(Selection {
            anchor: TextPos { page, ch: from },
            focus: TextPos { page, ch: to },
        });
    }

    #[test]
    fn ctrl_f_with_a_selected_word_searches_for_it_and_long_or_multiline_selections_are_not_taken()
    {
        let pdf = {
            let long = "L".repeat(250);
            PdfBuilder::new()
                .page(3000.0, 842.0, |p| {
                    p.text(72.0, 100.0, 12.0, "alpha beta");
                    p.text(72.0, 130.0, 12.0, "gamma delta");
                    p.text(72.0, 200.0, 12.0, &long);
                })
                .write_temp("wp53-selection")
        };
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        let doc = rig.doc(tab);
        let page_text = doc.page_text_wait(0).expect("page text");
        let chars: String = page_text.chars.iter().map(|c| c.ch).collect();
        let start = chars.find("beta").expect("beta") as u32;

        let Some(Tab::Pdf(p)) = rig.state.tabs.get_mut(tab) else {
            panic!("pdf tab");
        };
        select(p, start, start + 4, 0);
        dispatch(&mut rig.state, Action::Find(tab), &rig.ctx);
        let s = rig.search(tab);
        assert!(s.open);
        assert_eq!(s.query.text, "beta");
        assert_eq!(s.starts, 1, "searched at once");
        rig.frames_until("the result", |rig| {
            let s = rig.search(tab);
            s.live && s.snapshot().finished
        });
        assert_eq!(rig.search(tab).snapshot().matches.len(), 1);

        // Two lines: not taken over (the old query stays).
        let end = chars.find("delta").expect("delta") as u32 + 5;
        let Some(Tab::Pdf(p)) = rig.state.tabs.get_mut(tab) else {
            panic!("pdf tab");
        };
        p.ui.search.close();
        p.ui.search.query.text.clear();
        select(p, start, end, 0);
        dispatch(&mut rig.state, Action::Find(tab), &rig.ctx);
        assert!(rig.search(tab).open);
        assert_eq!(rig.search(tab).query.text, "");
        assert_eq!(rig.search(tab).starts, 1, "nothing new was started");

        // The context menu entry takes the same two lines, joined.
        dispatch(&mut rig.state, Action::FindSelection(tab), &rig.ctx);
        let text = rig.search(tab).query.text.clone();
        assert!(
            text.starts_with("beta") && text.ends_with("delta") && !text.contains('\n'),
            "{text:?}"
        );
        assert_eq!(rig.search(tab).starts, 2);

        // Too long for Ctrl+F.
        let long_start = chars.find(&"L".repeat(250)).expect("long line") as u32;
        let Some(Tab::Pdf(p)) = rig.state.tabs.get_mut(tab) else {
            panic!("pdf tab");
        };
        p.ui.search.close();
        p.ui.search.query.text.clear();
        select(p, long_start, long_start + 250, 0);
        dispatch(&mut rig.state, Action::Find(tab), &rig.ctx);
        assert_eq!(rig.search(tab).query.text, "");
    }

    #[test]
    fn escape_closes_the_bar_clears_the_highlights_and_keeps_the_query() {
        let pdf = needle_doc("wp53-escape", &[(0, 150.0)]);
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.open_bar(tab);
        rig.search_for(tab, "needle");
        rig.frames(2);
        {
            let pdf_tab = rig.pdf(tab);
            let layout = pdf_tab.ui.layout().expect("layout");
            let rects =
                pdf_tab
                    .ui
                    .search
                    .highlights(layout, pdf_tab.ui.scroll(), pdf_tab.ui.viewport());
            assert_eq!(rects.len(), 1);
            assert!(rects[0].current);
        }
        rig.key(Key::Escape, Modifiers::NONE);
        rig.frames(2);
        let pdf_tab = rig.pdf(tab);
        assert!(!pdf_tab.ui.search.open);
        assert_eq!(pdf_tab.ui.search.query.text, "needle", "remembered");
        let layout = pdf_tab.ui.layout().expect("layout");
        assert!(
            pdf_tab
                .ui
                .search
                .highlights(layout, pdf_tab.ui.scroll(), pdf_tab.ui.viewport())
                .is_empty()
        );
        assert!(pdf_tab.ui.search.job.is_none(), "the search was cancelled");
        assert!(
            !rig.ctx.text_edit_focused(),
            "the field gave up the keyboard"
        );

        // Ctrl+F again: the remembered text is in the field and is searched again.
        rig.open_bar(tab);
        assert_eq!(rig.search(tab).query.text, "needle");
        rig.frames_until("the second search", |rig| {
            let s = rig.search(tab);
            s.live && s.snapshot().finished
        });
        assert_eq!(rig.search(tab).snapshot().matches.len(), 1);
    }

    #[test]
    fn enter_and_f3_move_between_matches_without_a_panic() {
        let pdf = needle_doc("wp53-enter", &[(1, 100.0), (3, 100.0), (5, 100.0)]);
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.open_bar(tab);
        rig.search_for(tab, "needle");
        let first = rig.search(tab).current_index();
        assert_eq!(first, Some(0));
        rig.key(Key::Enter, Modifiers::NONE);
        assert_eq!(rig.search(tab).current_index(), Some(1));
        rig.key(Key::F3, Modifiers::NONE);
        assert_eq!(rig.search(tab).current_index(), Some(2));
        rig.key(Key::Enter, Modifiers::SHIFT);
        assert_eq!(rig.search(tab).current_index(), Some(1));
        rig.key(Key::F3, Modifiers::SHIFT);
        assert_eq!(rig.search(tab).current_index(), Some(0));
        // Wraps from the first back to the last and says so.
        rig.key(Key::F3, Modifiers::SHIFT);
        assert_eq!(rig.search(tab).current_index(), Some(2));
        assert!(rig.search(tab).wrap_notice(rig.time).is_some());
        assert!(
            rig.ctx.text_edit_focused(),
            "the field keeps the keyboard through Enter"
        );
        assert_eq!(
            rig.search(tab).query.text,
            "needle",
            "Enter does not change the text"
        );
    }

    #[test]
    fn f3_works_from_the_view_through_the_command_table() {
        let pdf = needle_doc("wp53-f3", &[(1, 100.0), (3, 100.0)]);
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.open_bar(tab);
        rig.search_for(tab, "needle");
        // Esc leaves the field; the bar stays open only if Esc did not close it... so click out
        // by closing and reopening through F3 with the remembered query instead.
        rig.key(Key::Escape, Modifiers::NONE);
        assert!(!rig.search(tab).open);
        rig.key(Key::F3, Modifiers::NONE);
        assert!(rig.search(tab).open, "F3 without a search opens it");
        rig.frames_until("the search", |rig| {
            let s = rig.search(tab);
            s.live && s.snapshot().finished && s.current_index().is_some()
        });
        let at_first = rig.search(tab).current_index();
        rig.ctx.memory_mut(|m| m.surrender_focus(input_id(tab)));
        rig.frames(2);
        assert!(!rig.ctx.text_edit_focused());
        rig.key(Key::F3, Modifiers::NONE);
        assert_ne!(rig.search(tab).current_index(), at_first, "F3 stepped");
        rig.key(Key::F3, Modifiers::SHIFT);
        assert_eq!(
            rig.search(tab).current_index(),
            at_first,
            "Shift+F3 stepped back"
        );
    }

    #[test]
    fn typing_in_the_field_does_not_scroll_the_view_but_the_same_keys_do_without_it() {
        let pdf = needle_doc("wp53-keys", &[(0, 150.0)]);
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.open_bar(tab);
        let key_event = |key| Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        let before = rig.pdf(tab).ui.scroll();
        rig.frame(vec![
            key_event(Key::Space),
            Event::Text(" ".into()),
            key_event(Key::ArrowDown),
            key_event(Key::PageDown),
            Event::Text("zz".into()),
        ]);
        rig.frames(2);
        assert_eq!(rig.pdf(tab).ui.scroll(), before, "the view did not move");
        assert_eq!(
            rig.search(tab).query.text,
            " zz",
            "the keys went to the field"
        );

        // With the bar closed the same keys scroll the view.
        rig.key(Key::Escape, Modifiers::NONE);
        assert!(!rig.search(tab).open);
        rig.frame(vec![key_event(Key::PageDown)]);
        rig.frames(2);
        assert!(
            rig.pdf(tab).ui.scroll().y > before.y + 10.0,
            "PageDown scrolls once the field is gone"
        );
    }

    #[test]
    fn find_is_disabled_on_the_start_page_and_in_a_comparison_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        assert!(matches!(
            state.tabs.get(state.focus.expect("focus")),
            Some(Tab::Start)
        ));
        for id in ["edit.find", "edit.find_next", "edit.find_prev"] {
            let command = commands::command(id).expect(id);
            assert!(!(command.enabled)(&state), "{id} on the start page");
            assert_eq!(command.run(&state), None);
        }

        let a = needle_doc("wp53-cmp-a", &[(1, 100.0)]);
        let b = needle_doc("wp53-cmp-b", &[(2, 100.0)]);
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget {
                path: a.path().to_path_buf(),
                page: None,
            }]),
            &ctx,
        );
        let pdf_tab = state.focus.expect("pdf tab");
        for id in ["edit.find", "edit.find_next", "edit.find_prev"] {
            let command = commands::command(id).expect(id);
            assert!((command.enabled)(&state), "{id} on a PDF tab");
        }
        assert_eq!(
            commands::command("edit.find").and_then(|c| c.run(&state)),
            Some(Action::Find(pdf_tab))
        );
        dispatch(
            &mut state,
            Action::StartDiff {
                left: a.path().to_path_buf(),
                right: b.path().to_path_buf(),
            },
            &ctx,
        );
        let diff_tab = state.focus.expect("diff tab");
        assert!(state.tabs.diff(diff_tab).is_some());
        for id in ["edit.find", "edit.find_next", "edit.find_prev"] {
            let command = commands::command(id).expect(id);
            assert!(!(command.enabled)(&state), "{id} in a comparison tab");
            assert_eq!(command.run(&state), None);
        }
        // The actions do nothing for a tab that is not a PDF tab.
        for action in [
            Action::Find(diff_tab),
            Action::FindNext(diff_tab),
            Action::FindPrev(diff_tab),
            Action::FindSelection(diff_tab),
        ] {
            dispatch(&mut state, action, &ctx);
        }
    }

    #[test]
    fn the_list_panel_draws_while_results_grow_and_follows_the_current_match() {
        let places: Vec<(usize, f32)> = (0..8).flat_map(|p| [(p, 100.0), (p, 300.0)]).collect();
        let pdf = needle_doc("wp53-list", &places);
        let mut rig = Rig::new();
        let tab = rig.open(&pdf);
        rig.open_bar(tab);
        // Open the list: the toggle is part of the bar, so set it as its click does.
        if let Some(Tab::Pdf(p)) = rig.state.tabs.get_mut(tab) {
            p.ui.search.list_open = true;
        }
        rig.search_for(tab, "needle");
        rig.frames(3);
        assert_eq!(rig.search(tab).snapshot().matches.len(), 16);
        for _ in 0..5 {
            rig.key(Key::Enter, Modifiers::NONE);
        }
        rig.frames(3);
        assert_eq!(rig.search(tab).current_index(), Some(5));
        // The rows are cached for the snapshot and cover every match.
        let outline = Outline::default();
        let rows = {
            let Some(Tab::Pdf(p)) = rig.state.tabs.get_mut(tab) else {
                panic!("pdf tab");
            };
            p.ui.search.rows(&outline, 1)
        };
        assert_eq!(rows.len(), 16 + 8);
        let again = {
            let Some(Tab::Pdf(p)) = rig.state.tabs.get_mut(tab) else {
                panic!("pdf tab");
            };
            p.ui.search.rows(&outline, 1)
        };
        assert!(
            Arc::ptr_eq(&rows, &again),
            "no new rows without new matches"
        );
        // Closing the bar hides the list with it.
        rig.key(Key::Escape, Modifiers::NONE);
        rig.frames(2);
        assert!(!rig.search(tab).open);
    }
}
