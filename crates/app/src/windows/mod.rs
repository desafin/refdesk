//! Windows: drawing every application window and the content of each.
//!
//! `windows[0]` is the main window, drawn into eframe's root viewport. Every other window is an
//! immediate child viewport, drawn from inside the root's frame (`docs/ARCHITECTURE.md` §5.2,
//! §6). Each window has its own menu bar (GNOME has no global menu), dock area and status bar.
//! Moving tabs between windows lives in [`transfer`]; the tab layout itself in [`dock`].
//!
//! Window-specific assumptions that M0 may still change (who quits when the main window
//! closes, whether textures are shared, the order of `logic` and child windows) are kept inside
//! this module.

pub mod dock;
pub mod transfer;

use std::collections::HashMap;

use egui::{
    CentralPanel, Id, Panel, Ui, ViewportBuilder, ViewportClass, ViewportCommand, ViewportId,
};
use refdesk_core::{DocStatus, DocumentHandle};

use crate::action::Action;
use crate::commands::{self, MenuGroup, TAB_MENU};
use crate::i18n::tr;
use crate::panels::search;
use crate::panels::status_bar;
use crate::panels::toc;
use crate::session;
use crate::state::{AppState, DocStore, PdfTab, Tab, TabId, TabStore, WindowId};
use crate::views::diff_view;
use crate::views::pdf_view::{self, ViewContext};
use crate::views::start_page::{self, StartPage};
use crate::views::textures::TextureCache;
use dock::DockHost;

/// The app id every window carries, so GNOME groups them under one application.
const APP_ID: &str = "refdesk";

/// The size a new window opens with. Its position is up to the compositor (Wayland).
const NEW_WINDOW_SIZE: [f32; 2] = [1100.0, 800.0];

/// The viewport that shows window `id`: the root viewport for the main window.
pub fn viewport_id(state: &AppState, id: WindowId) -> ViewportId {
    if id == state.main_window().id {
        ViewportId::ROOT
    } else {
        ViewportId::from_hash_of(("refdesk-window", id))
    }
}

/// Asks for window `id` to come to the front. Wayland compositors may refuse; then the window
/// stays where it is.
pub fn raise(ctx: &egui::Context, state: &AppState, id: WindowId) {
    if state.window_index(id).is_some() {
        ctx.send_viewport_cmd_to(viewport_id(state, id), ViewportCommand::Focus);
    }
}

/// Draws every window: the main window into `ui` (the root viewport), the others as child
/// viewports. Call it once per frame from the root's `ui()`. Widgets only queue actions.
///
/// Shortcuts are handled once per frame, by the window that has the keyboard, with that
/// window's input. When the backend cannot open more windows (headless tests), the child
/// windows are drawn as egui windows inside the root; they then share the root's input, so
/// they neither take shortcuts nor follow the keyboard focus.
pub fn show_all(ui: &mut Ui, state: &mut AppState, queue: &mut Vec<Action>) {
    let ctx = ui.ctx().clone();
    let mut keys_taken = false;
    draw_window(ui, state, 0, queue, true, &mut keys_taken);
    set_root_title(&ctx, window_title(state, 0));

    // `dispatch` runs after every window is drawn, so the window list is fixed here.
    for index in 1..state.windows.len() {
        let id = state.windows[index].id;
        // A window restored from the session opens at its saved size (WP-41).
        let geom = state.memory.initial_geometry(id);
        let mut builder = ViewportBuilder::default()
            .with_title(window_title(state, index))
            .with_app_id(APP_ID)
            .with_inner_size(geom.map_or(NEW_WINDOW_SIZE, |g| session::sane_size(g.size)));
        if geom.is_some_and(|g| g.maximized) {
            builder = builder.with_maximized(true);
        }
        ctx.show_viewport_immediate(viewport_id(state, id), builder, |ui, class| {
            let own_window = class != ViewportClass::EmbeddedWindow;
            if own_window && ui.ctx().input(|i| i.viewport().close_requested()) {
                // Not drawing the viewport next frame is what closes it.
                queue.push(Action::CloseWindow(id));
            }
            draw_window(ui, state, index, queue, own_window, &mut keys_taken);
        });
    }
}

/// Draws one whole window: menu bar, then [`show_window`]. `own_window` is false for a child
/// window drawn inside the root (see [`show_all`]). `keys_taken` is set once a window has
/// handled this frame's shortcuts.
fn draw_window(
    ui: &mut Ui,
    state: &mut AppState,
    index: usize,
    queue: &mut Vec<Action>,
    own_window: bool,
    keys_taken: &mut bool,
) {
    let Some(id) = state.windows.get(index).map(|w| w.id) else {
        return;
    };
    let ctx = ui.ctx().clone();
    if own_window {
        // The window size is saved in the session.
        session::note_window(&ctx, state, id);
    }
    // In a child viewport's callback, `ctx.input` is that window's input.
    if own_window && ctx.input(|i| i.focused) {
        state.follow_window_focus(index);
        if !*keys_taken {
            *keys_taken = true;
            queue.extend(commands::handle_shortcuts(&ctx, state));
        }
    }
    Panel::top(Id::unique(("menu_bar", id))).show(ui, |ui| menu_bar(ui, state, index, queue));
    show_window(ui, state, index, queue);
}

/// Sets the title of the root viewport, only when it changed (child viewports get theirs from
/// their builder, which egui compares itself).
fn set_root_title(ctx: &egui::Context, title: String) {
    let key = Id::unique("refdesk-root-title");
    let changed = ctx.data_mut(|d| {
        let last = d.get_temp_mut_or_default::<String>(key);
        if *last == title {
            false
        } else {
            last.clone_from(&title);
            true
        }
    });
    if changed {
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Title(title));
    }
}

/// The title of window `index`: "active tab — refdesk", or "refdesk" while it shows no
/// document.
pub fn window_title(state: &AppState, index: usize) -> String {
    let active = state
        .windows
        .get(index)
        .and_then(|w| w.dock.focused_tab())
        .filter(|t| !matches!(state.tabs.get(*t), Some(Tab::Start) | None));
    match active {
        Some(tab) => tr("window.title").replace("{title}", &tab_title(state, tab)),
        None => APP_ID.to_owned(),
    }
}

/// How window `index` is named in window lists: "Window 2 — active tab".
pub fn window_name(state: &AppState, index: usize) -> String {
    let active = state
        .windows
        .get(index)
        .and_then(|w| w.dock.focused_tab())
        .map(|t| tab_title(state, t))
        .unwrap_or_default();
    tr("window.name")
        .replace("{n}", &(index + 1).to_string())
        .replace("{title}", &active)
}

/// The menu bar of window `index`. Its entries are the rows of the command table; the Window
/// menu also lists the open windows.
fn menu_bar(ui: &mut Ui, state: &AppState, index: usize, queue: &mut Vec<Action>) {
    egui::MenuBar::new().ui(ui, |ui| {
        for group in MenuGroup::ALL {
            ui.menu_button(tr(group.name_key()), |ui| {
                for command in commands::in_menu(group) {
                    let mut button = egui::Button::new(tr(command.name_key));
                    if let Some(shortcut) = command.shortcut {
                        button = button.shortcut_text(ui.ctx().format_shortcut(&shortcut));
                    }
                    let enabled = (command.enabled)(state);
                    if ui.add_enabled(enabled, button).clicked() {
                        queue.extend(command.run(state));
                    }
                }
                if group == MenuGroup::Window {
                    ui.separator();
                    for (i, window) in state.windows.iter().enumerate() {
                        let button = egui::Button::selectable(i == index, window_name(state, i));
                        if ui.add(button).clicked() {
                            raise(ui.ctx(), state, window.id);
                        }
                    }
                }
            });
        }
    });
}

/// Draws the content of window `window_index`: its status bar and its panes. Widgets only queue
/// actions; nothing structural changes here (rule R-6), except tab drags inside the dock area
/// (see [`dock::DockLayout::show`]).
pub fn show_window(
    ui: &mut Ui,
    state: &mut AppState,
    window_index: usize,
    queue: &mut Vec<Action>,
) {
    // The palette is drawn first so it takes its keys before the views behind it.
    crate::overlays::palette::show(
        ui.ctx(),
        state,
        window_index,
        queue,
        &crate::session::recent_document_paths,
    );
    let Some(window) = state.windows.get(window_index) else {
        return;
    };
    let window_id = window.id;
    let shown = window.dock.tabs();
    let window_focus = window.dock.focused_tab();
    let (toc_open, toc_width) = (window.toc_open, window.toc_width);

    // The dialog of "Compare Documents…" shows in the window that has the focus.
    if window_index == state.focused_window_index()
        && let Some(dialog) = state.diff_dialog.as_mut()
    {
        let mut open: Vec<std::path::PathBuf> = state
            .docs
            .iter()
            .map(|(_, doc)| doc.path().to_path_buf())
            .collect();
        open.sort();
        if !diff_view::show_dialog(ui.ctx(), dialog, &open, queue) {
            state.diff_dialog = None;
        }
    }

    // Everything the tab bars and menus show is worked out before the window's layout is
    // borrowed for drawing.
    let titles: HashMap<TabId, (String, Option<String>)> = shown
        .iter()
        .map(|t| (*t, (tab_title(state, *t), tab_tooltip(state, *t))))
        .collect();
    let menus: HashMap<TabId, Vec<MenuItem>> = shown
        .iter()
        .map(|t| (*t, tab_menu(ui.ctx(), state, *t)))
        .collect();
    let start = shown
        .iter()
        .any(|t| matches!(state.tabs.get(*t), Some(Tab::Start)))
        .then(|| session::start_page(state, ui.ctx()));

    Panel::bottom(Id::unique(("status_bar", window_id))).show(ui, |ui| {
        // The status bar describes the focused tab of this window.
        status_bar::show(ui, state, window_focus, queue);
    });

    if toc_open {
        // The panel shows the contents of this window's focused tab; the width the user
        // dragged it to is remembered per window.
        let shown = Panel::left(Id::unique(("toc", window_id)))
            .default_size(toc_width)
            .size_range(toc::TOC_WIDTH_RANGE.0..=toc::TOC_WIDTH_RANGE.1)
            .show(ui, |ui| {
                toc::show(ui, &mut state.tabs, &state.docs, window_focus, queue);
            });
        if let Some(window) = state.windows.get_mut(window_index) {
            window.toc_width = shown.response.rect.width();
        }
    }

    // The matches of the search of this window's focused tab (FIND-4).
    search::show_results_panel(ui, &mut state.tabs, &state.docs, window_id, window_focus);

    let AppState {
        windows,
        tabs,
        docs,
        textures,
        focus,
        ..
    } = state;
    let Some(window) = windows.get_mut(window_index) else {
        return;
    };
    let mut host = WindowHost {
        tabs,
        docs,
        textures,
        focus: *focus,
        titles,
        menus,
        start,
        queue,
    };
    CentralPanel::default().show(ui, |ui| {
        window
            .dock
            .show(ui, Id::unique(("dock", window_id)), &mut host);
    });
}

/// One clickable entry of a tab's right-click menu, ready to draw.
struct MenuEntry {
    label: String,
    shortcut: Option<String>,
    enabled: bool,
    action: Action,
}

/// One row of a tab's right-click menu.
enum MenuItem {
    /// A plain entry.
    Entry(MenuEntry),
    /// A submenu ("Move to Window ▸").
    Submenu {
        label: &'static str,
        entries: Vec<MenuEntry>,
    },
}

/// The right-click menu of a tab: the rows of [`TAB_MENU`], then "Move to Window" with the
/// other windows (hidden when there is no other window).
fn tab_menu(ctx: &egui::Context, state: &AppState, tab: TabId) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = TAB_MENU
        .iter()
        .map(|entry| {
            MenuItem::Entry(MenuEntry {
                label: tr(entry.name_key).to_owned(),
                shortcut: entry
                    .command_id
                    .and_then(|id| commands::shortcut_text(ctx, id)),
                enabled: (entry.enabled)(state, tab),
                action: (entry.action)(tab),
            })
        })
        .collect();
    if let Some(compare) = compare_menu(state, tab) {
        items.push(compare);
    }
    let own = state.window_of(tab);
    let movable = matches!(state.tabs.get(tab), Some(Tab::Pdf(_) | Tab::Diff(_)));
    let targets: Vec<MenuEntry> = state
        .windows
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != own)
        .map(|(i, w)| MenuEntry {
            label: window_name(state, i),
            shortcut: None,
            enabled: movable,
            action: Action::MoveTab { tab, to: w.id },
        })
        .collect();
    if !targets.is_empty() {
        items.push(MenuItem::Submenu {
            label: tr("window.move_to"),
            entries: targets,
        });
    }
    items
}

/// "Compare With ▸" for a PDF tab: the other open documents; choosing one compares this tab's
/// document (older, left) with it (newer, right).
fn compare_menu(state: &AppState, tab: TabId) -> Option<MenuItem> {
    let own = state.docs.get(state.tabs.pdf(tab)?.doc)?.path();
    let mut others: Vec<std::path::PathBuf> = state
        .docs
        .iter()
        .map(|(_, doc)| doc.path().to_path_buf())
        .filter(|path| path != own)
        .collect();
    others.sort();
    let mut entries: Vec<MenuEntry> = others
        .into_iter()
        .map(|path| MenuEntry {
            label: diff_view::file_name(&path),
            shortcut: None,
            enabled: true,
            action: Action::StartDiff {
                left: own.to_path_buf(),
                right: path,
            },
        })
        .collect();
    if entries.is_empty() {
        entries.push(MenuEntry {
            label: tr("diff.no_other_docs").to_owned(),
            shortcut: None,
            enabled: false,
            action: Action::ShowDiffDialog,
        });
    }
    Some(MenuItem::Submenu {
        label: tr("diff.compare_with"),
        entries,
    })
}

/// Draws one menu entry; a click queues its action and closes the menu.
fn menu_entry(ui: &mut Ui, entry: &MenuEntry, queue: &mut Vec<Action>) {
    let mut button = egui::Button::new(entry.label.as_str());
    if let Some(keys) = &entry.shortcut {
        button = button.shortcut_text(keys.as_str());
    }
    if ui.add_enabled(entry.enabled, button).clicked() {
        queue.push(entry.action.clone());
        ui.close();
    }
}

/// The file name of the document a tab shows. When several tabs show the same document, the
/// second and later (in the order they were opened) get " (2)", " (3)", ….
pub fn tab_title(state: &AppState, tab: TabId) -> String {
    match state.tabs.get(tab) {
        Some(Tab::Pdf(pdf)) => {
            let name = state
                .docs
                .get(pdf.doc)
                .and_then(|doc| doc.path().file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| tr("tab.unknown").to_owned());
            let earlier = state
                .windows
                .iter()
                .flat_map(|w| w.dock.tabs())
                .filter(|t| *t < tab && state.tabs.pdf(*t).is_some_and(|p| p.doc == pdf.doc))
                .count();
            if earlier == 0 {
                name
            } else {
                tr("tab.nth")
                    .replace("{name}", &name)
                    .replace("{n}", &(earlier + 1).to_string())
            }
        }
        Some(Tab::Diff(diff)) => diff_view::tab_title(diff, &state.docs),
        Some(Tab::Start) => tr("tab.start").to_owned(),
        None => tr("tab.unknown").to_owned(),
    }
}

/// The full path of a PDF tab's document (both paths for a comparison tab).
fn tab_tooltip(state: &AppState, tab: TabId) -> Option<String> {
    if let Some(diff) = state.tabs.diff(tab) {
        return diff_view::tab_tooltip(diff, &state.docs);
    }
    let pdf = state.tabs.pdf(tab)?;
    let doc = state.docs.get(pdf.doc)?;
    Some(doc.path().display().to_string())
}

/// The application side of the dock area of one window.
struct WindowHost<'a> {
    tabs: &'a mut TabStore,
    docs: &'a DocStore,
    textures: &'a mut TextureCache,
    focus: Option<TabId>,
    titles: HashMap<TabId, (String, Option<String>)>,
    menus: HashMap<TabId, Vec<MenuItem>>,
    /// What the start page shows; `None` when this window has no start page.
    start: Option<StartPage>,
    queue: &'a mut Vec<Action>,
}

impl DockHost for WindowHost<'_> {
    fn title(&mut self, tab: TabId) -> String {
        self.titles
            .get(&tab)
            .map(|(title, _)| title.clone())
            .unwrap_or_else(|| tr("tab.unknown").to_owned())
    }

    fn tooltip(&mut self, tab: TabId) -> Option<String> {
        self.titles.get(&tab).and_then(|(_, tip)| tip.clone())
    }

    fn ui(&mut self, ui: &mut Ui, tab: TabId) {
        match self.tabs.get_mut(tab) {
            Some(Tab::Pdf(pdf)) => match self.docs.get(pdf.doc) {
                Some(doc) => {
                    if let DocStatus::Failed(message) = doc.status() {
                        // Retry and close buttons; a missing file stays a tab (SES-6).
                        start_page::show_failed_document(ui, tab, doc.path(), &message, self.queue);
                        return;
                    }
                    if let Some(info) = doc.info() {
                        // A remembered place may lie past the end of a shorter new version.
                        session::fit_view_to_document(&mut pdf.view, info.page_sizes.len());
                    }
                    let cx = ViewContext {
                        tab_id: tab,
                        focused: self.focus == Some(tab),
                        textures: self.textures,
                        queue: self.queue,
                    };
                    draw_pdf_tab(ui, pdf, doc, cx);
                }
                None => {
                    ui.label(tr("tab.unknown"));
                }
            },
            Some(Tab::Diff(diff)) => {
                let cx = ViewContext {
                    tab_id: tab,
                    focused: self.focus == Some(tab),
                    textures: self.textures,
                    queue: self.queue,
                };
                diff_view::show(ui, diff, self.docs, cx);
            }
            Some(Tab::Start) => draw_start_page(ui, self.start.as_ref(), self.queue),
            None => {}
        }
    }

    fn context_menu(&mut self, ui: &mut Ui, tab: TabId) {
        let Some(items) = self.menus.get(&tab) else {
            return;
        };
        let queue = &mut *self.queue;
        for item in items {
            match item {
                MenuItem::Entry(entry) => menu_entry(ui, entry, queue),
                MenuItem::Submenu { label, entries } => {
                    ui.menu_button(*label, |ui| {
                        for entry in entries {
                            menu_entry(ui, entry, queue);
                        }
                    });
                }
            }
        }
    }

    fn is_closeable(&self, tab: TabId) -> bool {
        // The start page has no close button; it goes away by itself when a document opens.
        matches!(self.tabs.get(tab), Some(Tab::Pdf(_) | Tab::Diff(_)))
    }

    fn close_requested(&mut self, tab: TabId) {
        self.queue.push(Action::CloseTab(tab));
    }

    fn activated(&mut self, tab: TabId) {
        if self.focus != Some(tab) {
            self.queue.push(Action::FocusTab(tab));
        }
    }
}

/// Draws the content of a PDF tab: the PDF view (WP-15). It gets the tab (whose view state it
/// may change), the document handle, the shared textures and the action queue.
pub fn draw_pdf_tab(ui: &mut Ui, tab: &mut PdfTab, doc: &DocumentHandle, cx: ViewContext<'_>) {
    search::show_bar(ui, tab, doc, cx.tab_id, cx.focused);
    pdf_view::show(ui, tab, doc, cx);
}

/// Draws the start page: the application name, a button to open files, the recent documents
/// and a few shortcuts (`views/start_page.rs`).
fn draw_start_page(ui: &mut Ui, page: Option<&StartPage>, queue: &mut Vec<Action>) {
    start_page::show(ui, page.unwrap_or(&StartPage::default()), queue);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{OpenTarget, dispatch, dispatch_queued};
    use crate::i18n::{current_lang, lookup};
    use crate::windows::dock::SplitDir;
    use egui::{Pos2, Rect, vec2};

    fn open(state: &mut AppState, ctx: &egui::Context, path: &str) -> TabId {
        dispatch(state, Action::OpenFiles(vec![OpenTarget::parse(path)]), ctx);
        state.focus.expect("focused")
    }

    fn frame(ctx: &egui::Context, state: &mut AppState) -> Vec<Action> {
        let mut queue = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 700.0))),
            ..Default::default()
        };
        ctx.run_ui(input, |ui| show_window(ui, state, 0, &mut queue))
            .drop_without_applying_deltas();
        queue
    }

    #[test]
    fn second_and_later_tabs_of_a_document_are_numbered() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/dir/spec.pdf");
        let b = open(&mut state, &ctx, "/refdesk-test/other.pdf");
        dispatch(&mut state, Action::DuplicateTab(a), &ctx);
        let a2 = state.focus.expect("copy");
        dispatch(
            &mut state,
            Action::SplitTab {
                tab: a,
                dir: SplitDir::Right,
            },
            &ctx,
        );
        let a3 = state.focus.expect("split view");

        let nth = lookup(current_lang(), "tab.nth").expect("key");
        assert_eq!(tab_title(&state, a), "spec.pdf");
        assert_eq!(tab_title(&state, b), "other.pdf");
        assert_eq!(
            tab_title(&state, a2),
            nth.replace("{name}", "spec.pdf").replace("{n}", "2")
        );
        assert_eq!(
            tab_title(&state, a3),
            nth.replace("{name}", "spec.pdf").replace("{n}", "3")
        );
        assert_eq!(
            tab_tooltip(&state, a2).as_deref(),
            Some("/refdesk-test/dir/spec.pdf")
        );

        // Closing the first view renumbers the others.
        dispatch(&mut state, Action::CloseTab(a), &ctx);
        assert_eq!(tab_title(&state, a2), "spec.pdf");
    }

    #[test]
    fn a_pdf_view_in_a_pane_is_laid_out_to_the_pane_width() {
        use crate::views::test_support::watch_repaints;
        use refdesk_core::testutil::PdfBuilder;

        let pdf = PdfBuilder::new()
            .page(595.0, 842.0, |p| {
                p.text(72.0, 100.0, 24.0, "Page 1");
            })
            .write_temp("wp20-pane-width");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        let tab = open(
            &mut state,
            &ctx,
            pdf.path().to_str().expect("utf-8 temp path"),
        );
        let doc = state
            .docs
            .get(state.tabs.pdf(tab).expect("pdf").doc)
            .expect("doc")
            .clone();
        repaints.wait_until("the document info", || doc.info().is_some());
        let zoom_of = |state: &AppState, t: TabId| {
            state
                .tabs
                .pdf(t)
                .and_then(|p| p.ui.effective_zoom)
                .expect("laid out")
        };

        frame(&ctx, &mut state);
        frame(&ctx, &mut state);
        let whole = zoom_of(&state, tab);

        dispatch(
            &mut state,
            Action::SplitTab {
                tab,
                dir: SplitDir::Right,
            },
            &ctx,
        );
        let other = state.focus.expect("split view");
        frame(&ctx, &mut state);
        frame(&ctx, &mut state);
        let (left, right) = (zoom_of(&state, tab), zoom_of(&state, other));
        // Fit width: each half is laid out to about half the width.
        assert!(left < whole * 0.6 && left > whole * 0.4, "{whole} {left}");
        assert!((left - right).abs() < whole * 0.05, "{left} {right}");
    }

    #[test]
    fn the_start_page_title_and_menu() {
        let ctx = egui::Context::default();
        let state = AppState::new();
        let start = state.main_window().dock.tabs()[0];
        assert_eq!(tab_title(&state, start), tr("tab.start"));
        assert_eq!(tab_tooltip(&state, start), None);
        let menu = tab_menu(&ctx, &state, start);
        assert_eq!(menu.len(), TAB_MENU.len());
        assert!(
            menu.iter()
                .all(|m| matches!(m, MenuItem::Entry(e) if !e.enabled))
        );
    }

    #[test]
    fn drawing_two_tabs_and_a_split_does_not_panic_and_reports_two_panes() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        for _ in 0..2 {
            assert!(frame(&ctx, &mut state).is_empty(), "nothing was clicked");
        }
        let a = open(&mut state, &ctx, "/refdesk-test/missing-a.pdf");
        open(&mut state, &ctx, "/refdesk-test/missing-b.pdf");
        for _ in 0..3 {
            assert!(frame(&ctx, &mut state).is_empty(), "nothing was clicked");
        }
        assert_eq!(state.main_window().dock.pane_rects().len(), 1);

        dispatch(
            &mut state,
            Action::SplitTab {
                tab: a,
                dir: SplitDir::Right,
            },
            &ctx,
        );
        for _ in 0..3 {
            let queue = frame(&ctx, &mut state);
            assert!(queue.is_empty(), "nothing was clicked: {queue:?}");
        }
        let rects = state.main_window().dock.pane_rects();
        assert_eq!(rects.len(), 2);
        let (left, right) = (rects[0].0, rects[1].0);
        assert!(left.is_positive() && right.is_positive());
        assert!(left.center().x < right.center().x, "{left:?} {right:?}");
    }

    #[test]
    fn clicking_another_tab_title_focuses_it() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/missing-a.pdf");
        let b = open(&mut state, &ctx, "/refdesk-test/missing-b.pdf");
        frame(&ctx, &mut state);
        frame(&ctx, &mut state);
        // The tab bar is at the top of the pane; the first tab starts at its left edge.
        let (pane, _) = state.main_window().dock.pane_rects()[0];
        let at = pane.left_top() + vec2(20.0, 10.0);
        let click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut queue = Vec::new();
        for events in [
            vec![egui::Event::PointerMoved(at)],
            vec![click(true)],
            vec![click(false)],
        ] {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 700.0))),
                events,
                ..Default::default()
            };
            ctx.run_ui(input, |ui| show_window(ui, &mut state, 0, &mut queue))
                .drop_without_applying_deltas();
        }
        assert!(queue.contains(&Action::FocusTab(a)), "{queue:?}");
        state.actions.append(&mut queue);
        dispatch_queued(&mut state, &ctx);
        assert_eq!(state.focus, Some(a));
        assert_ne!(state.focus, Some(b));
        assert_eq!(state.main_window().dock.focused_tab(), Some(a));
    }

    /// Runs one headless frame of every window with these input events. `focused` says whether
    /// the root window has the keyboard. Returns the queued actions and the root's viewport
    /// commands.
    fn frame_all(
        ctx: &egui::Context,
        state: &mut AppState,
        events: Vec<egui::Event>,
        focused: bool,
    ) -> (Vec<Action>, Vec<ViewportCommand>) {
        let mut queue = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 1000.0))),
            focused,
            events,
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| show_all(ui, state, &mut queue));
        let commands = output
            .viewport_output
            .get(&ViewportId::ROOT)
            .map(|v| v.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();
        (queue, commands)
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// The main window shows a.pdf, a second window b.pdf, which has the focus.
    fn two_windows(ctx: &egui::Context) -> (AppState, TabId, TabId) {
        let mut state = AppState::new();
        let a = open(&mut state, ctx, "/refdesk-test/a.pdf");
        let b = open(&mut state, ctx, "/refdesk-test/b.pdf");
        dispatch(&mut state, Action::DetachTab(b), ctx);
        (state, a, b)
    }

    #[test]
    fn window_titles_follow_the_active_tab() {
        let ctx = egui::Context::default();
        assert_eq!(window_title(&AppState::new(), 0), "refdesk");
        let (mut state, _, _) = two_windows(&ctx);
        let template = lookup(current_lang(), "window.title").expect("key");
        assert_eq!(
            window_title(&state, 0),
            template.replace("{title}", "a.pdf")
        );
        assert_eq!(
            window_title(&state, 1),
            template.replace("{title}", "b.pdf")
        );
        dispatch(&mut state, Action::NewWindow, &ctx);
        assert_eq!(window_title(&state, 2), "refdesk");
        // Out of range: no panic.
        assert_eq!(window_title(&state, 9), "refdesk");
    }

    #[test]
    fn window_names_number_the_windows_and_name_their_active_tab() {
        let ctx = egui::Context::default();
        let (state, _, _) = two_windows(&ctx);
        let template = lookup(current_lang(), "window.name").expect("key");
        assert_eq!(
            window_name(&state, 0),
            template.replace("{n}", "1").replace("{title}", "a.pdf")
        );
        assert_eq!(
            window_name(&state, 1),
            template.replace("{n}", "2").replace("{title}", "b.pdf")
        );
    }

    fn submenu(menu: &[MenuItem], key: &str) -> Option<Vec<(String, Action, bool)>> {
        menu.iter().find_map(|item| match item {
            MenuItem::Submenu { label, entries } if *label == tr(key) => Some(
                entries
                    .iter()
                    .map(|e| (e.label.clone(), e.action.clone(), e.enabled))
                    .collect(),
            ),
            _ => None,
        })
    }

    fn move_targets(menu: &[MenuItem]) -> Option<Vec<(String, Action, bool)>> {
        submenu(menu, "window.move_to")
    }

    #[test]
    fn compare_with_lists_the_other_open_documents() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let alone = submenu(&tab_menu(&ctx, &state, a), "diff.compare_with").expect("submenu");
        assert_eq!(alone.len(), 1);
        assert!(!alone[0].2, "nothing to compare with yet");
        open(&mut state, &ctx, "/refdesk-test/c.pdf");
        open(&mut state, &ctx, "/refdesk-test/b.pdf");
        let entries = submenu(&tab_menu(&ctx, &state, a), "diff.compare_with").expect("submenu");
        let start = |right: &str| Action::StartDiff {
            left: "/refdesk-test/a.pdf".into(),
            right: right.into(),
        };
        assert_eq!(
            entries,
            vec![
                ("b.pdf".to_owned(), start("/refdesk-test/b.pdf"), true),
                ("c.pdf".to_owned(), start("/refdesk-test/c.pdf"), true),
            ]
        );

        // A comparison tab: its own title and tooltip, closeable, no compare submenu.
        dispatch(&mut state, start("/refdesk-test/b.pdf"), &ctx);
        let diff = state.focus.expect("comparison");
        let title = tab_title(&state, diff);
        assert!(
            title.contains("a.pdf") && title.contains("b.pdf"),
            "{title}"
        );
        assert_eq!(
            tab_tooltip(&state, diff).as_deref(),
            Some("/refdesk-test/a.pdf\n/refdesk-test/b.pdf")
        );
        assert_eq!(
            window_title(&state, 0),
            tr("window.title").replace("{title}", &title)
        );
        let menu = tab_menu(&ctx, &state, diff);
        assert!(submenu(&menu, "diff.compare_with").is_none());
        assert!(
            matches!(&menu[0], MenuItem::Entry(e) if e.enabled && e.action == Action::CloseTab(diff))
        );
        for _ in 0..2 {
            assert!(frame(&ctx, &mut state).is_empty());
        }
    }

    #[test]
    fn the_move_to_window_submenu_lists_the_other_windows() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let a = open(&mut state, &ctx, "/refdesk-test/a.pdf");
        // One window: no submenu.
        assert!(move_targets(&tab_menu(&ctx, &state, a)).is_none());

        let b = open(&mut state, &ctx, "/refdesk-test/b.pdf");
        dispatch(&mut state, Action::DetachTab(b), &ctx);
        dispatch(&mut state, Action::NewWindow, &ctx);
        let (w2, w3) = (state.windows[1].id, state.windows[2].id);
        let main = state.main_window().id;

        let targets = move_targets(&tab_menu(&ctx, &state, a)).expect("submenu");
        assert_eq!(
            targets,
            vec![
                (
                    window_name(&state, 1),
                    Action::MoveTab { tab: a, to: w2 },
                    true
                ),
                (
                    window_name(&state, 2),
                    Action::MoveTab { tab: a, to: w3 },
                    true
                ),
            ]
        );
        // The tab's own window is left out.
        let targets = move_targets(&tab_menu(&ctx, &state, b)).expect("submenu");
        assert_eq!(
            targets
                .iter()
                .map(|(_, action, _)| action.clone())
                .collect::<Vec<_>>(),
            vec![
                Action::MoveTab { tab: b, to: main },
                Action::MoveTab { tab: b, to: w3 }
            ]
        );
        // The detach entry is in the tab menu.
        let menu = tab_menu(&ctx, &state, a);
        assert!(menu.iter().any(|item| matches!(
            item,
            MenuItem::Entry(e) if e.action == Action::DetachTab(a) && e.enabled
        )));
        // A start page cannot move.
        let start = state.windows[2].dock.tabs()[0];
        let targets = move_targets(&tab_menu(&ctx, &state, start)).expect("submenu");
        assert!(targets.iter().all(|(_, _, enabled)| !enabled));
    }

    #[test]
    fn drawing_three_windows_draws_the_children_without_panicking() {
        let ctx = egui::Context::default();
        let (mut state, _, _) = two_windows(&ctx);
        dispatch(&mut state, Action::NewWindow, &ctx);
        for _ in 0..3 {
            let (queue, _) = frame_all(&ctx, &mut state, Vec::new(), true);
            assert!(queue.is_empty(), "nothing was clicked: {queue:?}");
        }
        // Headless, the children are egui windows inside the root, but they were laid out.
        for w in 0..3 {
            let rects = state.windows[w].dock.pane_rects();
            assert_eq!(rects.len(), 1);
            assert!(rects[0].0.is_positive(), "window {w}: {rects:?}");
        }
    }

    #[test]
    fn a_shortcut_applies_to_the_focused_tab_of_the_window_that_has_the_keyboard() {
        let ctx = egui::Context::default();
        let (mut state, a, b) = two_windows(&ctx);
        assert_eq!(state.focus, Some(b), "detaching focused the moved tab");

        // The root window has the keyboard: Ctrl+W closes its tab, not the one in window 2.
        let (queue, _) = frame_all(
            &ctx,
            &mut state,
            vec![key(egui::Key::W, egui::Modifiers::CTRL)],
            true,
        );
        assert_eq!(queue, vec![Action::CloseTab(a)]);
        assert_eq!(state.focus, Some(a));
    }

    #[test]
    fn no_window_with_the_keyboard_takes_no_shortcut() {
        let ctx = egui::Context::default();
        let (mut state, _, b) = two_windows(&ctx);
        let (queue, _) = frame_all(
            &ctx,
            &mut state,
            vec![key(egui::Key::W, egui::Modifiers::CTRL)],
            false,
        );
        assert!(queue.is_empty(), "{queue:?}");
        assert_eq!(state.focus, Some(b));
    }

    #[test]
    fn a_shortcut_is_taken_once_per_frame() {
        let ctx = egui::Context::default();
        let (mut state, _, _) = two_windows(&ctx);
        dispatch(&mut state, Action::NewWindow, &ctx);
        // Embedded children share the root's input; still only one action comes out.
        let (queue, _) = frame_all(
            &ctx,
            &mut state,
            vec![key(
                egui::Key::N,
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
            )],
            true,
        );
        assert_eq!(queue, vec![Action::NewWindow]);
    }

    #[test]
    fn the_root_title_is_sent_only_when_it_changes() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let titles = |commands: Vec<ViewportCommand>| -> Vec<String> {
            commands
                .into_iter()
                .filter_map(|c| match c {
                    ViewportCommand::Title(t) => Some(t),
                    _ => None,
                })
                .collect()
        };
        let (_, first) = frame_all(&ctx, &mut state, Vec::new(), true);
        assert_eq!(titles(first), vec!["refdesk".to_owned()]);
        let (_, again) = frame_all(&ctx, &mut state, Vec::new(), true);
        assert!(titles(again).is_empty());
        open(&mut state, &ctx, "/refdesk-test/a.pdf");
        let (_, changed) = frame_all(&ctx, &mut state, Vec::new(), true);
        assert_eq!(titles(changed), vec![window_title(&state, 0)]);
    }

    #[test]
    fn viewports_are_the_root_for_the_main_window_and_distinct_for_others() {
        let ctx = egui::Context::default();
        let (mut state, _, _) = two_windows(&ctx);
        dispatch(&mut state, Action::NewWindow, &ctx);
        let ids: Vec<ViewportId> = state
            .windows
            .iter()
            .map(|w| viewport_id(&state, w.id))
            .collect();
        assert_eq!(ids[0], ViewportId::ROOT);
        assert!(ids[1] != ViewportId::ROOT && ids[2] != ViewportId::ROOT);
        assert_ne!(ids[1], ids[2]);
    }

    #[test]
    fn moving_a_tab_asks_its_new_window_to_come_forward() {
        // Headless, only the root viewport keeps commands (child viewports are never really
        // opened), so this moves a tab into the main window.
        let ctx = egui::Context::default();
        let (mut state, _, b) = two_windows(&ctx);
        let main = state.main_window().id;
        let output = ctx.run_ui(egui::RawInput::default(), |_ui| {
            dispatch(&mut state, Action::MoveTab { tab: b, to: main }, &ctx);
        });
        let commands = output
            .viewport_output
            .get(&ViewportId::ROOT)
            .map(|v| v.commands.clone())
            .unwrap_or_default();
        output.drop_without_applying_deltas();
        assert!(commands.contains(&ViewportCommand::Focus), "{commands:?}");
        assert_eq!(state.windows.len(), 1, "the emptied window closed");
    }
}
