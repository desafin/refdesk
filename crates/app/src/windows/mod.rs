//! Windows: drawing the content of one window (its panes, tabs and status bar).
//!
//! `show_window` takes only what one window needs, so the root window and the child windows of
//! later work packages draw the same way. The tab layout itself lives in [`dock`].

pub mod dock;

use std::collections::HashMap;

use egui::{CentralPanel, Id, Panel, Ui};
use refdesk_core::DocumentHandle;

use crate::action::Action;
use crate::commands::{self, TAB_MENU};
use crate::i18n::tr;
use crate::panels::status_bar;
use crate::state::{AppState, DocStore, PdfTab, Tab, TabId, TabStore};
use crate::views::pdf_view::{self, ViewContext};
use crate::views::textures::TextureCache;
use dock::DockHost;

/// Draws window `window_index` of `state`: its status bar and its panes. Widgets only queue
/// actions; nothing structural changes here (rule R-6), except tab drags inside the dock area
/// (see [`dock::DockLayout::show`]).
pub fn show_window(
    ui: &mut Ui,
    state: &mut AppState,
    window_index: usize,
    queue: &mut Vec<Action>,
) {
    let Some(window) = state.windows.get(window_index) else {
        return;
    };
    let window_id = window.id;
    let shown = window.dock.tabs();
    let window_focus = window.dock.focused_tab();

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

    Panel::bottom(Id::unique(("status_bar", window_id))).show(ui, |ui| {
        // The status bar describes the focused tab of this window. It reads `state.focus`, so
        // that is pointed at this window's tab while it draws and restored right after.
        let saved = state.focus;
        state.focus = window_focus;
        status_bar::show(ui, state, queue);
        state.focus = saved;
    });

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
        queue,
    };
    CentralPanel::default().show(ui, |ui| {
        window
            .dock
            .show(ui, Id::unique(("dock", window_id)), &mut host);
    });
}

/// One entry of a tab's right-click menu, ready to draw.
struct MenuItem {
    label: &'static str,
    shortcut: Option<String>,
    enabled: bool,
    action: Action,
}

/// The right-click menu of a tab, from [`TAB_MENU`].
fn tab_menu(ctx: &egui::Context, state: &AppState, tab: TabId) -> Vec<MenuItem> {
    TAB_MENU
        .iter()
        .map(|entry| MenuItem {
            label: tr(entry.name_key),
            shortcut: entry
                .command_id
                .and_then(|id| commands::shortcut_text(ctx, id)),
            enabled: (entry.enabled)(state, tab),
            action: (entry.action)(tab),
        })
        .collect()
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
        Some(Tab::Start) => tr("tab.start").to_owned(),
        None => tr("tab.unknown").to_owned(),
    }
}

/// The full path of a PDF tab's document.
fn tab_tooltip(state: &AppState, tab: TabId) -> Option<String> {
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
            Some(Tab::Start) => draw_start_page(ui, self.queue),
            None => {}
        }
    }

    fn context_menu(&mut self, ui: &mut Ui, tab: TabId) {
        let Some(items) = self.menus.get(&tab) else {
            return;
        };
        for item in items {
            let mut button = egui::Button::new(item.label);
            if let Some(keys) = &item.shortcut {
                button = button.shortcut_text(keys.as_str());
            }
            if ui.add_enabled(item.enabled, button).clicked() {
                self.queue.push(item.action.clone());
                ui.close();
            }
        }
    }

    fn is_closeable(&self, tab: TabId) -> bool {
        // The start page has no close button; it goes away by itself when a document opens.
        matches!(self.tabs.get(tab), Some(Tab::Pdf(_)))
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
    pdf_view::show(ui, tab, doc, cx);
}

/// Draws the start page: the application name and a button to open files.
fn draw_start_page(ui: &mut Ui, queue: &mut Vec<Action>) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.3);
        ui.heading("refdesk");
        ui.label(tr("start.hint"));
        ui.add_space(8.0);
        if ui.button(tr("start.open_button")).clicked() {
            queue.push(Action::OpenFileDialog);
        }
    });
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
        assert!(menu.iter().all(|m| !m.enabled));
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
}
