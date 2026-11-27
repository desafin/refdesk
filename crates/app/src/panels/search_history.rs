//! The search history in the user interface (FIND-13 ~ FIND-15, `docs/ARCHITECTURE.md` §5.11).
//!
//! The history itself and its rules are `refdesk_core::SearchHistory`. This module gives three
//! parts that attach to **one input field each**, independent of each other, so that the search
//! bar of a document and the search across all documents use the same code:
//!
//! * [`Recall`]: ↑ / ↓ in the field walk through the history.
//! * [`history_button`]: the small button beside the field and the list it opens.
//! * [`Settle`]: "a query that has results and stays unchanged for [`SETTLE_SECS`] is
//!   recorded", and the replacing of a typed beginning ("hel" → "hello").
//!
//! What each of them decides is a plain method or function that does not touch egui, so it is
//! tested without a window (R-10). The only drawing is in [`history_button`].
//!
//! The functions at the end ([`record`], [`remove`], [`clear`], [`load_into`]) are what
//! `dispatch` does for the three actions; they are the only code that changes
//! `AppState::search_history` (R-6) and writes `search_history.json`.

use std::time::Duration;

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{
    Align2, Button, Color32, Id, Popup, PopupCloseBehavior, Rect, RichText, ScrollArea, Sense,
    TextStyle, Ui, WidgetInfo, pos2, vec2,
};
use refdesk_core::{SearchHistory, SearchHistoryEntry, SearchQuery};

use crate::i18n::tr;
use crate::state::{AppState, StatusNotice};

/// A query with results that stays unchanged this long (seconds) is recorded.
pub const SETTLE_SECS: f64 = 2.0;
/// The width of the history list.
const LIST_WIDTH: f32 = 380.0;
/// The history list scrolls past this height.
const LIST_MAX_HEIGHT: f32 = 320.0;
/// Seconds the "history cleared" notice stays in the status bar.
const CLEARED_NOTICE_SECS: f64 = 3.0;
/// The glyph of the button (a clock face, in egui's bundled emoji font).
const BUTTON_GLYPH: &str = "🕓";
/// The glyph of the button that removes one entry (the multiplication sign, like the close
/// button of the search bar: "✕" is in none of egui's bundled fonts).
const REMOVE_GLYPH: &str = "×";

// ---------------------------------------------------------------------------------------------
// (a) Recall: ↑ / ↓ walk through the history
// ---------------------------------------------------------------------------------------------

/// The state of walking through the history with ↑ and ↓ in one input field.
///
/// Call [`Recall::older`] for ↑ and [`Recall::newer`] for ↓ and put the query they return into
/// the field. Call [`Recall::reset`] when the user edits the text, changes an option, or the
/// field goes away: the next ↑ starts from the newest entry again.
#[derive(Clone, Debug, Default)]
pub struct Recall {
    /// The entry in the field: its place in the history when it was shown, and what it was.
    /// `None` while the user is typing.
    shown: Option<(usize, SearchQuery)>,
    /// What was in the field before the walk began; ↓ past the newest entry returns to it.
    draft: Option<SearchQuery>,
}

impl Recall {
    /// An entry of the history is in the field.
    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        self.shown.is_some()
    }

    /// Forgets the walk: the field holds what the user typed.
    pub fn reset(&mut self) {
        *self = Recall::default();
    }

    /// Where the shown entry is in `history` now. The history may have changed since (another
    /// tab recorded something): the entry is looked for, and when it is gone the old place is
    /// kept as far as the list reaches. `None` when the history is empty.
    fn position(&self, history: &SearchHistory) -> Option<usize> {
        let (index, query) = self.shown.as_ref()?;
        let entries = history.entries();
        if entries.get(*index).is_some_and(|e| e.is(query)) {
            return Some(*index);
        }
        if let Some(found) = entries.iter().position(|e| e.is(query)) {
            return Some(found);
        }
        entries.len().checked_sub(1).map(|last| (*index).min(last))
    }

    /// ↑: the next older entry. The first press takes `current` (what is in the field) as the
    /// draft and gives the newest entry, or the one after it when the newest is `current`
    /// already (so that one press gives a different text). `None` when there is nothing older:
    /// the field stays as it is.
    pub fn older(&mut self, history: &SearchHistory, current: &SearchQuery) -> Option<SearchQuery> {
        let entries = history.entries();
        let next = if self.shown.is_some() {
            match self.position(history) {
                Some(position) => position + 1,
                None => {
                    self.reset();
                    return None;
                }
            }
        } else if entries.first().is_some_and(|e| e.is(current)) {
            1
        } else {
            0
        };
        let query = entries.get(next)?.to_query();
        if self.shown.is_none() {
            self.draft = Some(current.clone());
        }
        self.shown = Some((next, query.clone()));
        Some(query)
    }

    /// ↓: the next newer entry; past the newest one the draft comes back. `None` while no entry
    /// of the history is in the field.
    pub fn newer(&mut self, history: &SearchHistory, current: &SearchQuery) -> Option<SearchQuery> {
        self.shown.as_ref()?;
        let entries = history.entries();
        let position = self.position(history).unwrap_or(0);
        // The draft equal to the newest entry was skipped on the way up; skip it on the way down.
        let draft_is_newest = self
            .draft
            .as_ref()
            .zip(entries.first())
            .is_some_and(|(draft, newest)| newest.is(draft));
        if position == 0 || (position == 1 && draft_is_newest) {
            let draft = self.draft.take().unwrap_or_else(|| current.clone());
            self.reset();
            return Some(draft);
        }
        let query = entries.get(position - 1)?.to_query();
        self.shown = Some((position - 1, query.clone()));
        Some(query)
    }
}

// ---------------------------------------------------------------------------------------------
// (c) Settle: "unchanged for two seconds" and replacing a typed beginning
// ---------------------------------------------------------------------------------------------

/// What [`Settle::observe`] asks for in a frame.
#[derive(Clone, Debug, PartialEq)]
pub enum SettleStep {
    /// Nothing to do.
    Idle,
    /// Nothing yet; look again after this long (ask for a frame then).
    Wait(Duration),
    /// Record `query`. When `replaces` is set, it is the query this watcher recorded just
    /// before: give it to `RecordSearch`, which drops it from the history if it is only the
    /// beginning of `query`.
    Record {
        /// The query to record.
        query: SearchQuery,
        /// The query recorded by settling just before, to be replaced when it is a beginning.
        replaces: Option<SearchQuery>,
    },
}

/// Decides when the query of one input field is recorded because it stayed (FIND-14).
///
/// Call [`Settle::observe`] every frame the field is shown. Tell it with [`Settle::used`] when
/// the user used a query on purpose (Enter, F3, a click on a result): that one is recorded by the
/// caller, is not recorded again by settling, and is never replaced.
#[derive(Clone, Debug, Default)]
pub struct Settle {
    /// The query seen in the last frame, and when it first showed up (seconds).
    watching: Option<SearchQuery>,
    since: f64,
    /// The query that was recorded last, by settling or on purpose.
    recorded: Option<SearchQuery>,
    /// The query recorded by settling just before, and not used on purpose since.
    settled: Option<SearchQuery>,
}

impl Settle {
    /// One frame. `has_matches`: the search for `query` found something. `now` is in seconds.
    /// The clock restarts whenever the query (the text or an option) changes.
    pub fn observe(&mut self, query: &SearchQuery, has_matches: bool, now: f64) -> SettleStep {
        if self.watching.as_ref() != Some(query) {
            self.watching = Some(query.clone());
            self.since = now;
        }
        let nothing_to_record =
            query.text.trim().is_empty() || self.recorded.as_ref() == Some(query);
        if nothing_to_record || !has_matches {
            return SettleStep::Idle;
        }
        let waited = now - self.since;
        if waited < SETTLE_SECS {
            return SettleStep::Wait(Duration::from_secs_f64(SETTLE_SECS - waited));
        }
        let replaces = self.settled.take().filter(|older| older != query);
        self.settled = Some(query.clone());
        self.recorded = Some(query.clone());
        SettleStep::Record {
            query: query.clone(),
            replaces,
        }
    }

    /// The user used `query` on purpose. It is not recorded by settling and nothing replaces it.
    pub fn used(&mut self, query: &SearchQuery) {
        self.recorded = Some(query.clone());
        self.settled = None;
    }

    /// Forgets everything (the field went away).
    pub fn reset(&mut self) {
        *self = Settle::default();
    }
}

// ---------------------------------------------------------------------------------------------
// (b) The history button and its list
// ---------------------------------------------------------------------------------------------

/// What the user did in the history list.
#[derive(Clone, Debug, PartialEq)]
pub enum HistoryEvent {
    /// A line was clicked: use this query. The list closes.
    Pick(SearchQuery),
    /// The × of a line was clicked: remove the entry at this index. The list stays open.
    Remove(usize),
    /// "Clear Search History" was clicked. The list closes.
    Clear,
}

/// One line of the history list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryLine {
    /// The search text on one line (the list shortens it to the width it has).
    pub text: String,
    /// The options that are on, as the search bar labels them ("Aa", "Word", ".*"), joined by
    /// blanks. Empty when none is on.
    pub options: String,
}

impl HistoryLine {
    /// The line for one entry.
    pub fn of(entry: &SearchHistoryEntry) -> HistoryLine {
        let text = entry
            .text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let mut options = Vec::new();
        if entry.case_sensitive {
            options.push("Aa");
        }
        if entry.whole_word {
            options.push(tr("find.word_label"));
        }
        if entry.regex {
            options.push(".*");
        }
        HistoryLine {
            text,
            options: options.join(" "),
        }
    }

    /// The name screen readers give the line: the text, then the options.
    pub fn accessible_name(&self) -> String {
        if self.options.is_empty() {
            self.text.clone()
        } else {
            format!("{} ({})", self.text, self.options)
        }
    }
}

/// The lines of the history list, newest first.
pub fn history_lines(history: &SearchHistory) -> Vec<HistoryLine> {
    history.entries().iter().map(HistoryLine::of).collect()
}

/// The id of the list opened by the button made with `id`.
fn popup_id(id: Id) -> Id {
    id.with("history_list")
}

/// The history list of the button made with `id` is open (as of the last frame).
pub fn is_list_open(ctx: &egui::Context, id: Id) -> bool {
    Popup::is_id_open(ctx, popup_id(id))
}

/// The small button beside an input field, and the list it opens. `id` tells this button from
/// the ones of other fields. Disabled when the history is empty.
pub fn history_button(ui: &mut Ui, id: Id, history: &SearchHistory) -> Option<HistoryEvent> {
    let list_id = popup_id(id);
    let empty = history.is_empty();
    let hint = if empty {
        tr("find.history_none")
    } else {
        tr("find.history_hint")
    };
    let response = ui
        .push_id(id, |ui| {
            ui.add_enabled(!empty, Button::new(BUTTON_GLYPH))
                .on_hover_text(hint)
                .on_disabled_hover_text(hint)
        })
        .inner;
    response
        .widget_info(|| WidgetInfo::labeled(egui::Role::Button, !empty, tr("find.history_label")));
    if empty {
        Popup::close_id(ui.ctx(), list_id);
        return None;
    }

    let mut event = None;
    Popup::from_toggle_button_response(&response)
        .id(list_id)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .width(LIST_WIDTH)
        .show(|ui| {
            ui.set_min_width(LIST_WIDTH);
            ScrollArea::vertical()
                .max_height(LIST_MAX_HEIGHT)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let lines = history_lines(history);
                    for (index, (entry, line)) in history.entries().iter().zip(&lines).enumerate() {
                        if let Some(e) = show_line(ui, index, entry, line) {
                            event = Some(e);
                        }
                    }
                });
            ui.separator();
            let clear = ui.button(tr("find.history_clear"));
            if clear.clicked() {
                event = Some(HistoryEvent::Clear);
            }
        });
    if matches!(event, Some(HistoryEvent::Pick(_) | HistoryEvent::Clear)) {
        Popup::close_id(ui.ctx(), list_id);
    }
    event
}

/// Draws one line of the list: the text, the options that are on, and the × at the right end.
fn show_line(
    ui: &mut Ui,
    index: usize,
    entry: &SearchHistoryEntry,
    line: &HistoryLine,
) -> Option<HistoryEvent> {
    let row_h = ui.text_style_height(&TextStyle::Body) + 8.0;
    let (rect, row) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
    let side = row_h - 4.0;
    let remove_rect = Rect::from_center_size(
        pos2(rect.right() - side * 0.5 - 2.0, rect.center().y),
        vec2(side, side),
    );
    let remove = ui.interact(remove_rect, row.id.with("remove"), Sense::click());
    row.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, line.accessible_name()));
    remove.widget_info(|| {
        WidgetInfo::labeled(
            egui::Role::Button,
            true,
            format!("{}: {}", tr("find.history_remove"), line.text),
        )
    });

    let visuals = ui.visuals().clone();
    let painter = ui.painter();
    if row.hovered() || remove.hovered() {
        painter.rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let font = ui.style().text_styles[&TextStyle::Body].clone();
    let small = ui.style().text_styles[&TextStyle::Small].clone();

    // From the right: the ×, then the options, and the text in what is left.
    let remove_color = if remove.hovered() {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    painter.text(
        remove_rect.center(),
        Align2::CENTER_CENTER,
        REMOVE_GLYPH,
        font.clone(),
        remove_color,
    );
    let mut text_right = remove_rect.left() - 4.0;
    if !line.options.is_empty() {
        let galley = painter.layout_no_wrap(line.options.clone(), small, visuals.weak_text_color());
        let at = pos2(
            text_right - galley.size().x,
            rect.center().y - galley.size().y / 2.0,
        );
        text_right = at.x - 8.0;
        painter.galley(at, galley, visuals.weak_text_color());
    }
    let width = (text_right - rect.left() - 8.0).max(0.0);
    let color = visuals.text_color();
    let galley = painter.layout_job(one_line_job(&line.text, font, color, width));
    let at = pos2(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0);
    painter.galley(at, galley, color);

    if remove.clicked() {
        return Some(HistoryEvent::Remove(index));
    }
    // The whole text on hover: the line shows only what fits.
    let row = row.on_hover_text(RichText::new(entry.text.clone()));
    row.clicked().then(|| HistoryEvent::Pick(entry.to_query()))
}

/// `text` as one row, shortened with "…" past `width`.
fn one_line_job(text: &str, font: egui::FontId, color: Color32, width: f32) -> LayoutJob {
    let mut job = LayoutJob {
        wrap: TextWrapping {
            max_width: width,
            max_rows: 1,
            break_anywhere: true,
            ..Default::default()
        },
        ..Default::default()
    };
    job.append(text, 0.0, TextFormat::simple(font, color));
    job
}

// ---------------------------------------------------------------------------------------------
// What dispatch does
// ---------------------------------------------------------------------------------------------

/// Reads `search_history.json` into the state, when the state has a place to read it from.
pub fn load_into(state: &mut AppState) {
    if let Some(paths) = &state.paths {
        state.search_history = SearchHistory::load(&paths.search_history_file());
    }
}

/// Writes the history. A failure is logged and the app goes on.
fn save(state: &AppState) {
    if let Some(paths) = &state.paths {
        let file = paths.search_history_file();
        if let Err(err) = state.search_history.save(&file) {
            log::warn!("could not save {}: {err}", file.display());
        }
    }
}

/// `Action::RecordSearch`: the query goes first in the history. `replaces` is dropped before it
/// when it is only the beginning of `query`. A query that is the newest entry already changes
/// nothing and writes nothing (F3 pressed again and again must not rewrite the file).
pub fn record(state: &mut AppState, query: &SearchQuery, replaces: Option<&SearchQuery>) {
    let mut changed = false;
    if let Some(older) = replaces {
        changed |= state.search_history.replace_latest_if_prefix(older, query);
    }
    if !state.search_history.is_latest(query) {
        let now = state.memory.now();
        changed |= state.search_history.record(query, now);
    }
    if changed {
        save(state);
    }
}

/// `Action::RemoveSearchHistory`: drops the entry at `index` (nothing for an index past the end).
pub fn remove(state: &mut AppState, index: usize) {
    if state.search_history.remove(index) {
        save(state);
    }
}

/// `Action::ClearSearchHistory`: forgets every search and says so in the status bar.
pub fn clear(state: &mut AppState, ctx: &egui::Context) {
    if state.search_history.is_empty() {
        return;
    }
    state.search_history.clear();
    save(state);
    state.status_notice = Some(StatusNotice {
        text: tr("status.search_history_cleared").to_string(),
        until: ctx.input(|i| i.time) + CLEARED_NOTICE_SECS,
    });
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, dispatch};
    use refdesk_core::Paths;

    /// A directory under the system temp directory, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "refdesk-wp56-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn q(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.to_string(),
            ..SearchQuery::default()
        }
    }

    fn opts(text: &str, case: bool, word: bool, regex: bool) -> SearchQuery {
        SearchQuery {
            text: text.to_string(),
            case_sensitive: case,
            whole_word: word,
            regex,
        }
    }

    /// A history with the given texts, the last one the newest.
    fn history(texts: &[&str]) -> SearchHistory {
        let mut h = SearchHistory::default();
        for (i, t) in texts.iter().enumerate() {
            h.record(&q(t), i as i64 + 1);
        }
        h
    }

    // --- Recall ------------------------------------------------------------------------------

    #[test]
    fn up_walks_from_the_newest_to_the_oldest_and_stops_at_the_end() {
        let h = history(&["a", "b", "c"]);
        let mut r = Recall::default();
        let typed = q("zz");
        assert_eq!(r.older(&h, &typed), Some(q("c")));
        assert!(r.is_active());
        assert_eq!(r.older(&h, &q("c")), Some(q("b")));
        assert_eq!(r.older(&h, &q("b")), Some(q("a")));
        assert_eq!(r.older(&h, &q("a")), None, "stops at the oldest");
        assert_eq!(r.older(&h, &q("a")), None);
        // Still at the oldest: one down goes back to b.
        assert_eq!(r.newer(&h, &q("a")), Some(q("b")));
    }

    #[test]
    fn down_comes_back_and_ends_at_the_draft() {
        let h = history(&["a", "b", "c"]);
        let mut r = Recall::default();
        let typed = opts("zz", true, false, true);
        assert_eq!(r.newer(&h, &typed), None, "not walking: nothing");
        r.older(&h, &typed);
        r.older(&h, &q("c"));
        assert_eq!(r.newer(&h, &q("b")), Some(q("c")));
        assert_eq!(
            r.newer(&h, &q("c")),
            Some(typed.clone()),
            "the text and the options typed before"
        );
        assert!(!r.is_active());
        assert_eq!(r.newer(&h, &typed), None, "the walk is over");
        // The next up starts at the newest again.
        assert_eq!(r.older(&h, &typed), Some(q("c")));
    }

    #[test]
    fn the_newest_entry_equal_to_the_field_is_skipped() {
        let h = history(&["a", "b", "c"]);
        let mut r = Recall::default();
        assert_eq!(
            r.older(&h, &q("c")),
            Some(q("b")),
            "one press shows another text"
        );
        assert_eq!(r.older(&h, &q("b")), Some(q("a")));
        assert_eq!(r.newer(&h, &q("a")), Some(q("b")));
        assert_eq!(
            r.newer(&h, &q("b")),
            Some(q("c")),
            "the draft, which equals the newest entry"
        );
        assert!(!r.is_active());

        // With a single entry that is the field's text there is nothing else to show.
        let one = history(&["only"]);
        let mut r = Recall::default();
        assert_eq!(r.older(&one, &q("only")), None);
        assert!(!r.is_active());
        // The same text with other options is another entry.
        assert_eq!(
            r.older(&one, &opts("only", true, false, false)),
            Some(q("only"))
        );
    }

    #[test]
    fn an_empty_history_gives_nothing() {
        let h = SearchHistory::default();
        let mut r = Recall::default();
        assert_eq!(r.older(&h, &q("x")), None);
        assert_eq!(r.newer(&h, &q("x")), None);
        assert!(!r.is_active());
    }

    #[test]
    fn reset_starts_over_from_the_newest() {
        let h = history(&["a", "b", "c"]);
        let mut r = Recall::default();
        r.older(&h, &q(""));
        r.older(&h, &q("c"));
        r.reset();
        assert!(!r.is_active());
        assert_eq!(r.older(&h, &q("typed")), Some(q("c")));
        assert_eq!(r.newer(&h, &q("c")), Some(q("typed")));
    }

    #[test]
    fn a_history_that_changes_during_the_walk_does_not_panic_or_lose_the_place() {
        let mut h = history(&["a", "b", "c", "d"]);
        let mut r = Recall::default();
        r.older(&h, &q("")); // d
        r.older(&h, &q("d")); // c
        r.older(&h, &q("c")); // b
        // The history shrinks to nothing under the walk.
        h.clear();
        assert_eq!(r.older(&h, &q("b")), None);
        assert!(!r.is_active(), "nothing left to walk");

        // An entry at the shown place is replaced: the place is clamped.
        let mut h = history(&["a", "b", "c", "d"]);
        let mut r = Recall::default();
        r.older(&h, &q(""));
        r.older(&h, &q("d"));
        r.older(&h, &q("c"));
        r.older(&h, &q("b")); // a, at the end
        h.remove(3);
        h.remove(2);
        assert!(r.newer(&h, &q("a")).is_some());
        r.older(&h, &q("x"));
        r.older(&h, &q("x"));
        r.older(&h, &q("x"));

        // The shown entry moved to the front (it was recorded in another tab): the walk goes on
        // from where the entry is now.
        let mut h = history(&["a", "b", "c"]);
        let mut r = Recall::default();
        r.older(&h, &q("")); // c
        r.older(&h, &q("c")); // b, at index 1
        h.record(&q("b"), 10); // now b, c, a
        assert_eq!(r.older(&h, &q("b")), Some(q("c")));
        assert_eq!(r.older(&h, &q("c")), Some(q("a")));
    }

    // --- Settle ------------------------------------------------------------------------------

    fn recorded(step: SettleStep) -> (SearchQuery, Option<SearchQuery>) {
        match step {
            SettleStep::Record { query, replaces } => (query, replaces),
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn a_query_is_recorded_once_after_two_seconds() {
        let mut s = Settle::default();
        let hello = q("hello");
        match s.observe(&hello, true, 10.0) {
            SettleStep::Wait(d) => assert!((d.as_secs_f64() - 2.0).abs() < 1e-9),
            other => panic!("{other:?}"),
        }
        match s.observe(&hello, true, 11.5) {
            SettleStep::Wait(d) => assert!((d.as_secs_f64() - 0.5).abs() < 1e-9),
            other => panic!("{other:?}"),
        }
        let (query, replaces) = recorded(s.observe(&hello, true, 12.0));
        assert_eq!(query, hello);
        assert_eq!(replaces, None);
        assert_eq!(s.observe(&hello, true, 12.1), SettleStep::Idle);
        assert_eq!(s.observe(&hello, true, 99.0), SettleStep::Idle);
    }

    #[test]
    fn nothing_is_recorded_without_results_or_text() {
        let mut s = Settle::default();
        let x = q("nothing");
        assert_eq!(s.observe(&x, false, 0.0), SettleStep::Idle);
        assert_eq!(s.observe(&x, false, 30.0), SettleStep::Idle);
        // The results arrive late: the query has been stable all along.
        assert!(matches!(
            s.observe(&x, true, 31.0),
            SettleStep::Record { .. }
        ));
        let mut s = Settle::default();
        assert_eq!(s.observe(&q("  "), true, 0.0), SettleStep::Idle);
        assert_eq!(s.observe(&q("  "), true, 50.0), SettleStep::Idle);
    }

    #[test]
    fn a_change_restarts_the_clock() {
        let mut s = Settle::default();
        s.observe(&q("a"), true, 0.0);
        assert!(matches!(s.observe(&q("a"), true, 1.9), SettleStep::Wait(_)));
        // The text changes: two seconds count from here.
        assert!(matches!(
            s.observe(&q("ab"), true, 2.0),
            SettleStep::Wait(_)
        ));
        assert!(matches!(
            s.observe(&q("ab"), true, 3.9),
            SettleStep::Wait(_)
        ));
        assert!(matches!(
            s.observe(&q("ab"), true, 4.0),
            SettleStep::Record { .. }
        ));
        // An option change restarts it too.
        s.observe(&opts("ab", true, false, false), true, 5.0);
        assert!(matches!(
            s.observe(&opts("ab", true, false, false), true, 6.0),
            SettleStep::Wait(_)
        ));
        assert!(matches!(
            s.observe(&opts("ab", true, false, false), true, 7.0),
            SettleStep::Record { .. }
        ));
    }

    #[test]
    fn a_settled_beginning_is_offered_for_replacing() {
        let mut s = Settle::default();
        s.observe(&q("hel"), true, 0.0);
        let (query, replaces) = recorded(s.observe(&q("hel"), true, 2.0));
        assert_eq!((query, replaces), (q("hel"), None));
        s.observe(&q("hello"), true, 3.0);
        let (query, replaces) = recorded(s.observe(&q("hello"), true, 5.0));
        assert_eq!(query, q("hello"));
        assert_eq!(replaces, Some(q("hel")));
        // Only the one before: a third query replaces "hello", not "hel".
        s.observe(&q("hello world"), true, 6.0);
        let (_, replaces) = recorded(s.observe(&q("hello world"), true, 8.0));
        assert_eq!(replaces, Some(q("hello")));
    }

    #[test]
    fn a_query_used_on_purpose_is_not_replaced_and_not_recorded_again() {
        let mut s = Settle::default();
        s.observe(&q("hel"), true, 0.0);
        s.used(&q("hel"));
        assert_eq!(
            s.observe(&q("hel"), true, 10.0),
            SettleStep::Idle,
            "already recorded"
        );
        s.observe(&q("hello"), true, 11.0);
        let (query, replaces) = recorded(s.observe(&q("hello"), true, 13.0));
        assert_eq!(query, q("hello"));
        assert_eq!(replaces, None, "Enter's \"hel\" stays");

        // A query used before it was ever observed is not recorded by settling either.
        let mut s = Settle::default();
        s.used(&q("found"));
        assert_eq!(s.observe(&q("found"), true, 0.0), SettleStep::Idle);
        assert_eq!(s.observe(&q("found"), true, 5.0), SettleStep::Idle);

        // Using a query after settling on an earlier one cuts the link to that one.
        let mut s = Settle::default();
        s.observe(&q("he"), true, 0.0);
        recorded(s.observe(&q("he"), true, 2.0));
        s.used(&q("help"));
        s.observe(&q("helpful"), true, 3.0);
        let (_, replaces) = recorded(s.observe(&q("helpful"), true, 5.0));
        assert_eq!(replaces, None);
    }

    #[test]
    fn reset_forgets_what_was_recorded() {
        let mut s = Settle::default();
        s.observe(&q("a"), true, 0.0);
        recorded(s.observe(&q("a"), true, 2.0));
        s.reset();
        s.observe(&q("a"), true, 10.0);
        let (_, replaces) = recorded(s.observe(&q("a"), true, 12.0));
        assert_eq!(replaces, None);
    }

    // --- the lines of the list ---------------------------------------------------------------

    #[test]
    fn a_line_has_the_text_and_the_options_that_are_on() {
        let mut h = SearchHistory::default();
        h.record(&q("plain"), 1);
        h.record(&opts("SWS_EM_01004", true, true, false), 2);
        h.record(&opts("fo+\nbar", false, false, true), 3);
        h.record(&opts("all", true, true, true), 4);
        let lines = history_lines(&h);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].text, "all");
        assert_eq!(lines[0].options, format!("Aa {} .*", tr("find.word_label")));
        assert_eq!(lines[1].text, "fo+ bar", "one line");
        assert_eq!(lines[1].options, ".*");
        assert_eq!(lines[2].text, "SWS_EM_01004");
        assert_eq!(lines[2].options, format!("Aa {}", tr("find.word_label")));
        assert_eq!(lines[3].text, "plain");
        assert_eq!(lines[3].options, "");
        assert_eq!(lines[3].accessible_name(), "plain");
        assert_eq!(lines[1].accessible_name(), "fo+ bar (.*)");
    }

    // --- dispatch ----------------------------------------------------------------------------

    fn state_in(dir: &std::path::Path) -> AppState {
        let mut state = AppState::new();
        state.paths = Some(Paths::under(dir));
        state
    }

    fn file(dir: &std::path::Path) -> std::path::PathBuf {
        Paths::under(dir).search_history_file()
    }

    fn record_action(query: SearchQuery, replaces: Option<SearchQuery>) -> Action {
        Action::RecordSearch { query, replaces }
    }

    fn texts_of(state: &AppState) -> Vec<String> {
        state
            .search_history
            .entries()
            .iter()
            .map(|e| e.text.clone())
            .collect()
    }

    #[test]
    fn recording_changes_the_state_and_the_file() {
        let dir = TempDir::new();
        let ctx = egui::Context::default();
        let mut state = state_in(dir.path());
        dispatch(&mut state, record_action(q("alpha"), None), &ctx);
        dispatch(
            &mut state,
            record_action(opts("beta", true, false, false), None),
            &ctx,
        );
        assert_eq!(texts_of(&state), ["beta", "alpha"]);
        let saved = SearchHistory::load(&file(dir.path()));
        assert_eq!(saved, state.search_history);
        assert!(saved.entries()[0].case_sensitive);
        // An empty query records nothing.
        dispatch(&mut state, record_action(q("  "), None), &ctx);
        assert_eq!(state.search_history.entries().len(), 2);
    }

    #[test]
    fn a_typed_beginning_is_replaced_by_the_whole() {
        let dir = TempDir::new();
        let ctx = egui::Context::default();
        let mut state = state_in(dir.path());
        dispatch(&mut state, record_action(q("hel"), None), &ctx);
        dispatch(&mut state, record_action(q("hello"), Some(q("hel"))), &ctx);
        assert_eq!(texts_of(&state), ["hello"]);
        assert_eq!(SearchHistory::load(&file(dir.path())).entries().len(), 1);
        // Not a beginning: both stay.
        dispatch(
            &mut state,
            record_action(q("world"), Some(q("hello"))),
            &ctx,
        );
        assert_eq!(texts_of(&state), ["world", "hello"]);
    }

    #[test]
    fn recording_the_newest_query_again_writes_nothing() {
        let dir = TempDir::new();
        let ctx = egui::Context::default();
        let mut state = state_in(dir.path());
        state.memory.set_clock(std::sync::Arc::new(|| 100));
        dispatch(&mut state, record_action(q("same"), None), &ctx);
        let path = file(dir.path());
        let before = std::fs::read(&path).unwrap();
        // Delete the file: a write would bring it back.
        std::fs::remove_file(&path).unwrap();
        state.memory.set_clock(std::sync::Arc::new(|| 200));
        for _ in 0..3 {
            dispatch(&mut state, record_action(q(" same "), None), &ctx);
        }
        assert!(!path.exists(), "no write for the newest query");
        assert_eq!(
            state.search_history.latest().map(|e| e.last_used),
            Some(100)
        );
        // The file had the first write's content.
        assert!(String::from_utf8_lossy(&before).contains("same"));
        // Another query writes again.
        dispatch(&mut state, record_action(q("other"), None), &ctx);
        assert!(path.exists());
    }

    #[test]
    fn remove_and_clear_change_the_file_too() {
        let dir = TempDir::new();
        let ctx = egui::Context::default();
        let mut state = state_in(dir.path());
        for t in ["a", "b", "c"] {
            dispatch(&mut state, record_action(q(t), None), &ctx);
        }
        dispatch(&mut state, Action::RemoveSearchHistory(1), &ctx);
        assert_eq!(texts_of(&state), ["c", "a"]);
        assert_eq!(SearchHistory::load(&file(dir.path())), state.search_history);
        // Past the end: nothing happens.
        dispatch(&mut state, Action::RemoveSearchHistory(9), &ctx);
        assert_eq!(texts_of(&state), ["c", "a"]);

        assert!(state.status_notice.is_none());
        dispatch(&mut state, Action::ClearSearchHistory, &ctx);
        assert!(state.search_history.is_empty());
        assert!(SearchHistory::load(&file(dir.path())).is_empty());
        assert_eq!(
            state.status_notice.as_ref().map(|n| n.text.clone()),
            Some(tr("status.search_history_cleared").to_string())
        );
    }

    #[test]
    fn without_a_place_to_save_everything_still_works() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        assert!(state.paths.is_none());
        dispatch(&mut state, record_action(q("a"), None), &ctx);
        dispatch(&mut state, record_action(q("b"), None), &ctx);
        dispatch(&mut state, Action::RemoveSearchHistory(0), &ctx);
        assert_eq!(texts_of(&state), ["a"]);
        dispatch(&mut state, Action::ClearSearchHistory, &ctx);
        assert!(state.search_history.is_empty());
    }

    #[test]
    fn a_save_that_fails_is_only_logged() {
        let dir = TempDir::new();
        let ctx = egui::Context::default();
        let mut state = state_in(dir.path());
        // A file where the state directory should be: every write fails.
        std::fs::write(dir.path().join("state"), "not a directory").unwrap();
        dispatch(&mut state, record_action(q("a"), None), &ctx);
        assert_eq!(texts_of(&state), ["a"], "the history lives on in memory");
    }

    #[test]
    fn the_history_is_still_there_after_a_restart() {
        let dir = TempDir::new();
        let ctx = egui::Context::default();
        let mut state = state_in(dir.path());
        dispatch(
            &mut state,
            record_action(opts("SWS_EM_01004", true, true, false), None),
            &ctx,
        );
        dispatch(&mut state, record_action(q("Execution"), None), &ctx);
        drop(state);

        let mut again = state_in(dir.path());
        assert!(again.search_history.is_empty());
        load_into(&mut again);
        assert_eq!(texts_of(&again), ["Execution", "SWS_EM_01004"]);
        assert!(again.search_history.entries()[1].whole_word);
    }
}
