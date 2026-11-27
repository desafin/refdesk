//! Search in all open documents (FIND-7 ~ FIND-12, `docs/ARCHITECTURE.md` §5.10).
//!
//! One search for the whole application ([`GlobalSearch`], `AppState::global_search`): a query
//! and its options, and for every document that a PDF tab shows one `refdesk_core::SearchJob`
//! with what it found so far. The sidebar of every window can show it (its "Search" view); the
//! PDF views tint its matches on their pages under the matches of their own search; F4 and
//! Shift+F4 step through its matches across documents.
//!
//! What decides something is a plain function or a method that does not touch egui, so it is
//! tested without a window (rule R-10): which documents are searched and in which order, how the
//! result list is laid out as rows, the summary line and the percent done, the next and the
//! previous match across documents, and which tab shows a result. [`show_view`] only draws and
//! turns input into actions; moving tabs and views is `dispatch`'s job (R-6), done by
//! [`go_to_result`] and [`step`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use egui::{
    Color32, Id, Key, Modifiers, Pos2, Rect, RichText, ScrollArea, Sense, TextEdit, TextStyle, Ui,
    WidgetInfo, vec2,
};
use refdesk_core::{
    LayoutNode, Notifier, PageLayout, Place, Point, Rect as PageRect, SearchError, SearchHistory,
    SearchJob, SearchMatch, SearchQuery, SearchSnapshot, Size, natural_cmp,
};

use crate::action::Action;
use crate::i18n::tr;
use crate::panels::search::{
    self, MatchKey, MatchRect, PageIndex, find_key, line_job, match_job, match_key, match_rects,
    row_text,
};
use crate::panels::search_history::{self, HistoryEvent, Recall, Settle, SettleStep};
use crate::panels::sidebar::SidebarView;
use crate::panels::toc::{scroll_target, triangle};
use crate::state::{
    AppState, DocId, DocStore, PdfTab, Tab, TabId, TabStore, WindowId, WindowState,
};
use crate::views::links::page_label;
use crate::windows::dock::DockLayout;

/// Room left of a match line, so that it sits under the name of its document.
const MATCH_INDENT: f32 = 18.0;

// ---------------------------------------------------------------------------------------------
// Pure logic: which documents are searched
// ---------------------------------------------------------------------------------------------

/// The file name of `path`, for showing and for ordering.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The documents to search, in the order the result list shows them: each document once, by the
/// natural order of the file names (`doc2` before `doc10`, letters without regard to case), then
/// by the whole path.
pub fn target_order(docs: Vec<(DocId, PathBuf)>) -> Vec<(DocId, PathBuf)> {
    let mut seen = HashSet::new();
    let mut docs: Vec<(DocId, PathBuf)> =
        docs.into_iter().filter(|(d, _)| seen.insert(*d)).collect();
    docs.sort_by(|a, b| {
        natural_cmp(&file_name(&a.1), &file_name(&b.1)).then_with(|| a.1.cmp(&b.1))
    });
    docs
}

/// The documents that the PDF tabs of all windows show, in [`target_order`]. Comparison tabs and
/// start pages search nothing.
pub fn targets(windows: &[WindowState], tabs: &TabStore, docs: &DocStore) -> Vec<(DocId, PathBuf)> {
    let shown = windows
        .iter()
        .flat_map(|w| w.dock.tabs())
        .filter_map(|t| tabs.pdf(t).map(|p| p.doc))
        .filter_map(|d| docs.get(d).map(|h| (d, h.path().to_path_buf())))
        .collect();
    target_order(shown)
}

// ---------------------------------------------------------------------------------------------
// Pure logic: the summary
// ---------------------------------------------------------------------------------------------

/// The percent of the pages searched, over all documents: the pages done of every document over
/// the pages of every document. `None` once every search has finished. Each part is (pages
/// done, pages in the document, finished).
pub fn progress_percent(parts: impl IntoIterator<Item = (u32, u32, bool)>) -> Option<u32> {
    let (mut done, mut total, mut running) = (0u64, 0u64, false);
    for (d, t, finished) in parts {
        running |= !finished;
        // A finished search counts all of its pages as done, whatever it reported on the way.
        done += u64::from(if finished { t.max(d) } else { d.min(t) });
        total += u64::from(t);
    }
    running.then(|| {
        done.saturating_mul(100)
            .checked_div(total)
            .unwrap_or(0)
            .min(100) as u32
    })
}

/// What the search found, over all documents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Matches in all documents.
    pub matches: usize,
    /// Documents with at least one match.
    pub docs_with_matches: usize,
    /// Percent of the pages searched while a search runs ([`progress_percent`]).
    pub percent: Option<u32>,
    /// Some document has more matches than a search collects (`SearchSnapshot::truncated`).
    pub truncated: bool,
}

/// The line above the results: "124 results in 3 documents" (with "(searching 62%)" while a
/// search runs), "No results" once the search is over, "No open documents" without a PDF tab.
/// `None` when there is nothing to say (no query).
pub fn summary_text(totals: &Totals, any_docs: bool, searched: bool) -> Option<String> {
    if !any_docs {
        return Some(tr("gsearch.no_docs").to_owned());
    }
    if !searched {
        return None;
    }
    let searching = totals
        .percent
        .map(|p| tr("gsearch.searching").replace("{percent}", &p.to_string()));
    if totals.matches == 0 {
        return Some(match totals.percent {
            Some(p) => tr("gsearch.searching_alone").replace("{percent}", &p.to_string()),
            None => tr("gsearch.none").to_owned(),
        });
    }
    let found = found_text(totals.matches, totals.docs_with_matches);
    Some(match searching {
        Some(s) => format!("{found} {s}"),
        None => found,
    })
}

/// "124 results in 3 documents". English has a singular ("1 result in 1 document"); the Korean
/// text is the same for every count.
pub fn found_text(matches: usize, docs: usize) -> String {
    let key = match (matches, docs) {
        (1, _) => "gsearch.summary_one",
        (_, 1) => "gsearch.summary_one_doc",
        _ => "gsearch.summary",
    };
    tr(key)
        .replace("{docs}", &docs.to_string())
        .replace("{n}", &matches.to_string())
}

// ---------------------------------------------------------------------------------------------
// Pure logic: the rows of the result list
// ---------------------------------------------------------------------------------------------

/// One row of the result list. Every row is one line high.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// The head of a document: its name and number of matches.
    Head {
        /// The document.
        doc: DocId,
        /// How many matches it has.
        count: usize,
        /// Its matches are folded away.
        collapsed: bool,
    },
    /// One match: an index into the matches of the document.
    Match {
        /// The document.
        doc: DocId,
        /// The match.
        index: usize,
    },
}

/// The rows of one document: its head, then its matches unless it is folded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Group {
    doc: DocId,
    /// The row of the head.
    first_row: usize,
    count: usize,
    collapsed: bool,
}

/// The flat list of rows, described per document: the n-th row is found by bisecting the first
/// rows of the documents, so a list of hundreds of thousands of rows costs only what the visible
/// rows cost.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowIndex {
    groups: Vec<Group>,
    len: usize,
}

impl RowIndex {
    /// The rows of documents with these numbers of matches, in this order. A document without a
    /// match has no row; a folded one only its head.
    pub fn build(counts: &[(DocId, usize)], collapsed: &HashSet<DocId>) -> RowIndex {
        let mut groups = Vec::with_capacity(counts.len());
        let mut len = 0;
        for &(doc, count) in counts {
            if count == 0 {
                continue;
            }
            let folded = collapsed.contains(&doc);
            groups.push(Group {
                doc,
                first_row: len,
                count,
                collapsed: folded,
            });
            len += 1 + if folded { 0 } else { count };
        }
        RowIndex { groups, len }
    }

    /// The number of rows.
    pub fn len(&self) -> usize {
        self.len
    }

    /// There is no row at all.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The n-th row.
    pub fn row(&self, n: usize) -> Option<Row> {
        if n >= self.len {
            return None;
        }
        let at = self
            .groups
            .partition_point(|g| g.first_row <= n)
            .checked_sub(1)?;
        let g = self.groups.get(at)?;
        let offset = n - g.first_row;
        Some(if offset == 0 {
            Row::Head {
                doc: g.doc,
                count: g.count,
                collapsed: g.collapsed,
            }
        } else {
            Row::Match {
                doc: g.doc,
                index: offset - 1,
            }
        })
    }

    /// The row that shows match `index` of `doc`; `None` when the document is folded or has no
    /// such match.
    pub fn row_of(&self, doc: DocId, index: usize) -> Option<usize> {
        let g = self.groups.iter().find(|g| g.doc == doc)?;
        (!g.collapsed && index < g.count).then_some(g.first_row + 1 + index)
    }
}

// ---------------------------------------------------------------------------------------------
// Pure logic: next and previous
// ---------------------------------------------------------------------------------------------

/// Where a step to the next or the previous match starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepFrom {
    /// From the current result: match `.1` of document `.0`.
    Current(DocId, usize),
    /// From a place in a document (where its focused view is): `.1` matches of document `.0` lie
    /// before it.
    Position(DocId, usize),
    /// From nowhere: forward goes to the very first match, backward to the very last.
    Nowhere,
}

/// The match one step forward (or backward) from `from`, in document order and then in the order
/// of the matches of a document. Past the last match of a document it goes on with the first
/// match of the next document, past the last document with the first; backward the other way
/// round. Documents without matches are passed over (folded ones are not). `counts` are the
/// documents in their order with their numbers of matches. `None` when there is no match at all.
pub fn step_target(
    counts: &[(DocId, usize)],
    from: StepFrom,
    forward: bool,
) -> Option<(DocId, usize)> {
    let with: Vec<(DocId, usize)> = counts.iter().copied().filter(|(_, n)| *n > 0).collect();
    let len = with.len();
    if len == 0 {
        return None;
    }
    let first_of = |k: usize| (with[k].0, 0);
    let last_of = |k: usize| (with[k].0, with[k].1 - 1);
    let nowhere = if forward {
        first_of(0)
    } else {
        last_of(len - 1)
    };
    let (k, before) = match from {
        // `before`: how many matches of document `k` lie before the place stepped from.
        StepFrom::Current(doc, i) => match with.iter().position(|(d, n)| *d == doc && i < *n) {
            Some(k) if forward => (k, i + 1),
            Some(k) => (k, i),
            None => return Some(nowhere),
        },
        StepFrom::Position(doc, p) => match with.iter().position(|(d, _)| *d == doc) {
            Some(k) => (k, p.min(with[k].1)),
            None => return Some(nowhere),
        },
        StepFrom::Nowhere => return Some(nowhere),
    };
    Some(if forward {
        if before < with[k].1 {
            (with[k].0, before)
        } else {
            first_of((k + 1) % len)
        }
    } else if before > 0 {
        (with[k].0, before - 1)
    } else {
        last_of((k + len - 1) % len)
    })
}

// ---------------------------------------------------------------------------------------------
// Pure logic: which tab shows a result
// ---------------------------------------------------------------------------------------------

/// A PDF tab that shows the document of a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabSpot {
    /// The tab.
    pub tab: TabId,
    /// Its window.
    pub window: WindowId,
    /// It is the active tab of its pane, so its view is on screen.
    pub visible: bool,
}

/// The tab to show a result in, of the tabs that show its document (windows in order, tabs in
/// order): the focused tab when it is one of them; else one of window `window` (where the
/// result was asked for), a visible one first; else one of another window, a visible one first.
/// `None` when no tab shows the document (any more).
pub fn pick_tab(
    spots: &[TabSpot],
    focus: Option<TabId>,
    window: Option<WindowId>,
) -> Option<TabId> {
    if let Some(f) = focus
        && spots.iter().any(|s| s.tab == f)
    {
        return Some(f);
    }
    let best = |of: &mut dyn Iterator<Item = &TabSpot>| {
        let all: Vec<&TabSpot> = of.collect();
        all.iter()
            .find(|s| s.visible)
            .or(all.first())
            .map(|s| s.tab)
    };
    if let Some(w) = window
        && let Some(tab) = best(&mut spots.iter().filter(|s| s.window == w))
    {
        return Some(tab);
    }
    best(&mut spots.iter())
}

/// The active tab of every pane of `dock`, read from its session form.
fn visible_tabs(dock: &DockLayout) -> Vec<TabId> {
    let all = dock.tabs();
    let key_of = |t: TabId| {
        all.iter()
            .position(|x| *x == t)
            .map_or_else(String::new, |i| i.to_string())
    };
    let mut out = Vec::new();
    let mut stack = vec![dock.to_layout(key_of)];
    while let Some(node) = stack.pop() {
        match node {
            LayoutNode::Tabs { tabs, active } => {
                if let Some(tab) = tabs
                    .get(active)
                    .and_then(|k| k.parse::<usize>().ok())
                    .and_then(|i| all.get(i))
                {
                    out.push(*tab);
                }
            }
            LayoutNode::Split { first, second, .. } => {
                stack.push(*second);
                stack.push(*first);
            }
        }
    }
    out
}

/// The PDF tabs of all windows that show `doc`.
fn tab_spots(state: &AppState, doc: DocId) -> Vec<TabSpot> {
    let mut spots = Vec::new();
    for window in &state.windows {
        let visible = visible_tabs(&window.dock);
        for tab in window.dock.tabs() {
            if state.tabs.pdf(tab).is_some_and(|p| p.doc == doc) {
                spots.push(TabSpot {
                    tab,
                    window: window.id,
                    visible: visible.contains(&tab),
                });
            }
        }
    }
    spots
}

// ---------------------------------------------------------------------------------------------
// Pure logic: highlights
// ---------------------------------------------------------------------------------------------

/// What a PDF view needs to tint the matches of its document: borrowed for one frame, nothing is
/// copied.
#[derive(Clone, Copy, Debug)]
pub struct DocMarks<'a> {
    /// Which matches lie on which page.
    pub index: &'a PageIndex,
    /// The matches of the document.
    pub matches: &'a [SearchMatch],
    /// The current result, when it is a match of this document.
    pub current: Option<usize>,
}

impl DocMarks<'_> {
    /// The rectangles of the matches on the pages that overlap the viewport (see
    /// [`match_rects`]).
    pub fn rects(&self, layout: &PageLayout, scroll: Point, viewport: Size) -> Vec<MatchRect> {
        match_rects(
            self.index,
            self.matches,
            self.current,
            layout,
            scroll,
            viewport,
        )
    }
}

/// The marks for the view of `tab`: those of its document for a PDF tab, none for a comparison
/// tab or a start page.
pub fn marks_for_tab<'a>(search: &'a GlobalSearch, tab: &Tab) -> Option<DocMarks<'a>> {
    match tab {
        Tab::Pdf(pdf) => search.marks(pdf.doc),
        Tab::Diff(_) | Tab::Start => None,
    }
}

// ---------------------------------------------------------------------------------------------
// The state
// ---------------------------------------------------------------------------------------------

/// When the typed query is searched.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Due {
    /// At once: an option was toggled, a search was asked for.
    Now,
    /// [`search::INPUT_DELAY_SECS`] after this time (seconds): the text was edited.
    After(f64),
}

/// The moments at which the user is taken to have used the query (where the search history
/// records it, FIND-14). See [`GlobalSearch::note_used`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryUse {
    /// Enter, Shift+Enter, F4 or Shift+F4 moved to another result.
    Step,
    /// A result row was clicked.
    ResultClicked,
    /// The clear button (or command) is about to drop the query.
    Cleared,
}

/// The search of one document.
struct DocEntry {
    id: DocId,
    job: Option<SearchJob>,
    /// The generation of the document the job runs on.
    generation: Option<u32>,
    /// What is shown: the matches, in document order.
    snapshot: Arc<SearchSnapshot>,
    /// `snapshot` is the one of the current job (not what the job before it left).
    live: bool,
    index: PageIndex,
    /// (pages done, pages in the document, finished) of the job as of the last frame.
    progress: (u32, u32, bool),
}

impl DocEntry {
    fn new(id: DocId) -> DocEntry {
        DocEntry {
            id,
            job: None,
            generation: None,
            snapshot: Arc::default(),
            live: false,
            index: PageIndex::default(),
            progress: (0, 0, true),
        }
    }
}

/// The scroll position of the result list of one window, as of the last frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ListView {
    offset: f32,
    view_h: f32,
    /// The value of [`GlobalSearch::follow`] this list scrolled to last.
    followed: u64,
}

/// The search in all open documents. One for the application; not saved.
#[derive(Default)]
pub struct GlobalSearch {
    /// The query as typed and its three options.
    pub query: SearchQuery,
    /// The window whose input field takes the keyboard (and selects its text) in the next frame.
    pub focus_input: Option<WindowId>,
    /// How many document searches were started (tests).
    pub starts: u32,
    /// The last time the user was taken to have used the query (tests; see
    /// [`GlobalSearch::note_used`]).
    pub last_use: Option<QueryUse>,
    /// ↑ / ↓ in the input field walk through the search history (FIND-13).
    pub recall: Recall,
    /// Records the query when it has stayed for a while with results (FIND-14).
    pub settle: Settle,
    /// The query the jobs run; `None` while nothing is searched.
    running: Option<SearchQuery>,
    due: Option<Due>,
    /// Why the query cannot be searched (an invalid regular expression).
    error: Option<String>,
    /// The searched documents, in [`target_order`].
    docs: Vec<DocEntry>,
    /// Documents whose matches are folded away in the list.
    collapsed: HashSet<DocId>,
    current: Option<(DocId, MatchKey)>,
    /// Counts changes of the documents and their matches; the rows are made again on a change.
    version: u64,
    /// Counts changes of `collapsed`.
    folds: u64,
    rows: Option<(u64, u64, Arc<RowIndex>)>,
    /// Counts changes of the current result; each window's list scrolls to it once.
    follow: u64,
    views: HashMap<WindowId, ListView>,
}

impl GlobalSearch {
    /// The text in the input field is not empty.
    pub fn has_query(&self) -> bool {
        !self.query.text.trim().is_empty()
    }

    /// A search ran (or runs) for a query: the results describe it.
    pub fn searched(&self) -> bool {
        self.running.is_some()
    }

    /// Why the query cannot be searched, if it cannot.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The text of the field changed at `now` (seconds): the search starts after the input delay.
    pub fn edited(&mut self, now: f64) {
        self.due = Some(Due::After(now));
    }

    /// An option changed, or the query was set from outside: the search starts at once.
    pub fn search_now(&mut self) {
        self.due = Some(Due::Now);
    }

    /// The query was used (see [`QueryUse`]): remembers how. [`used`] calls this and writes the
    /// query to the search history.
    pub fn note_used(&mut self, how: QueryUse) {
        if self.has_query() {
            self.last_use = Some(how);
        }
    }

    /// The search for the query shown has results: what [`Settle`] waits for.
    fn has_current_matches(&self) -> bool {
        self.running.as_ref() == Some(&self.query)
            && self
                .docs
                .iter()
                .any(|d| d.live && !d.snapshot.matches.is_empty())
    }

    /// Puts a query from the history into the field of `window` and searches for it at once.
    /// The field gets the keyboard with all its text selected.
    fn use_query(&mut self, query: SearchQuery, window: WindowId) {
        self.query = query;
        self.focus_input = Some(window);
        self.search_now();
    }

    /// Drops every job and everything they found; the query stays.
    fn clear_results(&mut self) {
        self.docs.clear();
        self.running = None;
        self.current = None;
        self.version += 1;
    }

    /// Clears the search: the query, the jobs, the results and the highlights. The options stay.
    pub fn clear(&mut self) {
        self.recall.reset();
        self.settle.reset();
        self.clear_results();
        self.query.text.clear();
        self.due = None;
        self.error = None;
        self.collapsed.clear();
        self.folds += 1;
    }

    /// One frame of the search. `targets` are the documents to search ([`targets`]), `docs` the
    /// open documents, `now` the time in seconds. Starts the searches of documents that came in,
    /// drops those of documents that went away, searches a reloaded document again (R-7),
    /// starts the query that is due, and takes over what the jobs found. Never waits (R-1).
    /// Returns when a frame is needed (a typed query becomes due); the jobs ask for frames
    /// themselves through `notifier`.
    pub fn update(
        &mut self,
        targets: &[(DocId, PathBuf)],
        docs: &DocStore,
        notifier: &Notifier,
        now: f64,
    ) -> Option<Duration> {
        let mut wait = None;
        let restart = match self.due {
            Some(Due::Now) => true,
            Some(Due::After(edited)) => match search::input_delay_left(edited, now) {
                None => true,
                left => {
                    wait = left;
                    false
                }
            },
            None => false,
        };
        if !restart && self.running.is_none() {
            return wait;
        }
        self.sync_targets(targets, docs, notifier, !restart);
        if restart {
            self.restart(docs, notifier);
        }
        if self.running.is_some() {
            self.follow_generations(docs, notifier);
        }
        self.take_snapshots();
        wait
    }

    /// Makes the documents those of `targets`: new ones are searched for the running query
    /// (when `start` is true), gone ones are dropped with their jobs.
    fn sync_targets(
        &mut self,
        targets: &[(DocId, PathBuf)],
        docs: &DocStore,
        notifier: &Notifier,
        start: bool,
    ) {
        let same = self.docs.len() == targets.len()
            && self.docs.iter().zip(targets).all(|(e, (d, _))| e.id == *d);
        if same {
            return;
        }
        let mut old: HashMap<DocId, DocEntry> = self.docs.drain(..).map(|e| (e.id, e)).collect();
        let query = self.running.clone();
        let mut failed = None;
        for (id, _) in targets {
            let entry = match old.remove(id) {
                Some(entry) => entry,
                None => {
                    let mut entry = DocEntry::new(*id);
                    if start
                        && let Some(query) = &query
                        && let Err(message) = self.start_entry(&mut entry, docs, query, notifier)
                    {
                        failed = Some(message);
                    }
                    entry
                }
            };
            self.docs.push(entry);
        }
        if self
            .current
            .is_some_and(|(d, _)| !self.docs.iter().any(|e| e.id == d))
        {
            self.current = None;
        }
        self.version += 1;
        if let Some(message) = failed {
            self.clear_results();
            self.error = Some(message);
        }
    }

    /// Starts searching every document for the query. An empty query clears the results; an
    /// invalid regular expression clears them and says why.
    fn restart(&mut self, docs: &DocStore, notifier: &Notifier) {
        self.due = None;
        self.error = None;
        if !self.has_query() {
            self.clear_results();
            return;
        }
        let query = self.query.clone();
        self.running = Some(query.clone());
        self.current = None;
        let mut entries = std::mem::take(&mut self.docs);
        let mut failed = None;
        for entry in &mut entries {
            if let Err(message) = self.start_entry(entry, docs, &query, notifier) {
                failed = Some(message);
                break;
            }
        }
        self.docs = entries;
        if let Some(message) = failed {
            self.clear_results();
            self.error = Some(message);
        }
    }

    /// Starts the job of one document. What the job before it found stays shown until the new
    /// one has something, so typing does not make the results blink.
    fn start_entry(
        &mut self,
        entry: &mut DocEntry,
        docs: &DocStore,
        query: &SearchQuery,
        notifier: &Notifier,
    ) -> Result<(), String> {
        let Some(doc) = docs.get(entry.id) else {
            entry.job = None;
            entry.progress = (0, 0, true);
            return Ok(());
        };
        match SearchJob::start(doc.clone(), query.clone(), 0, notifier.clone()) {
            Ok(job) => {
                self.starts += 1;
                entry.job = Some(job);
                entry.live = false;
                entry.generation = doc.info().map(|i| i.generation);
                entry.progress = (0, entry.progress.1, false);
                Ok(())
            }
            Err(SearchError::Empty) => {
                entry.job = None;
                entry.snapshot = Arc::default();
                entry.index = PageIndex::default();
                entry.progress = (0, 0, true);
                Ok(())
            }
            Err(SearchError::InvalidRegex(message)) => Err(message),
            Err(SearchError::Thread(message)) => {
                log::error!("search: {message}");
                Err(message)
            }
        }
    }

    /// Searches a document again whose generation changed since its job started (R-7).
    fn follow_generations(&mut self, docs: &DocStore, notifier: &Notifier) {
        let Some(query) = self.running.clone() else {
            return;
        };
        let mut entries = std::mem::take(&mut self.docs);
        let mut failed = None;
        for entry in &mut entries {
            let now = docs
                .get(entry.id)
                .and_then(|d| d.info())
                .map(|i| i.generation);
            match (entry.generation, now) {
                (Some(old), Some(new)) if old != new => {
                    if let Err(message) = self.start_entry(entry, docs, &query, notifier) {
                        failed = Some(message);
                    }
                }
                (None, Some(_)) if entry.job.is_some() => entry.generation = now,
                _ => {}
            }
        }
        self.docs = entries;
        if let Some(message) = failed {
            self.clear_results();
            self.error = Some(message);
        }
    }

    /// Takes over what the jobs found. Only a document whose snapshot changed gets a new page
    /// index.
    fn take_snapshots(&mut self) {
        let mut changed = false;
        for entry in &mut self.docs {
            let Some(job) = &entry.job else {
                continue;
            };
            let live = job.snapshot();
            entry.progress = (live.pages_done, live.pages_total, live.finished);
            let fresh = live.pages_done > 0 || live.finished;
            if fresh && !(entry.live && Arc::ptr_eq(&live, &entry.snapshot)) {
                entry.index = PageIndex::build(&live.matches, live.pages_total);
                entry.snapshot = live;
                entry.live = true;
                changed = true;
            }
        }
        if changed {
            self.version += 1;
        }
    }

    /// The searched documents and their numbers of matches, in list order.
    pub fn counts(&self) -> Vec<(DocId, usize)> {
        self.docs
            .iter()
            .map(|e| (e.id, e.snapshot.matches.len()))
            .collect()
    }

    /// What the search found, over all documents.
    pub fn totals(&self) -> Totals {
        let mut totals = Totals {
            percent: progress_percent(self.docs.iter().map(|e| e.progress)),
            ..Totals::default()
        };
        for e in &self.docs {
            let n = e.snapshot.matches.len();
            totals.matches += n;
            totals.docs_with_matches += usize::from(n > 0);
            totals.truncated |= e.snapshot.truncated;
        }
        totals
    }

    /// The matches shown for `doc`, in document order.
    pub fn matches(&self, doc: DocId) -> Option<&[SearchMatch]> {
        self.docs
            .iter()
            .find(|e| e.id == doc)
            .map(|e| e.snapshot.matches.as_slice())
    }

    /// The current result: match `.1` of document `.0`. `None` when there is none or its match is
    /// gone (the query changed).
    pub fn current(&self) -> Option<(DocId, usize)> {
        let (doc, key) = self.current?;
        let index = find_key(self.matches(doc)?, key)?;
        Some((doc, index))
    }

    /// Makes match `index` of `doc` the current result; the lists scroll to it.
    pub fn set_current(&mut self, doc: DocId, index: usize) {
        if let Some(m) = self.matches(doc).and_then(|ms| ms.get(index)) {
            self.current = Some((doc, match_key(m)));
            self.follow += 1;
        }
    }

    /// The marks for the views of `doc`; `None` when the search has no match in it.
    pub fn marks(&self, doc: DocId) -> Option<DocMarks<'_>> {
        let entry = self.docs.iter().find(|e| e.id == doc)?;
        if entry.snapshot.matches.is_empty() {
            return None;
        }
        let current = self
            .current
            .filter(|(d, _)| *d == doc)
            .and_then(|(_, key)| find_key(&entry.snapshot.matches, key));
        Some(DocMarks {
            index: &entry.index,
            matches: &entry.snapshot.matches,
            current,
        })
    }

    /// The rows of the result list, made again only when the matches or the folding changed.
    pub fn rows(&mut self) -> Arc<RowIndex> {
        if let Some((version, folds, rows)) = &self.rows
            && *version == self.version
            && *folds == self.folds
        {
            return Arc::clone(rows);
        }
        let rows = Arc::new(RowIndex::build(&self.counts(), &self.collapsed));
        self.rows = Some((self.version, self.folds, Arc::clone(&rows)));
        rows
    }

    /// Folds or unfolds the matches of `doc` in the list.
    pub fn toggle_collapsed(&mut self, doc: DocId) {
        if !self.collapsed.remove(&doc) {
            self.collapsed.insert(doc);
        }
        self.folds += 1;
    }

    /// The number of document searches that run (tests).
    #[cfg(test)]
    pub fn running_jobs(&self) -> usize {
        self.docs.iter().filter(|e| e.job.is_some()).count()
    }

    /// Every document search has finished and its result is shown (tests).
    #[cfg(test)]
    pub fn finished(&self) -> bool {
        self.running.is_some()
            && self
                .docs
                .iter()
                .all(|e| e.job.is_none() || (e.live && e.snapshot.finished))
    }

    /// The searched documents, in list order (tests).
    #[cfg(test)]
    pub fn doc_ids(&self) -> Vec<DocId> {
        self.docs.iter().map(|e| e.id).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// What `dispatch` does
// ---------------------------------------------------------------------------------------------

/// One frame of the search, called once per frame from `logic()`.
pub fn tick(state: &mut AppState, ctx: &egui::Context) {
    let search = &state.global_search;
    if search.running.is_none() && search.due.is_none() {
        return;
    }
    let targets = targets(&state.windows, &state.tabs, &state.docs);
    let now = ctx.input(|i| i.time);
    let notifier = crate::action::notifier(ctx);
    if let Some(wait) = state
        .global_search
        .update(&targets, &state.docs, &notifier, now)
    {
        ctx.request_repaint_after(wait);
    }
}

/// Opens the sidebar of the focused window on its search view and returns the window.
fn open_view(state: &mut AppState) -> Option<WindowId> {
    let w = state.focused_window_index();
    let window = state.windows.get_mut(w)?;
    window.toc_open = true;
    window.sidebar_view = SidebarView::Search;
    Some(window.id)
}

/// `Action::ShowGlobalSearch` (Ctrl+Shift+F): shows the search view in the sidebar of the focused
/// window and puts the keyboard into its input field, with the text selected. A selected text of
/// one line in the focused tab becomes the query and is searched at once.
pub fn show(state: &mut AppState, ctx: &egui::Context) {
    let selected = state.focused_pdf().and_then(|(_, pdf)| {
        let doc = state.docs.get(pdf.doc)?;
        search::selected_query(pdf, doc, true)
    });
    let window = open_view(state);
    let search = &mut state.global_search;
    search.focus_input = window;
    if let Some(text) = selected {
        search.query.text = text;
        search.search_now();
    }
    ctx.request_repaint();
}

/// The user used the query (see [`QueryUse`]): it goes to the search history unless it is the
/// newest entry already, and settling does not record it again (FIND-14). Nothing for an empty
/// query.
fn used(state: &mut AppState, how: QueryUse) {
    if !state.global_search.has_query() {
        return;
    }
    state.global_search.note_used(how);
    let query = state.global_search.query.clone();
    state.global_search.settle.used(&query);
    search_history::record(state, &query, None);
}

/// `Action::GlobalSearchFor` (`--find`): searches all documents for `text` and shows the search
/// view in the sidebar of the focused window. A text asked for by name is a used query.
pub fn search_for(state: &mut AppState, text: String, ctx: &egui::Context) {
    open_view(state);
    let search = &mut state.global_search;
    search.recall.reset();
    search.query.text = text;
    search.search_now();
    let query = search.query.clone();
    search.settle.used(&query);
    search_history::record(state, &query, None);
    ctx.request_repaint();
}

/// `Action::ClearGlobalSearch`. A query that was searched is remembered before it goes.
pub fn clear(state: &mut AppState) {
    if state.global_search.searched() {
        used(state, QueryUse::Cleared);
    }
    state.global_search.clear();
}

/// Moves a view to show a match whose first line is `rect` on `page`. A move to another place is
/// a long jump in the history, so Back returns.
fn reveal(pdf: &mut PdfTab, page: u32, rect: PageRect) {
    let Some(to) = search::reveal_target(pdf, page, rect) else {
        return;
    };
    let doc = pdf.view.history.current().doc.clone();
    let from = Place {
        doc: doc.clone(),
        loc: pdf.view.anchor,
    };
    pdf.view.history.jump(from, Place { doc, loc: to });
    pdf.view.anchor = to;
}

/// Goes to match `index` of `doc`: picks the tab ([`pick_tab`]; `window` is where it was asked
/// for), focuses it the way `Action::FocusTab` does (its window comes forward), moves its view
/// and makes the match the current result. Returns false when no tab shows the document.
pub fn go_to_result(
    state: &mut AppState,
    doc: DocId,
    index: usize,
    window: Option<WindowId>,
    ctx: &egui::Context,
) -> bool {
    let Some((page, rect)) = state
        .global_search
        .matches(doc)
        .and_then(|ms| ms.get(index))
        .map(|m| (m.page, m.rects.first().copied().unwrap_or_default()))
    else {
        return false;
    };
    let Some(tab) = pick_tab(&tab_spots(state, doc), state.focus, window) else {
        return false;
    };
    if state.focus != Some(tab) {
        crate::action::dispatch(state, Action::FocusTab(tab), ctx);
    }
    state.global_search.set_current(doc, index);
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        reveal(pdf, page, rect);
    }
    ctx.request_repaint();
    true
}

/// `Action::GlobalSearchGoTo`: a click on a result row.
pub fn go_to_clicked(
    state: &mut AppState,
    doc: DocId,
    index: usize,
    window: WindowId,
    ctx: &egui::Context,
) {
    used(state, QueryUse::ResultClicked);
    go_to_result(state, doc, index, Some(window), ctx);
}

/// `Action::GlobalSearchStep` (F4, Shift+F4, Enter in the field): goes to the next or the
/// previous result ([`step_target`]). Without a current result it starts where the focused view
/// is when its document has matches. Without a query it shows the search view instead.
pub fn step(state: &mut AppState, forward: bool, ctx: &egui::Context) {
    if !state.global_search.has_query() {
        show(state, ctx);
        return;
    }
    let search = &state.global_search;
    let from = match search.current() {
        Some((doc, index)) => StepFrom::Current(doc, index),
        None => state
            .focused_pdf()
            .and_then(|(_, pdf)| {
                let matches = search.matches(pdf.doc).filter(|m| !m.is_empty())?;
                let before =
                    search::next_from(matches, pdf.view.anchor, false).unwrap_or(matches.len());
                Some(StepFrom::Position(pdf.doc, before))
            })
            .unwrap_or(StepFrom::Nowhere),
    };
    let Some((doc, index)) = step_target(&search.counts(), from, forward) else {
        return;
    };
    used(state, QueryUse::Step);
    let window = state.focused_window().id;
    go_to_result(state, doc, index, Some(window), ctx);
}

// ---------------------------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------------------------

fn input_id(window: WindowId) -> Id {
    Id::unique(("global_find_input", window))
}

/// Draws the search view of window `window` into `ui` (the sidebar below its view switch): the
/// input field with the options, the summary and the result list. `any_docs` says whether some
/// PDF tab is open.
pub fn show_view(
    ui: &mut Ui,
    search: &mut GlobalSearch,
    docs: &DocStore,
    any_docs: bool,
    window: WindowId,
    history: &SearchHistory,
    queue: &mut Vec<Action>,
) {
    ui.add_space(2.0);
    show_input(ui, search, window, history, queue);
    if let Some(message) = search.error() {
        ui.colored_label(
            ui.visuals().error_fg_color,
            tr("find.regex_error").replace("{}", message),
        );
    }
    let totals = search.totals();
    if let Some(text) = summary_text(
        &totals,
        any_docs,
        search.searched() && search.error().is_none(),
    ) {
        ui.label(RichText::new(text).weak());
    }
    if totals.truncated {
        ui.label(RichText::new(tr("gsearch.truncated")).color(ui.visuals().warn_fg_color));
    }
    ui.separator();
    show_results(ui, search, docs, window, queue);
}

/// The input field of the search view and its history, option and clear buttons, with their
/// keys. This is where the query is typed; the moments it counts as used are marked ([`used`]
/// is called for them from `dispatch`: Enter and F4 queue `GlobalSearchStep`, the clear button
/// `ClearGlobalSearch`, a result row `GlobalSearchGoTo`).
///
/// `history` is the search history, shared with the search bars of the views: ↑ / ↓ in the
/// field walk through it, the history button lists it, and a query that stays for a while with
/// results is recorded (FIND-13, FIND-14).
pub fn show_input(
    ui: &mut Ui,
    search: &mut GlobalSearch,
    window: WindowId,
    history: &SearchHistory,
    queue: &mut Vec<Action>,
) {
    let ctx = ui.ctx().clone();
    let now = ui.input(|i| i.time);
    let id = input_id(window);
    let history_id = id.with("history");
    let in_field = ui.memory(|m| m.has_focus(id));
    // An open history list has Esc and Enter.
    let list_open = search_history::is_list_open(&ctx, history_id);

    // ↑ / ↓ walk through the history. A one-line field would move its cursor with them, so they
    // are taken before it sees them; with Shift they stay the field's (they select).
    if in_field && !list_open {
        let (older, newer) = ui.input_mut(|i| {
            let walk = !i.modifiers.shift;
            (
                walk && i.consume_key(Modifiers::NONE, Key::ArrowUp),
                walk && i.consume_key(Modifiers::NONE, Key::ArrowDown),
            )
        });
        if older || newer {
            let current = search.query.clone();
            let recalled = if older {
                search.recall.older(history, &current)
            } else {
                search.recall.newer(history, &current)
            };
            if let Some(query) = recalled {
                search.use_query(query, window);
                ctx.request_repaint();
            }
        }
    }

    // The keys of the field, taken before the field sees them. The shortcut table does not see
    // keys while a text field has the keyboard, so F4 is handled here too. Shift is compared
    // loosely, so the variants with Shift are asked first.
    if in_field && !list_open {
        let (escape, prev, next, select) = ui.input_mut(|i| {
            let escape = i.consume_key(Modifiers::NONE, Key::Escape);
            let prev = i.consume_key(Modifiers::SHIFT, Key::Enter)
                || i.consume_key(Modifiers::SHIFT, Key::F4);
            let next = i.consume_key(Modifiers::NONE, Key::Enter)
                || i.consume_key(Modifiers::NONE, Key::F4);
            let select = i.consume_key(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::F);
            (escape, prev, next, select)
        });
        if escape {
            // The keyboard goes back to the focused view; the search stays.
            ui.memory_mut(|m| m.surrender_focus(id));
        } else if prev || next {
            // Used: stepping through the results (FIND-14).
            queue.push(Action::GlobalSearchStep { forward: next });
        } else if select {
            search.focus_input = Some(window);
        }
    }

    if search.focus_input == Some(window) {
        search.focus_input = None;
        let mut state = TextEdit::load_state(&ctx, id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::select_all_str(
                &search.query.text,
            )));
        TextEdit::store_state(&ctx, id, state);
        ui.memory_mut(|m| m.request_focus(id));
    }

    let mut edited = false;
    let mut options_changed = false;
    let mut clear = false;
    let mut history_event = None;
    let error = search.error().is_some();
    let no_result =
        search.searched() && search.totals() == Totals::default() && !search.query.text.is_empty();
    let dark = ui.visuals().dark_mode;
    ui.horizontal_wrapped(|ui| {
        let buttons = 5.0 * 30.0;
        let mut field = TextEdit::singleline(&mut search.query.text)
            .id(id)
            .hint_text(tr("gsearch.hint"))
            .desired_width((ui.available_width() - buttons).max(80.0));
        if error || no_result {
            field = field.background_color(if dark {
                Color32::from_rgb(92, 38, 38)
            } else {
                Color32::from_rgb(255, 208, 208)
            });
        }
        if ui.add(field).changed() {
            edited = true;
        }
        history_event = search_history::history_button(ui, history_id, history);
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
        if !search.query.text.is_empty()
            && ui.button("×").on_hover_text(tr("gsearch.clear")).clicked()
        {
            clear = true;
        }
    });

    match history_event {
        Some(HistoryEvent::Pick(query)) => {
            search.recall.reset();
            search.use_query(query, window);
            ctx.request_repaint();
        }
        Some(HistoryEvent::Remove(index)) => queue.push(Action::RemoveSearchHistory(index)),
        Some(HistoryEvent::Clear) => queue.push(Action::ClearSearchHistory),
        None => {}
    }
    if edited || options_changed {
        // What is in the field is the user's own again: ↑ starts from the newest entry.
        search.recall.reset();
    }
    if edited {
        search.edited(now);
        // The search starts when the delay is over; wake up then.
        ctx.request_repaint_after(Duration::from_secs_f64(search::INPUT_DELAY_SECS));
    }
    if options_changed {
        search.search_now();
        ctx.request_repaint();
    }
    if clear {
        // Used: the clear button (FIND-14); `dispatch` notes it before the query goes.
        queue.push(Action::ClearGlobalSearch);
        return;
    }

    // Settling: a query with results that stays for a while is recorded (FIND-14).
    let has_matches = search.has_current_matches();
    match search.settle.observe(&search.query, has_matches, now) {
        SettleStep::Idle => {}
        SettleStep::Wait(left) => ctx.request_repaint_after(left),
        SettleStep::Record { query, replaces } => {
            queue.push(Action::RecordSearch { query, replaces });
        }
    }
}

/// The result list: the rows that are on screen, and only those.
fn show_results(
    ui: &mut Ui,
    search: &mut GlobalSearch,
    docs: &DocStore,
    window: WindowId,
    queue: &mut Vec<Action>,
) {
    let rows = search.rows();
    if rows.is_empty() {
        return;
    }
    let row_h = ui.text_style_height(&TextStyle::Body) + 4.0;
    let stride = row_h + ui.spacing().item_spacing.y;
    let in_list = ui.rect_contains_pointer(ui.max_rect());
    let current = search.current();
    let follow_now = search.follow;
    let view = search.views.entry(window).or_default();
    // The list scrolls to a new current result, unless the pointer is on it (a click there).
    let follow = (view.followed != follow_now && !in_list)
        .then(|| {
            let (doc, index) = current?;
            let row = rows.row_of(doc, index)?;
            scroll_target(row, stride, view.offset, view.view_h)
        })
        .flatten();
    view.followed = follow_now;
    let mut area = ScrollArea::vertical()
        .id_salt(("global_results", window))
        .auto_shrink([false, false]);
    if let Some(offset) = follow {
        area = area.vertical_scroll_offset(offset);
    }

    let mut toggles = Vec::new();
    let output = area.show_rows(ui, row_h, rows.len(), |ui, range| {
        let mut infos: HashMap<DocId, Option<Arc<refdesk_core::DocumentInfo>>> = HashMap::new();
        for n in range {
            match rows.row(n) {
                Some(Row::Head {
                    doc,
                    count,
                    collapsed,
                }) => {
                    let path = docs.get(doc).map(|d| d.path().to_path_buf());
                    if head_row(ui, row_h, path.as_deref(), count, collapsed) {
                        toggles.push(doc);
                    }
                }
                Some(Row::Match { doc, index }) => {
                    let info = infos
                        .entry(doc)
                        .or_insert_with(|| docs.get(doc).and_then(|d| d.info()));
                    let labels = info.as_ref().map_or(&[][..], |i| i.page_labels.as_slice());
                    let Some(m) = search.matches(doc).and_then(|ms| ms.get(index)) else {
                        continue;
                    };
                    if match_row(ui, row_h, m, labels, current == Some((doc, index))) {
                        // Used: a result row was clicked (FIND-14).
                        queue.push(Action::GlobalSearchGoTo { doc, index, window });
                    }
                }
                None => {}
            }
        }
    });
    if let Some(view) = search.views.get_mut(&window) {
        view.offset = output.state.clamped_offset().y;
        view.view_h = output.inner_rect.height();
    }
    for doc in toggles {
        search.toggle_collapsed(doc);
    }
}

/// The accessible name of a document's head row: "geotopo.pdf, 34 results".
pub fn head_label(name: &str, count: usize) -> String {
    let key = if count == 1 {
        "gsearch.head_one"
    } else {
        "gsearch.head"
    };
    tr(key)
        .replace("{name}", name)
        .replace("{n}", &count.to_string())
}

/// The accessible name of a match row: "Page 12: …text before match text after".
pub fn match_label(m: &SearchMatch, labels: &[String]) -> String {
    let text = row_text(m);
    format!(
        "{}: {}{}{}",
        tr("link.page").replace("{page}", &page_label(m.page, labels)),
        text.before,
        text.matched,
        text.after
    )
}

/// Draws the head row of a document; returns whether it was clicked (to fold it).
fn head_row(ui: &mut Ui, row_h: f32, path: Option<&Path>, count: usize, collapsed: bool) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
    let name = path.map(file_name).unwrap_or_default();
    response
        .widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, head_label(&name, count)));
    ui.ctx().accesskit_node_builder(response.id, |node| {
        node.set_expanded(!collapsed);
    });
    let visuals = ui.visuals().clone();
    let fill = if response.hovered() {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        visuals.faint_bg_color
    };
    ui.painter().rect_filled(rect, 2.0, fill);
    let fold = Rect::from_min_size(rect.min, vec2(row_h, row_h));
    ui.painter()
        .add(triangle(fold, !collapsed, visuals.strong_text_color()));
    let font = ui.style().text_styles[&TextStyle::Body].clone();
    let count_galley =
        ui.painter()
            .layout_no_wrap(count.to_string(), font.clone(), visuals.weak_text_color());
    let count_pos = Pos2::new(
        rect.right() - 6.0 - count_galley.size().x,
        rect.center().y - count_galley.size().y / 2.0,
    );
    let name_width = (count_pos.x - fold.right() - 8.0).max(0.0);
    let galley = ui.painter().layout_job(line_job(
        &name,
        font,
        visuals.strong_text_color(),
        name_width,
    ));
    let pos = Pos2::new(fold.right(), rect.center().y - galley.size().y / 2.0);
    ui.painter()
        .galley(pos, galley, visuals.strong_text_color());
    ui.painter()
        .galley(count_pos, count_galley, visuals.weak_text_color());
    let response = match path {
        Some(p) => response.on_hover_text(p.display().to_string()),
        None => response,
    };
    response.clicked()
}

/// Draws the row of one match; returns whether it was clicked.
fn match_row(
    ui: &mut Ui,
    row_h: f32,
    m: &SearchMatch,
    labels: &[String],
    is_current: bool,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, match_label(m, labels)));
    let visuals = ui.visuals().clone();
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
    let font = ui.style().text_styles[&TextStyle::Body].clone();
    let label = ui.painter().layout_no_wrap(
        page_label(m.page, labels),
        font.clone(),
        visuals.weak_text_color(),
    );
    let left = rect.left() + MATCH_INDENT;
    let label_pos = Pos2::new(left, rect.center().y - label.size().y / 2.0);
    let text_left = left + label.size().x + 8.0;
    ui.painter()
        .galley(label_pos, label, visuals.weak_text_color());
    let job = match_job(
        &row_text(m),
        font,
        color,
        visuals.strong_text_color(),
        search::global_highlight_colors().1,
        (rect.right() - text_left - 4.0).max(0.0),
    );
    let galley = ui.painter().layout_job(job);
    let pos = Pos2::new(text_left, rect.center().y - galley.size().y / 2.0);
    ui.painter().galley(pos, galley, color);
    response.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use refdesk_core::testutil::PdfBuilder;
    use refdesk_core::{LayoutParams, Location, Selection, TextPos, ZoomMode};

    use crate::action::{OpenTarget, dispatch, dispatch_queued};
    use crate::commands::{self, MenuGroup};
    use crate::views::test_support::{Repaints, TIMEOUT, watch_repaints};
    use crate::windows::show_window;

    fn d(n: u64) -> DocId {
        DocId::for_test(n)
    }

    fn t(n: u64) -> TabId {
        TabId::for_test(n)
    }

    /// A match on `page` whose first line is at height `y`; `start` is its first character.
    fn m(page: u32, start: u32, y: f32) -> SearchMatch {
        SearchMatch {
            page,
            chars: start..start + 5,
            rects: vec![PageRect::new(72.0, y, 110.0, y + 12.0)],
            cont: None,
            before: "say ".into(),
            matched: "hello".into(),
            after: " there".into(),
        }
    }

    // --- which documents ---------------------------------------------------------------------

    #[test]
    fn targets_are_ordered_by_the_natural_order_of_their_names_then_by_path() {
        let docs = vec![
            (d(1), PathBuf::from("/x/doc10.pdf")),
            (d(2), PathBuf::from("/x/doc2.pdf")),
            (d(3), PathBuf::from("/x/B.pdf")),
            (d(4), PathBuf::from("/y/a.pdf")),
            (d(5), PathBuf::from("/x/a.pdf")),
            (d(2), PathBuf::from("/x/doc2.pdf")),
        ];
        let order: Vec<u64> = target_order(docs)
            .iter()
            .map(|(id, _)| match *id {
                id if id == d(1) => 1,
                id if id == d(2) => 2,
                id if id == d(3) => 3,
                id if id == d(4) => 4,
                _ => 5,
            })
            .collect();
        // a.pdf (two folders, by path), B.pdf (case does not count), doc2 before doc10.
        assert_eq!(order, vec![5, 4, 3, 2, 1]);
    }

    #[test]
    fn targets_are_the_documents_of_pdf_tabs_once_without_comparisons_and_start_pages() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let open = |state: &mut AppState, path: &str| {
            dispatch(
                state,
                Action::OpenFiles(vec![OpenTarget::parse(path)]),
                &ctx,
            );
            state.focus.expect("focus")
        };
        let a = open(&mut state, "/refdesk-test/wp55/doc10.pdf");
        let b = open(&mut state, "/refdesk-test/wp55/doc2.pdf");
        dispatch(&mut state, Action::DuplicateTab(a), &ctx);
        dispatch(
            &mut state,
            Action::StartDiff {
                left: PathBuf::from("/refdesk-test/wp55/old.pdf"),
                right: PathBuf::from("/refdesk-test/wp55/new.pdf"),
            },
            &ctx,
        );
        // A start page in a second window.
        let id = state.new_window_id();
        let start = state.tabs.insert(Tab::Start);
        let mut window = WindowState::new(id);
        window.dock.add_tab(start, None);
        state.windows.push(window);

        let found = targets(&state.windows, &state.tabs, &state.docs);
        let names: Vec<String> = found.iter().map(|(_, p)| file_name(p)).collect();
        assert_eq!(names, vec!["doc2.pdf", "doc10.pdf"]);
        let doc_of = |tab| state.tabs.pdf(tab).expect("pdf").doc;
        assert_eq!(found[0].0, doc_of(b));
        assert_eq!(found[1].0, doc_of(a));
    }

    // --- summary -----------------------------------------------------------------------------

    #[test]
    fn the_percent_is_over_the_pages_of_all_documents() {
        assert_eq!(progress_percent([(10, 10, true), (5, 30, false)]), Some(37));
        assert_eq!(progress_percent([(10, 10, true), (30, 30, true)]), None);
        // A document not known yet has no pages to count.
        assert_eq!(progress_percent([(0, 0, false)]), Some(0));
        assert_eq!(progress_percent([(0, 0, true), (1, 4, false)]), Some(25));
        assert_eq!(progress_percent(std::iter::empty()), None);
    }

    #[test]
    fn the_summary_tells_results_searching_nothing_and_no_documents() {
        let totals = Totals {
            matches: 124,
            docs_with_matches: 3,
            percent: None,
            truncated: false,
        };
        let found = tr("gsearch.summary")
            .replace("{docs}", "3")
            .replace("{n}", "124");
        assert_eq!(summary_text(&totals, true, true), Some(found.clone()));
        let running = Totals {
            percent: Some(62),
            ..totals
        };
        assert_eq!(
            summary_text(&running, true, true),
            Some(format!(
                "{found} {}",
                tr("gsearch.searching").replace("{percent}", "62")
            ))
        );
        let none = Totals::default();
        assert_eq!(
            summary_text(&none, true, true).as_deref(),
            Some(tr("gsearch.none"))
        );
        assert_eq!(
            summary_text(
                &Totals {
                    percent: Some(5),
                    ..none
                },
                true,
                true
            ),
            Some(tr("gsearch.searching_alone").replace("{percent}", "5"))
        );
        assert_eq!(summary_text(&none, true, false), None);
        assert_eq!(
            summary_text(&none, false, true).as_deref(),
            Some(tr("gsearch.no_docs"))
        );
    }

    // --- rows --------------------------------------------------------------------------------

    #[test]
    fn one_result_and_one_document_are_singular_in_english() {
        use crate::i18n::{Lang, lookup};
        let fill =
            |key: &str, n: &str, docs: &str| tr(key).replace("{n}", n).replace("{docs}", docs);
        assert_eq!(found_text(1, 1), fill("gsearch.summary_one", "1", "1"));
        assert_eq!(found_text(5, 1), fill("gsearch.summary_one_doc", "5", "1"));
        assert_eq!(found_text(5, 2), fill("gsearch.summary", "5", "2"));
        assert_eq!(
            head_label("a.pdf", 1),
            tr("gsearch.head_one")
                .replace("{name}", "a.pdf")
                .replace("{n}", "1")
        );
        assert_eq!(
            lookup(Lang::En, "gsearch.summary_one"),
            Some("{n} result in {docs} document")
        );
        assert_eq!(
            lookup(Lang::En, "gsearch.summary_one_doc"),
            Some("{n} results in {docs} document")
        );
        assert_eq!(
            lookup(Lang::En, "gsearch.head_one"),
            Some("{name}, {n} result")
        );
    }

    #[test]
    fn rows_are_a_head_per_document_with_matches_followed_by_its_matches() {
        let counts = [(d(1), 2), (d(2), 0), (d(3), 3)];
        let rows = RowIndex::build(&counts, &HashSet::new());
        assert_eq!(rows.len(), 1 + 2 + 1 + 3);
        let head = |doc, count| Row::Head {
            doc,
            count,
            collapsed: false,
        };
        let all: Vec<Row> = (0..rows.len()).filter_map(|n| rows.row(n)).collect();
        assert_eq!(
            all,
            vec![
                head(d(1), 2),
                Row::Match {
                    doc: d(1),
                    index: 0
                },
                Row::Match {
                    doc: d(1),
                    index: 1
                },
                head(d(3), 3),
                Row::Match {
                    doc: d(3),
                    index: 0
                },
                Row::Match {
                    doc: d(3),
                    index: 1
                },
                Row::Match {
                    doc: d(3),
                    index: 2
                },
            ]
        );
        assert_eq!(rows.row(7), None);
        assert_eq!(rows.row_of(d(1), 1), Some(2));
        assert_eq!(rows.row_of(d(3), 0), Some(4));
        assert_eq!(rows.row_of(d(3), 3), None);
        assert_eq!(
            rows.row_of(d(2), 0),
            None,
            "a document without matches has no row"
        );
    }

    #[test]
    fn a_folded_document_keeps_only_its_head() {
        let counts = [(d(1), 2), (d(3), 3)];
        let folded: HashSet<DocId> = [d(1)].into_iter().collect();
        let rows = RowIndex::build(&counts, &folded);
        assert_eq!(rows.len(), 1 + 1 + 3);
        assert_eq!(
            rows.row(0),
            Some(Row::Head {
                doc: d(1),
                count: 2,
                collapsed: true
            })
        );
        assert_eq!(
            rows.row(1),
            Some(Row::Head {
                doc: d(3),
                count: 3,
                collapsed: false
            })
        );
        assert_eq!(rows.row_of(d(1), 0), None);
        assert_eq!(rows.row_of(d(3), 2), Some(4));
        assert!(RowIndex::build(&[], &folded).is_empty());
    }

    #[test]
    fn rows_of_two_hundred_thousand_matches_are_looked_up_quickly() {
        let counts = [(d(1), 100_000), (d(2), 0), (d(3), 100_000)];
        let started = Instant::now();
        let rows = RowIndex::build(&counts, &HashSet::new());
        assert_eq!(rows.len(), 200_002);
        let mut x: u64 = 12345;
        for _ in 0..200_000 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let n = (x >> 33) as usize % rows.len();
            match rows.row(n) {
                Some(Row::Match { doc, index }) => assert_eq!(rows.row_of(doc, index), Some(n)),
                Some(Row::Head { .. }) => assert!(n == 0 || n == 100_001),
                None => panic!("row {n} is in range"),
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    // --- next and previous -------------------------------------------------------------------

    #[test]
    fn a_step_moves_within_a_document_and_over_to_the_next() {
        let counts = [(d(1), 2), (d(2), 0), (d(3), 3)];
        let step = |from, forward| step_target(&counts, from, forward);
        assert_eq!(step(StepFrom::Current(d(1), 0), true), Some((d(1), 1)));
        assert_eq!(step(StepFrom::Current(d(1), 1), true), Some((d(3), 0)));
        assert_eq!(step(StepFrom::Current(d(3), 0), false), Some((d(1), 1)));
        assert_eq!(step(StepFrom::Current(d(3), 2), false), Some((d(3), 1)));
    }

    #[test]
    fn a_step_past_the_end_wraps_to_the_start_and_back() {
        let counts = [(d(1), 2), (d(3), 3)];
        let step = |from, forward| step_target(&counts, from, forward);
        assert_eq!(step(StepFrom::Current(d(3), 2), true), Some((d(1), 0)));
        assert_eq!(step(StepFrom::Current(d(1), 0), false), Some((d(3), 2)));
        // One document with one match steps to itself.
        assert_eq!(
            step_target(&[(d(1), 1)], StepFrom::Current(d(1), 0), true),
            Some((d(1), 0))
        );
    }

    #[test]
    fn without_a_current_result_a_step_starts_where_the_focused_view_is() {
        let counts = [(d(1), 2), (d(2), 4), (d(3), 3)];
        let step = |from, forward| step_target(&counts, from, forward);
        // Two matches of document 2 lie above the view.
        assert_eq!(step(StepFrom::Position(d(2), 2), true), Some((d(2), 2)));
        assert_eq!(step(StepFrom::Position(d(2), 2), false), Some((d(2), 1)));
        // Below its last match the next document follows; above its first, the one before.
        assert_eq!(step(StepFrom::Position(d(2), 4), true), Some((d(3), 0)));
        assert_eq!(step(StepFrom::Position(d(2), 0), false), Some((d(1), 1)));
        // A focused document without matches, or no focused document: the very first or last.
        assert_eq!(step(StepFrom::Position(d(9), 0), true), Some((d(1), 0)));
        assert_eq!(step(StepFrom::Nowhere, true), Some((d(1), 0)));
        assert_eq!(step(StepFrom::Nowhere, false), Some((d(3), 2)));
        // A current result whose match is gone counts as none.
        assert_eq!(step(StepFrom::Current(d(1), 7), true), Some((d(1), 0)));
    }

    #[test]
    fn without_any_match_a_step_goes_nowhere() {
        assert_eq!(step_target(&[], StepFrom::Nowhere, true), None);
        assert_eq!(
            step_target(&[(d(1), 0)], StepFrom::Current(d(1), 0), false),
            None
        );
    }

    // --- which tab ---------------------------------------------------------------------------

    #[test]
    fn the_focused_tab_then_the_asking_window_then_other_windows_show_a_result() {
        let (w1, w2) = (WindowId::MAIN, crate::state::WindowId::MAIN);
        // A second window id that is not the main one.
        let mut state = AppState::new();
        let w2 = if w2 == w1 { state.new_window_id() } else { w2 };
        let spot = |tab, window, visible| TabSpot {
            tab: t(tab),
            window,
            visible,
        };
        let spots = [
            spot(1, w1, false),
            spot(2, w1, true),
            spot(3, w2, false),
            spot(4, w2, true),
        ];
        // ① The focused tab when it shows the document.
        assert_eq!(pick_tab(&spots, Some(t(3)), Some(w1)), Some(t(3)));
        // ② Else a tab of the asking window, a visible one first.
        assert_eq!(pick_tab(&spots, Some(t(9)), Some(w1)), Some(t(2)));
        assert_eq!(pick_tab(&spots, None, Some(w2)), Some(t(4)));
        assert_eq!(
            pick_tab(&[spot(1, w1, false), spot(3, w2, false)], None, Some(w2)),
            Some(t(3))
        );
        // ③ Else a tab of another window, a visible one first.
        assert_eq!(pick_tab(&spots[2..], Some(t(9)), Some(w1)), Some(t(4)));
        assert_eq!(pick_tab(&spots[..1], None, Some(w2)), Some(t(1)));
        // No tab shows it (any more).
        assert_eq!(pick_tab(&[], Some(t(1)), Some(w1)), None);
        drop(state);
    }

    // --- highlights without a document -------------------------------------------------------

    fn layout() -> PageLayout {
        let sizes = vec![Size::new(595.0, 842.0); 6];
        PageLayout::new(
            &sizes,
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
    fn marks_give_the_rectangles_of_the_visible_pages_and_tell_the_current_one() {
        let matches = vec![m(0, 0, 100.0), m(0, 40, 300.0), m(4, 0, 100.0)];
        let index = PageIndex::build(&matches, 6);
        let marks = DocMarks {
            index: &index,
            matches: &matches,
            current: Some(1),
        };
        let layout = layout();
        let rects = marks.rects(&layout, Point::new(0.0, 0.0), Size::new(800.0, 600.0));
        assert_eq!(rects.len(), 2, "page 4 is off screen: {rects:?}");
        assert_eq!(rects.iter().filter(|r| r.current).count(), 1);
        assert!(rects[1].current);
    }

    // --- with documents ----------------------------------------------------------------------

    /// A folder for the test's documents, removed at the end.
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("refdesk-wp55-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir");
            Dir(path)
        }

        /// A document of `pages` pages with "say hello there" at the (page, y) places.
        fn doc(&self, name: &str, pages: usize, hits: &[(usize, f32)]) -> PathBuf {
            let mut b = PdfBuilder::new();
            for page in 0..pages {
                b = b.page(595.0, 842.0, |p| {
                    p.text(72.0, 60.0, 12.0, &format!("heading {page}"));
                    for (at, y) in hits {
                        if *at == page {
                            p.text(72.0, *y, 12.0, "say hello there");
                        }
                    }
                });
            }
            let path = self.0.join(name);
            std::fs::write(&path, b.build()).expect("write pdf");
            path
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Rig {
        ctx: egui::Context,
        repaints: Repaints,
        state: AppState,
        time: f64,
        /// How far the clock goes with every frame. Tests of the two-second settling set it to
        /// 0 and move `time` themselves: frames run as often as the searches ask for them.
        step: f64,
    }

    impl Rig {
        fn new() -> Rig {
            let ctx = egui::Context::default();
            let repaints = watch_repaints(&ctx);
            Rig {
                ctx,
                repaints,
                state: AppState::new(),
                time: 0.0,
                step: 1.0 / 60.0,
            }
        }

        /// Opens `path` in the focused pane and runs frames until its view is laid out.
        fn open(&mut self, path: &Path) -> TabId {
            let target = OpenTarget {
                path: path.to_path_buf(),
                page: None,
            };
            dispatch(&mut self.state, Action::OpenFiles(vec![target]), &self.ctx);
            let tab = self.state.focus.expect("focused");
            self.frames_until("the layout", |rig| {
                rig.state
                    .tabs
                    .pdf(tab)
                    .is_some_and(|p| p.ui.layout().is_some())
            });
            tab
        }

        fn doc(&self, tab: TabId) -> DocId {
            self.state.tabs.pdf(tab).expect("pdf tab").doc
        }

        fn input(&mut self, events: Vec<egui::Event>) -> egui::RawInput {
            self.time += self.step;
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 700.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            }
        }

        /// One frame: the logic step of the search, the window, then the actions it queued.
        fn frame(&mut self, events: Vec<egui::Event>) {
            tick(&mut self.state, &self.ctx);
            let input = self.input(events);
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

        fn frames_until(&mut self, what: &str, mut check: impl FnMut(&Rig) -> bool) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.frame(Vec::new());
                if check(self) {
                    return;
                }
                self.repaints.wait_one(what, deadline);
            }
        }

        /// Searches all documents for `text` and runs frames until every search finished.
        fn search(&mut self, text: &str) {
            dispatch(
                &mut self.state,
                Action::GlobalSearchFor(text.to_owned()),
                &self.ctx,
            );
            self.settle();
        }

        /// Runs frames until every search finished.
        fn settle(&mut self) {
            self.frames_until("the searches to finish", |rig| {
                rig.state.global_search.finished()
            });
        }

        fn counts(&self) -> Vec<(DocId, usize)> {
            self.state.global_search.counts()
        }

        /// The texts of the accessibility tree of one drawn frame of the main window.
        fn texts(&mut self) -> Vec<String> {
            self.ctx.enable_accesskit();
            tick(&mut self.state, &self.ctx);
            let input = self.input(Vec::new());
            let state = &mut self.state;
            let mut queue = Vec::new();
            let output = self
                .ctx
                .run_ui(input, |ui| show_window(ui, state, 0, &mut queue));
            let update = output.platform_output.accesskit_update.clone();
            output.drop_without_applying_deltas();
            update
                .map(|u| u.nodes)
                .unwrap_or_default()
                .into_iter()
                .flat_map(|(_, n)| [n.label().map(str::to_owned), n.value().map(str::to_owned)])
                .flatten()
                .collect()
        }
    }

    /// Three documents with 2, 1 and 3 matches, and one without.
    fn three_docs(rig: &mut Rig, dir: &Dir) -> [TabId; 4] {
        let a = rig.open(&dir.doc("alpha.pdf", 6, &[(1, 100.0), (4, 500.0)]));
        let b = rig.open(&dir.doc("beta.pdf", 3, &[(2, 300.0)]));
        let c = rig.open(&dir.doc("gamma.pdf", 6, &[(0, 100.0), (0, 400.0), (5, 700.0)]));
        let z = rig.open(&dir.doc("zeta.pdf", 2, &[]));
        [a, b, c, z]
    }

    #[test]
    fn three_documents_are_searched_at_once_with_counts_per_document_and_in_total() {
        let dir = Dir::new("three");
        let mut rig = Rig::new();
        let [a, b, c, z] = three_docs(&mut rig, &dir);
        rig.search("hello");
        let counts = rig.counts();
        assert_eq!(
            counts,
            vec![
                (rig.doc(a), 2),
                (rig.doc(b), 1),
                (rig.doc(c), 3),
                (rig.doc(z), 0)
            ]
        );
        let totals = rig.state.global_search.totals();
        assert_eq!(totals.matches, 6);
        assert_eq!(totals.docs_with_matches, 3);
        assert_eq!(totals.percent, None);
        assert_eq!(
            summary_text(&totals, true, true),
            Some(
                tr("gsearch.summary")
                    .replace("{docs}", "3")
                    .replace("{n}", "6")
            )
        );
        // The document without a match has no row.
        let rows = rig.state.global_search.rows();
        assert_eq!(rows.len(), 3 + 6);
        let heads: Vec<DocId> = (0..rows.len())
            .filter_map(|n| match rows.row(n) {
                Some(Row::Head { doc, .. }) => Some(doc),
                _ => None,
            })
            .collect();
        assert_eq!(heads, vec![rig.doc(a), rig.doc(b), rig.doc(c)]);
        // Nothing changed: the rows are not made again.
        assert!(Arc::ptr_eq(&rows, &rig.state.global_search.rows()));
        assert_eq!(rig.state.global_search.running_jobs(), 4);
    }

    #[test]
    fn a_typed_query_waits_for_the_input_delay_options_start_at_once_and_new_input_drops_old_jobs()
    {
        let dir = Dir::new("delay");
        let mut rig = Rig::new();
        rig.open(&dir.doc("one.pdf", 2, &[(0, 100.0)]));
        rig.open(&dir.doc("two.pdf", 2, &[(1, 100.0)]));
        let found = targets(&rig.state.windows, &rig.state.tabs, &rig.state.docs);
        let notifier: Notifier = Arc::new(|| {});
        let search = &mut rig.state.global_search;
        let docs = &rig.state.docs;
        search.query.text = "hello".into();
        search.edited(10.0);
        let wait = search.update(&found, docs, &notifier, 10.05);
        assert_eq!(search.starts, 0, "not within the delay");
        assert!(wait.is_some_and(|w| w.as_secs_f64() <= search::INPUT_DELAY_SECS));
        search.update(
            &found,
            docs,
            &notifier,
            10.0 + search::INPUT_DELAY_SECS + 0.01,
        );
        assert_eq!(
            search.starts, 2,
            "one job per document once the delay is over"
        );
        // Typing again within the delay starts nothing; after it, both again, replacing the jobs.
        search.query.text = "hell".into();
        search.edited(11.0);
        search.update(&found, docs, &notifier, 11.1);
        assert_eq!(search.starts, 2);
        search.update(&found, docs, &notifier, 11.2);
        assert_eq!(search.starts, 4);
        assert_eq!(search.running_jobs(), 2);
        // An option starts at once.
        search.query.case_sensitive = true;
        search.search_now();
        search.update(&found, docs, &notifier, 11.21);
        assert_eq!(search.starts, 6);
        // Nothing due: nothing starts.
        search.update(&found, docs, &notifier, 20.0);
        assert_eq!(search.starts, 6);
    }

    #[test]
    fn an_opened_document_joins_the_search_and_a_closed_one_leaves_it() {
        let dir = Dir::new("join");
        let mut rig = Rig::new();
        let a = rig.open(&dir.doc("a.pdf", 3, &[(0, 100.0), (2, 100.0)]));
        rig.search("hello");
        assert_eq!(rig.counts(), vec![(rig.doc(a), 2)]);
        // Opened while the search is on: searched too.
        let b = rig.open(&dir.doc("b.pdf", 3, &[(1, 100.0)]));
        rig.settle();
        assert_eq!(rig.counts(), vec![(rig.doc(a), 2), (rig.doc(b), 1)]);
        // Two tabs of a: closing one leaves it in.
        dispatch(&mut rig.state, Action::DuplicateTab(a), &rig.ctx);
        let a2 = rig.state.focus.expect("the copy");
        dispatch(&mut rig.state, Action::CloseTab(a), &rig.ctx);
        rig.frame(Vec::new());
        assert_eq!(rig.counts(), vec![(rig.doc(a2), 2), (rig.doc(b), 1)]);
        // Closing the last one drops it, with its highlights.
        let doc_a = rig.doc(a2);
        dispatch(&mut rig.state, Action::CloseTab(a2), &rig.ctx);
        rig.frame(Vec::new());
        assert_eq!(rig.counts(), vec![(rig.doc(b), 1)]);
        assert!(rig.state.global_search.marks(doc_a).is_none());
        assert_eq!(rig.state.global_search.totals().matches, 1);
    }

    #[test]
    fn a_reloaded_document_alone_is_searched_again() {
        let dir = Dir::new("reload");
        let mut rig = Rig::new();
        let a = rig.open(&dir.doc("a.pdf", 2, &[(0, 100.0)]));
        let b = rig.open(&dir.doc("b.pdf", 2, &[(1, 100.0)]));
        rig.search("hello");
        assert_eq!(rig.state.global_search.starts, 2);
        let handle = rig.state.docs.get(rig.doc(a)).expect("doc").clone();
        let before = handle.info().map(|i| i.generation);
        handle.reload();
        rig.frames_until("the reload", |_| {
            handle
                .info()
                .map(|i| i.generation)
                .is_some_and(|g| Some(g) != before)
        });
        rig.settle();
        assert_eq!(rig.state.global_search.starts, 3, "only the reloaded one");
        assert_eq!(rig.counts(), vec![(rig.doc(a), 1), (rig.doc(b), 1)]);
    }

    #[test]
    fn an_invalid_regex_is_an_error_without_results_and_an_empty_query_searches_nothing() {
        let dir = Dir::new("regex");
        let mut rig = Rig::new();
        let a = rig.open(&dir.doc("a.pdf", 2, &[(0, 100.0)]));
        rig.state.global_search.query.regex = true;
        dispatch(
            &mut rig.state,
            Action::GlobalSearchFor("hel(lo".into()),
            &rig.ctx,
        );
        rig.frame(Vec::new());
        rig.frame(Vec::new());
        let search = &rig.state.global_search;
        assert!(search.error().is_some());
        assert_eq!(search.totals(), Totals::default());
        assert_eq!(search.running_jobs(), 0);
        assert!(search.marks(rig.doc(a)).is_none());
        // Fixed, it searches.
        rig.search("hel+o");
        assert!(rig.state.global_search.error().is_none());
        assert_eq!(rig.counts(), vec![(rig.doc(a), 1)]);
        // Emptied, nothing runs and nothing is shown.
        let now = rig.time;
        rig.state.global_search.query.text.clear();
        rig.state.global_search.edited(now - 1.0);
        rig.frame(Vec::new());
        let search = &rig.state.global_search;
        assert_eq!(search.running_jobs(), 0);
        assert!(!search.searched());
        assert_eq!(search.totals(), Totals::default());
        let starts = search.starts;
        rig.frame(Vec::new());
        assert_eq!(rig.state.global_search.starts, starts);
    }

    #[test]
    fn stepping_as_often_as_there_are_matches_visits_each_once_and_comes_back() {
        let dir = Dir::new("steps");
        let mut rig = Rig::new();
        let [a, b, c, _] = three_docs(&mut rig, &dir);
        rig.search("hello");
        // From the top of the first document.
        dispatch(&mut rig.state, Action::FocusTab(a), &rig.ctx);
        if let Some(Tab::Pdf(pdf)) = rig.state.tabs.get_mut(a) {
            pdf.view.anchor = Location::new(0, 0.0, 0.0);
        }
        let mut seen = Vec::new();
        for _ in 0..6 {
            dispatch(
                &mut rig.state,
                Action::GlobalSearchStep { forward: true },
                &rig.ctx,
            );
            seen.push(rig.state.global_search.current().expect("a result"));
        }
        let expect: Vec<(DocId, usize)> = [(a, 0), (a, 1), (b, 0), (c, 0), (c, 1), (c, 2)]
            .iter()
            .map(|(tab, i)| (rig.doc(*tab), *i))
            .collect();
        assert_eq!(seen, expect);
        // Each step focused the tab of the result.
        assert_eq!(rig.state.focus, Some(c));
        dispatch(
            &mut rig.state,
            Action::GlobalSearchStep { forward: true },
            &rig.ctx,
        );
        assert_eq!(rig.state.global_search.current(), Some(expect[0]));
        assert_eq!(rig.state.focus, Some(a));
        dispatch(
            &mut rig.state,
            Action::GlobalSearchStep { forward: false },
            &rig.ctx,
        );
        assert_eq!(rig.state.global_search.current(), Some(expect[5]));
        assert_eq!(rig.state.global_search.last_use, Some(QueryUse::Step));
    }

    #[test]
    fn going_to_a_result_focuses_its_tab_moves_its_view_and_back_returns() {
        let dir = Dir::new("goto");
        let mut rig = Rig::new();
        let a = rig.open(&dir.doc("a.pdf", 6, &[(0, 100.0)]));
        let b = rig.open(&dir.doc("b.pdf", 6, &[(4, 500.0)]));
        rig.search("hello");
        dispatch(&mut rig.state, Action::FocusTab(a), &rig.ctx);
        let before = rig.state.tabs.pdf(b).expect("b").view.anchor;
        let main = rig.state.main_window().id;
        let doc_b = rig.doc(b);
        dispatch(
            &mut rig.state,
            Action::GlobalSearchGoTo {
                doc: doc_b,
                index: 0,
                window: main,
            },
            &rig.ctx,
        );
        assert_eq!(rig.state.focus, Some(b));
        assert_eq!(rig.state.main_window().dock.focused_tab(), Some(b));
        assert_eq!(rig.state.tabs.pdf(b).expect("b").view.anchor.page, 4);
        assert_eq!(rig.state.global_search.current(), Some((rig.doc(b), 0)));
        assert_eq!(
            rig.state.global_search.last_use,
            Some(QueryUse::ResultClicked)
        );
        dispatch(&mut rig.state, Action::Back(b), &rig.ctx);
        assert_eq!(rig.state.tabs.pdf(b).expect("b").view.anchor, before);
        // A result of a document no tab shows any more goes nowhere.
        dispatch(&mut rig.state, Action::CloseTab(b), &rig.ctx);
        assert!(!go_to_result(
            &mut rig.state,
            doc_b,
            0,
            Some(main),
            &rig.ctx
        ));
    }

    #[test]
    fn going_to_a_result_in_another_window_brings_that_window_forward() {
        // Headless, only the root viewport keeps commands, so the result is in the main window
        // and the focus in another one.
        let dir = Dir::new("raise");
        let mut rig = Rig::new();
        let a = rig.open(&dir.doc("a.pdf", 3, &[(0, 100.0)]));
        let b = rig.open(&dir.doc("b.pdf", 3, &[(2, 100.0)]));
        rig.search("hello");
        dispatch(&mut rig.state, Action::DetachTab(a), &rig.ctx);
        assert_eq!(rig.state.windows.len(), 2);
        dispatch(&mut rig.state, Action::FocusTab(a), &rig.ctx);
        let other = rig.state.windows[1].id;
        let doc_b = rig.doc(b);
        let state = &mut rig.state;
        let ctx = rig.ctx.clone();
        let output = ctx.run_ui(egui::RawInput::default(), |_ui| {
            dispatch(
                state,
                Action::GlobalSearchGoTo {
                    doc: doc_b,
                    index: 0,
                    window: other,
                },
                &ctx,
            );
        });
        let sent = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();
        assert!(sent.contains(&egui::ViewportCommand::Focus), "{sent:?}");
        assert_eq!(rig.state.focus, Some(b));
    }

    #[test]
    fn show_global_search_opens_the_sidebar_on_search_and_takes_a_selected_word() {
        let dir = Dir::new("show");
        let mut rig = Rig::new();
        let a = rig.open(&dir.doc("a.pdf", 2, &[(0, 100.0)]));
        let main = rig.state.main_window().id;
        assert!(!rig.state.main_window().toc_open);
        dispatch(&mut rig.state, Action::ShowGlobalSearch, &rig.ctx);
        let w = rig.state.main_window();
        assert!(w.toc_open);
        assert_eq!(w.sidebar_view, SidebarView::Search);
        assert_eq!(rig.state.global_search.focus_input, Some(main));
        assert!(!rig.state.global_search.has_query(), "nothing selected");
        rig.frame(Vec::new());
        assert!(rig.ctx.text_edit_focused(), "the field has the keyboard");

        // A selected word becomes the query and is searched at once.
        let handle = rig.state.docs.get(rig.doc(a)).expect("doc").clone();
        let text = handle.page_text_wait(0).expect("page text");
        let chars: String = text.chars.iter().map(|c| c.ch).collect();
        let at = chars.find("hello").expect("hello") as u32;
        if let Some(Tab::Pdf(pdf)) = rig.state.tabs.get_mut(a) {
            pdf.ui.selection.set_for_test(Selection {
                anchor: TextPos { page: 0, ch: at },
                focus: TextPos {
                    page: 0,
                    ch: at + 5,
                },
            });
        }
        dispatch(&mut rig.state, Action::ShowGlobalSearch, &rig.ctx);
        assert_eq!(rig.state.global_search.query.text, "hello");
        rig.settle();
        assert_eq!(rig.counts(), vec![(rig.doc(a), 1)]);

        // The explorer command turns the sidebar back to its files.
        dispatch(&mut rig.state, Action::ShowExplorer, &rig.ctx);
        assert_eq!(rig.state.main_window().sidebar_view, SidebarView::Files);
        // Hiding and showing the whole sidebar keeps the view.
        dispatch(&mut rig.state, Action::ShowGlobalSearch, &rig.ctx);
        dispatch(&mut rig.state, Action::ToggleToc, &rig.ctx);
        dispatch(&mut rig.state, Action::ToggleToc, &rig.ctx);
        assert_eq!(rig.state.main_window().sidebar_view, SidebarView::Search);
        // Folding a section is a files thing too.
        dispatch(
            &mut rig.state,
            Action::ToggleSidebarSection {
                window: main,
                section: crate::panels::sidebar::SidebarSection::Outline,
            },
            &rig.ctx,
        );
        assert_eq!(rig.state.main_window().sidebar_view, SidebarView::Files);
    }

    #[test]
    fn f4_without_a_query_shows_the_search_view() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(&mut state, Action::GlobalSearchStep { forward: true }, &ctx);
        assert!(state.main_window().toc_open);
        assert_eq!(state.main_window().sidebar_view, SidebarView::Search);
    }

    #[test]
    fn clearing_drops_the_query_the_results_and_the_highlights() {
        let dir = Dir::new("clear");
        let mut rig = Rig::new();
        let a = rig.open(&dir.doc("a.pdf", 2, &[(0, 100.0)]));
        rig.search("hello");
        assert!(rig.state.global_search.marks(rig.doc(a)).is_some());
        dispatch(&mut rig.state, Action::ClearGlobalSearch, &rig.ctx);
        let search = &rig.state.global_search;
        assert_eq!(search.last_use, Some(QueryUse::Cleared));
        assert!(!search.has_query() && !search.searched());
        assert_eq!(search.totals(), Totals::default());
        assert!(search.marks(rig.doc(a)).is_none());
        assert_eq!(search.running_jobs(), 0);
        assert!(search.doc_ids().is_empty());
        let starts = search.starts;
        rig.frame(Vec::new());
        assert_eq!(
            rig.state.global_search.starts, starts,
            "nothing starts again"
        );
    }

    #[test]
    fn views_tint_the_matches_of_their_document_even_with_their_own_search_closed() {
        let dir = Dir::new("marks");
        let mut rig = Rig::new();
        let a = rig.open(&dir.doc("a.pdf", 4, &[(0, 100.0), (0, 300.0), (3, 100.0)]));
        let old = dir.doc("old.pdf", 1, &[(0, 100.0)]);
        rig.search("hello");
        let pdf = rig.state.tabs.pdf(a).expect("a");
        assert!(!pdf.ui.search.open, "the view's own search is closed");
        let rects_of = |rig: &Rig| {
            let tab = rig.state.tabs.get(a).expect("tab");
            let pdf = rig.state.tabs.pdf(a).expect("a");
            let layout = pdf.ui.layout().expect("laid out");
            marks_for_tab(&rig.state.global_search, tab)
                .map(|m| m.rects(layout, pdf.ui.scroll(), pdf.ui.viewport()))
                .unwrap_or_default()
        };
        let rects = rects_of(&rig);
        assert!(!rects.is_empty());
        assert!(rects.iter().all(|r| !r.current));
        rig.state.global_search.set_current(rig.doc(a), 1);
        let rects = rects_of(&rig);
        assert_eq!(rects.iter().filter(|r| r.current).count(), 1);
        // A comparison tab of the same document gets no marks.
        let path = rig
            .state
            .docs
            .get(rig.doc(a))
            .expect("doc")
            .path()
            .to_path_buf();
        dispatch(
            &mut rig.state,
            Action::StartDiff {
                left: old,
                right: path,
            },
            &rig.ctx,
        );
        let diff = rig.state.focus.expect("the comparison");
        let tab = rig.state.tabs.get(diff).expect("tab");
        assert!(matches!(tab, Tab::Diff(_)));
        assert!(marks_for_tab(&rig.state.global_search, tab).is_none());
        // Cleared: nothing.
        dispatch(&mut rig.state, Action::ClearGlobalSearch, &rig.ctx);
        assert!(rects_of(&rig).is_empty());
    }

    #[test]
    fn the_search_view_draws_the_summary_heads_and_lines_for_the_accessibility_tree() {
        let dir = Dir::new("a11y");
        let mut rig = Rig::new();
        let [a, ..] = three_docs(&mut rig, &dir);
        dispatch(&mut rig.state, Action::ShowGlobalSearch, &rig.ctx);
        rig.search("hello");
        rig.state.global_search.set_current(rig.doc(a), 1);
        for _ in 0..2 {
            rig.frame(Vec::new());
        }
        let texts = rig.texts();
        let has = |needle: &str| texts.iter().any(|t| t.contains(needle));
        let summary = tr("gsearch.summary")
            .replace("{docs}", "3")
            .replace("{n}", "6");
        assert!(has(&summary), "{summary} in {texts:?}");
        assert!(has(&head_label("alpha.pdf", 2)), "{texts:?}");
        assert!(has(&head_label("gamma.pdf", 3)), "{texts:?}");
        assert!(
            !has(&head_label("zeta.pdf", 0)),
            "no row for a document without matches"
        );
        // The second match of alpha.pdf, on its fifth page, with the text around it.
        let line = format!(
            "{}: heading 4 say hello there",
            tr("link.page").replace("{page}", "5")
        );
        assert!(
            texts.iter().any(|t| t.starts_with(&line)),
            "{line} in {texts:?}"
        );
        let switch = tr("sidebar.search_count").replace("{n}", "6");
        assert!(has(&switch), "{switch} in {texts:?}");

        // Folding a document by its head drops its lines.
        rig.state.global_search.toggle_collapsed(rig.doc(a));
        rig.frame(Vec::new());
        let texts = rig.texts();
        assert!(texts.iter().any(|t| t == &head_label("alpha.pdf", 2)));
        assert!(!texts.iter().any(|t| t.starts_with(&line)));

        // The files view draws the explorer and the contents as before.
        rig.state.windows[0].sidebar_view = SidebarView::Files;
        rig.frame(Vec::new());
        let texts = rig.texts();
        let has = |needle: &str| texts.iter().any(|t| t.contains(needle));
        assert!(has(tr("explorer.no_folder")), "the explorer: {texts:?}");
        assert!(has(tr("toc.empty")), "the contents: {texts:?}");
        assert!(!has(&summary));
    }

    /// The bounds of the accessibility node whose label starts with `prefix`, in one drawn frame.
    fn node_center(rig: &mut Rig, prefix: &str) -> Pos2 {
        rig.ctx.enable_accesskit();
        tick(&mut rig.state, &rig.ctx);
        let input = rig.input(Vec::new());
        let state = &mut rig.state;
        let mut queue = Vec::new();
        let output = rig
            .ctx
            .run_ui(input, |ui| show_window(ui, state, 0, &mut queue));
        let update = output.platform_output.accesskit_update.clone();
        output.drop_without_applying_deltas();
        let nodes = update.map(|u| u.nodes).unwrap_or_default();
        let b = nodes
            .iter()
            .find(|(_, n)| n.label().is_some_and(|l| l.starts_with(prefix)))
            .and_then(|(_, n)| n.bounds())
            .unwrap_or_else(|| panic!("no node {prefix}"));
        Pos2::new(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32)
    }

    fn click(rig: &mut Rig, pos: Pos2) {
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        rig.frame(vec![egui::Event::PointerMoved(pos)]);
        rig.frame(vec![button(true)]);
        rig.frame(vec![button(false)]);
    }

    #[test]
    fn clicking_a_line_goes_to_its_match_and_clicking_a_head_folds_it() {
        let dir = Dir::new("click");
        let mut rig = Rig::new();
        let [a, b, ..] = three_docs(&mut rig, &dir);
        dispatch(&mut rig.state, Action::ShowGlobalSearch, &rig.ctx);
        rig.search("hello");
        assert_ne!(rig.state.focus, Some(b));
        // beta.pdf's only match is on its third page.
        let line = format!("{}: ", tr("link.page").replace("{page}", "3"));
        let at = node_center(&mut rig, &line);
        click(&mut rig, at);
        assert_eq!(rig.state.global_search.current(), Some((rig.doc(b), 0)));
        assert_eq!(rig.state.focus, Some(b));
        assert_eq!(rig.state.tabs.pdf(b).expect("b").view.anchor.page, 2);
        // The head of alpha.pdf folds its lines away.
        let at = node_center(&mut rig, &head_label("alpha.pdf", 2));
        click(&mut rig, at);
        let rows = rig.state.global_search.rows();
        assert_eq!(
            rows.row(0),
            Some(Row::Head {
                doc: rig.doc(a),
                count: 2,
                collapsed: true
            })
        );
    }

    #[test]
    fn typing_in_the_field_searches_and_enter_steps() {
        let dir = Dir::new("typing");
        let mut rig = Rig::new();
        let a = rig.open(&dir.doc("a.pdf", 3, &[(0, 100.0), (2, 100.0)]));
        dispatch(&mut rig.state, Action::ShowGlobalSearch, &rig.ctx);
        rig.frame(Vec::new());
        rig.frame(vec![egui::Event::Text("hello".into())]);
        assert_eq!(rig.state.global_search.query.text, "hello");
        rig.settle();
        assert_eq!(rig.counts(), vec![(rig.doc(a), 2)]);
        let key = |key, modifiers| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        };
        rig.frame(vec![key(Key::Enter, Modifiers::NONE)]);
        let first = rig.state.global_search.current().expect("a result");
        assert!(rig.ctx.text_edit_focused(), "the field keeps the keyboard");
        rig.frame(vec![key(Key::F4, Modifiers::NONE)]);
        let second = rig.state.global_search.current().expect("a result");
        assert_ne!(first, second);
        rig.frame(vec![
            egui::Event::ModifiersChanged(Modifiers::SHIFT),
            key(Key::Enter, Modifiers::SHIFT),
        ]);
        assert_eq!(rig.state.global_search.current(), Some(first));
        // Esc gives the keyboard back and keeps the search.
        rig.frame(vec![
            egui::Event::ModifiersChanged(Modifiers::NONE),
            key(Key::Escape, Modifiers::NONE),
        ]);
        rig.frame(Vec::new());
        assert!(!rig.ctx.text_edit_focused());
        assert!(rig.state.global_search.has_query());
        // From the view, F4 goes through the command table.
        rig.frame(vec![key(Key::F4, Modifiers::NONE)]);
        assert_eq!(rig.state.global_search.current(), Some(second));
    }

    #[test]
    fn the_new_commands_are_in_the_edit_menu_with_their_keys() {
        let edit: Vec<&commands::Command> = commands::COMMANDS
            .iter()
            .filter(|c| c.menu == MenuGroup::Edit)
            .collect();
        let find = |id: &str| {
            edit.iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("{id} in the edit menu"))
        };
        let ctrl_shift = Modifiers::CTRL.plus(Modifiers::SHIFT);
        assert_eq!(
            find("edit.find_all").shortcut,
            Some(egui::KeyboardShortcut::new(ctrl_shift, Key::F))
        );
        assert_eq!(
            find("edit.find_all_next").shortcut,
            Some(egui::KeyboardShortcut::new(Modifiers::NONE, Key::F4))
        );
        assert_eq!(
            find("edit.find_all_prev").shortcut,
            Some(egui::KeyboardShortcut::new(Modifiers::SHIFT, Key::F4))
        );
        let state = AppState::new();
        assert_eq!(
            find("edit.find_all").run(&state),
            Some(Action::ShowGlobalSearch)
        );
        assert_eq!(
            find("edit.find_all_prev").run(&state),
            Some(Action::GlobalSearchStep { forward: false })
        );
        assert_eq!(
            find("edit.find_all_clear").run(&state),
            None,
            "nothing to clear"
        );
        // "Find in All Documents" comes right after "Find" and "Find in This Chapter".
        let ids: Vec<&str> = edit.iter().map(|c| c.id).collect();
        let at = ids.iter().position(|i| *i == "edit.find").expect("find");
        assert_eq!(ids[at + 1], "edit.find_in_chapter");
        assert_eq!(ids[at + 2], "edit.find_all");
    }

    #[test]
    fn ctrl_shift_f_and_shift_f4_are_told_apart_from_ctrl_f_and_f4() {
        let dir = Dir::new("keys");
        let mut rig = Rig::new();
        rig.open(&dir.doc("a.pdf", 2, &[(0, 100.0)]));
        let ctrl_shift = Modifiers::CTRL.plus(Modifiers::SHIFT);
        let key = |key, modifiers| {
            vec![
                egui::Event::ModifiersChanged(modifiers),
                egui::Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
            ]
        };
        let input = rig.input(key(Key::F, ctrl_shift));
        let state = &rig.state;
        let mut action = None;
        rig.ctx
            .run_ui(input, |ui| {
                action = commands::handle_shortcuts(ui.ctx(), state)
            })
            .drop_without_applying_deltas();
        assert_eq!(action, Some(Action::ShowGlobalSearch));
        let input = rig.input(key(Key::F4, Modifiers::SHIFT));
        let state = &rig.state;
        rig.ctx
            .run_ui(input, |ui| {
                action = commands::handle_shortcuts(ui.ctx(), state)
            })
            .drop_without_applying_deltas();
        assert_eq!(action, Some(Action::GlobalSearchStep { forward: false }));
    }

    // -----------------------------------------------------------------------------------------
    // The search history (WP-57: FIND-13, FIND-14 for this input field)
    // -----------------------------------------------------------------------------------------

    fn history_texts(rig: &Rig) -> Vec<String> {
        let entries = rig.state.search_history.entries();
        entries.iter().map(|e| e.text.clone()).collect()
    }

    fn plain(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.to_owned(),
            ..Default::default()
        }
    }

    fn press(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    /// Types `text` into the focused field, lets the input delay pass and waits for the
    /// searches. The clock stands still otherwise (see `Rig::step`).
    fn type_text(rig: &mut Rig, text: &str) {
        rig.step = 0.0;
        rig.frame(vec![egui::Event::Text(text.into())]);
        rig.time += search::INPUT_DELAY_SECS + 0.05;
        // `tick` reads the clock of the frame before it: one frame to move the clock on, one
        // in which the search that is due starts.
        rig.frame(Vec::new());
        rig.frame(Vec::new());
        rig.settle();
    }

    /// Shows the search view, types `text` into its field and waits for the searches.
    fn type_query(rig: &mut Rig, text: &str) {
        dispatch(&mut rig.state, Action::ShowGlobalSearch, &rig.ctx);
        rig.frame(Vec::new());
        type_text(rig, text);
        assert_eq!(rig.state.global_search.query.text, text);
    }

    #[test]
    fn stepping_clicking_and_clearing_record_the_query_but_typing_alone_does_not() {
        let dir = Dir::new("history-used");
        let mut rig = Rig::new();
        let a = rig.open(&dir.doc("a.pdf", 3, &[(0, 100.0), (2, 100.0)]));
        type_query(&mut rig, "hello");
        assert!(
            history_texts(&rig).is_empty(),
            "typing alone records nothing"
        );

        dispatch(
            &mut rig.state,
            Action::GlobalSearchStep { forward: true },
            &rig.ctx,
        );
        assert_eq!(history_texts(&rig), ["hello"]);

        rig.state.search_history.clear();
        let (doc, window) = (rig.doc(a), rig.state.windows[0].id);
        let go = Action::GlobalSearchGoTo {
            doc,
            index: 1,
            window,
        };
        dispatch(&mut rig.state, go, &rig.ctx);
        assert_eq!(history_texts(&rig), ["hello"]);

        rig.state.search_history.clear();
        dispatch(&mut rig.state, Action::ClearGlobalSearch, &rig.ctx);
        assert_eq!(history_texts(&rig), ["hello"], "remembered before it goes");
        assert!(!rig.state.global_search.has_query());

        // A text that was never searched is not remembered when it is cleared.
        rig.state.search_history.clear();
        rig.state.global_search.query.text = "never".to_owned();
        dispatch(&mut rig.state, Action::ClearGlobalSearch, &rig.ctx);
        assert!(history_texts(&rig).is_empty());
    }

    #[test]
    fn a_text_asked_for_by_name_is_recorded_at_once() {
        let dir = Dir::new("history-find");
        let mut rig = Rig::new();
        rig.open(&dir.doc("a.pdf", 2, &[(0, 100.0)]));
        rig.search("hello");
        assert_eq!(history_texts(&rig), ["hello"]);
        // Asking again does not make a second entry.
        rig.search("hello");
        assert_eq!(history_texts(&rig), ["hello"]);
    }

    #[test]
    fn a_query_that_stays_with_results_is_recorded_after_two_seconds() {
        let dir = Dir::new("history-settle");
        let mut rig = Rig::new();
        rig.open(&dir.doc("a.pdf", 2, &[(0, 100.0)]));
        type_query(&mut rig, "hello");
        rig.frame(Vec::new());
        assert!(history_texts(&rig).is_empty(), "not yet");
        rig.time += search_history::SETTLE_SECS + 0.5;
        rig.frame(Vec::new());
        assert_eq!(history_texts(&rig), ["hello"]);

        // A query without results is not recorded however long it stays.
        type_text(&mut rig, "zz");
        assert_eq!(rig.state.global_search.query.text, "hellozz");
        assert_eq!(rig.state.global_search.totals().matches, 0);
        rig.time += search_history::SETTLE_SECS + 0.5;
        rig.frame(Vec::new());
        rig.frame(Vec::new());
        assert_eq!(history_texts(&rig), ["hello"]);
    }

    #[test]
    fn up_and_down_in_the_field_walk_through_the_history_and_search_at_once() {
        let dir = Dir::new("history-recall");
        let mut rig = Rig::new();
        let a = rig.open(&dir.doc("a.pdf", 3, &[(0, 100.0), (2, 100.0)]));
        rig.state.search_history.record(&plain("heading"), 1);
        rig.state.search_history.record(&plain("hello"), 2);
        dispatch(&mut rig.state, Action::ShowGlobalSearch, &rig.ctx);
        // The field keeps the arrow keys from its second frame with the keyboard on.
        rig.frame(Vec::new());
        rig.frame(Vec::new());
        assert!(rig.ctx.text_edit_focused(), "the field has the keyboard");

        rig.frame(vec![press(Key::ArrowUp)]);
        assert_eq!(rig.state.global_search.query.text, "hello", "the newest");
        rig.settle();
        assert_eq!(rig.counts(), vec![(rig.doc(a), 2)], "searched at once");
        assert!(rig.ctx.text_edit_focused(), "the field keeps the keyboard");

        rig.frame(vec![press(Key::ArrowUp)]);
        assert_eq!(rig.state.global_search.query.text, "heading");
        rig.frame(vec![press(Key::ArrowUp)]);
        assert_eq!(rig.state.global_search.query.text, "heading", "the oldest");
        rig.frame(vec![press(Key::ArrowDown)]);
        assert_eq!(rig.state.global_search.query.text, "hello");
        rig.frame(vec![press(Key::ArrowDown)]);
        assert_eq!(
            rig.state.global_search.query.text, "",
            "back at what was typed"
        );
        // Walking through the history is not using a query.
        assert_eq!(history_texts(&rig), ["hello", "heading"]);
    }

    #[test]
    fn the_history_button_stands_beside_the_field() {
        let dir = Dir::new("history-button");
        let mut rig = Rig::new();
        rig.open(&dir.doc("a.pdf", 2, &[(0, 100.0)]));
        dispatch(&mut rig.state, Action::ShowGlobalSearch, &rig.ctx);
        rig.frame(Vec::new());
        let texts = rig.texts();
        assert!(
            texts.iter().any(|t| t == tr("find.history_label")),
            "no history button in {texts:?}"
        );
    }
}
