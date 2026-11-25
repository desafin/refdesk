//! Moving tabs between windows: detaching into a new window, moving to another window, merging
//! every window into the main one, and closing a window (`docs/ARCHITECTURE.md` §5.4).
//!
//! These functions are the structural half of the window actions; only `dispatch` calls them
//! (rule R-6). A moved tab keeps its [`crate::state::TabId`] and its view state: only its place
//! in the window layouts changes.
//!
//! Rules every move follows:
//! - A window that only shows the start page drops it when a tab arrives (as when a document
//!   opens).
//! - A window left without tabs closes; the main window shows the start page instead.

use crate::state::{AppState, ClosedTab, DocId, Tab, TabId, WindowId};

/// Opens a new window that shows the start page, and focuses that page.
pub fn new_window(state: &mut AppState) -> WindowId {
    let id = state.add_window();
    let start = state.tabs.insert(Tab::Start);
    if let Some(w) = state.window_index(id) {
        state.windows[w].dock.add_tab(start, None);
    }
    state.focus = Some(start);
    id
}

/// Moves a PDF or comparison tab into a new window of its own and focuses it. Returns the new
/// window, or `None` (and changes nothing) when the tab is not such a tab in some window.
pub fn detach_tab(state: &mut AppState, tab: TabId) -> Option<WindowId> {
    if !movable(state, tab) {
        return None;
    }
    let from = state.windows[state.window_of(tab)?].id;
    let to = state.add_window();
    move_between(state, tab, from, to);
    Some(to)
}

/// Moves a PDF or comparison tab into the focused pane of window `to` and focuses it. Returns
/// false (and changes nothing) when there is no such window, the tab is not such a tab in some
/// window, or it already is in `to`.
pub fn move_tab(state: &mut AppState, tab: TabId, to: WindowId) -> bool {
    if !movable(state, tab) || state.window_index(to).is_none() {
        return false;
    }
    let Some(from) = state.window_of(tab).map(|w| state.windows[w].id) else {
        return false;
    };
    if from == to {
        return false;
    }
    move_between(state, tab, from, to);
    true
}

/// Moves the tabs of every other window into the focused pane of the main window (windows in
/// list order, tabs in layout order) and closes those windows. Start pages are dropped, not
/// moved. The focused tab stays focused.
pub fn merge_all(state: &mut AppState) {
    let main = state.main_window().id;
    let before = state.focus;
    let others: Vec<WindowId> = state.windows.iter().skip(1).map(|w| w.id).collect();
    for id in others {
        let Some(w) = state.window_index(id) else {
            continue;
        };
        for tab in state.windows[w].dock.tabs() {
            state.windows[w].dock.remove_tab(tab);
            if movable(state, tab) {
                put_tab(state, tab, 0, None);
            } else {
                state.tabs.remove(tab);
            }
        }
        state.remove_window(id);
    }
    let main_dock = &mut state.windows[0].dock;
    match before.filter(|t| main_dock.contains(*t)) {
        Some(tab) => {
            main_dock.activate(tab);
        }
        None => state.focus = main_dock.focused_tab(),
    }
    debug_assert_eq!(state.windows.len(), 1);
    debug_assert_eq!(state.windows[0].id, main);
}

/// Closes a window other than the main one with all its tabs. The PDF tabs are remembered for
/// reopening; a document no other tab (PDF or comparison) shows is dropped. Returns false (and changes nothing) for
/// the main window and for an unknown id.
pub fn close_window(state: &mut AppState, id: WindowId) -> bool {
    if id == state.main_window().id {
        return false;
    }
    let Some(w) = state.window_index(id) else {
        return false;
    };
    let mut docs: Vec<DocId> = Vec::new();
    for tab in state.windows[w].dock.tabs() {
        match state.tabs.remove(tab) {
            Some(Tab::Pdf(pdf)) => {
                if let Some(doc) = state.docs.get(pdf.doc) {
                    let path = doc.path().to_path_buf();
                    state.remember_closed(ClosedTab {
                        path,
                        view: pdf.view,
                    });
                }
                docs.push(pdf.doc);
            }
            Some(Tab::Diff(diff)) => docs.extend(diff.docs()),
            Some(Tab::Start) | None => {}
        }
    }
    state.remove_window(id);
    for doc in docs {
        if !state.tabs.doc_in_use(doc) {
            state.docs.remove(doc);
        }
    }
    if state.focus.is_none_or(|t| state.window_of(t).is_none()) {
        state.focus = state.main_window().dock.focused_tab();
    }
    true
}

/// Tabs that move between windows: PDF and comparison tabs (start pages are dropped instead).
fn movable(state: &AppState, tab: TabId) -> bool {
    matches!(state.tabs.get(tab), Some(Tab::Pdf(_) | Tab::Diff(_)))
}

/// Puts `tab` into window `w`: right after `near` when given and in that window, else into the
/// focused pane. A window that only shows start pages drops them first. Does not touch the
/// focus.
pub fn put_tab(state: &mut AppState, tab: TabId, w: usize, near: Option<TabId>) {
    let Some(window) = state.windows.get(w) else {
        return;
    };
    let present = window.dock.tabs();
    if present
        .iter()
        .all(|t| matches!(state.tabs.get(*t), Some(Tab::Start)))
    {
        for t in present {
            state.windows[w].dock.remove_tab(t);
            state.tabs.remove(t);
        }
    }
    state.windows[w].dock.add_tab(tab, near);
}

/// Moves `tab` from window `from` to window `to`, focuses it there and settles `from`.
fn move_between(state: &mut AppState, tab: TabId, from: WindowId, to: WindowId) {
    let (Some(f), Some(t)) = (state.window_index(from), state.window_index(to)) else {
        return;
    };
    state.windows[f].dock.remove_tab(tab);
    put_tab(state, tab, t, None);
    state.focus = Some(tab);
    settle(state, from);
}

/// After a tab left window `id`: an empty main window shows the start page, any other empty
/// window closes.
fn settle(state: &mut AppState, id: WindowId) {
    let Some(w) = state.window_index(id) else {
        return;
    };
    if !state.windows[w].dock.is_empty() {
        return;
    }
    if w == 0 {
        let start = state.tabs.insert(Tab::Start);
        state.windows[0].dock.add_tab(start, None);
    } else {
        state.remove_window(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, OpenTarget, dispatch};
    use refdesk_core::Location;

    fn open(state: &mut AppState, ctx: &egui::Context, path: &str) -> TabId {
        dispatch(state, Action::OpenFiles(vec![OpenTarget::parse(path)]), ctx);
        state.focus.expect("focused")
    }

    fn tabs_of(state: &AppState, w: usize) -> Vec<TabId> {
        state.windows[w].dock.tabs()
    }

    fn is_start_only(state: &AppState, w: usize) -> bool {
        let tabs = tabs_of(state, w);
        tabs.len() == 1 && matches!(state.tabs.get(tabs[0]), Some(Tab::Start))
    }

    fn ids(state: &AppState) -> Vec<WindowId> {
        state.windows.iter().map(|w| w.id).collect()
    }

    #[test]
    fn new_window_adds_a_window_with_the_start_page_and_ids_are_never_reused() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(&mut state, Action::NewWindow, &ctx);
        assert_eq!(state.windows.len(), 2);
        assert!(is_start_only(&state, 1));
        assert_eq!(state.focus, Some(tabs_of(&state, 1)[0]));
        let first = state.windows[1].id;
        assert_ne!(first, state.main_window().id);

        dispatch(&mut state, Action::CloseWindow(first), &ctx);
        assert_eq!(state.windows.len(), 1);
        dispatch(&mut state, Action::NewWindow, &ctx);
        dispatch(&mut state, Action::NewWindow, &ctx);
        let all = ids(&state);
        assert_eq!(all.len(), 3);
        assert!(!all.contains(&first), "a closed window's id came back");
        assert_ne!(all[1], all[2]);
    }

    #[test]
    fn detaching_moves_the_tab_with_its_view_into_a_new_window() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let b = open(&mut state, &ctx, "/refdesk-test/b.pdf");
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(b) {
            pdf.view.anchor = Location::new(7, 10.0, 300.0);
        }
        let before = state.tabs.pdf(b).expect("pdf").view.clone();
        let tab_count = state.tabs.len();

        dispatch(&mut state, Action::DetachTab(b), &ctx);
        assert_eq!(state.windows.len(), 2);
        assert_eq!(tabs_of(&state, 0), vec![a]);
        assert_eq!(tabs_of(&state, 1), vec![b]);
        assert_eq!(state.focus, Some(b));
        assert_eq!(state.windows[1].dock.focused_tab(), Some(b));
        // The same tab, not a copy: same id, same view, no tab added or lost.
        assert_eq!(state.tabs.len(), tab_count);
        assert_eq!(state.tabs.pdf(b).expect("pdf").view, before);
    }

    #[test]
    fn detaching_the_last_tab_of_the_main_window_leaves_the_start_page() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        dispatch(&mut state, Action::DetachTab(a), &ctx);
        assert_eq!(state.windows.len(), 2);
        assert!(is_start_only(&state, 0));
        assert_eq!(tabs_of(&state, 1), vec![a]);
        assert_eq!(state.focus, Some(a));
    }

    #[test]
    fn detaching_the_last_tab_of_another_window_closes_that_window() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let b = open(&mut state, &ctx, "/refdesk-test/b.pdf");
        dispatch(&mut state, Action::DetachTab(b), &ctx);
        let second = state.windows[1].id;

        dispatch(&mut state, Action::DetachTab(b), &ctx);
        assert_eq!(state.windows.len(), 2);
        assert!(!ids(&state).contains(&second));
        assert_eq!(tabs_of(&state, 1), vec![b]);
        assert_eq!(state.focus, Some(b));
    }

    #[test]
    fn the_start_page_and_unknown_tabs_are_not_detached() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let start = tabs_of(&state, 0)[0];
        dispatch(&mut state, Action::DetachTab(start), &ctx);
        dispatch(&mut state, Action::DetachTab(TabId::for_test(999)), &ctx);
        assert_eq!(state.windows.len(), 1);
        assert!(is_start_only(&state, 0));
    }

    #[test]
    fn moving_goes_into_the_focused_pane_of_the_target() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let b = open(&mut state, &ctx, "/refdesk-test/b.pdf");
        let c = open(&mut state, &ctx, "/refdesk-test/c.pdf");
        dispatch(&mut state, Action::DetachTab(b), &ctx);
        let second = state.windows[1].id;
        // Split the second window; its right pane gets the focus.
        dispatch(
            &mut state,
            Action::SplitTab {
                tab: b,
                dir: crate::windows::dock::SplitDir::Right,
            },
            &ctx,
        );
        let b2 = state.focus.expect("split view");

        dispatch(&mut state, Action::MoveTab { tab: c, to: second }, &ctx);
        assert_eq!(tabs_of(&state, 0), vec![a]);
        assert_eq!(state.windows[1].dock.pane_tabs(b2), vec![b2, c]);
        assert_eq!(state.windows[1].dock.pane_tabs(b), vec![b]);
        assert_eq!(state.focus, Some(c));
    }

    #[test]
    fn moving_into_a_start_page_window_replaces_the_start_page() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let b = open(&mut state, &ctx, "/refdesk-test/b.pdf");
        dispatch(&mut state, Action::NewWindow, &ctx);
        let second = state.windows[1].id;
        let start = tabs_of(&state, 1)[0];

        dispatch(&mut state, Action::MoveTab { tab: a, to: second }, &ctx);
        assert_eq!(tabs_of(&state, 1), vec![a]);
        assert!(state.tabs.get(start).is_none(), "start page tab dropped");
        assert_eq!(tabs_of(&state, 0), vec![b]);
    }

    #[test]
    fn moving_to_an_unknown_or_the_same_window_does_nothing() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        dispatch(&mut state, Action::NewWindow, &ctx);
        let windows_before = ids(&state);
        let focus_before = state.focus;

        let unknown = state.new_window_id();
        dispatch(
            &mut state,
            Action::MoveTab {
                tab: a,
                to: unknown,
            },
            &ctx,
        );
        let main = state.main_window().id;
        dispatch(&mut state, Action::MoveTab { tab: a, to: main }, &ctx);
        assert_eq!(ids(&state), windows_before);
        assert_eq!(tabs_of(&state, 0), vec![a]);
        assert!(is_start_only(&state, 1));
        assert_eq!(state.focus, focus_before);
    }

    #[test]
    fn moving_the_last_tab_out_of_the_main_window_leaves_the_start_page() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        dispatch(&mut state, Action::NewWindow, &ctx);
        let second = state.windows[1].id;
        dispatch(&mut state, Action::MoveTab { tab: a, to: second }, &ctx);
        assert!(is_start_only(&state, 0));
        assert_eq!(tabs_of(&state, 1), vec![a]);
    }

    #[test]
    fn merging_brings_every_tab_to_the_main_window_in_order() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let b = open(&mut state, &ctx, "/refdesk-test/b.pdf");
        let c = open(&mut state, &ctx, "/refdesk-test/c.pdf");
        let d = open(&mut state, &ctx, "/refdesk-test/d.pdf");
        let e = open(&mut state, &ctx, "/refdesk-test/e.pdf");
        dispatch(&mut state, Action::DetachTab(b), &ctx);
        let second = state.windows[1].id;
        dispatch(&mut state, Action::MoveTab { tab: c, to: second }, &ctx);
        dispatch(&mut state, Action::DetachTab(d), &ctx);
        let third = state.windows[2].id;
        dispatch(&mut state, Action::MoveTab { tab: e, to: third }, &ctx);
        dispatch(&mut state, Action::NewWindow, &ctx);
        assert_eq!(state.windows.len(), 4);
        assert_eq!(tabs_of(&state, 0), vec![a]);
        let tab_count = state.tabs.len();

        // Focus a tab of the second window: it stays focused after the merge.
        dispatch(&mut state, Action::FocusTab(c), &ctx);
        dispatch(&mut state, Action::MergeAllWindows, &ctx);
        assert_eq!(state.windows.len(), 1);
        assert_eq!(tabs_of(&state, 0), vec![a, b, c, d, e]);
        assert_eq!(state.focus, Some(c));
        assert_eq!(state.main_window().dock.focused_tab(), Some(c));
        // Only the fourth window's start page went away.
        assert_eq!(state.tabs.len(), tab_count - 1);
    }

    #[test]
    fn merging_into_a_start_page_main_window_drops_its_start_page() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        dispatch(&mut state, Action::DetachTab(a), &ctx);
        dispatch(&mut state, Action::MergeAllWindows, &ctx);
        assert_eq!(state.windows.len(), 1);
        assert_eq!(tabs_of(&state, 0), vec![a]);
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.focus, Some(a));
    }

    #[test]
    fn closing_a_window_closes_its_tabs_and_drops_documents_no_one_shows() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let b = open(&mut state, &ctx, "/refdesk-test/b.pdf");
        // A second view of a stays in the main window.
        dispatch(&mut state, Action::DuplicateTab(a), &ctx);
        let a2 = state.focus.expect("copy");
        dispatch(&mut state, Action::DetachTab(a2), &ctx);
        let second = state.windows[1].id;
        dispatch(&mut state, Action::MoveTab { tab: b, to: second }, &ctx);
        assert_eq!(tabs_of(&state, 1), vec![a2, b]);
        assert_eq!(state.docs.len(), 2);

        dispatch(&mut state, Action::CloseWindow(second), &ctx);
        assert_eq!(state.windows.len(), 1);
        assert_eq!(tabs_of(&state, 0), vec![a]);
        assert!(state.tabs.get(a2).is_none() && state.tabs.get(b).is_none());
        // a is still shown in the main window; b is shown nowhere.
        assert_eq!(state.docs.len(), 1);
        assert!(
            state
                .docs
                .find(std::path::Path::new("/refdesk-test/a.pdf"))
                .is_some()
        );
        assert!(
            state
                .docs
                .find(std::path::Path::new("/refdesk-test/b.pdf"))
                .is_none()
        );
        assert_eq!(state.focus, Some(a));
        assert_eq!(state.closed_tabs.len(), 2);

        // Ctrl+Shift+T brings them back into the focused window, last closed first.
        dispatch(&mut state, Action::ReopenClosedTab, &ctx);
        let reopened = state.focus.expect("reopened");
        let doc = state.tabs.pdf(reopened).expect("pdf").doc;
        assert_eq!(
            state.docs.get(doc).expect("doc").path(),
            std::path::Path::new("/refdesk-test/b.pdf")
        );
        assert_eq!(state.window_of(reopened), Some(0));
    }

    #[test]
    fn reopening_goes_to_the_focused_window() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let b = open(&mut state, &ctx, "/refdesk-test/b.pdf");
        dispatch(&mut state, Action::CloseTab(b), &ctx);
        dispatch(&mut state, Action::NewWindow, &ctx);
        dispatch(&mut state, Action::ReopenClosedTab, &ctx);
        let reopened = state.focus.expect("reopened");
        assert_eq!(state.window_of(reopened), Some(1));
        assert_eq!(tabs_of(&state, 1), vec![reopened], "start page made room");
    }

    #[test]
    fn closing_the_main_window_quits_and_keeps_the_state() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let main = state.main_window().id;
        let output = ctx.run_ui(egui::RawInput::default(), |_ui| {
            dispatch(&mut state, Action::CloseWindow(main), &ctx);
        });
        let commands: Vec<_> = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();
        assert!(commands.contains(&egui::ViewportCommand::Close));
        // Nothing is closed by hand: the app saves and exits with everything in place.
        assert_eq!(tabs_of(&state, 0), vec![a]);
        assert!(state.closed_tabs.is_empty());
    }

    #[test]
    fn closing_an_unknown_window_does_nothing() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let unknown = state.new_window_id();
        dispatch(&mut state, Action::CloseWindow(unknown), &ctx);
        assert_eq!(state.windows.len(), 1);
        assert_eq!(state.tabs.len(), 1);
    }

    #[test]
    fn one_document_in_two_windows_keeps_two_independent_views() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        dispatch(&mut state, Action::DuplicateTab(a), &ctx);
        let a2 = state.focus.expect("copy");
        dispatch(&mut state, Action::DetachTab(a2), &ctx);
        for (tab, page) in [(a, 3), (a2, 40)] {
            if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
                pdf.view.anchor = Location::new(page, 0.0, 0.0);
            }
        }
        // Moving windows around does not touch either view.
        dispatch(&mut state, Action::NewWindow, &ctx);
        let third = state.windows[2].id;
        dispatch(&mut state, Action::MoveTab { tab: a2, to: third }, &ctx);

        assert_eq!(state.docs.len(), 1);
        let (pa, pa2) = (
            state.tabs.pdf(a).expect("a"),
            state.tabs.pdf(a2).expect("a2"),
        );
        assert_eq!(pa.doc, pa2.doc);
        assert_eq!(pa.view.anchor.page, 3);
        assert_eq!(pa2.view.anchor.page, 40);
        assert_eq!(state.window_of(a), Some(0));
        assert_eq!(state.window_of(a2), Some(1));
    }

    #[test]
    fn opening_a_document_open_in_another_window_focuses_its_tab_there() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        open(&mut state, &ctx, "/refdesk-test/b.pdf");
        dispatch(&mut state, Action::DetachTab(a), &ctx);
        // Back in the main window.
        let main_tab = state.main_window().dock.focused_tab().expect("tab");
        dispatch(&mut state, Action::FocusTab(main_tab), &ctx);

        let again = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        assert_eq!(again, a);
        assert_eq!(state.window_of(a), Some(1));
        assert_eq!(state.windows[1].dock.focused_tab(), Some(a));
        assert_eq!(state.docs.len(), 2);
        assert_eq!(state.tabs.len(), 2);
    }

    fn start_diff(state: &mut AppState, ctx: &egui::Context, left: &str, right: &str) -> TabId {
        dispatch(
            state,
            Action::StartDiff {
                left: left.into(),
                right: right.into(),
            },
            ctx,
        );
        let tab = state.focus.expect("focused");
        assert!(state.tabs.diff(tab).is_some(), "a comparison tab");
        tab
    }

    #[test]
    fn a_comparison_tab_can_be_detached_and_moved_to_another_window() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let diff = start_diff(
            &mut state,
            &ctx,
            "/refdesk-test/a.pdf",
            "/refdesk-test/b.pdf",
        );
        dispatch(&mut state, Action::DetachTab(diff), &ctx);
        assert_eq!(state.windows.len(), 2);
        assert_eq!(tabs_of(&state, 1), vec![diff]);
        assert_eq!(state.focus, Some(diff));

        let main = state.main_window().id;
        dispatch(
            &mut state,
            Action::MoveTab {
                tab: diff,
                to: main,
            },
            &ctx,
        );
        assert_eq!(state.window_of(diff), Some(0));
        assert_eq!(state.windows.len(), 1, "the emptied window closed");
        assert!(
            state.tabs.diff(diff).is_some(),
            "the tab and its views are kept"
        );
        assert_eq!(state.docs.len(), 2);
    }

    #[test]
    fn merging_all_windows_keeps_a_comparison_tab_of_a_secondary_window() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        dispatch(&mut state, Action::NewWindow, &ctx);
        let diff = start_diff(
            &mut state,
            &ctx,
            "/refdesk-test/b.pdf",
            "/refdesk-test/c.pdf",
        );
        assert_eq!(state.window_of(diff), Some(1));
        dispatch(&mut state, Action::MergeAllWindows, &ctx);
        assert_eq!(state.windows.len(), 1);
        assert_eq!(state.window_of(diff), Some(0));
        assert!(state.tabs.diff(diff).is_some());
        assert_eq!(state.docs.len(), 3);
        assert_eq!(state.focus, Some(diff));
    }

    #[test]
    fn closing_a_window_with_a_comparison_tab_releases_its_documents() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        dispatch(&mut state, Action::NewWindow, &ctx);
        let diff = start_diff(
            &mut state,
            &ctx,
            "/refdesk-test/a.pdf",
            "/refdesk-test/b.pdf",
        );
        let id = state.windows[1].id;
        assert_eq!(state.docs.len(), 2);
        dispatch(&mut state, Action::CloseWindow(id), &ctx);
        assert!(state.tabs.get(diff).is_none());
        assert_eq!(
            state.docs.len(),
            1,
            "only the document a pdf tab still shows is left"
        );
        assert!(
            state
                .docs
                .find(std::path::Path::new("/refdesk-test/a.pdf"))
                .is_some()
        );
        assert!(
            state.closed_tabs.is_empty(),
            "comparison tabs are not reopened as pdf tabs"
        );
    }
}
