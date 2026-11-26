//! Actions and `dispatch`, the only place that changes the application structure (rule R-6).
//!
//! Widgets push [`Action`]s onto a queue; the queue is dispatched at the end of the frame
//! (`docs/ARCHITECTURE.md` §5.2). `dispatch` only changes state, so tests run it without a window.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use refdesk_core::{DiffOptions, DocumentHandle, Location, Notifier, Place, Rect, ZoomMode};

// `OpenTarget` lives in core because it travels between processes (`refdesk_core::ipc`).
use crate::overlays::capture;
use crate::overlays::palette::{self, PaletteMode};
use crate::overlays::translate;
use crate::panels::explorer;
use crate::panels::global_search;
use crate::panels::search;
use crate::panels::sidebar::{SidebarSection, SidebarView};
use crate::panels::status_bar::GotoInput;
use crate::panels::toc::TocState;
use crate::session;
use crate::state::{
    AppState, ClosedTab, DocId, PdfTab, StatusNotice, Tab, TabId, WindowId, normalize_path,
};
use crate::views::diff_view::{self, DiffDialog, DiffTab};
use crate::views::pdf_view::PdfViewUiState;
use crate::windows::dock::SplitDir;
use crate::windows::{self, transfer};
pub use refdesk_core::OpenTarget;

/// Something the user (or the outside world) asked for.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Open these files; the last one gets the focus.
    OpenFiles(Vec<OpenTarget>),
    /// Ask for the file chooser.
    OpenFileDialog,
    /// Make this folder the workspace and show it in the explorer (WS-1).
    OpenFolder(PathBuf),
    /// Ask for the folder chooser (WS-1).
    OpenFolderDialog,
    /// Close the workspace folder. Open tabs stay.
    CloseFolder,
    /// Scan the workspace folder again (WS-11).
    RefreshWorkspace,
    /// Open a file from the explorer of a window, in that window (WS-3, WS-9). Every way the
    /// explorer opens a file makes this action; `how` says what kind of tab.
    OpenFromExplorer {
        /// The file.
        path: PathBuf,
        /// The window whose explorer was used.
        window: WindowId,
        /// A preview tab, an ordinary one, one behind the current tab, …
        how: ExplorerOpen,
    },
    /// Close a tab.
    CloseTab(TabId),
    /// Close every other tab of the pane that holds this tab.
    CloseOtherTabs(TabId),
    /// Open the most recently closed PDF tab again, at the place it showed.
    ReopenClosedTab,
    /// Open the document of a PDF tab again, e.g. after the missing file came back (SES-6).
    RetryOpen(TabId),
    /// Show a tab and make it the target of commands.
    FocusTab(TabId),
    /// Open another view of the same document, copying the view state.
    DuplicateTab(TabId),
    /// Make a preview tab an ordinary tab that the explorer does not replace (WS-3).
    PinTab(TabId),
    /// Show another view of a PDF tab's document in a new pane beside it (TAB-3).
    SplitTab {
        /// The tab whose view is copied.
        tab: TabId,
        /// Where the new pane goes.
        dir: SplitDir,
    },
    /// In the focused pane, activate the tab after the active one, wrapping around.
    NextTab,
    /// In the focused pane, activate the tab before the active one, wrapping around.
    PrevTab,
    /// Move the focus to the next pane of the focused window.
    FocusNextPane,
    /// Change the zoom of a PDF tab.
    SetZoom {
        /// The tab to change.
        tab: TabId,
        /// The new zoom.
        zoom: ZoomMode,
    },
    /// Set the scroll speed multiplier, or `None` to go back to the value of `config.toml`
    /// (VIEW-12). The choice is kept in `prefs.json`.
    SetScrollSpeed(Option<f32>),
    /// Switch translation mode on or off (TR-1). The choice is kept in `prefs.json`; switching
    /// on loads the model, switching off closes the translation popups.
    ToggleTranslateMode,
    /// Translate the selected text of a PDF tab once, in translation mode or not (TR-2).
    TranslateSelection(TabId),
    /// Use this translation model, or `None` for the one of `config.toml` (TR-12). The choice is
    /// kept in `prefs.json`.
    SetTranslateModel(Option<String>),
    /// Fetch the model list of the translation server again (TR-12).
    RefreshTranslateModels,
    /// A translation was asked for while loading the model had failed: try loading it again.
    RetryTranslateWarmUp,
    /// Show a short message at the right end of the status bar for a few seconds.
    ShowNotice(String),
    /// Copy the selected text of a PDF tab (of the side in use for a comparison tab) to the
    /// clipboard (SEL-2).
    CopySelection(TabId),
    /// Remember a search in the search history (FIND-14). `replaces` is the query a search box
    /// recorded by settling just before; it is dropped when it is only the beginning of `query`.
    RecordSearch {
        /// The search to remember.
        query: refdesk_core::SearchQuery,
        /// A beginning of `query` that may be dropped from the history.
        replaces: Option<refdesk_core::SearchQuery>,
    },
    /// Remove the entry at this index (newest is 0) from the search history.
    RemoveSearchHistory(usize),
    /// Forget every search in the history.
    ClearSearchHistory,
    /// Open the search bar of a PDF tab; a selected text of one line is searched for (FIND-1,
    /// FIND-6).
    Find(TabId),
    /// Search for the selected text of a PDF tab, even over several lines (FIND-6).
    FindSelection(TabId),
    /// Turn the capture mode of a PDF tab on, or off when it is on: the next drag on a page
    /// captures that area (SHOT-1).
    StartCapture(TabId),
    /// Capture the whole page a PDF tab shows (SHOT-4).
    CapturePage(TabId),
    /// Capture a region of a page of a PDF tab: the rectangle is in page space and `scale_milli`
    /// is the render scale times 1000 (SHOT-1, SHOT-2). Sent by the view when a drag ends.
    CaptureRegion {
        /// The tab that was dragged in.
        tab: TabId,
        /// The page (zero-based).
        page: u32,
        /// The region in page space.
        rect: Rect,
        /// The render scale times 1000.
        scale_milli: u32,
    },
    /// Save the last capture of a PDF tab as a PNG file (SHOT-3).
    SaveCapture(TabId),
    /// Move a PDF tab to the next match of its search (FIND-1).
    FindNext(TabId),
    /// Move a PDF tab to the previous match of its search (FIND-1).
    FindPrev(TabId),
    /// Show the search in all open documents in the sidebar of the focused window and put the
    /// keyboard into its input field; a selected text of one line becomes the query (FIND-7).
    ShowGlobalSearch,
    /// Search all open documents for this text and show the search view (`--find`, FIND-12).
    GlobalSearchFor(String),
    /// Go to a result of the search in all documents: match `index` of `doc`, shown in a tab
    /// picked by `global_search::pick_tab` (FIND-8). Sent by a click on a result row.
    GlobalSearchGoTo {
        /// The document.
        doc: DocId,
        /// The match, an index into the matches of the document.
        index: usize,
        /// The window whose result list was clicked.
        window: WindowId,
    },
    /// Go to the next (or previous) result of the search in all documents, across documents
    /// (F4, Shift+F4, FIND-10). Without a query it shows the search view.
    GlobalSearchStep {
        /// Next rather than previous.
        forward: bool,
    },
    /// Clear the search in all documents: query, results and highlights (FIND-9).
    ClearGlobalSearch,
    /// Compare two documents in a new comparison tab of the focused pane (DIFF-1).
    StartDiff {
        /// The older document.
        left: PathBuf,
        /// The newer document.
        right: PathBuf,
    },
    /// Show or hide the thin lines at the edges of the excluded margins of a comparison tab
    /// while its options are closed (DIFF-11).
    ToggleDiffMarginGuides(TabId),
    /// Open the dialog for choosing two documents to compare.
    ShowDiffDialog,
    /// Compare the documents of a comparison tab again (after a cancel, an error or a change).
    RerunDiff(TabId),
    /// Stop the running comparison of a comparison tab.
    CancelDiff(TabId),
    /// Change what a comparison ignores; it compares again when the options differ.
    SetDiffOptions {
        /// The comparison tab.
        tab: TabId,
        /// The new options.
        options: DiffOptions,
    },
    /// Exchange the older and the newer document of a comparison tab and compare again.
    SwapDiffSides(TabId),
    /// Move both views of a comparison tab to the next change (DIFF-5).
    NextChange(TabId),
    /// Move both views of a comparison tab to the previous change (DIFF-5).
    PrevChange(TabId),
    /// Move both views of a comparison tab to change `index` and make it the current one (a
    /// click in the change list, DIFF-5).
    GoToChange {
        /// The comparison tab.
        tab: TabId,
        /// Index into `DiffResult::changes`.
        index: usize,
    },
    /// Show or hide the change list of a comparison tab (DIFF-5).
    ToggleChangesPanel(TabId),
    /// Open the page number field of the status bar for a PDF tab.
    ShowGoToPage(TabId),
    /// Open the palette with this list, or close it when it already shows it (CMD-1, CMD-2).
    ShowPalette(PaletteMode),
    /// Move a PDF tab to the top of a page (zero-based; limited to the document). Recorded in
    /// the history as a long jump.
    GoToPage {
        /// The tab to move.
        tab: TabId,
        /// Zero-based page.
        page: u32,
    },
    /// Open a new window that shows the start page (WIN-4).
    NewWindow,
    /// Move a PDF tab into a new window of its own (WIN-1).
    DetachTab(TabId),
    /// Move a PDF tab into the focused pane of another window (WIN-2).
    MoveTab {
        /// The tab to move.
        tab: TabId,
        /// The window it goes to.
        to: WindowId,
    },
    /// Move every tab of the other windows into the main window and close them (WIN-2).
    MergeAllWindows,
    /// Close a window and its tabs. For the main window this quits the application (WIN-5).
    CloseWindow(WindowId),
    /// Move a PDF tab to a place in its document as a recorded long jump (LINK-3), here or in a
    /// new view. The place is limited to the pages of the document. Used by the table of
    /// contents and by links.
    Navigate {
        /// The tab to move, or whose view is copied for the other [`OpenHow`] kinds.
        tab: TabId,
        /// Where to go.
        to: Location,
        /// Which view shows the place.
        how: OpenHow,
    },
    /// Move a PDF tab one step back in its history (LINK-2), to the exact place it left. The
    /// place it leaves is recorded, so that `Forward` returns to it. Nothing happens at the
    /// oldest entry.
    Back(TabId),
    /// Move a PDF tab one step forward in its history, mirroring `Back`.
    Forward(TabId),
    /// Show or hide the sidebar (explorer and table of contents) of the focused window (TOC-3,
    /// WS-7).
    ToggleToc,
    /// Show the explorer of the focused window: open the sidebar and its explorer section and
    /// put the keyboard in the filter box (WS-7).
    ShowExplorer,
    /// Fold or unfold one section of a window's sidebar. A hidden sidebar is shown with the
    /// section open (WS-7).
    ToggleSidebarSection {
        /// The window.
        window: WindowId,
        /// The section.
        section: SidebarSection,
    },
    /// Show or hide the section path line above PDF views (TOC-6).
    ToggleBreadcrumb,
    /// Close the application.
    Quit,
}

/// Which view an [`Action::Navigate`] moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenHow {
    /// The view that asked.
    Here,
    /// A new tab on the same document, next to the asking one. The asking view stays where it is.
    NewTab,
    /// A new tab on the same document in a new pane to the right. The asking view stays.
    SplitSide,
}

/// What kind of tab [`Action::OpenFromExplorer`] opens (WS-3, WS-9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplorerOpen {
    /// A preview tab: replaces the preview tab of the focused pane, else a new preview tab. A
    /// document that is open already gets the focus instead.
    Preview,
    /// An ordinary tab; a document already open in a preview tab pins that tab.
    Pinned,
    /// An ordinary tab right after the current one, which keeps the focus.
    Background,
    /// A new view in a new pane to the right of the focused pane.
    SplitSide,
    /// A new view in a new window.
    NewWindow,
}

/// What a click on a file row of the explorer opens (WS-3): a single click a preview tab, a
/// double click an ordinary tab (the first click of it already opened the preview, which the
/// second pins), a middle click or Ctrl+click an ordinary tab behind the current one.
pub fn explorer_click(double: bool, middle: bool, ctrl: bool) -> ExplorerOpen {
    if middle || ctrl {
        ExplorerOpen::Background
    } else if double {
        ExplorerOpen::Pinned
    } else {
        ExplorerOpen::Preview
    }
}

/// The preview tab among the tabs of a pane, if it has one (the first, should it have several).
pub fn preview_tab_in(pane: &[TabId], is_preview: impl Fn(TabId) -> bool) -> Option<TabId> {
    pane.iter().copied().find(|t| is_preview(*t))
}

/// Seconds a notice from [`Action::ShowNotice`] stays in the status bar.
const NOTICE_SECS: f64 = 4.0;

/// The notifier handed to documents: it asks egui for a new frame.
pub(crate) fn notifier(ctx: &egui::Context) -> Notifier {
    let ctx = ctx.clone();
    Arc::new(move || ctx.request_repaint())
}

/// Seconds the "scroll speed" notice stays in the status bar.
const SCROLL_NOTICE_SECS: f64 = 2.0;

/// Gives egui the scroll speed in effect. egui has one setting for all windows.
pub(crate) fn apply_scroll_speed(state: &AppState, ctx: &egui::Context) {
    let line = refdesk_core::line_scroll_speed(state.scroll_speed());
    ctx.options_mut(|o| o.input_options.line_scroll_speed = line);
}

/// Changes the scroll speed preference, applies it, saves `prefs.json` (when there is a place to
/// save) and tells the user. A failed save is only logged: the speed still applies until exit.
fn set_scroll_speed(state: &mut AppState, speed: Option<f32>, ctx: &egui::Context) {
    state.prefs.scroll_speed = speed.map(refdesk_core::clamp_scroll_speed);
    apply_scroll_speed(state, ctx);
    if let Some(paths) = &state.paths
        && let Err(err) = state.prefs.save(&paths.prefs_file())
    {
        log::warn!("could not save {}: {err}", paths.prefs_file().display());
    }
    let label = refdesk_core::scroll_speed_label(state.scroll_speed());
    state.status_notice = Some(StatusNotice {
        text: crate::i18n::tr("status.scroll_speed").replace("{}", &label),
        until: ctx.input(|i| i.time) + SCROLL_NOTICE_SECS,
    });
    ctx.request_repaint();
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
        Action::OpenFolder(path) => {
            explorer::open_folder(state, &path, ctx);
            show_files_view(state);
        }
        Action::OpenFolderDialog => {
            state.folder_dialog_requested = true;
            ctx.request_repaint();
        }
        Action::CloseFolder => explorer::close_folder(state),
        Action::RefreshWorkspace => explorer::refresh(state),
        Action::OpenFromExplorer { path, window, how } => {
            // The file opens in the window whose explorer was used.
            if let Some(w) = state.window_index(window) {
                state.follow_window_focus(w);
            }
            open_from_explorer(state, &path, how, ctx);
        }
        Action::CloseTab(tab) => close_tab(state, tab),
        Action::CloseOtherTabs(tab) => close_other_tabs(state, tab),
        Action::ReopenClosedTab => reopen_closed_tab(state, ctx),
        Action::RetryOpen(tab) => {
            if let Some(doc) = state.tabs.pdf(tab).and_then(|p| state.docs.get(p.doc)) {
                doc.reload();
            }
        }
        Action::FocusTab(tab) => {
            let before = state.focused_window_index();
            focus_tab(state, tab);
            // Quick open can pick a tab of another window: bring that window forward too.
            if let Some(w) = state.window_of(tab)
                && w != before
            {
                let id = state.windows[w].id;
                windows::raise(ctx, state, id);
            }
        }
        Action::DuplicateTab(tab) => {
            duplicate_tab(state, tab);
        }
        Action::PinTab(tab) => pin_tab(state, tab),
        Action::SplitTab { tab, dir } => {
            split_tab(state, tab, dir);
        }
        Action::NextTab => cycle_tab(state, true),
        Action::PrevTab => cycle_tab(state, false),
        Action::FocusNextPane => focus_next_pane(state),
        Action::SetZoom { tab, zoom } => match state.tabs.get_mut(tab) {
            Some(Tab::Pdf(pdf)) => pdf.view.zoom = zoom,
            // Both sides while they scroll together, else the side in use.
            Some(Tab::Diff(diff)) if diff.sync_scroll => {
                diff.left.view.zoom = zoom;
                diff.right.view.zoom = zoom;
            }
            Some(Tab::Diff(diff)) => diff.side_mut(diff.active).view.zoom = zoom,
            _ => {}
        },
        Action::SetScrollSpeed(speed) => set_scroll_speed(state, speed, ctx),
        Action::ToggleTranslateMode => translate::toggle_mode(state, ctx),
        Action::TranslateSelection(tab) => translate::translate_selection(state, tab, ctx),
        Action::SetTranslateModel(model) => translate::set_model(state, model, ctx),
        Action::RefreshTranslateModels => translate::refresh_models(state, ctx),
        Action::RetryTranslateWarmUp => translate::retry_warm_up(state, ctx),
        Action::ShowNotice(text) => {
            state.status_notice = Some(StatusNotice {
                text,
                until: ctx.input(|i| i.time) + NOTICE_SECS,
            });
            ctx.request_repaint();
        }
        Action::CopySelection(tab) => copy_selection(state, tab, ctx),
        Action::RecordSearch { query, replaces } => {
            crate::panels::search_history::record(state, &query, replaces.as_ref())
        }
        Action::RemoveSearchHistory(index) => crate::panels::search_history::remove(state, index),
        Action::ClearSearchHistory => crate::panels::search_history::clear(state, ctx),
        Action::Find(tab) => search::find(state, tab, false, ctx),
        Action::FindSelection(tab) => search::find(state, tab, true, ctx),
        Action::StartCapture(tab) => capture::start(state, tab),
        Action::CapturePage(tab) => capture::capture_page(state, tab, ctx),
        Action::CaptureRegion {
            tab,
            page,
            rect,
            scale_milli,
        } => capture::capture_region(state, tab, page, rect, scale_milli, ctx),
        Action::SaveCapture(tab) => capture::save(state, tab, ctx),
        Action::FindNext(tab) => search::step(state, tab, true, ctx),
        Action::FindPrev(tab) => search::step(state, tab, false, ctx),
        Action::ShowGlobalSearch => global_search::show(state, ctx),
        Action::GlobalSearchFor(text) => global_search::search_for(state, text, ctx),
        Action::GlobalSearchGoTo { doc, index, window } => {
            global_search::go_to_clicked(state, doc, index, window, ctx);
        }
        Action::GlobalSearchStep { forward } => global_search::step(state, forward, ctx),
        Action::ClearGlobalSearch => global_search::clear(state),
        Action::StartDiff { left, right } => {
            // The tabs whose documents are compared are kept open; a pick in an explorer is
            // used up.
            pin_tabs_showing(state, &[&left, &right]);
            for window in &mut state.windows {
                window.explorer.clear_compare_pick();
            }
            let tab = insert_diff_tab(state, &left, &right, ctx);
            place_new_tab(state, tab, None);
        }
        Action::ToggleDiffMarginGuides(tab) => {
            if let Some(Tab::Diff(diff)) = state.tabs.get_mut(tab) {
                diff.margin_guides = !diff.margin_guides;
            }
            ctx.request_repaint();
        }
        Action::ShowDiffDialog => {
            let path = |id: DocId| state.docs.get(id).map(|d| d.path().to_path_buf());
            let dialog = match state.focus.and_then(|t| state.tabs.get(t)) {
                Some(Tab::Pdf(pdf)) => DiffDialog::new(path(pdf.doc), None),
                Some(Tab::Diff(diff)) => DiffDialog::new(path(diff.left.doc), path(diff.right.doc)),
                _ => DiffDialog::default(),
            };
            state.diff_dialog = Some(dialog);
        }
        Action::RerunDiff(tab) => {
            if let Some(Tab::Diff(diff)) = state.tabs.get_mut(tab) {
                diff.start(&state.docs, notifier(ctx));
            }
        }
        Action::CancelDiff(tab) => {
            if let Some(Tab::Diff(diff)) = state.tabs.get_mut(tab) {
                diff.cancel();
            }
        }
        Action::SetDiffOptions { tab, options } => {
            if let Some(Tab::Diff(diff)) = state.tabs.get_mut(tab) {
                diff.set_options(options, &state.docs, notifier(ctx));
                let margins = diff.options.margins;
                diff_view::remember_margins(state, margins);
            }
        }
        Action::SwapDiffSides(tab) => {
            if let Some(Tab::Diff(diff)) = state.tabs.get_mut(tab) {
                diff.swap(&state.docs, notifier(ctx));
            }
        }
        Action::NextChange(tab) => {
            go_to_change(state, tab, true);
            // The change list follows the current change on the next frame.
            ctx.request_repaint();
        }
        Action::PrevChange(tab) => {
            go_to_change(state, tab, false);
            ctx.request_repaint();
        }
        Action::GoToChange { tab, index } => {
            if let Some(Tab::Diff(diff)) = state.tabs.get_mut(tab)
                && let Some(result) = diff.fresh_result(&state.docs)
            {
                diff.move_to_change(&result, index);
            }
            ctx.request_repaint();
        }
        Action::ToggleChangesPanel(tab) => {
            if let Some(Tab::Diff(diff)) = state.tabs.get_mut(tab) {
                diff.changes.open = !diff.changes.open;
            }
            ctx.request_repaint();
        }
        Action::ShowGoToPage(tab) => {
            if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
                pdf.ui.goto = Some(GotoInput::default());
            }
        }
        Action::ShowPalette(mode) => palette::toggle(state, mode, ctx),
        Action::GoToPage { tab, page } => go_to_page(state, tab, page),
        Action::NewWindow => {
            transfer::new_window(state);
        }
        Action::DetachTab(tab) => {
            if transfer::detach_tab(state, tab).is_some() {
                pin_tab(state, tab);
            }
        }
        Action::MoveTab { tab, to } => {
            if transfer::move_tab(state, tab, to) {
                pin_tab(state, tab);
                windows::raise(ctx, state, to);
            }
        }
        Action::MergeAllWindows => {
            // The panes of the main window would get several preview tabs.
            let moved: Vec<TabId> = state.windows[1..]
                .iter()
                .flat_map(|w| w.dock.tabs())
                .collect();
            for tab in moved {
                pin_tab(state, tab);
            }
            transfer::merge_all(state);
        }
        Action::CloseWindow(id) => {
            if id == state.main_window().id {
                quit(ctx);
            } else {
                session::record_window(state, id);
                transfer::close_window(state, id);
            }
        }
        Action::Navigate { tab, to, how } => navigate(state, tab, to, how),
        Action::Back(tab) => step_history(state, tab, false),
        Action::Forward(tab) => step_history(state, tab, true),
        Action::ToggleToc => {
            let w = state.focused_window_index();
            if let Some(window) = state.windows.get_mut(w) {
                window.toc_open = !window.toc_open;
            }
        }
        Action::ShowExplorer => {
            let w = state.focused_window_index();
            if let Some(window) = state.windows.get_mut(w) {
                window.toc_open = true;
                window.explorer_open = true;
                window.explorer.focus_filter = true;
                window.sidebar_view = SidebarView::Files;
            }
            ctx.request_repaint();
        }
        Action::ToggleSidebarSection { window, section } => {
            if let Some(window) = state
                .window_index(window)
                .and_then(|w| state.windows.get_mut(w))
            {
                window.sidebar_view = SidebarView::Files;
                let open = match section {
                    SidebarSection::Explorer => &mut window.explorer_open,
                    SidebarSection::Outline => &mut window.outline_open,
                };
                if window.toc_open {
                    *open = !*open;
                } else {
                    *open = true;
                    window.toc_open = true;
                }
            }
        }
        Action::ToggleBreadcrumb => {
            state.show_breadcrumb = !state.show_breadcrumb;
            ctx.request_repaint();
        }
        Action::Quit => quit(ctx),
    }
    palette::after_dispatch(state);
}

/// Turns the sidebar of the focused window back to its explorer and table of contents.
fn show_files_view(state: &mut AppState) {
    let w = state.focused_window_index();
    if let Some(window) = state.windows.get_mut(w) {
        window.sidebar_view = SidebarView::Files;
    }
}

/// Closes the main window, which ends the application (the state is saved on the way out).
fn quit(ctx: &egui::Context) {
    ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
}

/// Dispatches everything in `state.actions`, in order.
pub fn dispatch_queued(state: &mut AppState, ctx: &egui::Context) {
    let queue = std::mem::take(&mut state.actions);
    for action in queue {
        dispatch(state, action, ctx);
    }
}

/// Makes `tab` the active tab of its pane, gives the pane the focus and makes the tab the target
/// of commands. Nothing happens for a tab that is in no window.
fn focus_tab(state: &mut AppState, tab: TabId) {
    if let Some(w) = state.window_of(tab) {
        state.windows[w].dock.activate(tab);
        state.focus = Some(tab);
    }
}

/// Puts a new tab into the focused window (right after `near` when given, else into the focused
/// pane) and focuses it. A window that only shows the start page makes room first.
fn place_new_tab(state: &mut AppState, tab: TabId, near: Option<TabId>) {
    let w = near
        .and_then(|t| state.window_of(t))
        .unwrap_or_else(|| state.focused_window_index());
    transfer::put_tab(state, tab, w, near);
    focus_tab(state, tab);
}

/// Opens a document and shows it in a tab of the focused pane. An already open document is not
/// opened again: its existing tab gets the focus instead (OPEN-5).
fn open_file(state: &mut AppState, target: OpenTarget, ctx: &egui::Context) {
    if focus_open_document(state, &target, ctx) {
        return;
    }
    let tab = new_pdf_tab(state, target, false, ctx);
    place_new_tab(state, tab, None);
}

/// When a tab shows the document of `target` already, focuses it (jumping to the requested page)
/// and returns true.
fn focus_open_document(state: &mut AppState, target: &OpenTarget, ctx: &egui::Context) -> bool {
    let Some(tab) = state
        .docs
        .find(&target.path)
        .and_then(|doc| existing_tab_for(state, doc))
    else {
        return false;
    };
    if let Some(page) = target.page {
        jump_to_page(state, tab, page);
    }
    let from = state.focused_window_index();
    focus_tab(state, tab);
    // The tab may be in another window: bring that window forward.
    if let Some(w) = state.window_of(tab).filter(|w| *w != from) {
        windows::raise(ctx, state, state.windows[w].id);
    }
    true
}

/// A new PDF tab on the document of `target`, which is opened unless it is open already. It
/// starts where the reader left the document last time (SES-3), else at the top at the default
/// zoom. The tab is not put into any window yet.
fn new_pdf_tab(
    state: &mut AppState,
    target: OpenTarget,
    preview: bool,
    ctx: &egui::Context,
) -> TabId {
    let path = normalize_path(&target.path);
    // A document shown only by a comparison tab gets a view of its own on the same handle.
    let doc = open_document(state, &path, ctx);
    let remembered = session::remembered_view(state, &path);
    let mut view = remembered.view;
    if let Some(page) = target.page.filter(|_| !remembered.found) {
        let loc = Location::new(page, 0.0, 0.0);
        view.anchor = loc;
        view.history = refdesk_core::History::new(Place {
            doc: path.clone(),
            loc,
        });
    }
    let mut toc = Box::<TocState>::default();
    if let Some(collapsed) = remembered.toc_collapsed {
        toc.set_collapsed_paths(collapsed);
    }
    let tab = state.tabs.insert(Tab::Pdf(PdfTab {
        doc,
        view,
        ui: PdfViewUiState::default(),
        toc,
        preview,
    }));
    if remembered.found
        && let Some(page) = target.page
    {
        // A requested page is a jump from the remembered place, so Back returns there.
        jump_to_page(state, tab, page);
    }
    // Without a record by path, the content fingerprint may still find one (a moved file).
    let waiting = (!remembered.found && target.page.is_none()).then_some(tab);
    session::document_opened(state, waiting, &path);
    tab
}

/// `Action::OpenFromExplorer` after the focus moved to the explorer's window: opens `path` in
/// the kind of tab `how` asks for (WS-3, WS-9).
fn open_from_explorer(state: &mut AppState, path: &Path, how: ExplorerOpen, ctx: &egui::Context) {
    let target = OpenTarget {
        path: path.to_path_buf(),
        page: None,
    };
    let existing = state
        .docs
        .find(path)
        .and_then(|doc| existing_tab_for(state, doc));
    match how {
        ExplorerOpen::Preview => {
            if focus_open_document(state, &target, ctx) {
                return;
            }
            let w = state.focused_window_index();
            let dock = &state.windows[w].dock;
            let pane = dock
                .focused_tab()
                .map(|t| dock.pane_tabs(t))
                .unwrap_or_default();
            let is_preview = |t: TabId| state.tabs.pdf(t).is_some_and(|p| p.preview);
            match preview_tab_in(&pane, is_preview) {
                Some(old) => {
                    // A pane keeps at most one preview tab (a drag may have brought another).
                    for other in pane.iter().filter(|t| **t != old) {
                        pin_tab(state, *other);
                    }
                    let tab = replace_preview(state, old, target, ctx);
                    focus_tab(state, tab);
                }
                None => {
                    let tab = new_pdf_tab(state, target, true, ctx);
                    place_new_tab(state, tab, None);
                }
            }
        }
        ExplorerOpen::Pinned => {
            if focus_open_document(state, &target, ctx) {
                if let Some(tab) = state.focus {
                    pin_tab(state, tab);
                }
                return;
            }
            let tab = new_pdf_tab(state, target, false, ctx);
            place_new_tab(state, tab, None);
        }
        ExplorerOpen::Background => {
            if let Some(tab) = existing {
                pin_tab(state, tab);
                return;
            }
            let tab = new_pdf_tab(state, target, false, ctx);
            let w = state.focused_window_index();
            let keep = state.focus.filter(|t| state.windows[w].dock.contains(*t));
            transfer::put_tab(state, tab, w, keep);
            // A window that showed only the start page has nothing else to keep showing.
            match keep.filter(|t| state.windows[w].dock.contains(*t)) {
                Some(keep) => {
                    state.windows[w].dock.activate(keep);
                }
                None => focus_tab(state, tab),
            }
        }
        ExplorerOpen::SplitSide => {
            let Some(tab) = view_for(state, existing, target, ctx) else {
                return;
            };
            let w = state.focused_window_index();
            let base = state.windows[w]
                .dock
                .focused_tab()
                .filter(|t| matches!(state.tabs.get(*t), Some(Tab::Pdf(_) | Tab::Diff(_))));
            match base {
                Some(base) if state.windows[w].dock.split(base, tab, SplitDir::Right) => {
                    state.focus = Some(tab);
                }
                _ => place_new_tab(state, tab, None),
            }
        }
        ExplorerOpen::NewWindow => {
            let Some(tab) = view_for(state, existing, target, ctx) else {
                return;
            };
            let id = state.add_window();
            if let Some(w) = state.window_index(id) {
                state.windows[w].dock.add_tab(tab, None);
            }
            state.focus = Some(tab);
        }
    }
}

/// A new ordinary tab for a document: a copy of the view of `existing` when a tab shows the
/// document already, else a tab opened as usual. Not put into any window yet.
fn view_for(
    state: &mut AppState,
    existing: Option<TabId>,
    target: OpenTarget,
    ctx: &egui::Context,
) -> Option<TabId> {
    match existing {
        Some(tab) => copy_view(state, tab),
        None => Some(new_pdf_tab(state, target, false, ctx)),
    }
}

/// Shows the document of `target` in place of preview tab `old`: a new preview tab takes the
/// same spot in the same pane and `old` goes. Where `old` was in its document is remembered
/// (SES-3), and its document is closed when no other tab shows it. Returns the new tab.
///
/// A new tab rather than a changed one, so that nothing kept per tab (the wait for a
/// fingerprint, the focus order) carries over from the old document to the new one.
fn replace_preview(
    state: &mut AppState,
    old: TabId,
    target: OpenTarget,
    ctx: &egui::Context,
) -> TabId {
    session::record_tab(state, old);
    let tab = new_pdf_tab(state, target, true, ctx);
    let Some(w) = state.window_of(old) else {
        place_new_tab(state, tab, None);
        return tab;
    };
    state.windows[w].dock.add_tab(tab, Some(old));
    state.windows[w].dock.remove_tab(old);
    if let Some(Tab::Pdf(pdf)) = state.tabs.remove(old)
        && !state.tabs.doc_in_use(pdf.doc)
    {
        state.docs.remove(pdf.doc);
    }
    if state.focus == Some(old) {
        state.focus = Some(tab);
    }
    tab
}

/// Makes a preview tab an ordinary one. Nothing for other tabs.
fn pin_tab(state: &mut AppState, tab: TabId) {
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        pdf.preview = false;
    }
}

/// Pins every preview tab that shows one of `paths`.
fn pin_tabs_showing(state: &mut AppState, paths: &[&Path]) {
    let docs: Vec<DocId> = paths.iter().filter_map(|p| state.docs.find(p)).collect();
    let tabs: Vec<TabId> = state
        .tabs
        .iter()
        .filter(|(_, tab)| matches!(tab, Tab::Pdf(p) if p.preview && docs.contains(&p.doc)))
        .map(|(id, _)| id)
        .collect();
    for tab in tabs {
        pin_tab(state, tab);
    }
}

/// Moves both views of a comparison tab to the next or previous change, when its result is
/// still valid.
fn go_to_change(state: &mut AppState, tab: TabId, forward: bool) {
    if let Some(Tab::Diff(diff)) = state.tabs.get_mut(tab)
        && let Some(result) = diff.fresh_result(&state.docs)
    {
        diff.go_to_change(&result, forward);
    }
}

/// The open document of `path`, opened now if no tab shows it yet.
fn open_document(state: &mut AppState, path: &Path, ctx: &egui::Context) -> DocId {
    let path = normalize_path(path);
    match state.docs.find(&path) {
        Some(doc) => doc,
        None => state
            .docs
            .insert(DocumentHandle::open(&path, notifier(ctx))),
    }
}

/// Creates a comparison tab of `left` (older) and `right` (newer) and starts comparing; the
/// documents are shared with the tabs that already show them. The tab is not put into any
/// window yet. Used by [`Action::StartDiff`] and for restoring a `SessionTab::Diff`.
pub fn insert_diff_tab(
    state: &mut AppState,
    left: &Path,
    right: &Path,
    ctx: &egui::Context,
) -> TabId {
    let left_doc = open_document(state, left, ctx);
    let right_doc = open_document(state, right, ctx);
    let mut diff = DiffTab::new(
        diff_view::side_tab(left_doc, normalize_path(left)),
        diff_view::side_tab(right_doc, normalize_path(right)),
    );
    diff.start_with_margins(state.prefs.diff_margins);
    diff.start(&state.docs, notifier(ctx));
    state.tabs.insert(Tab::Diff(Box::new(diff)))
}

/// The tab to show for a document that is already open: the focused one if it shows the
/// document, else the first in window order.
fn existing_tab_for(state: &AppState, doc: DocId) -> Option<TabId> {
    let shows_doc = |t: &TabId| matches!(state.tabs.get(*t), Some(Tab::Pdf(p)) if p.doc == doc);
    state
        .focus
        .filter(|t| shows_doc(t) && state.window_of(*t).is_some())
        .or_else(|| {
            state
                .windows
                .iter()
                .flat_map(|w| w.dock.tabs())
                .find(shows_doc)
        })
}

/// Moves a view to `to` as a recorded long jump: the place the view is at now and the new place
/// go into the history (LINK-3), and a jump to the same spot adds nothing.
fn jump_to(state: &mut AppState, tab: TabId, to: Location) {
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        let doc = pdf.view.history.current().doc.clone();
        let from = Place {
            doc: doc.clone(),
            loc: pdf.view.anchor,
        };
        let to = Place { doc, loc: to };
        pdf.view.anchor = to.loc;
        pdf.view.history.jump(from, to);
    }
}

/// Moves a view to the top of `page` as a recorded long jump.
fn jump_to_page(state: &mut AppState, tab: TabId, page: u32) {
    jump_to(state, tab, Location::new(page, 0.0, 0.0));
}

/// `page` limited to the pages of the document of `tab`, when they are known. `None` for a
/// document without pages.
fn limit_page(state: &AppState, tab: TabId, page: u32) -> Option<u32> {
    let pdf = state.tabs.pdf(tab)?;
    let count = state
        .docs
        .get(pdf.doc)
        .and_then(|doc| doc.info())
        .map(|info| info.page_sizes.len() as u32);
    match count {
        Some(0) => None,
        Some(n) => Some(page.min(n - 1)),
        None => Some(page),
    }
}

/// Jumps a PDF tab to the top of `page`, limited to the pages of its document when they are
/// known.
fn go_to_page(state: &mut AppState, tab: TabId, page: u32) {
    let Some(page) = limit_page(state, tab, page) else {
        return;
    };
    jump_to_page(state, tab, page);
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        pdf.ui.goto = None;
    }
}

/// Moves `tab` (or a new view copied from it) to `to` as a recorded long jump. A new view
/// copies the history of `tab` first, so going back there leads to where `tab` was.
fn navigate(state: &mut AppState, tab: TabId, to: Location, how: OpenHow) {
    let Some(page) = limit_page(state, tab, to.page) else {
        return;
    };
    let to = Location { page, ..to };
    let target = match how {
        OpenHow::Here => Some(tab),
        OpenHow::NewTab => duplicate_tab(state, tab),
        OpenHow::SplitSide => split_tab(state, tab, SplitDir::Right),
    };
    if let Some(target) = target {
        jump_to(state, target, to);
        mark_arrival(state, target, to);
    }
}

/// Moves `tab` one entry back or forward in its history, to the place stored there. The place
/// the view is at now is handed to the history first, so that the step the other way returns
/// here. The history gets no new entry.
fn step_history(state: &mut AppState, tab: TabId, forward: bool) {
    let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) else {
        return;
    };
    let current = Place {
        doc: pdf.view.history.current().doc.clone(),
        loc: pdf.view.anchor,
    };
    let step = if forward {
        pdf.view.history.forward(current)
    } else {
        pdf.view.history.back(current)
    };
    let Some(place) = step else {
        return;
    };
    // The document may have changed since the place was stored.
    let page = limit_page(state, tab, place.loc.page).unwrap_or(place.loc.page);
    let loc = Location { page, ..place.loc };
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        pdf.view.anchor = loc;
    }
    mark_arrival(state, tab, loc);
}

/// Has the view of `tab` mark the place it just jumped to for a moment (LINK-9).
fn mark_arrival(state: &mut AppState, tab: TabId, loc: Location) {
    if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
        pdf.ui.links.mark_arrival(loc);
    }
}

/// Copies the selected text of a PDF tab. Does nothing without a selection.
fn copy_selection(state: &mut AppState, tab: TabId, ctx: &egui::Context) {
    let AppState { docs, tabs, .. } = state;
    // A comparison tab copies from the side in use.
    let pdf = match tabs.get_mut(tab) {
        Some(Tab::Pdf(pdf)) => pdf,
        Some(Tab::Diff(diff)) => {
            let side = diff.active;
            diff.side_mut(side)
        }
        _ => return,
    };
    if let Some(doc) = docs.get(pdf.doc) {
        crate::views::pdf_view::copy_selection(ctx, doc, &mut pdf.ui);
    }
}

/// Closes a tab, remembers it for reopening, drops its document when no other tab shows it, and
/// keeps the window non-empty by showing the start page after the last tab. When the focus was in
/// that window, it moves to what the window now shows focused (a neighbor in the same pane, or
/// another pane).
fn close_tab(state: &mut AppState, tab: TabId) {
    let Some(w) = state.window_of(tab) else {
        return;
    };
    // Closing the start page when it is the only tab would just bring it back.
    if state.windows[w].dock.tab_count() == 1 && matches!(state.tabs.get(tab), Some(Tab::Start)) {
        return;
    }
    session::record_tab(state, tab);
    let focus_here = state
        .focus
        .is_none_or(|f| f == tab || state.window_of(f) == Some(w));
    state.windows[w].dock.remove_tab(tab);
    match state.tabs.remove(tab) {
        Some(Tab::Pdf(pdf)) => {
            if let Some(doc) = state.docs.get(pdf.doc) {
                let path = doc.path().to_path_buf();
                state.remember_closed(ClosedTab {
                    path,
                    view: pdf.view,
                });
            }
            if !state.tabs.doc_in_use(pdf.doc) {
                state.docs.remove(pdf.doc);
            }
        }
        // Dropping the tab cancels its comparison; its documents go when no tab shows them.
        Some(Tab::Diff(diff)) => {
            for doc in diff.docs() {
                if !state.tabs.doc_in_use(doc) {
                    state.docs.remove(doc);
                }
            }
        }
        Some(Tab::Start) | None => {}
    }
    if state.windows[w].dock.is_empty() {
        let start = state.tabs.insert(Tab::Start);
        state.windows[w].dock.add_tab(start, None);
    }
    if focus_here {
        state.focus = state.windows[w].dock.focused_tab();
    }
}

/// Closes the other tabs of the pane that holds `tab`, then focuses `tab`.
fn close_other_tabs(state: &mut AppState, tab: TabId) {
    let Some(w) = state.window_of(tab) else {
        return;
    };
    for other in state.windows[w].dock.pane_tabs(tab) {
        if other != tab {
            close_tab(state, other);
        }
    }
    focus_tab(state, tab);
}

/// Opens the most recently closed PDF tab again with its view state, in the focused pane. The
/// document is opened again when no tab shows it any more.
fn reopen_closed_tab(state: &mut AppState, ctx: &egui::Context) {
    let Some(closed) = state.closed_tabs.pop() else {
        return;
    };
    let doc = match state.docs.find(&closed.path) {
        Some(doc) => doc,
        None => {
            let doc = state
                .docs
                .insert(DocumentHandle::open(&closed.path, notifier(ctx)));
            session::document_opened(state, None, &closed.path);
            doc
        }
    };
    let tab = state.tabs.insert(Tab::Pdf(PdfTab {
        doc,
        view: closed.view,
        ui: PdfViewUiState::default(),
        toc: Box::default(),
        // A tab brought back is an ordinary tab, whatever it was.
        preview: false,
    }));
    place_new_tab(state, tab, None);
}

/// A new PDF tab on the same document as `tab` with a copy of its view state; `None` when `tab`
/// is not a PDF tab.
fn copy_view(state: &mut AppState, tab: TabId) -> Option<TabId> {
    let mut source = state.tabs.pdf(tab).cloned()?;
    // The copy starts with a fresh view; its scroll follows from the copied anchor.
    source.ui = PdfViewUiState::default();
    source.preview = false;
    Some(state.tabs.insert(Tab::Pdf(source)))
}

/// Adds a new tab on the same document right after `tab`, with a copy of the view state, and
/// returns it.
fn duplicate_tab(state: &mut AppState, tab: TabId) -> Option<TabId> {
    state.window_of(tab)?;
    let new = copy_view(state, tab)?;
    // A tab that was worth a second view is kept (WS-3).
    pin_tab(state, tab);
    place_new_tab(state, new, Some(tab));
    Some(new)
}

/// Splits the pane of `tab` and shows a copy of its view in the new pane, which gets the focus.
/// Returns the new tab.
fn split_tab(state: &mut AppState, tab: TabId, dir: SplitDir) -> Option<TabId> {
    let w = state.window_of(tab)?;
    let new = copy_view(state, tab)?;
    if state.windows[w].dock.split(tab, new, dir) {
        pin_tab(state, tab);
        state.focus = Some(new);
        Some(new)
    } else {
        state.tabs.remove(new);
        None
    }
}

/// Activates the neighbor of the active tab in the focused pane, wrapping around.
fn cycle_tab(state: &mut AppState, forward: bool) {
    if let Some(tab) = state.focused_window().dock.cycle(forward) {
        focus_tab(state, tab);
    }
}

/// Moves the focus to the next pane of the focused window.
fn focus_next_pane(state: &mut AppState) {
    let w = state.focused_window_index();
    if let Some(tab) = state.windows[w].dock.focus_next_pane() {
        state.focus = Some(tab);
    }
}

/// Tests of the scroll speed setting (VIEW-12).
#[cfg(test)]
pub(crate) mod scroll_tests {
    use super::*;
    use refdesk_core::{Config, Paths, Prefs};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A directory under the system temp directory, removed on drop. Tests never touch the real
    /// home directory: every path is injected from here.
    pub(crate) struct ScratchDir(pub PathBuf);

    impl ScratchDir {
        pub(crate) fn new(name: &str) -> ScratchDir {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "refdesk-wp18-{}-{}-{name}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            ScratchDir(path)
        }

        pub(crate) fn paths(&self) -> Paths {
            Paths::under(&self.0)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn line_speed(ctx: &egui::Context) -> f32 {
        ctx.options(|o| o.input_options.line_scroll_speed)
    }

    fn state_with_paths(dir: &ScratchDir) -> AppState {
        let mut state = AppState::new();
        state.paths = Some(dir.paths());
        state
    }

    #[test]
    fn setting_a_speed_changes_the_state_egui_and_the_saved_file() {
        let dir = ScratchDir::new("set");
        let ctx = egui::Context::default();
        let mut state = state_with_paths(&dir);
        dispatch(&mut state, Action::SetScrollSpeed(Some(3.0)), &ctx);
        assert_eq!(state.scroll_speed(), 3.0);
        assert_eq!(line_speed(&ctx), 120.0);
        let saved = Prefs::load(&dir.paths().prefs_file());
        assert_eq!(saved.scroll_speed, Some(3.0));
        assert!(!refdesk_core::store::bad_path(&dir.paths().prefs_file()).exists());
    }

    #[test]
    fn a_speed_outside_the_range_is_limited() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(&mut state, Action::SetScrollSpeed(Some(99.0)), &ctx);
        assert_eq!(state.scroll_speed(), 10.0);
        assert_eq!(line_speed(&ctx), 400.0);
        dispatch(&mut state, Action::SetScrollSpeed(Some(0.0)), &ctx);
        assert_eq!(state.scroll_speed(), 0.25);
    }

    #[test]
    fn none_goes_back_to_the_config_value_and_clears_the_saved_item() {
        let dir = ScratchDir::new("reset");
        let ctx = egui::Context::default();
        let mut state = state_with_paths(&dir);
        state.config = Config {
            scroll_speed: 4.0,
            ..Config::default()
        };
        dispatch(&mut state, Action::SetScrollSpeed(Some(1.0)), &ctx);
        assert_eq!(state.scroll_speed(), 1.0);
        assert_eq!(line_speed(&ctx), 40.0);

        dispatch(&mut state, Action::SetScrollSpeed(None), &ctx);
        assert_eq!(state.scroll_speed(), 4.0);
        assert_eq!(line_speed(&ctx), 160.0);
        assert_eq!(state.prefs.scroll_speed, None);
        let text = std::fs::read_to_string(dir.paths().prefs_file()).expect("prefs.json");
        assert!(!text.contains("scroll_speed"), "{text}");
        assert_eq!(Prefs::load(&dir.paths().prefs_file()).scroll_speed, None);
    }

    #[test]
    fn without_a_save_path_nothing_is_written_and_the_speed_still_applies() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        assert!(state.paths.is_none());
        dispatch(&mut state, Action::SetScrollSpeed(Some(3.0)), &ctx);
        assert_eq!(state.scroll_speed(), 3.0);
        assert_eq!(line_speed(&ctx), 120.0);
    }

    #[test]
    fn a_failed_save_does_not_stop_the_change() {
        let dir = ScratchDir::new("unwritable");
        // The state directory is a file, so `prefs.json` cannot be created inside it.
        std::fs::write(dir.0.join("state"), b"in the way").expect("write blocker");
        let ctx = egui::Context::default();
        let mut state = state_with_paths(&dir);
        dispatch(&mut state, Action::SetScrollSpeed(Some(5.0)), &ctx);
        assert_eq!(state.scroll_speed(), 5.0);
        assert_eq!(line_speed(&ctx), 200.0);
        assert!(!dir.paths().prefs_file().exists());
    }

    #[test]
    fn changing_the_speed_shows_a_notice_that_runs_out() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        assert!(state.status_notice.is_none());
        dispatch(&mut state, Action::SetScrollSpeed(Some(2.5)), &ctx);
        let notice = state.status_notice.clone().expect("a notice");
        assert!(notice.text.contains("2.5×"), "{}", notice.text);
        assert!(notice.until > 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MAX_CLOSED_TABS;
    use refdesk_core::ViewState;
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
        state.main_window().dock.focused_tab()
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
        assert_eq!(state.main_window().dock.tabs().len(), 2);
        let last = *state.main_window().dock.tabs().last().expect("two tabs");
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
        let first = state.main_window().dock.tabs()[0];
        assert_ne!(state.focus, Some(first));

        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        assert_eq!(state.docs.len(), 2);
        assert_eq!(state.main_window().dock.tabs().len(), 2);
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
        assert_eq!(state.main_window().dock.tabs().len(), 1);
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
        assert_eq!(state.main_window().dock.tabs().len(), 1);
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
        assert_eq!(state.main_window().dock.tabs().len(), 2);
        let (copy, copy_tab) = state.focused_pdf().expect("the copy is focused");
        assert_ne!(copy, tab);
        let original = state.tabs.pdf(tab).expect("original");
        assert_eq!(copy_tab.doc, original.doc);
        assert_eq!(copy_tab.view, original.view);
        assert_eq!(copy_tab.view.zoom, ZoomMode::Custom(2.0));
        // The copy sits next to its source.
        assert_eq!(state.main_window().dock.tabs(), vec![tab, copy]);
    }

    #[test]
    fn duplicating_the_start_page_does_nothing() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let start = state.main_window().dock.tabs()[0];
        dispatch(&mut state, Action::DuplicateTab(start), &ctx);
        assert_eq!(state.main_window().dock.tabs().len(), 1);
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
        let tabs = state.main_window().dock.tabs();
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
        let tabs = state.main_window().dock.tabs();
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

        assert_eq!(state.main_window().dock.tabs().len(), 1);
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
        let tabs = state.main_window().dock.tabs();
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
        let start = state.main_window().dock.tabs()[0];
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
    fn start_diff_opens_both_documents_in_one_comparison_tab() {
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
        let tabs = state.main_window().dock.tabs();
        assert_eq!(tabs.len(), 1);
        let diff = state.tabs.diff(tabs[0]).expect("a comparison tab");
        let path_of = |doc: DocId| state.docs.get(doc).expect("document").path().to_path_buf();
        assert_eq!(
            path_of(diff.left.doc),
            PathBuf::from("/refdesk-test/old.pdf")
        );
        assert_eq!(
            path_of(diff.right.doc),
            PathBuf::from("/refdesk-test/new.pdf")
        );
        assert_eq!(state.focus, Some(tabs[0]));
    }

    #[test]
    fn closing_a_comparison_tab_drops_documents_no_other_tab_shows() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        for _ in 0..2 {
            dispatch(
                &mut state,
                Action::StartDiff {
                    left: PathBuf::from("/refdesk-test/same.pdf"),
                    right: PathBuf::from("/refdesk-test/same.pdf"),
                },
                &ctx,
            );
        }
        assert_eq!(state.docs.len(), 1, "one file, opened once");
        let tabs = state.main_window().dock.tabs();
        dispatch(&mut state, Action::CloseTab(tabs[0]), &ctx);
        assert_eq!(state.docs.len(), 1, "the other comparison still shows it");
        dispatch(&mut state, Action::CloseTab(tabs[1]), &ctx);
        assert_eq!(state.docs.len(), 0);
        assert!(
            state.closed_tabs.is_empty(),
            "comparisons are not reopened as pdf tabs"
        );
    }

    #[test]
    fn zooming_a_comparison_tab_zooms_both_sides_while_synchronized() {
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
        let tab = state.focus.expect("comparison");
        let zoom = ZoomMode::Custom(1.5);
        dispatch(&mut state, Action::SetZoom { tab, zoom }, &ctx);
        let diff = state.tabs.diff(tab).expect("comparison");
        assert_eq!((diff.left.view.zoom, diff.right.view.zoom), (zoom, zoom));
        if let Some(Tab::Diff(diff)) = state.tabs.get_mut(tab) {
            diff.sync_scroll = false;
            diff.active = refdesk_core::Side::Right;
        }
        dispatch(
            &mut state,
            Action::SetZoom {
                tab,
                zoom: ZoomMode::FitPage,
            },
            &ctx,
        );
        let diff = state.tabs.diff(tab).expect("comparison");
        assert_eq!(
            (diff.left.view.zoom, diff.right.view.zoom),
            (zoom, ZoomMode::FitPage)
        );
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

    // --- panes (WP-20) -----------------------------------------------------------------------

    fn split(state: &mut AppState, ctx: &egui::Context, tab: TabId, dir: SplitDir) -> TabId {
        dispatch(state, Action::SplitTab { tab, dir }, ctx);
        state.focus.expect("the new view is focused")
    }

    fn panes(state: &AppState) -> Vec<Vec<TabId>> {
        state
            .main_window()
            .dock
            .pane_rects()
            .into_iter()
            .map(|(_, tabs)| tabs)
            .collect()
    }

    #[test]
    fn opening_a_file_adds_its_tab_to_the_focused_pane() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (a, _) = state.focused_pdf().expect("pdf tab");
        let a2 = split(&mut state, &ctx, a, SplitDir::Right);
        open(&mut state, &ctx, "/refdesk-test/b.pdf");
        let (b, _) = state.focused_pdf().expect("pdf tab");
        assert_eq!(panes(&state), vec![vec![a], vec![a2, b]]);

        dispatch(&mut state, Action::FocusTab(a), &ctx);
        open(&mut state, &ctx, "/refdesk-test/c.pdf");
        let (c, _) = state.focused_pdf().expect("pdf tab");
        assert_eq!(panes(&state), vec![vec![a, c], vec![a2, b]]);
        assert_eq!(state.main_window().dock.focused_tab(), Some(c));
    }

    #[test]
    fn splitting_shows_two_independent_views_of_one_document() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (a, _) = state.focused_pdf().expect("pdf tab");
        let b = split(&mut state, &ctx, a, SplitDir::Down);

        assert_ne!(a, b);
        assert_eq!(state.docs.len(), 1);
        assert_eq!(state.main_window().dock.pane_count(), 2);
        assert_eq!(state.main_window().dock.focused_tab(), Some(b));
        let doc_of = |t: TabId| state.tabs.pdf(t).expect("pdf").doc;
        assert_eq!(doc_of(a), doc_of(b));

        // Moving one view leaves the other where it was.
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(b) {
            pdf.view.anchor = Location::new(7, 10.0, 300.0);
        }
        dispatch(
            &mut state,
            Action::SetZoom {
                tab: b,
                zoom: ZoomMode::Custom(2.0),
            },
            &ctx,
        );
        let va = &state.tabs.pdf(a).expect("a").view;
        let vb = &state.tabs.pdf(b).expect("b").view;
        assert_eq!(va.anchor, Location::new(0, 0.0, 0.0));
        assert_eq!(va.zoom, ZoomMode::FitWidth);
        assert_eq!(vb.anchor.page, 7);
        assert_ne!(va, vb);
    }

    #[test]
    fn the_start_page_cannot_be_split() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let start = state.main_window().dock.tabs()[0];
        dispatch(
            &mut state,
            Action::SplitTab {
                tab: start,
                dir: SplitDir::Right,
            },
            &ctx,
        );
        assert_eq!(state.main_window().dock.pane_count(), 1);
        assert_eq!(state.tabs.len(), 1);
    }

    #[test]
    fn closing_a_tab_focuses_its_neighbor_in_the_same_pane() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        for name in ["a", "b", "c"] {
            open(&mut state, &ctx, &format!("/refdesk-test/{name}.pdf"));
        }
        let tabs = state.main_window().dock.tabs();
        let other = split(&mut state, &ctx, tabs[0], SplitDir::Right);
        dispatch(&mut state, Action::FocusTab(tabs[1]), &ctx);

        dispatch(&mut state, Action::CloseTab(tabs[1]), &ctx);
        assert_eq!(state.focus, Some(tabs[2]));
        assert_eq!(panes(&state), vec![vec![tabs[0], tabs[2]], vec![other]]);
    }

    #[test]
    fn closing_the_last_tab_of_a_pane_removes_the_pane_and_focuses_another() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        open(&mut state, &ctx, "/refdesk-test/b.pdf");
        let (b, _) = state.focused_pdf().expect("pdf tab");
        let b2 = split(&mut state, &ctx, b, SplitDir::Right);

        dispatch(&mut state, Action::CloseTab(b2), &ctx);
        assert_eq!(state.main_window().dock.pane_count(), 1);
        assert_eq!(state.focus, Some(b));
        assert_eq!(state.main_window().dock.focused_tab(), Some(b));
    }

    #[test]
    fn closing_a_tab_in_another_pane_keeps_the_focus() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (a, _) = state.focused_pdf().expect("pdf tab");
        let a2 = split(&mut state, &ctx, a, SplitDir::Right);
        dispatch(&mut state, Action::CloseTab(a), &ctx);
        assert_eq!(state.focus, Some(a2));
        assert_eq!(state.main_window().dock.pane_count(), 1);
    }

    #[test]
    fn closing_every_tab_of_a_split_window_shows_the_start_page() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (a, _) = state.focused_pdf().expect("pdf tab");
        let a2 = split(&mut state, &ctx, a, SplitDir::Down);
        dispatch(&mut state, Action::CloseTab(a), &ctx);
        dispatch(&mut state, Action::CloseTab(a2), &ctx);
        let tabs = state.main_window().dock.tabs();
        assert_eq!(tabs.len(), 1);
        assert!(matches!(state.tabs.get(tabs[0]), Some(Tab::Start)));
        assert_eq!(state.focus, Some(tabs[0]));
        assert_eq!(state.docs.len(), 0);
    }

    #[test]
    fn reopening_a_closed_tab_restores_its_place() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (a, _) = state.focused_pdf().expect("pdf tab");
        dispatch(&mut state, Action::GoToPage { tab: a, page: 12 }, &ctx);
        dispatch(
            &mut state,
            Action::SetZoom {
                tab: a,
                zoom: ZoomMode::Custom(1.5),
            },
            &ctx,
        );
        let before = state.tabs.pdf(a).expect("a").view.clone();

        dispatch(&mut state, Action::CloseTab(a), &ctx);
        // The closed tab does not keep the document open.
        assert_eq!(state.docs.len(), 0);
        assert_eq!(state.closed_tabs.len(), 1);

        dispatch(&mut state, Action::ReopenClosedTab, &ctx);
        let (back, pdf) = state.focused_pdf().expect("reopened");
        assert_ne!(back, a);
        assert_eq!(pdf.view, before);
        assert!(pdf.view.history.can_back(), "the history came back too");
        assert_eq!(state.docs.len(), 1);
        assert!(state.closed_tabs.is_empty());
        // The reopened tab replaced the start page.
        assert_eq!(state.main_window().dock.tabs(), vec![back]);

        // Nothing left to reopen: nothing happens.
        dispatch(&mut state, Action::ReopenClosedTab, &ctx);
        assert_eq!(state.tabs.len(), 1);
    }

    #[test]
    fn reopening_a_view_of_a_still_open_document_adds_a_view() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (a, _) = state.focused_pdf().expect("pdf tab");
        dispatch(&mut state, Action::DuplicateTab(a), &ctx);
        let (copy, _) = state.focused_pdf().expect("copy");
        dispatch(&mut state, Action::GoToPage { tab: copy, page: 3 }, &ctx);
        dispatch(&mut state, Action::CloseTab(copy), &ctx);
        assert_eq!(state.docs.len(), 1);

        dispatch(&mut state, Action::ReopenClosedTab, &ctx);
        let (back, pdf) = state.focused_pdf().expect("reopened");
        assert_eq!(pdf.view.anchor.page, 3);
        assert_eq!(pdf.doc, state.tabs.pdf(a).expect("a").doc);
        assert_eq!(state.main_window().dock.tabs(), vec![a, back]);
    }

    #[test]
    fn the_start_page_is_not_remembered_and_only_twenty_tabs_are() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        for i in 0..25 {
            open(&mut state, &ctx, &format!("/refdesk-test/{i}.pdf"));
            let (tab, _) = state.focused_pdf().expect("pdf tab");
            dispatch(&mut state, Action::CloseTab(tab), &ctx);
        }
        assert_eq!(state.closed_tabs.len(), MAX_CLOSED_TABS);
        assert_eq!(
            state.closed_tabs.first().map(|c| c.path.clone()),
            Some(PathBuf::from("/refdesk-test/5.pdf"))
        );
        assert_eq!(
            state.closed_tabs.last().map(|c| c.path.clone()),
            Some(PathBuf::from("/refdesk-test/24.pdf"))
        );
    }

    #[test]
    fn close_other_tabs_only_touches_the_pane() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        for name in ["a", "b", "c"] {
            open(&mut state, &ctx, &format!("/refdesk-test/{name}.pdf"));
        }
        let tabs = state.main_window().dock.tabs();
        let other = split(&mut state, &ctx, tabs[2], SplitDir::Right);
        dispatch(&mut state, Action::CloseOtherTabs(tabs[1]), &ctx);
        assert_eq!(panes(&state), vec![vec![tabs[1]], vec![other]]);
        assert_eq!(state.focus, Some(tabs[1]));
        assert_eq!(state.closed_tabs.len(), 2);
        // `c` is still shown by the split view.
        assert_eq!(state.docs.len(), 2);
    }

    #[test]
    fn next_tab_and_focus_next_pane_stay_inside_their_scope() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        open(&mut state, &ctx, "/refdesk-test/b.pdf");
        let tabs = state.main_window().dock.tabs();
        let right = split(&mut state, &ctx, tabs[1], SplitDir::Right);

        // The new pane has one tab: next tab stays on it.
        dispatch(&mut state, Action::NextTab, &ctx);
        assert_eq!(state.focus, Some(right));

        dispatch(&mut state, Action::FocusNextPane, &ctx);
        assert_eq!(state.focus, Some(tabs[1]));
        dispatch(&mut state, Action::NextTab, &ctx);
        assert_eq!(state.focus, Some(tabs[0]));
        dispatch(&mut state, Action::PrevTab, &ctx);
        assert_eq!(state.focus, Some(tabs[1]));
        dispatch(&mut state, Action::FocusNextPane, &ctx);
        assert_eq!(state.focus, Some(right));
        assert_eq!(state.main_window().dock.focused_tab(), Some(right));
    }

    #[test]
    fn reopening_an_open_document_focuses_its_tab_in_another_pane() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (a, _) = state.focused_pdf().expect("pdf tab");
        open(&mut state, &ctx, "/refdesk-test/b.pdf");
        let (b, _) = state.focused_pdf().expect("pdf tab");
        split(&mut state, &ctx, b, SplitDir::Right);
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        assert_eq!(state.focus, Some(a));
        assert_eq!(state.main_window().dock.focused_tab(), Some(a));
        assert_eq!(state.tabs.len(), 3);
    }

    fn at(page: u32, x: f32, y: f32) -> Location {
        Location::new(page, x, y)
    }

    fn view(state: &AppState, tab: TabId) -> &ViewState {
        &state.tabs.pdf(tab).expect("pdf tab").view
    }

    #[test]
    fn navigate_here_moves_the_view_and_back_returns_to_the_exact_spot() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        // The reader has scrolled into the middle of page 2.
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
            pdf.view.anchor = at(1, 12.0, 250.0);
        }
        let to = at(5, 0.0, 120.0);
        dispatch(
            &mut state,
            Action::Navigate {
                tab,
                to,
                how: OpenHow::Here,
            },
            &ctx,
        );
        assert_eq!(view(&state, tab).anchor, to);
        assert!(view(&state, tab).history.can_back());
        assert_eq!(view(&state, tab).history.len(), 2);

        // Going back leads to where the reader was, including the position inside the page.
        let current = Place {
            doc: PathBuf::from("/refdesk-test/a.pdf"),
            loc: to,
        };
        let mut history = view(&state, tab).history.clone();
        let back = history
            .back(current)
            .expect("there is a place to go back to");
        assert_eq!(back.loc, at(1, 12.0, 250.0));
        let forward = history.forward(back).expect("and forward again");
        assert_eq!(forward.loc, to);
    }

    #[test]
    fn back_and_forward_return_to_the_place_that_was_left_without_new_entries() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        let start = at(1, 12.0, 250.0);
        let to = at(5, 0.0, 120.0);
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
            pdf.view.anchor = start;
        }
        dispatch(
            &mut state,
            Action::Navigate {
                tab,
                to,
                how: OpenHow::Here,
            },
            &ctx,
        );
        // The reader scrolls a little before going back: that place is the one to return to.
        let read_on = at(5, 0.0, 400.0);
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
            pdf.view.anchor = read_on;
        }
        dispatch(&mut state, Action::Back(tab), &ctx);
        assert_eq!(view(&state, tab).anchor, start);
        assert!(!view(&state, tab).history.can_back());
        assert!(view(&state, tab).history.can_forward());
        dispatch(&mut state, Action::Forward(tab), &ctx);
        assert_eq!(view(&state, tab).anchor, read_on);
        assert_eq!(view(&state, tab).history.len(), 2);
    }

    #[test]
    fn back_and_forward_with_nowhere_to_go_change_nothing() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        let before = view(&state, tab).clone();
        dispatch(&mut state, Action::Back(tab), &ctx);
        dispatch(&mut state, Action::Forward(tab), &ctx);
        assert_eq!(view(&state, tab), &before);
        let Some(Tab::Pdf(pdf)) = state.tabs.get(tab) else {
            panic!("pdf tab");
        };
        assert!(pdf.ui.links.arrival.is_none(), "no move, no mark");
        // A tab that is not a PDF view: no panic.
        dispatch(&mut state, Action::Back(TabId::for_test(999)), &ctx);
        dispatch(&mut state, Action::Forward(TabId::for_test(999)), &ctx);
    }

    #[test]
    fn jumps_back_and_forward_mark_the_place_they_arrive_at() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        let arrival = |state: &AppState| match state.tabs.get(tab) {
            Some(Tab::Pdf(pdf)) => pdf.ui.links.arrival,
            _ => None,
        };
        let to = at(5, 0.0, 120.0);
        dispatch(
            &mut state,
            Action::Navigate {
                tab,
                to,
                how: OpenHow::Here,
            },
            &ctx,
        );
        assert_eq!(arrival(&state), Some(crate::views::links::Arrival::new(to)));
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
            pdf.ui.links.arrival = None;
        }
        dispatch(&mut state, Action::Back(tab), &ctx);
        let start = view(&state, tab).anchor;
        assert_eq!(
            arrival(&state),
            Some(crate::views::links::Arrival::new(start))
        );
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
            pdf.ui.links.arrival = None;
        }
        dispatch(&mut state, Action::Forward(tab), &ctx);
        assert!(arrival(&state).is_some_and(|a| a.loc == to));
    }

    #[test]
    fn navigating_to_the_same_spot_does_not_grow_the_history() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        let to = at(3, 0.0, 80.0);
        for _ in 0..3 {
            dispatch(
                &mut state,
                Action::Navigate {
                    tab,
                    to,
                    how: OpenHow::Here,
                },
                &ctx,
            );
        }
        assert_eq!(view(&state, tab).history.len(), 2);
        assert_eq!(view(&state, tab).anchor, to);
        // A spot a few points away on the same page is the same spot too (LINK-3).
        dispatch(
            &mut state,
            Action::Navigate {
                tab,
                to: at(3, 0.0, 81.0),
                how: OpenHow::Here,
            },
            &ctx,
        );
        assert_eq!(view(&state, tab).history.len(), 2);
    }

    #[test]
    fn go_to_page_uses_the_same_path_as_navigate() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (a, _) = state.focused_pdf().expect("pdf tab");
        dispatch(&mut state, Action::DuplicateTab(a), &ctx);
        let (b, _) = state.focused_pdf().expect("pdf tab");
        dispatch(&mut state, Action::GoToPage { tab: a, page: 4 }, &ctx);
        dispatch(
            &mut state,
            Action::Navigate {
                tab: b,
                to: at(4, 0.0, 0.0),
                how: OpenHow::Here,
            },
            &ctx,
        );
        assert_eq!(view(&state, a).anchor, view(&state, b).anchor);
        assert_eq!(view(&state, a).history, view(&state, b).history);
    }

    #[test]
    fn navigate_in_a_new_tab_leaves_the_original_where_it_was() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (a, _) = state.focused_pdf().expect("pdf tab");
        let before = view(&state, a).clone();
        let to = at(7, 0.0, 33.0);
        dispatch(
            &mut state,
            Action::Navigate {
                tab: a,
                to,
                how: OpenHow::NewTab,
            },
            &ctx,
        );
        assert_eq!(view(&state, a), &before, "the asking view did not move");
        let (new, _) = state.focused_pdf().expect("the new tab is focused");
        assert_ne!(new, a);
        assert_eq!(view(&state, new).anchor, to);
        assert!(
            view(&state, new).history.can_back(),
            "back in the new tab leads to where it came from"
        );
        // Same pane, right after the original; same document.
        assert_eq!(state.main_window().dock.pane_tabs(a), vec![a, new]);
        assert_eq!(state.docs.len(), 1);
        assert_eq!(
            state.tabs.pdf(a).map(|t| t.doc),
            state.tabs.pdf(new).map(|t| t.doc)
        );
    }

    #[test]
    fn navigate_to_a_split_adds_a_pane() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (a, _) = state.focused_pdf().expect("pdf tab");
        let before = view(&state, a).clone();
        assert_eq!(state.main_window().dock.pane_count(), 1);
        let to = at(2, 0.0, 10.0);
        dispatch(
            &mut state,
            Action::Navigate {
                tab: a,
                to,
                how: OpenHow::SplitSide,
            },
            &ctx,
        );
        assert_eq!(state.main_window().dock.pane_count(), 2);
        assert_eq!(view(&state, a), &before);
        let (new, _) = state.focused_pdf().expect("the new view is focused");
        assert_eq!(view(&state, new).anchor, to);
        let panes: Vec<Vec<TabId>> = state
            .main_window()
            .dock
            .pane_rects()
            .into_iter()
            .map(|(_, tabs)| tabs)
            .collect();
        assert_eq!(panes, vec![vec![a], vec![new]]);
    }

    #[test]
    fn navigate_ignores_tabs_that_are_not_pdf_tabs() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let start = state.main_window().dock.tabs()[0];
        for how in [OpenHow::Here, OpenHow::NewTab, OpenHow::SplitSide] {
            dispatch(
                &mut state,
                Action::Navigate {
                    tab: start,
                    to: at(1, 0.0, 0.0),
                    how,
                },
                &ctx,
            );
        }
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.main_window().dock.pane_count(), 1);
    }

    #[test]
    fn navigate_limits_the_page_to_the_document() {
        use refdesk_core::testutil::PdfBuilder;
        let pdf = PdfBuilder::new()
            .page(200.0, 200.0, |_| {})
            .page(200.0, 200.0, |_| {})
            .write_temp("navigate-limit");
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
        dispatch(
            &mut state,
            Action::Navigate {
                tab,
                to: at(99, 0.0, 40.0),
                how: OpenHow::Here,
            },
            &ctx,
        );
        assert_eq!(view(&state, tab).anchor, at(1, 0.0, 40.0));
    }

    #[test]
    fn toggle_toc_changes_only_the_focused_window() {
        use crate::panels::toc::DEFAULT_TOC_WIDTH;
        use crate::state::WindowState;
        use crate::windows::dock::DockLayout;
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let id = state.new_window_id();
        let second_tab = state.tabs.insert(Tab::Start);
        let mut dock = DockLayout::new();
        dock.add_tab(second_tab, None);
        state.windows.push(WindowState {
            dock,
            toc_open: false,
            toc_width: DEFAULT_TOC_WIDTH,
            ..WindowState::new(id)
        });
        let open_flags =
            |state: &AppState| -> Vec<bool> { state.windows.iter().map(|w| w.toc_open).collect() };
        assert_eq!(open_flags(&state), vec![false, false]);

        state.focus = Some(second_tab);
        dispatch(&mut state, Action::ToggleToc, &ctx);
        assert_eq!(open_flags(&state), vec![false, true]);
        dispatch(&mut state, Action::ToggleToc, &ctx);
        assert_eq!(open_flags(&state), vec![false, false]);

        let first_tab = state.main_window().dock.tabs()[0];
        state.focus = Some(first_tab);
        dispatch(&mut state, Action::ToggleToc, &ctx);
        assert_eq!(open_flags(&state), vec![true, false]);
        assert_eq!(state.windows[1].toc_width, DEFAULT_TOC_WIDTH);
    }
}

/// Tests of preview tabs and the other ways the explorer opens files (WS-3, WS-9).
#[cfg(test)]
mod preview_tests {
    use super::*;
    use crate::windows::tab_label;
    use refdesk_core::testutil::PdfBuilder;

    fn explorer_open(state: &mut AppState, ctx: &egui::Context, path: &str, how: ExplorerOpen) {
        let window = state.focused_window().id;
        explorer_open_in(state, ctx, path, how, window);
    }

    fn explorer_open_in(
        state: &mut AppState,
        ctx: &egui::Context,
        path: &str,
        how: ExplorerOpen,
        window: WindowId,
    ) {
        dispatch(
            state,
            Action::OpenFromExplorer {
                path: PathBuf::from(path),
                window,
                how,
            },
            ctx,
        );
    }

    fn open(state: &mut AppState, ctx: &egui::Context, path: &str) -> TabId {
        dispatch(state, Action::OpenFiles(vec![OpenTarget::parse(path)]), ctx);
        state.focus.expect("focused")
    }

    fn is_preview(state: &AppState, tab: TabId) -> bool {
        state.tabs.pdf(tab).expect("pdf tab").preview
    }

    fn path_of(state: &AppState, tab: TabId) -> PathBuf {
        let doc = state.tabs.pdf(tab).expect("pdf tab").doc;
        state.docs.get(doc).expect("document").path().to_path_buf()
    }

    /// The tabs of the main window with their documents and whether they are previews.
    fn main_tabs(state: &AppState) -> Vec<(String, bool)> {
        state
            .main_window()
            .dock
            .tabs()
            .into_iter()
            .map(|t| {
                let name = path_of(state, t)
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned();
                (name, is_preview(state, t))
            })
            .collect()
    }

    #[test]
    fn the_kind_of_click_decides_the_kind_of_tab() {
        assert_eq!(explorer_click(false, false, false), ExplorerOpen::Preview);
        assert_eq!(explorer_click(true, false, false), ExplorerOpen::Pinned);
        assert_eq!(explorer_click(false, true, false), ExplorerOpen::Background);
        assert_eq!(explorer_click(false, false, true), ExplorerOpen::Background);
        assert_eq!(explorer_click(true, false, true), ExplorerOpen::Background);
    }

    #[test]
    fn the_preview_tab_of_a_pane_is_found_among_its_tabs() {
        let (a, b, c) = (TabId::for_test(1), TabId::for_test(2), TabId::for_test(3));
        assert_eq!(preview_tab_in(&[a, b, c], |t| t == b), Some(b));
        assert_eq!(preview_tab_in(&[a, c], |t| t == b), None);
        assert_eq!(preview_tab_in(&[], |_| true), None);
    }

    #[test]
    fn a_single_click_opens_a_preview_tab_and_the_next_one_replaces_it_in_place() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/wp72/left.pdf");
        open(&mut state, &ctx, "/refdesk-test/wp72/right.pdf");
        let left = state.main_window().dock.tabs()[0];
        dispatch(&mut state, Action::FocusTab(left), &ctx);
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Preview,
        );
        let first = state.focus.expect("focus");
        assert!(is_preview(&state, first));
        assert_eq!(
            main_tabs(&state),
            [
                ("left.pdf".into(), false),
                ("right.pdf".into(), false),
                ("a.pdf".into(), true)
            ]
        );
        // Move the preview tab between the two others: its spot is kept when it is replaced.
        let tabs = state.main_window().dock.tabs();
        state.windows[0].dock.remove_tab(tabs[2]);
        state.windows[0].dock.add_tab(tabs[2], Some(tabs[0]));

        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/b.pdf",
            ExplorerOpen::Preview,
        );
        assert_eq!(
            main_tabs(&state),
            [
                ("left.pdf".into(), false),
                ("b.pdf".into(), true),
                ("right.pdf".into(), false)
            ],
            "same number of tabs, same spot, the new document"
        );
        let second = state.focus.expect("focus");
        assert_eq!(
            path_of(&state, second),
            Path::new("/refdesk-test/wp72/b.pdf")
        );
        assert!(
            state
                .docs
                .find(Path::new("/refdesk-test/wp72/a.pdf"))
                .is_none(),
            "the replaced document is closed"
        );
        assert_eq!(state.docs.len(), 3);
    }

    #[test]
    fn a_replaced_document_that_another_tab_shows_stays_open() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Preview,
        );
        let preview = state.focus.expect("focus");
        // Another view of `a` in a second pane. Splitting pins the source, so the source is
        // made a preview again by hand.
        let other = split_tab(&mut state, preview, SplitDir::Right).expect("split");
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(preview) {
            pdf.preview = true;
        }
        dispatch(&mut state, Action::FocusTab(preview), &ctx);
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/b.pdf",
            ExplorerOpen::Preview,
        );
        assert!(
            state
                .docs
                .find(Path::new("/refdesk-test/wp72/a.pdf"))
                .is_some()
        );
        assert_eq!(
            path_of(&state, other),
            Path::new("/refdesk-test/wp72/a.pdf")
        );
    }

    #[test]
    fn a_single_click_on_an_open_document_goes_to_its_tab_and_keeps_the_preview() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let pinned = open(&mut state, &ctx, "/refdesk-test/wp72/pinned.pdf");
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Preview,
        );
        let preview = state.focus.expect("focus");
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/pinned.pdf",
            ExplorerOpen::Preview,
        );
        assert_eq!(state.focus, Some(pinned));
        assert!(!is_preview(&state, pinned));
        assert!(is_preview(&state, preview), "the preview tab is left alone");
        assert_eq!(state.main_window().dock.tab_count(), 2);
        // The preview's own document: back to it, nothing replaced.
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Preview,
        );
        assert_eq!(state.focus, Some(preview));
        assert!(is_preview(&state, preview));
    }

    #[test]
    fn a_double_click_pins_the_preview_and_the_next_single_click_makes_a_new_preview() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        // The two clicks of a double click: the first opens the preview, the second pins it.
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Preview,
        );
        let a = state.focus.expect("focus");
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Pinned,
        );
        assert_eq!(state.focus, Some(a));
        assert!(
            !is_preview(&state, a),
            "the same tab became an ordinary tab"
        );
        assert_eq!(state.main_window().dock.tab_count(), 1);

        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/b.pdf",
            ExplorerOpen::Preview,
        );
        assert_eq!(
            main_tabs(&state),
            [("a.pdf".into(), false), ("b.pdf".into(), true)],
            "the pinned tab is not replaced"
        );
        // A double click on a file that is not open opens an ordinary tab directly.
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/c.pdf",
            ExplorerOpen::Pinned,
        );
        assert_eq!(main_tabs(&state)[2], ("c.pdf".into(), false));
        assert_eq!(main_tabs(&state)[1], ("b.pdf".into(), true));
    }

    #[test]
    fn using_a_preview_tab_pins_it() {
        let ctx = egui::Context::default();
        type Use = fn(&mut AppState, &egui::Context, TabId);
        let uses: [(&str, Use); 7] = [
            ("PinTab (title double click, tab menu)", |s, c, t| {
                dispatch(s, Action::PinTab(t), c)
            }),
            ("the Keep Tab Open command", |s, c, _| {
                let action = crate::commands::command("tab.pin")
                    .and_then(|cmd| cmd.run(s))
                    .expect("the command runs for a preview tab");
                dispatch(s, action, c);
            }),
            ("duplicate", |s, c, t| {
                dispatch(s, Action::DuplicateTab(t), c)
            }),
            ("split", |s, c, t| {
                dispatch(
                    s,
                    Action::SplitTab {
                        tab: t,
                        dir: SplitDir::Right,
                    },
                    c,
                )
            }),
            ("detach", |s, c, t| dispatch(s, Action::DetachTab(t), c)),
            ("move to another window", |s, c, t| {
                let to = transfer::new_window(s);
                dispatch(s, Action::MoveTab { tab: t, to }, c);
            }),
            ("compare", |s, c, t| {
                let left = path_of(s, t);
                dispatch(
                    s,
                    Action::StartDiff {
                        left,
                        right: PathBuf::from("/refdesk-test/wp72/other.pdf"),
                    },
                    c,
                );
            }),
        ];
        for (what, use_it) in uses {
            let mut state = AppState::new();
            open(&mut state, &ctx, "/refdesk-test/wp72/keep.pdf");
            explorer_open(
                &mut state,
                &ctx,
                "/refdesk-test/wp72/a.pdf",
                ExplorerOpen::Preview,
            );
            let tab = state.focus.expect("focus");
            assert!(is_preview(&state, tab));
            use_it(&mut state, &ctx, tab);
            assert!(!is_preview(&state, tab), "{what} pins the tab");
            // No copy is a preview either.
            assert!(
                state
                    .tabs
                    .iter()
                    .all(|(_, t)| !matches!(t, Tab::Pdf(p) if p.preview)),
                "{what}"
            );
        }
    }

    #[test]
    fn the_keep_open_command_is_only_possible_on_a_preview_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let pin = crate::commands::command("tab.pin").expect("command");
        assert!(!(pin.enabled)(&state));
        open(&mut state, &ctx, "/refdesk-test/wp72/keep.pdf");
        assert!(!(pin.enabled)(&state));
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Preview,
        );
        let tab = state.focus.expect("focus");
        assert_eq!(pin.run(&state), Some(Action::PinTab(tab)));
    }

    #[test]
    fn each_pane_has_its_own_preview_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let x = open(&mut state, &ctx, "/refdesk-test/wp72/x.pdf");
        let copy = split_tab(&mut state, x, SplitDir::Right).expect("split");
        // The right pane has the focus.
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Preview,
        );
        let right_preview = state.focus.expect("focus");
        assert_eq!(
            state.main_window().dock.pane_tabs(copy),
            [copy, right_preview]
        );
        // The left pane gets a preview of its own instead of taking the right one.
        dispatch(&mut state, Action::FocusTab(x), &ctx);
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/b.pdf",
            ExplorerOpen::Preview,
        );
        let left_preview = state.focus.expect("focus");
        assert_ne!(left_preview, right_preview);
        assert_eq!(state.main_window().dock.pane_tabs(x), [x, left_preview]);
        // The next click in the left pane replaces only the left preview.
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/c.pdf",
            ExplorerOpen::Preview,
        );
        let left_now = state.focus.expect("focus");
        assert_eq!(
            path_of(&state, left_now),
            Path::new("/refdesk-test/wp72/c.pdf")
        );
        assert_eq!(state.main_window().dock.pane_tabs(x), [x, left_now]);
        assert_eq!(
            path_of(&state, right_preview),
            Path::new("/refdesk-test/wp72/a.pdf")
        );
        assert!(is_preview(&state, right_preview));
    }

    #[test]
    fn a_click_in_the_explorer_of_the_second_window_opens_there() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let first = open(&mut state, &ctx, "/refdesk-test/wp72/main.pdf");
        let second = transfer::new_window(&mut state);
        // The keyboard is back in the main window; the click is in the second window.
        dispatch(&mut state, Action::FocusTab(first), &ctx);
        explorer_open_in(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Preview,
            second,
        );
        let tab = state.focus.expect("focus");
        assert_eq!(state.window_of(tab), state.window_index(second));
        assert!(is_preview(&state, tab));
        assert_eq!(
            state.windows[1].dock.tabs(),
            [tab],
            "the start page made room"
        );
        assert_eq!(state.main_window().dock.tabs(), [first]);
    }

    #[test]
    fn a_middle_click_opens_an_ordinary_tab_behind_the_current_one() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let x = open(&mut state, &ctx, "/refdesk-test/wp72/x.pdf");
        let y = open(&mut state, &ctx, "/refdesk-test/wp72/y.pdf");
        dispatch(&mut state, Action::FocusTab(x), &ctx);
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Background,
        );
        assert_eq!(state.focus, Some(x), "the focus stays");
        assert_eq!(state.main_window().dock.focused_tab(), Some(x));
        let tabs = state.main_window().dock.tabs();
        assert_eq!(tabs.len(), 3);
        assert_eq!((tabs[0], tabs[2]), (x, y), "right after the current tab");
        assert_eq!(
            path_of(&state, tabs[1]),
            Path::new("/refdesk-test/wp72/a.pdf")
        );
        assert!(!is_preview(&state, tabs[1]));
        // On an open document nothing new opens.
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/y.pdf",
            ExplorerOpen::Background,
        );
        assert_eq!(state.main_window().dock.tab_count(), 3);
        assert_eq!(state.focus, Some(x));
        // A window that shows only the start page shows the new tab.
        let mut fresh = AppState::new();
        explorer_open(
            &mut fresh,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Background,
        );
        let only = fresh.main_window().dock.tabs();
        assert_eq!(only.len(), 1);
        assert_eq!(fresh.focus, Some(only[0]));
    }

    #[test]
    fn opening_beside_and_in_a_new_window_make_ordinary_views() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let x = open(&mut state, &ctx, "/refdesk-test/wp72/x.pdf");
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::SplitSide,
        );
        let beside = state.focus.expect("focus");
        assert_eq!(state.main_window().dock.pane_count(), 2);
        assert_eq!(state.main_window().dock.pane_tabs(x), [x]);
        assert!(!is_preview(&state, beside));

        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::NewWindow,
        );
        let in_window = state.focus.expect("focus");
        assert_ne!(in_window, beside, "a second view of the open document");
        assert_eq!(state.windows.len(), 2);
        assert_eq!(state.windows[1].dock.tabs(), [in_window]);
        assert_eq!(state.docs.len(), 2);
    }

    #[test]
    fn the_preview_title_is_italic_until_the_tab_is_pinned() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Preview,
        );
        let tab = state.focus.expect("focus");
        let label = tab_label(&state, tab);
        assert_eq!(label.text, "a.pdf");
        assert!(label.italic);
        dispatch(&mut state, Action::PinTab(tab), &ctx);
        assert!(!tab_label(&state, tab).italic);
        let start = AppState::new();
        assert!(!tab_label(&start, start.main_window().dock.tabs()[0]).italic);
    }

    #[test]
    fn a_closed_preview_tab_comes_back_as_an_ordinary_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Preview,
        );
        let tab = state.focus.expect("focus");
        dispatch(&mut state, Action::CloseTab(tab), &ctx);
        dispatch(&mut state, Action::ReopenClosedTab, &ctx);
        let back = state.focus.expect("focus");
        assert_eq!(path_of(&state, back), Path::new("/refdesk-test/wp72/a.pdf"));
        assert!(!is_preview(&state, back));
    }

    #[test]
    fn the_preview_flag_survives_a_session_round_trip() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        open(&mut state, &ctx, "/refdesk-test/wp72/kept.pdf");
        explorer_open(
            &mut state,
            &ctx,
            "/refdesk-test/wp72/a.pdf",
            ExplorerOpen::Preview,
        );
        let saved = session::to_session(&state);
        let mut restored = AppState::new();
        session::restore(&mut restored, &saved, &ctx);
        assert_eq!(
            main_tabs(&restored),
            [("kept.pdf".into(), false), ("a.pdf".into(), true)]
        );
    }

    #[test]
    fn the_place_in_a_replaced_document_is_remembered_and_found_again() {
        let pdf = |label: &str| {
            let mut builder = PdfBuilder::new();
            for page in 0..5 {
                builder = builder.page(400.0, 600.0, |p| {
                    p.text(72.0, 100.0, 12.0, &format!("{label} page {page}"));
                });
            }
            builder.write_temp(&format!("wp72-{label}"))
        };
        let (a, b) = (pdf("a"), pdf("b"));
        let (a_path, b_path) = (
            a.path().to_str().expect("utf-8").to_owned(),
            b.path().to_str().expect("utf-8").to_owned(),
        );
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        explorer_open(&mut state, &ctx, &a_path, ExplorerOpen::Preview);
        let tab = state.focus.expect("focus");
        let place = Location::new(3, 0.0, 120.0);
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) {
            pdf.view.anchor = place;
        }
        explorer_open(&mut state, &ctx, &b_path, ExplorerOpen::Preview);
        assert!(state.docs.find(a.path()).is_none(), "a was closed");
        explorer_open(&mut state, &ctx, &a_path, ExplorerOpen::Preview);
        let again = state.focus.expect("focus");
        assert_eq!(state.tabs.pdf(again).expect("pdf").view.anchor, place);
        assert_eq!(state.main_window().dock.tab_count(), 1);
    }
}
