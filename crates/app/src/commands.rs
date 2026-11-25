//! The command table (`docs/ARCHITECTURE.md` §5.3).
//!
//! Every user command is one row: identifier, name (an i18n key), default shortcut, a function
//! that says whether it is possible now, and a function that builds its [`Action`]. Menus and
//! shortcut handling read this table and nothing else; there are no shortcuts or menu entries
//! written down elsewhere.

use egui::{Key, KeyboardShortcut, Modifiers};
use refdesk_core::ZoomMode;

use crate::action::Action;
use crate::state::{AppState, TabId};
use crate::windows::dock::SplitDir;

/// The menu a command appears in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuGroup {
    /// The "File" menu.
    File,
    /// The "View" menu.
    View,
    /// The "Tab" menu.
    Tab,
}

impl MenuGroup {
    /// The menus in the order they appear in the menu bar.
    pub const ALL: [MenuGroup; 3] = [MenuGroup::File, MenuGroup::View, MenuGroup::Tab];

    /// The i18n key of the menu title.
    pub fn name_key(self) -> &'static str {
        match self {
            MenuGroup::File => "menu.file",
            MenuGroup::View => "menu.view",
            MenuGroup::Tab => "menu.tab",
        }
    }
}

/// One row of the command table.
pub struct Command {
    /// Stable identifier, for configuration and the command palette.
    pub id: &'static str,
    /// i18n key of the displayed name.
    pub name_key: &'static str,
    /// The menu the command is listed in.
    pub menu: MenuGroup,
    /// The default shortcut, shown in the menu.
    pub shortcut: Option<KeyboardShortcut>,
    /// More key combinations that run the command but are not displayed. Used where keyboard
    /// layouts differ (`Ctrl+Shift+=` produces `+`, not `=`).
    pub aliases: &'static [KeyboardShortcut],
    /// Whether the command can run now.
    pub enabled: fn(&AppState) -> bool,
    /// The action the command produces; `None` when there is nothing to do.
    pub action: fn(&AppState) -> Option<Action>,
}

impl Command {
    /// The action to dispatch if the command is possible now.
    pub fn run(&self, state: &AppState) -> Option<Action> {
        log::debug!("command {}", self.id);
        if (self.enabled)(state) {
            (self.action)(state)
        } else {
            None
        }
    }

    /// Every key combination that triggers this command.
    pub fn all_shortcuts(&self) -> impl Iterator<Item = KeyboardShortcut> {
        self.shortcut
            .into_iter()
            .chain(self.aliases.iter().copied())
    }
}

const CTRL: Modifiers = Modifiers::CTRL;
const CTRL_SHIFT: Modifiers = Modifiers::CTRL.plus(Modifiers::SHIFT);

fn always(_: &AppState) -> bool {
    true
}

fn has_pdf_focus(state: &AppState) -> bool {
    state.focused_pdf().is_some()
}

/// The focused pane has more than one tab.
fn has_several_tabs(state: &AppState) -> bool {
    let dock = &state.focused_window().dock;
    dock.focused_tab()
        .is_some_and(|tab| dock.pane_tabs(tab).len() > 1)
}

/// The focused window is split.
fn has_several_panes(state: &AppState) -> bool {
    state.focused_window().dock.pane_count() > 1
}

fn has_closed_tabs(state: &AppState) -> bool {
    !state.closed_tabs.is_empty()
}

fn split_focused(state: &AppState, dir: SplitDir) -> Option<Action> {
    let (tab, _) = state.focused_pdf()?;
    Some(Action::SplitTab { tab, dir })
}

fn can_close_tab(state: &AppState) -> bool {
    state.focus.is_some_and(|tab| state.tabs.pdf(tab).is_some())
}

fn has_ready_pdf_focus(state: &AppState) -> bool {
    state
        .focused_pdf()
        .and_then(|(_, pdf)| state.docs.get(pdf.doc))
        .is_some_and(|doc| doc.info().is_some())
}

fn zoom_action(state: &AppState, change: fn(ZoomMode, Option<f32>) -> ZoomMode) -> Option<Action> {
    let (tab, pdf) = state.focused_pdf()?;
    Some(Action::SetZoom {
        tab,
        zoom: change(pdf.view.zoom, pdf.ui.effective_zoom),
    })
}

fn set_zoom(state: &AppState, zoom: ZoomMode) -> Option<Action> {
    let (tab, _) = state.focused_pdf()?;
    Some(Action::SetZoom { tab, zoom })
}

/// The command table. Order is the order within each menu.
pub static COMMANDS: &[Command] = &[
    Command {
        id: "file.open",
        name_key: "cmd.file.open",
        menu: MenuGroup::File,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::O)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::OpenFileDialog),
    },
    Command {
        id: "tab.close",
        name_key: "cmd.tab.close",
        menu: MenuGroup::File,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::W)),
        aliases: &[],
        enabled: can_close_tab,
        action: |s| s.focus.map(Action::CloseTab),
    },
    Command {
        id: "app.quit",
        name_key: "cmd.app.quit",
        menu: MenuGroup::File,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::Q)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::Quit),
    },
    Command {
        id: "view.zoom_in",
        name_key: "cmd.view.zoom_in",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::Equals)),
        aliases: &[KeyboardShortcut::new(CTRL, Key::Plus)],
        enabled: has_pdf_focus,
        action: |s| zoom_action(s, zoom_in),
    },
    Command {
        id: "view.zoom_out",
        name_key: "cmd.view.zoom_out",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::Minus)),
        aliases: &[],
        enabled: has_pdf_focus,
        action: |s| zoom_action(s, zoom_out),
    },
    Command {
        id: "view.zoom_reset",
        name_key: "cmd.view.zoom_reset",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::Num0)),
        aliases: &[],
        enabled: has_pdf_focus,
        action: |s| set_zoom(s, ZoomMode::Custom(1.0)),
    },
    Command {
        id: "view.fit_width",
        name_key: "cmd.view.fit_width",
        menu: MenuGroup::View,
        shortcut: None,
        aliases: &[],
        enabled: has_pdf_focus,
        action: |s| set_zoom(s, ZoomMode::FitWidth),
    },
    Command {
        id: "view.fit_page",
        name_key: "cmd.view.fit_page",
        menu: MenuGroup::View,
        shortcut: None,
        aliases: &[],
        enabled: has_pdf_focus,
        action: |s| set_zoom(s, ZoomMode::FitPage),
    },
    Command {
        id: "view.goto_page",
        name_key: "cmd.view.goto_page",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::G)),
        aliases: &[],
        enabled: has_ready_pdf_focus,
        action: |s| s.focus.map(Action::ShowGoToPage),
    },
    Command {
        id: "tab.next",
        name_key: "cmd.tab.next",
        menu: MenuGroup::Tab,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::Tab)),
        aliases: &[],
        enabled: has_several_tabs,
        action: |_| Some(Action::NextTab),
    },
    Command {
        id: "tab.prev",
        name_key: "cmd.tab.prev",
        menu: MenuGroup::Tab,
        shortcut: Some(KeyboardShortcut::new(CTRL_SHIFT, Key::Tab)),
        aliases: &[],
        enabled: has_several_tabs,
        action: |_| Some(Action::PrevTab),
    },
    Command {
        id: "tab.duplicate",
        name_key: "cmd.tab.duplicate",
        menu: MenuGroup::Tab,
        shortcut: None,
        aliases: &[],
        enabled: has_pdf_focus,
        action: |s| s.focus.map(Action::DuplicateTab),
    },
    Command {
        id: "tab.split_right",
        name_key: "cmd.tab.split_right",
        menu: MenuGroup::Tab,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::Backslash)),
        aliases: &[],
        enabled: has_pdf_focus,
        action: |s| split_focused(s, SplitDir::Right),
    },
    Command {
        id: "tab.split_down",
        name_key: "cmd.tab.split_down",
        menu: MenuGroup::Tab,
        shortcut: Some(KeyboardShortcut::new(CTRL_SHIFT, Key::Backslash)),
        // With Shift held, most layouts report `\` as `|`.
        aliases: &[KeyboardShortcut::new(CTRL_SHIFT, Key::Pipe)],
        enabled: has_pdf_focus,
        action: |s| split_focused(s, SplitDir::Down),
    },
    Command {
        id: "tab.focus_next_pane",
        name_key: "cmd.tab.focus_next_pane",
        menu: MenuGroup::Tab,
        shortcut: Some(KeyboardShortcut::new(Modifiers::NONE, Key::F6)),
        aliases: &[],
        enabled: has_several_panes,
        action: |_| Some(Action::FocusNextPane),
    },
    Command {
        id: "tab.reopen_closed",
        name_key: "cmd.tab.reopen_closed",
        menu: MenuGroup::Tab,
        shortcut: Some(KeyboardShortcut::new(CTRL_SHIFT, Key::T)),
        aliases: &[],
        enabled: has_closed_tabs,
        action: |_| Some(Action::ReopenClosedTab),
    },
];

/// One entry of the menu shown on a right click on a tab. Unlike [`Command`] it acts on the
/// clicked tab, not on the focused one.
pub struct TabMenuEntry {
    /// i18n key of the displayed name.
    pub name_key: &'static str,
    /// The command whose shortcut is displayed next to the entry, if any.
    pub command_id: Option<&'static str>,
    /// Whether the entry can run for this tab now.
    pub enabled: fn(&AppState, TabId) -> bool,
    /// The action for this tab.
    pub action: fn(TabId) -> Action,
}

fn is_pdf_tab(state: &AppState, tab: TabId) -> bool {
    state.tabs.pdf(tab).is_some()
}

fn has_pane_siblings(state: &AppState, tab: TabId) -> bool {
    state
        .window_of(tab)
        .is_some_and(|w| state.windows[w].dock.pane_tabs(tab).len() > 1)
}

/// The tab menu, in display order. Later work adds rows here (for example "Move to New Window").
pub static TAB_MENU: &[TabMenuEntry] = &[
    TabMenuEntry {
        name_key: "cmd.tab.close",
        command_id: Some("tab.close"),
        enabled: is_pdf_tab,
        action: Action::CloseTab,
    },
    TabMenuEntry {
        name_key: "cmd.tab.close_others",
        command_id: None,
        enabled: has_pane_siblings,
        action: Action::CloseOtherTabs,
    },
    TabMenuEntry {
        name_key: "cmd.tab.duplicate",
        command_id: Some("tab.duplicate"),
        enabled: is_pdf_tab,
        action: Action::DuplicateTab,
    },
    TabMenuEntry {
        name_key: "cmd.tab.split_right",
        command_id: Some("tab.split_right"),
        enabled: is_pdf_tab,
        action: |tab| Action::SplitTab {
            tab,
            dir: SplitDir::Right,
        },
    },
    TabMenuEntry {
        name_key: "cmd.tab.split_down",
        command_id: Some("tab.split_down"),
        enabled: is_pdf_tab,
        action: |tab| Action::SplitTab {
            tab,
            dir: SplitDir::Down,
        },
    },
];

/// The commands listed in a menu, in table order.
pub fn in_menu(group: MenuGroup) -> impl Iterator<Item = &'static Command> {
    COMMANDS.iter().filter(move |c| c.menu == group)
}

/// The zoom factors the zoom commands step through.
const ZOOM_STEPS: &[f32] = &[
    0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 5.0,
];

/// The factor a zoom mode stands for when stepping. The fit modes depend on the viewport: they
/// step from `actual`, the factor the view last laid out with, or from 100% before that.
fn base_zoom(zoom: ZoomMode, actual: Option<f32>) -> f32 {
    match zoom {
        ZoomMode::Custom(z) => z,
        ZoomMode::FitWidth | ZoomMode::FitPage => actual.unwrap_or(1.0),
    }
}

/// The next larger step. `actual` is the view's actual zoom factor, if known.
pub fn zoom_in(zoom: ZoomMode, actual: Option<f32>) -> ZoomMode {
    let base = base_zoom(zoom, actual);
    let next = ZOOM_STEPS
        .iter()
        .copied()
        .find(|s| *s > base + 0.001)
        .or_else(|| ZOOM_STEPS.last().copied())
        .unwrap_or(base);
    ZoomMode::Custom(next)
}

/// The next smaller step. `actual` is the view's actual zoom factor, if known.
pub fn zoom_out(zoom: ZoomMode, actual: Option<f32>) -> ZoomMode {
    let base = base_zoom(zoom, actual);
    let next = ZOOM_STEPS
        .iter()
        .rev()
        .copied()
        .find(|s| *s < base - 0.001)
        .or_else(|| ZOOM_STEPS.first().copied())
        .unwrap_or(base);
    ZoomMode::Custom(next)
}

/// The displayed shortcut of a command, formatted for this platform.
pub fn shortcut_text(ctx: &egui::Context, id: &str) -> Option<String> {
    COMMANDS
        .iter()
        .find(|c| c.id == id)
        .and_then(|c| c.shortcut)
        .map(|s| ctx.format_shortcut(&s))
}

/// How many modifier keys a combination needs beyond the primary one. Shortcuts are matched most
/// specific first because egui ignores extra Shift/Alt when matching (`Ctrl+Tab` would otherwise
/// swallow `Ctrl+Shift+Tab`).
fn specificity(shortcut: &KeyboardShortcut) -> u8 {
    u8::from(shortcut.modifiers.shift) + u8::from(shortcut.modifiers.alt)
}

/// Consumes at most one shortcut from this frame's input and returns its action.
///
/// Call it once per frame. Nothing is consumed while a text field has the keyboard, so typing
/// goes to the field.
pub fn handle_shortcuts(ctx: &egui::Context, state: &AppState) -> Option<Action> {
    if ctx.text_edit_focused() {
        return None;
    }
    let mut bindings: Vec<(KeyboardShortcut, &Command)> = COMMANDS
        .iter()
        .flat_map(|c| c.all_shortcuts().map(move |s| (s, c)))
        .collect();
    bindings.sort_by_key(|(s, _)| std::cmp::Reverse(specificity(s)));

    ctx.input_mut(|input| {
        for (shortcut, command) in bindings {
            if input.consume_shortcut(&shortcut) {
                // The key is consumed even when the command is not possible right now, so it
                // does not leak into a widget.
                return command.run(state);
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{OpenTarget, dispatch};
    use crate::i18n::{Lang, lookup};
    use std::collections::HashSet;

    #[test]
    fn command_ids_are_unique() {
        let mut seen = HashSet::new();
        for c in COMMANDS {
            assert!(seen.insert(c.id), "duplicate command id {}", c.id);
        }
    }

    #[test]
    fn shortcuts_are_unique() {
        let mut seen = HashSet::new();
        for c in COMMANDS {
            for s in c.all_shortcuts() {
                assert!(seen.insert(s), "duplicate shortcut {s:?} on {}", c.id);
            }
        }
    }

    #[test]
    fn every_name_key_exists_in_both_languages() {
        for c in COMMANDS {
            for lang in [Lang::Ko, Lang::En] {
                assert!(
                    lookup(lang, c.name_key).is_some(),
                    "{} has no {lang:?} text for {}",
                    c.id,
                    c.name_key
                );
            }
        }
        for g in MenuGroup::ALL {
            for lang in [Lang::Ko, Lang::En] {
                assert!(lookup(lang, g.name_key()).is_some());
            }
        }
    }

    #[test]
    fn pane_commands_are_in_the_tab_menu_with_their_shortcuts() {
        let tab_menu: Vec<&str> = in_menu(MenuGroup::Tab).map(|c| c.id).collect();
        for (id, shortcut) in [
            (
                "tab.split_right",
                KeyboardShortcut::new(CTRL, Key::Backslash),
            ),
            (
                "tab.split_down",
                KeyboardShortcut::new(CTRL_SHIFT, Key::Backslash),
            ),
            (
                "tab.focus_next_pane",
                KeyboardShortcut::new(Modifiers::NONE, Key::F6),
            ),
            (
                "tab.reopen_closed",
                KeyboardShortcut::new(CTRL_SHIFT, Key::T),
            ),
        ] {
            assert!(tab_menu.contains(&id), "{id} is not in the Tab menu");
            let c = COMMANDS.iter().find(|c| c.id == id).expect("command");
            assert_eq!(c.shortcut, Some(shortcut), "{id}");
        }
        for id in ["tab.next", "tab.prev", "tab.duplicate"] {
            assert!(tab_menu.contains(&id), "{id} is not in the Tab menu");
        }
    }

    #[test]
    fn tab_menu_entries_have_texts_and_known_commands() {
        let names: Vec<&str> = TAB_MENU.iter().map(|e| e.name_key).collect();
        assert_eq!(
            names,
            vec![
                "cmd.tab.close",
                "cmd.tab.close_others",
                "cmd.tab.duplicate",
                "cmd.tab.split_right",
                "cmd.tab.split_down",
            ]
        );
        for e in TAB_MENU {
            for lang in [Lang::Ko, Lang::En] {
                assert!(lookup(lang, e.name_key).is_some(), "{}", e.name_key);
            }
            if let Some(id) = e.command_id {
                assert!(COMMANDS.iter().any(|c| c.id == id), "unknown command {id}");
            }
        }
    }

    #[test]
    fn tab_menu_availability_follows_the_clicked_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let start = state.main_window().dock.tabs()[0];
        assert!(TAB_MENU.iter().all(|e| !(e.enabled)(&state, start)));
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/a.pdf")]),
            &ctx,
        );
        let (a, _) = state.focused_pdf().expect("pdf tab");
        let enabled: Vec<bool> = TAB_MENU.iter().map(|e| (e.enabled)(&state, a)).collect();
        assert_eq!(enabled, vec![true, false, true, true, true]);
        assert_eq!((TAB_MENU[0].action)(a), Action::CloseTab(a));
        dispatch(&mut state, Action::DuplicateTab(a), &ctx);
        assert!((TAB_MENU[1].enabled)(&state, a));
    }

    #[test]
    fn pane_command_availability() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let find = |id: &str| COMMANDS.iter().find(|c| c.id == id).expect("command");
        for id in [
            "tab.split_right",
            "tab.split_down",
            "tab.focus_next_pane",
            "tab.reopen_closed",
        ] {
            assert!(find(id).run(&state).is_none(), "{id} should be unavailable");
        }
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/a.pdf")]),
            &ctx,
        );
        let (a, _) = state.focused_pdf().expect("pdf tab");
        assert_eq!(
            find("tab.split_right").run(&state),
            Some(Action::SplitTab {
                tab: a,
                dir: SplitDir::Right
            })
        );
        assert_eq!(
            find("tab.split_down").run(&state),
            Some(Action::SplitTab {
                tab: a,
                dir: SplitDir::Down
            })
        );
        assert!(find("tab.focus_next_pane").run(&state).is_none());
        dispatch(
            &mut state,
            Action::SplitTab {
                tab: a,
                dir: SplitDir::Right,
            },
            &ctx,
        );
        assert_eq!(
            find("tab.focus_next_pane").run(&state),
            Some(Action::FocusNextPane)
        );
        // Each pane has one tab: next/previous tab have nothing to switch to.
        assert!(find("tab.next").run(&state).is_none());
        dispatch(&mut state, Action::CloseTab(a), &ctx);
        assert_eq!(
            find("tab.reopen_closed").run(&state),
            Some(Action::ReopenClosedTab)
        );
    }

    #[test]
    fn split_shortcuts_are_told_apart() {
        let ctx = egui::Context::default();
        let state = state_with_tabs(&ctx, 1);
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        assert_eq!(
            press(&ctx, &state, Modifiers::CTRL, Key::Backslash),
            Some(Action::SplitTab {
                tab,
                dir: SplitDir::Right
            })
        );
        for key in [Key::Backslash, Key::Pipe] {
            assert_eq!(
                press(&ctx, &state, CTRL_SHIFT, key),
                Some(Action::SplitTab {
                    tab,
                    dir: SplitDir::Down
                }),
                "{key:?}"
            );
        }
    }

    #[test]
    fn every_command_is_in_a_menu_and_menus_cover_the_table() {
        let listed: usize = MenuGroup::ALL.iter().map(|g| in_menu(*g).count()).sum();
        assert_eq!(listed, COMMANDS.len());
    }

    #[test]
    fn specified_commands_exist_with_their_default_shortcuts() {
        let shortcut = |id: &str| {
            COMMANDS
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("missing command {id}"))
                .shortcut
        };
        assert_eq!(
            shortcut("file.open"),
            Some(KeyboardShortcut::new(CTRL, Key::O))
        );
        assert_eq!(
            shortcut("tab.close"),
            Some(KeyboardShortcut::new(CTRL, Key::W))
        );
        assert_eq!(
            shortcut("tab.next"),
            Some(KeyboardShortcut::new(CTRL, Key::Tab))
        );
        assert_eq!(
            shortcut("tab.prev"),
            Some(KeyboardShortcut::new(CTRL_SHIFT, Key::Tab))
        );
        assert_eq!(
            shortcut("view.zoom_in"),
            Some(KeyboardShortcut::new(CTRL, Key::Equals))
        );
        assert_eq!(
            shortcut("view.zoom_out"),
            Some(KeyboardShortcut::new(CTRL, Key::Minus))
        );
        assert_eq!(
            shortcut("view.zoom_reset"),
            Some(KeyboardShortcut::new(CTRL, Key::Num0))
        );
        assert_eq!(
            shortcut("app.quit"),
            Some(KeyboardShortcut::new(CTRL, Key::Q))
        );
        for id in ["tab.duplicate", "view.fit_width", "view.fit_page"] {
            assert!(COMMANDS.iter().any(|c| c.id == id), "missing command {id}");
        }
    }

    #[test]
    fn availability_follows_the_focused_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let find = |id: &str| COMMANDS.iter().find(|c| c.id == id).expect("command");

        // Only the start page: opening and quitting work, tab commands do not.
        assert!(find("file.open").run(&state).is_some());
        assert!(find("app.quit").run(&state).is_some());
        for id in [
            "tab.close",
            "tab.next",
            "tab.duplicate",
            "view.zoom_in",
            "view.fit_page",
        ] {
            assert!(find(id).run(&state).is_none(), "{id} should be unavailable");
        }

        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/a.pdf")]),
            &ctx,
        );
        for id in [
            "tab.close",
            "tab.duplicate",
            "view.zoom_in",
            "view.fit_page",
        ] {
            assert!(find(id).run(&state).is_some(), "{id} should be available");
        }
        // Next/previous need a second tab.
        assert!(find("tab.next").run(&state).is_none());
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/b.pdf")]),
            &ctx,
        );
        assert_eq!(find("tab.next").run(&state), Some(Action::NextTab));
        assert_eq!(find("tab.prev").run(&state), Some(Action::PrevTab));
    }

    #[test]
    fn zoom_commands_step_from_the_current_zoom() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/a.pdf")]),
            &ctx,
        );
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        let find = |id: &str| COMMANDS.iter().find(|c| c.id == id).expect("command");

        // A fresh view is FitWidth, which steps from 100%.
        assert_eq!(
            find("view.zoom_in").run(&state),
            Some(Action::SetZoom {
                tab,
                zoom: ZoomMode::Custom(1.1)
            })
        );
        assert_eq!(
            find("view.zoom_out").run(&state),
            Some(Action::SetZoom {
                tab,
                zoom: ZoomMode::Custom(0.9)
            })
        );
        assert_eq!(
            find("view.zoom_reset").run(&state),
            Some(Action::SetZoom {
                tab,
                zoom: ZoomMode::Custom(1.0)
            })
        );
        assert_eq!(
            find("view.fit_width").run(&state),
            Some(Action::SetZoom {
                tab,
                zoom: ZoomMode::FitWidth
            })
        );
        assert_eq!(
            find("view.fit_page").run(&state),
            Some(Action::SetZoom {
                tab,
                zoom: ZoomMode::FitPage
            })
        );
    }

    #[test]
    fn zoom_steps_are_clamped_at_both_ends() {
        assert_eq!(zoom_in(ZoomMode::Custom(5.0), None), ZoomMode::Custom(5.0));
        assert_eq!(
            zoom_out(ZoomMode::Custom(0.25), None),
            ZoomMode::Custom(0.25)
        );
        assert_eq!(zoom_in(ZoomMode::Custom(1.3), None), ZoomMode::Custom(1.5));
        assert_eq!(
            zoom_out(ZoomMode::Custom(1.3), None),
            ZoomMode::Custom(1.25)
        );
    }

    #[test]
    fn fit_modes_step_from_their_actual_zoom() {
        // A fit-width view laid out at 137%.
        assert_eq!(
            zoom_in(ZoomMode::FitWidth, Some(1.37)),
            ZoomMode::Custom(1.5)
        );
        assert_eq!(
            zoom_out(ZoomMode::FitPage, Some(1.37)),
            ZoomMode::Custom(1.25)
        );
        // A custom zoom steps from itself whatever the layout said.
        assert_eq!(
            zoom_in(ZoomMode::Custom(1.0), Some(3.0)),
            ZoomMode::Custom(1.1)
        );
    }

    #[test]
    fn goto_page_command_needs_an_open_document() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let find = |id: &str| COMMANDS.iter().find(|c| c.id == id).expect("command");
        assert_eq!(
            find("view.goto_page").shortcut,
            Some(KeyboardShortcut::new(CTRL, Key::G))
        );
        assert!(find("view.goto_page").run(&state).is_none());
        // A document that cannot open has no pages to go to.
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/missing.pdf")]),
            &ctx,
        );
        assert!(find("view.goto_page").run(&state).is_none());
    }

    /// Runs one headless frame. The texture deltas are discarded: there is no renderer.
    fn run_frame(ctx: &egui::Context, input: egui::RawInput, f: impl FnMut(&mut egui::Ui)) {
        ctx.run_ui(input, f).drop_without_applying_deltas();
    }

    /// Runs one frame with the given key press and returns what the shortcut handler produced.
    fn press(
        ctx: &egui::Context,
        state: &AppState,
        modifiers: Modifiers,
        key: Key,
    ) -> Option<Action> {
        let mut out = None;
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key,
                physical_key: Some(key),
                pressed: true,
                repeat: false,
                modifiers,
            }],
            ..Default::default()
        };
        run_frame(ctx, input, |ui| {
            out = handle_shortcuts(ui.ctx(), state);
        });
        out
    }

    fn state_with_tabs(ctx: &egui::Context, n: usize) -> AppState {
        let mut state = AppState::new();
        for i in 0..n {
            dispatch(
                &mut state,
                Action::OpenFiles(vec![OpenTarget::parse(&format!("/refdesk-test/{i}.pdf"))]),
                ctx,
            );
        }
        state
    }

    #[test]
    fn ctrl_tab_and_ctrl_shift_tab_are_told_apart() {
        let ctx = egui::Context::default();
        let state = state_with_tabs(&ctx, 2);
        assert_eq!(
            press(&ctx, &state, Modifiers::CTRL, Key::Tab),
            Some(Action::NextTab)
        );
        assert_eq!(
            press(
                &ctx,
                &state,
                Modifiers::CTRL.plus(Modifiers::SHIFT),
                Key::Tab
            ),
            Some(Action::PrevTab)
        );
    }

    #[test]
    fn shortcut_alias_zoom_in_with_plus_key() {
        let ctx = egui::Context::default();
        let state = state_with_tabs(&ctx, 1);
        let a = press(
            &ctx,
            &state,
            Modifiers::CTRL.plus(Modifiers::SHIFT),
            Key::Plus,
        );
        assert!(matches!(a, Some(Action::SetZoom { .. })));
    }

    #[test]
    fn plain_keys_and_unavailable_commands_produce_nothing() {
        let ctx = egui::Context::default();
        let state = AppState::new();
        assert_eq!(press(&ctx, &state, Modifiers::NONE, Key::O), None);
        // Ctrl+W with only the start page: the command is not possible.
        assert_eq!(press(&ctx, &state, Modifiers::CTRL, Key::W), None);
        assert_eq!(
            press(&ctx, &state, Modifiers::CTRL, Key::O),
            Some(Action::OpenFileDialog)
        );
    }

    #[test]
    fn shortcuts_are_ignored_while_a_text_field_has_focus() {
        let ctx = egui::Context::default();
        let state = AppState::new();
        let mut text = String::new();
        // Frame 1: a text edit takes the focus.
        run_frame(&ctx, egui::RawInput::default(), |ui| {
            let r = ui.text_edit_singleline(&mut text);
            r.request_focus();
        });
        // Frame 2: the focus is in place now.
        run_frame(&ctx, egui::RawInput::default(), |ui| {
            ui.text_edit_singleline(&mut text);
        });
        assert!(ctx.text_edit_focused());
        assert_eq!(press(&ctx, &state, Modifiers::CTRL, Key::O), None);
    }
}
