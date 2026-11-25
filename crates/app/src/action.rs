//! Actions and `dispatch`, the only place that changes the application structure (rule R-6).
//!
//! Widgets push [`Action`]s onto a queue; the queue is dispatched at the end of the frame
//! (`docs/ARCHITECTURE.md` §5.2). `dispatch` only changes state, so tests run it without a window.

use std::path::PathBuf;
use std::sync::Arc;

use refdesk_core::{DocumentHandle, Location, Notifier, Place, ViewState, ZoomMode};

// `OpenTarget` lives in core because it travels between processes (`refdesk_core::ipc`).
use crate::panels::status_bar::GotoInput;
use crate::state::{AppState, PdfTab, Tab, TabId, normalize_path};
use crate::views::pdf_view::PdfViewUiState;
pub use refdesk_core::OpenTarget;

/// Something the user (or the outside world) asked for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Open these files; the last one gets the focus.
    OpenFiles(Vec<OpenTarget>),
    /// Ask for the file chooser.
    OpenFileDialog,
    /// Close a tab.
    CloseTab(TabId),
    /// Show a tab and make it the target of commands.
    FocusTab(TabId),
    /// Open another view of the same document, copying the view state.
    DuplicateTab(TabId),
    /// Activate the tab after the active one, wrapping around.
    NextTab,
    /// Activate the tab before the active one, wrapping around.
    PrevTab,
    /// Change the zoom of a PDF tab.
    SetZoom {
        /// The tab to change.
        tab: TabId,
        /// The new zoom.
        zoom: ZoomMode,
    },
    /// Compare two documents.
    StartDiff {
        /// The older document.
        left: PathBuf,
        /// The newer document.
        right: PathBuf,
    },
    /// Open the page number field of the status bar for a PDF tab.
    ShowGoToPage(TabId),
    /// Move a PDF tab to the top of a page (zero-based; limited to the document). Recorded in
    /// the history as a long jump.
    GoToPage {
        /// The tab to move.
        tab: TabId,
        /// Zero-based page.
        page: u32,
    },
    /// Close the application.
    Quit,
}

/// The notifier handed to documents: it asks egui for a new frame.
fn notifier(ctx: &egui::Context) -> Notifier {
    let ctx = ctx.clone();
    Arc::new(move || ctx.request_repaint())
}

/// Applies one action to the state.
pub fn dispatch(state: &mut AppState, action: Action, ctx: &egui::Context) {
    match action {
        Action::OpenFiles(targets) => {
            for target in targets {
                open_file(state, target, ctx);
            }
        }
        Action::OpenFileDialog => {
            state.file_dialog_requested = true;
            ctx.request_repaint();
        }
        Action::CloseTab(tab) => close_tab(state, tab),
        Action::FocusTab(tab) => focus_tab(state, tab),
        Action::DuplicateTab(tab) => duplicate_tab(state, tab),
        Action::NextTab => cycle_tab(state, 1),
        Action::PrevTab => cycle_tab(state, -1),
        Action::SetZoom { tab, zoom } => {
            if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
                pdf.view.zoom = zoom;
            }
        }
        Action::StartDiff { left, right } => {
            // The comparison view is built by a later work package. Until then the two documents
            // are only opened as tabs, left first, so the right one ends up focused.
            for path in [left, right] {
                open_file(state, OpenTarget { path, page: None }, ctx);
            }
        }
        Action::ShowGoToPage(tab) => {
            if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
                pdf.ui.goto = Some(GotoInput::default());
            }
        }
        Action::GoToPage { tab, page } => go_to_page(state, tab, page),
        Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
    }
}

/// Dispatches everything in `state.actions`, in order.
pub fn dispatch_queued(state: &mut AppState, ctx: &egui::Context) {
    let queue = std::mem::take(&mut state.actions);
    for action in queue {
        dispatch(state, action, ctx);
    }
}

/// Makes `tab` the active tab of the main window and the focus, if the window has it.
fn focus_tab(state: &mut AppState, tab: TabId) {
    if state.main_window_mut().activate(tab) {
        state.focus = Some(tab);
    }
}

/// Opens a document and shows it in a tab. An already open document is not opened again: its
/// existing tab gets the focus instead (OPEN-5).
fn open_file(state: &mut AppState, target: OpenTarget, ctx: &egui::Context) {
    let path = normalize_path(&target.path);
    if let Some(doc) = state.docs.find(&path) {
        if let Some(tab) = existing_tab_for(state, doc) {
            if let Some(page) = target.page {
                jump_to_page(state, tab, page);
            }
            focus_tab(state, tab);
            return;
        }
        // A document without a tab should not exist (closing the last tab drops it). Recover by
        // dropping the stale entry and opening it again.
        state.docs.remove(doc);
    }

    let handle = DocumentHandle::open(&path, notifier(ctx));
    let doc = state.docs.insert(handle);
    let mut view = ViewState::new(path.clone());
    if let Some(page) = target.page {
        let loc = Location::new(page, 0.0, 0.0);
        view.anchor = loc;
        view.history = refdesk_core::History::new(Place { doc: path, loc });
    }
    let tab = state.tabs.insert(Tab::Pdf(PdfTab {
        doc,
        view,
        ui: PdfViewUiState::default(),
    }));

    // The start page makes room for the first document.
    let start_tabs: Vec<TabId> = state
        .main_window()
        .tabs
        .iter()
        .copied()
        .filter(|t| matches!(state.tabs.get(*t), Some(Tab::Start)))
        .collect();
    let only_start = start_tabs.len() == state.main_window().tabs.len();
    if only_start {
        for t in start_tabs {
            state.main_window_mut().remove_tab(t);
            state.tabs.remove(t);
        }
    }

    state.main_window_mut().push_tab(tab);
    focus_tab(state, tab);
}

/// The tab to show for a document that is already open: the focused one if it shows the
/// document, else the first in window order.
fn existing_tab_for(state: &AppState, doc: crate::state::DocId) -> Option<TabId> {
    let shows_doc = |t: &TabId| matches!(state.tabs.get(*t), Some(Tab::Pdf(p)) if p.doc == doc);
    state
        .focus
        .filter(|t| shows_doc(t) && state.main_window().tabs.contains(t))
        .or_else(|| state.main_window().tabs.iter().copied().find(shows_doc))
}

/// Moves a view to the top of `page` as a recorded long jump.
fn jump_to_page(state: &mut AppState, tab: TabId, page: u32) {
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        let doc = pdf.view.history.current().doc.clone();
        let from = Place {
            doc: doc.clone(),
            loc: pdf.view.anchor,
        };
        let to = Place {
            doc,
            loc: Location::new(page, 0.0, 0.0),
        };
        pdf.view.anchor = to.loc;
        pdf.view.history.jump(from, to);
    }
}

/// Jumps a PDF tab to the top of `page`, limited to the pages of its document when they are
/// known.
fn go_to_page(state: &mut AppState, tab: TabId, page: u32) {
    let Some(pdf) = state.tabs.pdf(tab) else {
        return;
    };
    let count = state
        .docs
        .get(pdf.doc)
        .and_then(|doc| doc.info())
        .map(|info| info.page_sizes.len() as u32);
    let page = match count {
        Some(0) => return,
        Some(n) => page.min(n - 1),
        None => page,
    };
    jump_to_page(state, tab, page);
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        pdf.ui.goto = None;
    }
}

/// Closes a tab, drops its document when no other tab shows it, and keeps the window non-empty by
/// showing the start page after the last tab.
fn close_tab(state: &mut AppState, tab: TabId) {
    if !state.main_window().tabs.contains(&tab) {
        return;
    }
    // Closing the start page when it is the only tab would just bring it back.
    if state.main_window().tabs.len() == 1 && matches!(state.tabs.get(tab), Some(Tab::Start)) {
        return;
    }
    state.main_window_mut().remove_tab(tab);
    if let Some(Tab::Pdf(pdf)) = state.tabs.remove(tab)
        && !state.tabs.doc_in_use(pdf.doc)
    {
        state.docs.remove(pdf.doc);
    }
    if state.main_window().tabs.is_empty() {
        let start = state.tabs.insert(Tab::Start);
        state.main_window_mut().push_tab(start);
        state.main_window_mut().activate(start);
    }
    state.focus = state.main_window().active;
}

/// Adds a new tab on the same document, with a copy of the view state.
fn duplicate_tab(state: &mut AppState, tab: TabId) {
    let Some(mut source) = state.tabs.pdf(tab).cloned() else {
        return;
    };
    // The copy starts with a fresh view; its scroll follows from the copied anchor.
    source.ui = PdfViewUiState::default();
    let new = state.tabs.insert(Tab::Pdf(source));
    state.main_window_mut().insert_after(tab, new);
    focus_tab(state, new);
}

/// Activates the neighbor of the active tab, wrapping around.
fn cycle_tab(state: &mut AppState, step: isize) {
    if let Some(tab) = state.main_window().cycle(step) {
        focus_tab(state, tab);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn open(state: &mut AppState, ctx: &egui::Context, path: &str) {
        dispatch(
            state,
            Action::OpenFiles(vec![OpenTarget {
                path: PathBuf::from(path),
                page: None,
            }]),
            ctx,
        );
    }

    fn active(state: &AppState) -> Option<TabId> {
        state.main_window().active
    }

    #[test]
    fn open_files_creates_a_document_and_a_focused_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");

        assert_eq!(state.docs.len(), 1);
        // The start page made room for the document.
        assert_eq!(state.tabs.len(), 1);
        let tab = active(&state).expect("a tab is active");
        assert_eq!(state.focus, Some(tab));
        let pdf = state.tabs.pdf(tab).expect("a pdf tab");
        let doc = state.docs.get(pdf.doc).expect("the document exists");
        assert_eq!(doc.path(), Path::new("/refdesk-test/a.pdf"));
    }

    #[test]
    fn opening_several_files_focuses_the_last() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(
            &mut state,
            Action::OpenFiles(vec![
                OpenTarget::parse("/refdesk-test/a.pdf"),
                OpenTarget::parse("/refdesk-test/b.pdf#page=3"),
            ]),
            &ctx,
        );
        assert_eq!(state.main_window().tabs.len(), 2);
        let last = *state.main_window().tabs.last().expect("two tabs");
        assert_eq!(state.focus, Some(last));
        let pdf = state.tabs.pdf(last).expect("a pdf tab");
        assert_eq!(pdf.view.anchor.page, 2);
        assert_eq!(pdf.view.history.current().loc.page, 2);
    }

    #[test]
    fn opening_a_missing_file_still_creates_a_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/does/not/exist.pdf");
        assert_eq!(state.docs.len(), 1);
        assert!(state.focused_pdf().is_some());
    }

    #[test]
    fn opening_the_same_path_twice_focuses_the_existing_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        open(&mut state, &ctx, "/refdesk-test/b.pdf");
        let first = state.main_window().tabs[0];
        assert_ne!(state.focus, Some(first));

        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        assert_eq!(state.docs.len(), 2);
        assert_eq!(state.main_window().tabs.len(), 2);
        assert_eq!(state.focus, Some(first));
        assert_eq!(active(&state), Some(first));
    }

    #[test]
    fn differently_spelled_paths_of_one_file_are_one_document() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let cwd = std::env::current_dir().expect("cwd");
        let absolute = cwd.join("refdesk-test-missing.pdf");

        open(&mut state, &ctx, "./refdesk-test-missing.pdf");
        open(&mut state, &ctx, absolute.to_str().expect("utf-8 cwd"));
        open(&mut state, &ctx, "sub/../refdesk-test-missing.pdf");

        assert_eq!(state.docs.len(), 1);
        assert_eq!(state.main_window().tabs.len(), 1);
    }

    #[test]
    fn reopening_with_a_page_jumps_the_existing_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/a.pdf#page=5")]),
            &ctx,
        );
        assert_eq!(state.main_window().tabs.len(), 1);
        let (_, pdf) = state.focused_pdf().expect("pdf tab");
        assert_eq!(pdf.view.anchor.page, 4);
        assert!(pdf.view.history.can_back());
    }

    #[test]
    fn duplicate_tab_makes_a_second_tab_on_the_same_document() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        dispatch(
            &mut state,
            Action::SetZoom {
                tab,
                zoom: ZoomMode::Custom(2.0),
            },
            &ctx,
        );
        dispatch(&mut state, Action::DuplicateTab(tab), &ctx);

        assert_eq!(state.docs.len(), 1);
        assert_eq!(state.main_window().tabs.len(), 2);
        let (copy, copy_tab) = state.focused_pdf().expect("the copy is focused");
        assert_ne!(copy, tab);
        let original = state.tabs.pdf(tab).expect("original");
        assert_eq!(copy_tab.doc, original.doc);
        assert_eq!(copy_tab.view, original.view);
        assert_eq!(copy_tab.view.zoom, ZoomMode::Custom(2.0));
        // The copy sits next to its source.
        assert_eq!(state.main_window().tabs, vec![tab, copy]);
    }

    #[test]
    fn duplicating_the_start_page_does_nothing() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let start = state.main_window().tabs[0];
        dispatch(&mut state, Action::DuplicateTab(start), &ctx);
        assert_eq!(state.main_window().tabs.len(), 1);
    }

    #[test]
    fn closing_the_last_tab_of_a_document_drops_the_document() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        dispatch(&mut state, Action::DuplicateTab(tab), &ctx);
        assert_eq!(state.docs.len(), 1);

        // One tab left on the document: it stays.
        dispatch(&mut state, Action::CloseTab(tab), &ctx);
        assert_eq!(state.docs.len(), 1);
        let (rest, _) = state.focused_pdf().expect("the copy remains");

        // The last one goes: the document goes too.
        dispatch(&mut state, Action::CloseTab(rest), &ctx);
        assert_eq!(state.docs.len(), 0);
        assert!(state.docs.find(Path::new("/refdesk-test/a.pdf")).is_none());
    }

    #[test]
    fn closing_the_active_tab_activates_a_neighbor() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        for name in ["a", "b", "c"] {
            open(&mut state, &ctx, &format!("/refdesk-test/{name}.pdf"));
        }
        let tabs = state.main_window().tabs.clone();
        dispatch(&mut state, Action::FocusTab(tabs[1]), &ctx);

        dispatch(&mut state, Action::CloseTab(tabs[1]), &ctx);
        assert_eq!(active(&state), Some(tabs[2]));
        assert_eq!(state.focus, Some(tabs[2]));

        dispatch(&mut state, Action::CloseTab(tabs[2]), &ctx);
        assert_eq!(active(&state), Some(tabs[0]));
        assert_eq!(state.focus, Some(tabs[0]));
    }

    #[test]
    fn closing_an_inactive_tab_keeps_the_active_one() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        open(&mut state, &ctx, "/refdesk-test/b.pdf");
        let tabs = state.main_window().tabs.clone();
        dispatch(&mut state, Action::CloseTab(tabs[0]), &ctx);
        assert_eq!(active(&state), Some(tabs[1]));
        assert_eq!(state.docs.len(), 1);
    }

    #[test]
    fn closing_the_last_tab_shows_the_start_page() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        dispatch(&mut state, Action::CloseTab(tab), &ctx);

        assert_eq!(state.main_window().tabs.len(), 1);
        let shown = active(&state).expect("a tab is active");
        assert!(matches!(state.tabs.get(shown), Some(Tab::Start)));
        assert_eq!(state.focus, Some(shown));
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.docs.len(), 0);

        // Closing the start page itself changes nothing.
        dispatch(&mut state, Action::CloseTab(shown), &ctx);
        assert_eq!(active(&state), Some(shown));
        assert_eq!(state.tabs.len(), 1);
    }

    #[test]
    fn next_and_prev_tab_wrap_around() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        for name in ["a", "b", "c"] {
            open(&mut state, &ctx, &format!("/refdesk-test/{name}.pdf"));
        }
        let tabs = state.main_window().tabs.clone();
        assert_eq!(active(&state), Some(tabs[2]));

        dispatch(&mut state, Action::NextTab, &ctx);
        assert_eq!(active(&state), Some(tabs[0]));
        assert_eq!(state.focus, Some(tabs[0]));
        dispatch(&mut state, Action::PrevTab, &ctx);
        assert_eq!(active(&state), Some(tabs[2]));
        dispatch(&mut state, Action::PrevTab, &ctx);
        assert_eq!(active(&state), Some(tabs[1]));
    }

    #[test]
    fn set_zoom_changes_only_pdf_tabs() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let start = state.main_window().tabs[0];
        dispatch(
            &mut state,
            Action::SetZoom {
                tab: start,
                zoom: ZoomMode::FitPage,
            },
            &ctx,
        );
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        dispatch(
            &mut state,
            Action::SetZoom {
                tab,
                zoom: ZoomMode::FitPage,
            },
            &ctx,
        );
        assert_eq!(
            state.tabs.pdf(tab).expect("tab").view.zoom,
            ZoomMode::FitPage
        );
    }

    #[test]
    fn go_to_page_moves_to_the_top_of_the_page_and_records_history() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        dispatch(&mut state, Action::ShowGoToPage(tab), &ctx);
        assert!(state.tabs.pdf(tab).expect("tab").ui.goto.is_some());

        dispatch(&mut state, Action::GoToPage { tab, page: 6 }, &ctx);
        let pdf = state.tabs.pdf(tab).expect("tab");
        assert_eq!(pdf.view.anchor, Location::new(6, 0.0, 0.0));
        assert!(pdf.view.history.can_back());
        assert!(pdf.ui.goto.is_none(), "the field closes");
    }

    #[test]
    fn go_to_page_is_limited_to_the_document() {
        use refdesk_core::testutil::PdfBuilder;
        let pdf = PdfBuilder::new()
            .page(200.0, 200.0, |_| {})
            .page(200.0, 200.0, |_| {})
            .write_temp("goto-limit");
        let ctx = egui::Context::default();
        let repaints = crate::views::test_support::watch_repaints(&ctx);
        let mut state = AppState::new();
        open(
            &mut state,
            &ctx,
            pdf.path().to_str().expect("utf-8 temp path"),
        );
        let (tab, p) = state.focused_pdf().expect("pdf tab");
        let doc = state.docs.get(p.doc).expect("doc").clone();
        repaints.wait_until("the document info", || doc.info().is_some());
        dispatch(&mut state, Action::GoToPage { tab, page: 99 }, &ctx);
        assert_eq!(state.tabs.pdf(tab).expect("tab").view.anchor.page, 1);
    }

    #[test]
    fn duplicated_tabs_start_with_a_fresh_view_state() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        dispatch(&mut state, Action::ShowGoToPage(tab), &ctx);
        dispatch(&mut state, Action::DuplicateTab(tab), &ctx);
        let (_, copy) = state.focused_pdf().expect("copy");
        assert!(copy.ui.goto.is_none());
    }

    #[test]
    fn file_dialog_action_only_sets_a_request_flag() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(&mut state, Action::OpenFileDialog, &ctx);
        assert!(state.file_dialog_requested);
    }

    #[test]
    fn start_diff_opens_both_documents_as_tabs() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(
            &mut state,
            Action::StartDiff {
                left: PathBuf::from("/refdesk-test/old.pdf"),
                right: PathBuf::from("/refdesk-test/new.pdf"),
            },
            &ctx,
        );
        assert_eq!(state.docs.len(), 2);
        let tabs = state.main_window().tabs.clone();
        assert_eq!(tabs.len(), 2);
        let path_of = |tab: TabId| {
            let pdf = state.tabs.pdf(tab).expect("a pdf tab");
            state
                .docs
                .get(pdf.doc)
                .expect("document")
                .path()
                .to_path_buf()
        };
        assert_eq!(path_of(tabs[0]), PathBuf::from("/refdesk-test/old.pdf"));
        assert_eq!(path_of(tabs[1]), PathBuf::from("/refdesk-test/new.pdf"));
        assert_eq!(state.focus, Some(tabs[1]));
    }

    #[test]
    fn dispatch_queued_runs_actions_in_order_and_empties_the_queue() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        state.actions.push(Action::OpenFiles(vec![OpenTarget::parse(
            "/refdesk-test/a.pdf",
        )]));
        state.actions.push(Action::OpenFileDialog);
        dispatch_queued(&mut state, &ctx);
        assert!(state.actions.is_empty());
        assert_eq!(state.docs.len(), 1);
        assert!(state.file_dialog_requested);
    }
}
