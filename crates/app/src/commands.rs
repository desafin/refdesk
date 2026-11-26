//! The command table (`docs/ARCHITECTURE.md` §5.3).
//!
//! Every user command is one row: identifier, name (an i18n key), default shortcut, a function
//! that says whether it is possible now, and a function that builds its [`Action`]. Menus and
//! shortcut handling read this table and nothing else; there are no shortcuts or menu entries
//! written down elsewhere.

use egui::{Key, KeyboardShortcut, Modifiers};
use refdesk_core::ZoomMode;

use crate::action::Action;
use crate::overlays::palette::PaletteMode;
use crate::panels::sidebar::SidebarSection;
use crate::state::{AppState, PdfTab, Tab, TabId};
use crate::windows::dock::SplitDir;

/// The menu a command appears in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuGroup {
    /// The "File" menu.
    File,
    /// The "Edit" menu.
    Edit,
    /// The "View" menu.
    View,
    /// The "Go" menu: history and page jumps.
    Go,
    /// The "Tab" menu.
    Tab,
    /// The "Window" menu. It also lists the open windows (see `windows::menu_bar`).
    Window,
}

impl MenuGroup {
    /// The menus in the order they appear in the menu bar.
    pub const ALL: [MenuGroup; 6] = [
        MenuGroup::File,
        MenuGroup::Edit,
        MenuGroup::View,
        MenuGroup::Go,
        MenuGroup::Tab,
        MenuGroup::Window,
    ];

    /// The i18n key of the menu title.
    pub fn name_key(self) -> &'static str {
        match self {
            MenuGroup::File => "menu.file",
            MenuGroup::Edit => "menu.edit",
            MenuGroup::View => "menu.view",
            MenuGroup::Go => "menu.go",
            MenuGroup::Tab => "menu.tab",
            MenuGroup::Window => "menu.window",
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
const CTRL_ALT: Modifiers = Modifiers::CTRL.plus(Modifiers::ALT);

fn always(_: &AppState) -> bool {
    true
}

/// A folder is open as the workspace (WS-1).
fn has_workspace(state: &AppState) -> bool {
    state.workspace.is_some()
}

/// Folds or unfolds a sidebar section of the focused window (WS-7).
fn toggle_section(state: &AppState, section: SidebarSection) -> Option<Action> {
    Some(Action::ToggleSidebarSection {
        window: state.focused_window().id,
        section,
    })
}

fn has_pdf_focus(state: &AppState) -> bool {
    state.focused_pdf().is_some()
}

/// The focused tab can zoom: a PDF tab or a comparison tab.
fn has_zoom_focus(state: &AppState) -> bool {
    focused_zoom(state).is_some()
}

/// The focused tab that zooms, with its zoom mode and actual factor: a PDF tab, or the side in
/// use of a comparison tab.
fn focused_zoom(state: &AppState) -> Option<(TabId, ZoomMode, Option<f32>)> {
    let id = state.focus?;
    let pdf = match state.tabs.get(id)? {
        Tab::Pdf(pdf) => pdf,
        Tab::Diff(diff) => diff.side(diff.active),
        Tab::Start => return None,
    };
    Some((id, pdf.view.zoom, pdf.ui.effective_zoom))
}

/// The focused comparison tab has a valid result with changes to move between (DIFF-5).
fn has_diff_changes(state: &AppState) -> bool {
    state.focused_diff().is_some_and(|(_, diff)| {
        diff.fresh_result(&state.docs)
            .is_some_and(|r| !r.changes.is_empty())
    })
}

/// The focused tab is a comparison tab.
fn has_diff_focus(state: &AppState) -> bool {
    state.focused_diff().is_some()
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

/// Some search is remembered.
fn has_search_history(state: &AppState) -> bool {
    !state.search_history.is_empty()
}

/// The focused PDF view (the side in use of a comparison tab) has selected text.
fn has_selection(state: &AppState) -> bool {
    match state.focus.and_then(|id| state.tabs.get(id)) {
        Some(Tab::Pdf(pdf)) => pdf.ui.has_selection(),
        // A comparison tab copies from the side in use.
        Some(Tab::Diff(diff)) => diff.side(diff.active).ui.has_selection(),
        _ => false,
    }
}

/// The focused tab is a PDF tab with selected text (translation works in PDF tabs only).
fn has_pdf_selection(state: &AppState) -> bool {
    state
        .focused_pdf()
        .is_some_and(|(_, pdf)| pdf.ui.has_selection())
}

/// The focused PDF view has an earlier history entry.
fn can_go_back(state: &AppState) -> bool {
    state
        .focused_pdf()
        .is_some_and(|(_, pdf)| pdf.view.history.can_back())
}

/// The focused PDF view has a later history entry.
fn can_go_forward(state: &AppState) -> bool {
    state
        .focused_pdf()
        .is_some_and(|(_, pdf)| pdf.view.history.can_forward())
}

/// The focused tab is a preview tab (WS-3).
fn has_preview_focus(state: &AppState) -> bool {
    state.focus.is_some_and(|tab| is_preview_tab(state, tab))
}

fn has_closed_tabs(state: &AppState) -> bool {
    !state.closed_tabs.is_empty()
}

fn has_several_windows(state: &AppState) -> bool {
    state.windows.len() > 1
}

fn split_focused(state: &AppState, dir: SplitDir) -> Option<Action> {
    let (tab, _) = state.focused_pdf()?;
    Some(Action::SplitTab { tab, dir })
}

fn can_close_tab(state: &AppState) -> bool {
    state.focus.is_some_and(|tab| is_document_tab(state, tab))
}

fn has_ready_pdf_focus(state: &AppState) -> bool {
    state
        .focused_pdf()
        .and_then(|(_, pdf)| state.docs.get(pdf.doc))
        .is_some_and(|doc| doc.info().is_some())
}

fn zoom_action(state: &AppState, change: fn(ZoomMode, Option<f32>) -> ZoomMode) -> Option<Action> {
    let (tab, zoom, effective) = focused_zoom(state)?;
    Some(Action::SetZoom {
        tab,
        zoom: change(zoom, effective),
    })
}

fn set_zoom(state: &AppState, zoom: ZoomMode) -> Option<Action> {
    let (tab, _, _) = focused_zoom(state)?;
    Some(Action::SetZoom { tab, zoom })
}

/// Sets the scroll speed one step up or down the menu's list (VIEW-12).
fn scroll_speed_step(state: &AppState, up: bool) -> Option<Action> {
    let speed = refdesk_core::step_scroll_speed(state.scroll_speed(), up);
    Some(Action::SetScrollSpeed(Some(speed)))
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
        id: "file.open_folder",
        name_key: "cmd.file.open_folder",
        menu: MenuGroup::File,
        shortcut: Some(KeyboardShortcut::new(CTRL_SHIFT, Key::O)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::OpenFolderDialog),
    },
    Command {
        id: "file.close_folder",
        name_key: "cmd.file.close_folder",
        menu: MenuGroup::File,
        shortcut: None,
        aliases: &[],
        enabled: has_workspace,
        action: |_| Some(Action::CloseFolder),
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
        id: "edit.copy",
        name_key: "cmd.edit.copy",
        menu: MenuGroup::Edit,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::C)),
        aliases: &[],
        enabled: has_selection,
        action: |s| s.focus.map(Action::CopySelection),
    },
    Command {
        id: "edit.translate_selection",
        name_key: "cmd.edit.translate_selection",
        menu: MenuGroup::Edit,
        shortcut: Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::F2)),
        aliases: &[],
        enabled: has_pdf_selection,
        action: |s| s.focus.map(Action::TranslateSelection),
    },
    Command {
        id: "edit.clear_search_history",
        name_key: "cmd.edit.clear_search_history",
        menu: MenuGroup::Edit,
        shortcut: None,
        aliases: &[],
        enabled: has_search_history,
        action: |_| Some(Action::ClearSearchHistory),
    },
    Command {
        id: "edit.find",
        name_key: "cmd.edit.find",
        menu: MenuGroup::Edit,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::F)),
        aliases: &[],
        enabled: has_pdf_focus,
        action: |s| s.focus.map(Action::Find),
    },
    Command {
        id: "edit.find_all",
        name_key: "cmd.edit.find_all",
        menu: MenuGroup::Edit,
        shortcut: Some(KeyboardShortcut::new(CTRL_SHIFT, Key::F)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::ShowGlobalSearch),
    },
    Command {
        id: "edit.find_next",
        name_key: "cmd.edit.find_next",
        menu: MenuGroup::Edit,
        shortcut: Some(KeyboardShortcut::new(Modifiers::NONE, Key::F3)),
        aliases: &[],
        enabled: has_pdf_focus,
        action: |s| s.focus.map(Action::FindNext),
    },
    Command {
        id: "edit.find_prev",
        name_key: "cmd.edit.find_prev",
        menu: MenuGroup::Edit,
        shortcut: Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::F3)),
        aliases: &[],
        enabled: has_pdf_focus,
        action: |s| s.focus.map(Action::FindPrev),
    },
    Command {
        id: "edit.find_all_next",
        name_key: "cmd.edit.find_all_next",
        menu: MenuGroup::Edit,
        shortcut: Some(KeyboardShortcut::new(Modifiers::NONE, Key::F4)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::GlobalSearchStep { forward: true }),
    },
    Command {
        id: "edit.find_all_prev",
        name_key: "cmd.edit.find_all_prev",
        menu: MenuGroup::Edit,
        shortcut: Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::F4)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::GlobalSearchStep { forward: false }),
    },
    Command {
        id: "edit.find_all_clear",
        name_key: "cmd.edit.find_all_clear",
        menu: MenuGroup::Edit,
        shortcut: None,
        aliases: &[],
        enabled: |s| s.global_search.has_query(),
        action: |_| Some(Action::ClearGlobalSearch),
    },
    Command {
        id: "edit.capture_region",
        name_key: "cmd.edit.capture_region",
        menu: MenuGroup::Edit,
        shortcut: Some(KeyboardShortcut::new(CTRL_SHIFT, Key::C)),
        aliases: &[],
        enabled: has_ready_pdf_focus,
        action: |s| s.focus.map(Action::StartCapture),
    },
    Command {
        id: "edit.capture_page",
        name_key: "cmd.edit.capture_page",
        menu: MenuGroup::Edit,
        shortcut: None,
        aliases: &[],
        enabled: has_ready_pdf_focus,
        action: |s| s.focus.map(Action::CapturePage),
    },
    Command {
        id: "view.zoom_in",
        name_key: "cmd.view.zoom_in",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::Equals)),
        aliases: &[KeyboardShortcut::new(CTRL, Key::Plus)],
        enabled: has_zoom_focus,
        action: |s| zoom_action(s, zoom_in),
    },
    Command {
        id: "view.zoom_out",
        name_key: "cmd.view.zoom_out",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::Minus)),
        aliases: &[],
        enabled: has_zoom_focus,
        action: |s| zoom_action(s, zoom_out),
    },
    Command {
        id: "view.zoom_reset",
        name_key: "cmd.view.zoom_reset",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::Num0)),
        aliases: &[],
        enabled: has_zoom_focus,
        action: |s| set_zoom(s, ZoomMode::Custom(1.0)),
    },
    Command {
        id: "view.fit_width",
        name_key: "cmd.view.fit_width",
        menu: MenuGroup::View,
        shortcut: None,
        aliases: &[],
        enabled: has_zoom_focus,
        action: |s| set_zoom(s, ZoomMode::FitWidth),
    },
    Command {
        id: "view.fit_page",
        name_key: "cmd.view.fit_page",
        menu: MenuGroup::View,
        shortcut: None,
        aliases: &[],
        enabled: has_zoom_focus,
        action: |s| set_zoom(s, ZoomMode::FitPage),
    },
    Command {
        id: "view.scroll_faster",
        name_key: "cmd.view.scroll_faster",
        menu: MenuGroup::View,
        shortcut: None,
        aliases: &[],
        enabled: always,
        action: |s| scroll_speed_step(s, true),
    },
    Command {
        id: "view.scroll_slower",
        name_key: "cmd.view.scroll_slower",
        menu: MenuGroup::View,
        shortcut: None,
        aliases: &[],
        enabled: always,
        action: |s| scroll_speed_step(s, false),
    },
    Command {
        id: "nav.back",
        name_key: "cmd.nav.back",
        menu: MenuGroup::Go,
        shortcut: Some(KeyboardShortcut::new(Modifiers::ALT, Key::ArrowLeft)),
        aliases: &[],
        enabled: can_go_back,
        action: |s| s.focus.map(Action::Back),
    },
    Command {
        id: "nav.forward",
        name_key: "cmd.nav.forward",
        menu: MenuGroup::Go,
        shortcut: Some(KeyboardShortcut::new(Modifiers::ALT, Key::ArrowRight)),
        aliases: &[],
        enabled: can_go_forward,
        action: |s| s.focus.map(Action::Forward),
    },
    Command {
        id: "view.goto_page",
        name_key: "cmd.view.goto_page",
        menu: MenuGroup::Go,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::G)),
        aliases: &[],
        enabled: has_ready_pdf_focus,
        action: |s| s.focus.map(Action::ShowGoToPage),
    },
    Command {
        id: "view.toggle_toc",
        name_key: "cmd.view.toggle_toc",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(Modifiers::NONE, Key::F9)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::ToggleToc),
    },
    Command {
        id: "view.show_explorer",
        name_key: "cmd.view.show_explorer",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(CTRL_SHIFT, Key::E)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::ShowExplorer),
    },
    Command {
        id: "view.toggle_explorer_section",
        name_key: "cmd.view.toggle_explorer_section",
        menu: MenuGroup::View,
        shortcut: None,
        aliases: &[],
        enabled: always,
        action: |s| toggle_section(s, SidebarSection::Explorer),
    },
    Command {
        id: "view.toggle_outline_section",
        name_key: "cmd.view.toggle_outline_section",
        menu: MenuGroup::View,
        shortcut: None,
        aliases: &[],
        enabled: always,
        action: |s| toggle_section(s, SidebarSection::Outline),
    },
    Command {
        id: "view.refresh_explorer",
        name_key: "cmd.view.refresh_explorer",
        menu: MenuGroup::View,
        shortcut: None,
        aliases: &[],
        enabled: has_workspace,
        action: |_| Some(Action::RefreshWorkspace),
    },
    Command {
        id: "view.translate_mode",
        name_key: "cmd.view.translate_mode",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(Modifiers::NONE, Key::F2)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::ToggleTranslateMode),
    },
    Command {
        id: "view.toggle_breadcrumb",
        name_key: "cmd.view.toggle_breadcrumb",
        menu: MenuGroup::View,
        shortcut: None,
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::ToggleBreadcrumb),
    },
    Command {
        id: "palette.commands",
        name_key: "cmd.palette.commands",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(CTRL_SHIFT, Key::P)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::ShowPalette(PaletteMode::Commands)),
    },
    Command {
        id: "palette.quick_open",
        name_key: "cmd.palette.quick_open",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(CTRL, Key::P)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::ShowPalette(PaletteMode::QuickOpen)),
    },
    Command {
        id: "diff.start",
        name_key: "cmd.diff.start",
        menu: MenuGroup::View,
        shortcut: None,
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::ShowDiffDialog),
    },
    Command {
        id: "diff.next_change",
        name_key: "cmd.diff.next_change",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(Modifiers::NONE, Key::F7)),
        aliases: &[],
        enabled: has_diff_changes,
        action: |s| s.focus.map(Action::NextChange),
    },
    Command {
        id: "diff.prev_change",
        name_key: "cmd.diff.prev_change",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::F7)),
        aliases: &[],
        enabled: has_diff_changes,
        action: |s| s.focus.map(Action::PrevChange),
    },
    Command {
        id: "diff.toggle_changes",
        name_key: "cmd.diff.toggle_changes",
        menu: MenuGroup::View,
        shortcut: Some(KeyboardShortcut::new(Modifiers::NONE, Key::F8)),
        aliases: &[],
        enabled: has_diff_focus,
        action: |s| s.focus.map(Action::ToggleChangesPanel),
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
        id: "tab.pin",
        name_key: "cmd.tab.pin",
        menu: MenuGroup::Tab,
        shortcut: None,
        aliases: &[],
        enabled: has_preview_focus,
        action: |s| s.focus.map(Action::PinTab),
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
    Command {
        id: "window.new",
        name_key: "cmd.window.new",
        menu: MenuGroup::Window,
        shortcut: Some(KeyboardShortcut::new(CTRL_SHIFT, Key::N)),
        aliases: &[],
        enabled: always,
        action: |_| Some(Action::NewWindow),
    },
    Command {
        id: "window.detach_tab",
        name_key: "cmd.window.detach_tab",
        menu: MenuGroup::Window,
        shortcut: Some(KeyboardShortcut::new(CTRL_ALT, Key::N)),
        aliases: &[],
        enabled: can_close_tab,
        action: |s| s.focus.map(Action::DetachTab),
    },
    Command {
        id: "window.merge_all",
        name_key: "cmd.window.merge_all",
        menu: MenuGroup::Window,
        shortcut: None,
        aliases: &[],
        enabled: has_several_windows,
        action: |_| Some(Action::MergeAllWindows),
    },
    Command {
        id: "window.close",
        name_key: "cmd.window.close",
        menu: MenuGroup::Window,
        shortcut: Some(KeyboardShortcut::new(CTRL_SHIFT, Key::W)),
        aliases: &[],
        enabled: always,
        action: |s| Some(Action::CloseWindow(s.focused_window().id)),
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

/// A tab that shows documents (a PDF or a comparison), as opposed to the start page.
fn is_document_tab(state: &AppState, tab: TabId) -> bool {
    matches!(state.tabs.get(tab), Some(Tab::Pdf(_) | Tab::Diff(_)))
}

/// A preview tab, which the next file opened from the explorer replaces (WS-3).
fn is_preview_tab(state: &AppState, tab: TabId) -> bool {
    state.tabs.pdf(tab).is_some_and(|p| p.preview)
}

fn has_pane_siblings(state: &AppState, tab: TabId) -> bool {
    state
        .window_of(tab)
        .is_some_and(|w| state.windows[w].dock.pane_tabs(tab).len() > 1)
}

/// The tab menu, in display order. `windows::tab_menu` adds the dynamic "Move to Window" list
/// after these rows.
pub static TAB_MENU: &[TabMenuEntry] = &[
    TabMenuEntry {
        name_key: "cmd.tab.close",
        command_id: Some("tab.close"),
        enabled: is_document_tab,
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
        name_key: "cmd.tab.pin",
        command_id: Some("tab.pin"),
        enabled: is_preview_tab,
        action: Action::PinTab,
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
    TabMenuEntry {
        name_key: "cmd.window.detach_tab",
        command_id: Some("window.detach_tab"),
        enabled: is_document_tab,
        action: Action::DetachTab,
    },
];

/// One entry of the menu shown on a right click in a PDF view when text is selected (SEL-3).
/// It acts on the clicked view.
pub struct SelectionMenuEntry {
    /// i18n key of the displayed name.
    pub name_key: &'static str,
    /// The command whose shortcut is displayed next to the entry, if any.
    pub command_id: Option<&'static str>,
    /// Whether the entry can run for this view now.
    pub enabled: fn(&PdfTab) -> bool,
    /// The action for this tab.
    pub action: fn(TabId) -> Action,
}

fn tab_has_selection(tab: &PdfTab) -> bool {
    tab.ui.has_selection()
}

/// The view has a selection and translation is set up for it (the window sets it up for PDF
/// tabs only, not for the sides of a comparison).
fn tab_can_translate(tab: &PdfTab) -> bool {
    tab.ui.has_selection() && tab.ui.translate.env.is_some()
}

/// The selection menu, in display order.
pub static SELECTION_MENU: &[SelectionMenuEntry] = &[
    SelectionMenuEntry {
        name_key: "cmd.edit.copy",
        command_id: Some("edit.copy"),
        enabled: tab_has_selection,
        action: Action::CopySelection,
    },
    SelectionMenuEntry {
        name_key: "cmd.edit.search_selection",
        command_id: None,
        enabled: tab_has_selection,
        action: Action::FindSelection,
    },
    SelectionMenuEntry {
        name_key: "cmd.edit.translate",
        command_id: Some("edit.translate_selection"),
        enabled: tab_can_translate,
        action: Action::TranslateSelection,
    },
];

/// The command with this identifier.
pub fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.id == id)
}

/// The zoom factors, in percent, offered by the zoom menu of the status bar (VIEW-11).
pub const ZOOM_PRESETS: &[u32] = &[50, 75, 100, 125, 150, 200, 400];

/// The command of the table that sets exactly this zoom preset, if there is one; the zoom menu
/// runs it and shows its shortcut.
pub fn preset_command(percent: u32) -> Option<&'static Command> {
    match percent {
        100 => command("view.zoom_reset"),
        _ => None,
    }
}

/// Choosing a zoom preset (in percent) for the focused view.
pub fn zoom_preset(state: &AppState, percent: u32) -> Option<Action> {
    if !has_zoom_focus(state) {
        return None;
    }
    set_zoom(state, ZoomMode::Custom(percent as f32 / 100.0))
}

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
    // While the palette is open it owns the keyboard, including the shortcuts that open it.
    if ctx.text_edit_focused() || state.palette.open.is_some() {
        return None;
    }
    let mut bindings: Vec<(KeyboardShortcut, &Command)> = COMMANDS
        .iter()
        .flat_map(|c| c.all_shortcuts().map(move |s| (s, c)))
        .collect();
    bindings.sort_by_key(|(s, _)| std::cmp::Reverse(specificity(s)));

    ctx.input_mut(|input| {
        // Ctrl+C does not arrive as a key press: egui-winit turns it into `Event::Copy`. Take that
        // event only when the copy command can run, so other widgets still see it otherwise.
        if let Some(copy) = command("edit.copy").filter(|c| (c.enabled)(state))
            && let Some(at) = input
                .events
                .iter()
                .position(|e| matches!(e, egui::Event::Copy))
        {
            input.events.remove(at);
            return copy.run(state);
        }
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
    use crate::state::Tab;
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
    fn the_go_menu_lists_back_forward_and_go_to_page_with_their_shortcuts() {
        let go: Vec<&str> = in_menu(MenuGroup::Go).map(|c| c.id).collect();
        assert_eq!(go, vec!["nav.back", "nav.forward", "view.goto_page"]);
        let shortcut = |id| command(id).and_then(|c| c.shortcut);
        assert_eq!(
            shortcut("nav.back"),
            Some(KeyboardShortcut::new(Modifiers::ALT, Key::ArrowLeft))
        );
        assert_eq!(
            shortcut("nav.forward"),
            Some(KeyboardShortcut::new(Modifiers::ALT, Key::ArrowRight))
        );
        assert!(MenuGroup::ALL.contains(&MenuGroup::Go));
        assert!(!in_menu(MenuGroup::View).any(|c| c.id == "view.goto_page"));
    }

    #[test]
    fn back_and_forward_are_possible_only_where_the_history_goes() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let back = command("nav.back").expect("command");
        let forward = command("nav.forward").expect("command");
        // The start page: no history.
        assert!(!(back.enabled)(&state) && !(forward.enabled)(&state));
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/a.pdf")]),
            &ctx,
        );
        let tab = state.focus.expect("focus");
        assert!(!(back.enabled)(&state) && !(forward.enabled)(&state));
        dispatch(
            &mut state,
            Action::Navigate {
                tab,
                to: refdesk_core::Location::new(3, 0.0, 10.0),
                how: crate::action::OpenHow::Here,
            },
            &ctx,
        );
        assert_eq!(back.run(&state), Some(Action::Back(tab)));
        assert!(forward.run(&state).is_none());
        dispatch(&mut state, Action::Back(tab), &ctx);
        assert!(back.run(&state).is_none());
        assert_eq!(forward.run(&state), Some(Action::Forward(tab)));
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
    fn keep_tab_open_is_in_the_tab_menu_the_tab_bar_menu_and_the_palette() {
        let tab_menu: Vec<&str> = in_menu(MenuGroup::Tab).map(|c| c.id).collect();
        assert!(tab_menu.contains(&"tab.pin"));
        let pin = command("tab.pin").expect("command");
        assert_eq!(pin.shortcut, None);
        assert!(TAB_MENU.iter().any(|e| e.command_id == Some("tab.pin")));
        // The palette lists every command of the table.
        let state = AppState::new();
        let items = crate::overlays::palette::command_items(&state, Lang::En, "keep tab", &[]);
        assert_eq!(items.first().map(|i| i.command.id), Some("tab.pin"));
        assert!(!items[0].enabled, "no preview tab to keep");
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
                "cmd.tab.pin",
                "cmd.tab.split_right",
                "cmd.tab.split_down",
                "cmd.window.detach_tab",
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
        assert_eq!(enabled, vec![true, false, true, false, true, true, true]);
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
    fn diff_commands_are_in_the_view_menu_with_their_shortcuts() {
        let menu: Vec<&str> = in_menu(MenuGroup::View).map(|c| c.id).collect();
        for (id, shortcut) in [
            ("diff.start", None),
            (
                "diff.toggle_changes",
                Some(KeyboardShortcut::new(Modifiers::NONE, Key::F8)),
            ),
            (
                "diff.next_change",
                Some(KeyboardShortcut::new(Modifiers::NONE, Key::F7)),
            ),
            (
                "diff.prev_change",
                Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::F7)),
            ),
        ] {
            assert!(menu.contains(&id), "{id} is not in the View menu");
            assert_eq!(command(id).expect("command").shortcut, shortcut, "{id}");
        }
        // Without a comparison tab there is no change to move to.
        let state = AppState::new();
        assert!(
            command("diff.next_change")
                .expect("command")
                .run(&state)
                .is_none()
        );
        assert!(
            command("diff.prev_change")
                .expect("command")
                .run(&state)
                .is_none()
        );
        assert_eq!(
            command("diff.start").expect("command").run(&state),
            Some(Action::ShowDiffDialog)
        );
    }

    #[test]
    fn zoom_commands_apply_to_a_comparison_tab() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(
            &mut state,
            Action::StartDiff {
                left: "/refdesk-test/a.pdf".into(),
                right: "/refdesk-test/b.pdf".into(),
            },
            &ctx,
        );
        let tab = state.focus.expect("comparison");
        assert_eq!(
            command("view.zoom_reset").expect("command").run(&state),
            Some(Action::SetZoom {
                tab,
                zoom: ZoomMode::Custom(1.0)
            })
        );
        assert!(command("tab.close").expect("command").run(&state).is_some());
        assert!(
            command("tab.split_right")
                .expect("command")
                .run(&state)
                .is_none()
        );
    }

    #[test]
    fn window_commands_are_in_the_window_menu_with_their_shortcuts() {
        let menu: Vec<&str> = in_menu(MenuGroup::Window).map(|c| c.id).collect();
        assert_eq!(
            menu,
            vec![
                "window.new",
                "window.detach_tab",
                "window.merge_all",
                "window.close"
            ]
        );
        let find = |id: &str| COMMANDS.iter().find(|c| c.id == id).expect("command");
        assert_eq!(
            find("window.new").shortcut,
            Some(KeyboardShortcut::new(CTRL_SHIFT, Key::N))
        );
        assert_eq!(
            find("window.detach_tab").shortcut,
            Some(KeyboardShortcut::new(CTRL_ALT, Key::N))
        );
        assert_eq!(find("window.merge_all").shortcut, None);
        assert_eq!(
            find("window.close").shortcut,
            Some(KeyboardShortcut::new(CTRL_SHIFT, Key::W))
        );
    }

    #[test]
    fn window_command_availability_and_actions() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let find = |id: &str| COMMANDS.iter().find(|c| c.id == id).expect("command");
        let main = state.main_window().id;
        assert_eq!(find("window.new").run(&state), Some(Action::NewWindow));
        assert_eq!(find("window.detach_tab").run(&state), None);
        assert_eq!(find("window.merge_all").run(&state), None);
        assert_eq!(
            find("window.close").run(&state),
            Some(Action::CloseWindow(main))
        );

        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/a.pdf")]),
            &ctx,
        );
        let (a, _) = state.focused_pdf().expect("pdf tab");
        assert_eq!(
            find("window.detach_tab").run(&state),
            Some(Action::DetachTab(a))
        );
        dispatch(&mut state, Action::DetachTab(a), &ctx);
        let second = state.windows[1].id;
        assert_eq!(
            find("window.merge_all").run(&state),
            Some(Action::MergeAllWindows)
        );
        // Close Window acts on the window of the focused tab.
        assert_eq!(
            find("window.close").run(&state),
            Some(Action::CloseWindow(second))
        );
    }

    #[test]
    fn window_shortcuts_are_told_apart_from_tab_shortcuts() {
        let ctx = egui::Context::default();
        let state = state_with_tabs(&ctx, 2);
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        let main = state.main_window().id;
        assert_eq!(
            press(&ctx, &state, CTRL_SHIFT, Key::W),
            Some(Action::CloseWindow(main))
        );
        assert_eq!(
            press(&ctx, &state, Modifiers::CTRL, Key::W),
            Some(Action::CloseTab(tab))
        );
        assert_eq!(
            press(&ctx, &state, CTRL_SHIFT, Key::N),
            Some(Action::NewWindow)
        );
        assert_eq!(
            press(&ctx, &state, CTRL_ALT, Key::N),
            Some(Action::DetachTab(tab))
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
    fn workspace_and_sidebar_commands_are_in_their_menus_with_their_shortcuts() {
        let file: Vec<&str> = in_menu(MenuGroup::File).map(|c| c.id).collect();
        let view: Vec<&str> = in_menu(MenuGroup::View).map(|c| c.id).collect();
        // Next to the commands they belong with.
        assert_eq!(
            file[..3],
            ["file.open", "file.open_folder", "file.close_folder"]
        );
        let toc = view
            .iter()
            .position(|id| *id == "view.toggle_toc")
            .expect("toggle_toc");
        assert_eq!(
            view[toc..toc + 5],
            [
                "view.toggle_toc",
                "view.show_explorer",
                "view.toggle_explorer_section",
                "view.toggle_outline_section",
                "view.refresh_explorer",
            ]
        );
        let shortcut = |id: &str| command(id).and_then(|c| c.shortcut);
        assert_eq!(
            shortcut("file.open_folder"),
            Some(KeyboardShortcut::new(CTRL_SHIFT, Key::O))
        );
        assert_eq!(
            shortcut("view.show_explorer"),
            Some(KeyboardShortcut::new(CTRL_SHIFT, Key::E))
        );
        assert_eq!(
            shortcut("view.toggle_toc"),
            Some(KeyboardShortcut::new(Modifiers::NONE, Key::F9))
        );
    }

    #[test]
    fn workspace_commands_need_a_workspace_and_section_commands_name_the_focused_window() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let run = |state: &AppState, id: &str| command(id).and_then(|c| c.run(state));
        assert_eq!(run(&state, "file.close_folder"), None);
        assert_eq!(run(&state, "view.refresh_explorer"), None);
        assert_eq!(
            run(&state, "file.open_folder"),
            Some(Action::OpenFolderDialog)
        );
        assert_eq!(
            run(&state, "view.show_explorer"),
            Some(Action::ShowExplorer)
        );
        assert_eq!(
            run(&state, "view.toggle_outline_section"),
            Some(Action::ToggleSidebarSection {
                window: state.main_window().id,
                section: SidebarSection::Outline
            })
        );
        dispatch(&mut state, Action::NewWindow, &ctx);
        assert_eq!(
            run(&state, "view.toggle_explorer_section"),
            Some(Action::ToggleSidebarSection {
                window: state.windows[1].id,
                section: SidebarSection::Explorer
            })
        );
        dispatch(
            &mut state,
            Action::OpenFolder(std::path::PathBuf::from("/refdesk-test/wp71-folder")),
            &ctx,
        );
        assert_eq!(run(&state, "file.close_folder"), Some(Action::CloseFolder));
        assert_eq!(
            run(&state, "view.refresh_explorer"),
            Some(Action::RefreshWorkspace)
        );
    }

    #[test]
    fn ctrl_shift_o_opens_a_folder_and_ctrl_o_a_file() {
        let ctx = egui::Context::default();
        let state = AppState::new();
        assert_eq!(
            press(&ctx, &state, CTRL_SHIFT, Key::O),
            Some(Action::OpenFolderDialog)
        );
        assert_eq!(
            press(&ctx, &state, Modifiers::CTRL, Key::O),
            Some(Action::OpenFileDialog)
        );
        assert_eq!(
            press(&ctx, &state, CTRL_SHIFT, Key::E),
            Some(Action::ShowExplorer)
        );
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

    #[test]
    fn the_toc_command_is_f9_in_the_view_menu_and_always_possible() {
        let view_menu: Vec<&str> = in_menu(MenuGroup::View).map(|c| c.id).collect();
        assert!(view_menu.contains(&"view.toggle_toc"));
        let command = COMMANDS
            .iter()
            .find(|c| c.id == "view.toggle_toc")
            .expect("command");
        assert_eq!(
            command.shortcut,
            Some(KeyboardShortcut::new(Modifiers::NONE, Key::F9))
        );
        // Also on the start page, where the panel shows a hint.
        let ctx = egui::Context::default();
        let state = AppState::new();
        assert_eq!(command.run(&state), Some(Action::ToggleToc));
        assert_eq!(
            press(&ctx, &state, Modifiers::NONE, Key::F9),
            Some(Action::ToggleToc)
        );
        for lang in [Lang::Ko, Lang::En] {
            assert!(lookup(lang, command.name_key).is_some());
        }
    }

    #[test]
    fn the_section_path_command_is_in_the_view_menu_and_toggles_the_line() {
        let command = COMMANDS
            .iter()
            .find(|c| c.id == "view.toggle_breadcrumb")
            .expect("command");
        assert_eq!(command.menu, MenuGroup::View);
        assert!(in_menu(MenuGroup::View).any(|c| c.id == command.id));
        let state = AppState::new();
        assert_eq!(command.run(&state), Some(Action::ToggleBreadcrumb));
        assert!(state.show_breadcrumb, "on by default");
        for lang in [Lang::Ko, Lang::En] {
            assert!(lookup(lang, command.name_key).is_some());
        }
        assert_eq!(
            lookup(Lang::Ko, command.name_key),
            Some("절 경로 표시 켜기/끄기")
        );
    }

    #[test]
    fn the_scroll_speed_commands_are_in_the_view_menu_without_shortcuts() {
        for id in ["view.scroll_faster", "view.scroll_slower"] {
            let command = command(id).expect("command");
            assert_eq!(command.menu, MenuGroup::View);
            assert!(in_menu(MenuGroup::View).any(|c| c.id == id));
            assert_eq!(command.all_shortcuts().count(), 0, "{id}");
            for lang in [Lang::Ko, Lang::En] {
                assert!(lookup(lang, command.name_key).is_some());
            }
        }
        assert_eq!(
            lookup(Lang::Ko, "cmd.view.scroll_faster"),
            Some("스크롤 속도 높이기")
        );
        assert_eq!(
            lookup(Lang::Ko, "cmd.view.scroll_slower"),
            Some("스크롤 속도 낮추기")
        );
    }

    #[test]
    fn the_scroll_speed_commands_step_through_the_list() {
        let mut state = AppState::new();
        let faster = find("view.scroll_faster");
        let slower = find("view.scroll_slower");
        // 2.5 is the default.
        assert_eq!(faster.run(&state), Some(Action::SetScrollSpeed(Some(3.0))));
        assert_eq!(slower.run(&state), Some(Action::SetScrollSpeed(Some(2.0))));
        state.prefs.scroll_speed = Some(8.0);
        assert_eq!(faster.run(&state), Some(Action::SetScrollSpeed(Some(8.0))));
        state.prefs.scroll_speed = Some(0.5);
        assert_eq!(slower.run(&state), Some(Action::SetScrollSpeed(Some(0.5))));
        state.prefs.scroll_speed = Some(2.2);
        assert_eq!(faster.run(&state), Some(Action::SetScrollSpeed(Some(2.5))));
        assert_eq!(slower.run(&state), Some(Action::SetScrollSpeed(Some(2.0))));
    }

    #[test]
    fn the_scroll_speed_commands_are_in_the_palette_list() {
        // The palette lists every row of the table.
        for id in ["view.scroll_faster", "view.scroll_slower"] {
            assert!(COMMANDS.iter().any(|c| c.id == id));
        }
    }

    fn find(id: &str) -> &'static Command {
        command(id).expect("command")
    }

    /// Selects something in the view of the focused tab (the text itself is not needed).
    fn select_in_focused_tab(state: &mut AppState) -> TabId {
        use refdesk_core::{Selection, TextPos};
        let tab = state.focus.expect("a focused tab");
        let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(tab) else {
            panic!("a pdf tab");
        };
        pdf.ui.selection.set_for_test(Selection {
            anchor: TextPos { page: 0, ch: 2 },
            focus: TextPos { page: 0, ch: 9 },
        });
        tab
    }

    fn copy_event_frame(ctx: &egui::Context, state: &AppState) -> (Option<Action>, bool) {
        let mut out = None;
        let mut left = true;
        let input = egui::RawInput {
            events: vec![egui::Event::Copy],
            ..Default::default()
        };
        run_frame(ctx, input, |ui| {
            out = handle_shortcuts(ui.ctx(), state);
            left = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        });
        (out, left)
    }

    #[test]
    fn copy_is_in_the_edit_menu_with_ctrl_c() {
        let copy = find("edit.copy");
        assert_eq!(copy.menu, MenuGroup::Edit);
        assert_eq!(copy.shortcut, Some(KeyboardShortcut::new(CTRL, Key::C)));
        assert!(in_menu(MenuGroup::Edit).any(|c| c.id == "edit.copy"));
        assert!(MenuGroup::ALL.contains(&MenuGroup::Edit));
    }

    #[test]
    fn copy_is_disabled_without_a_selection_and_enabled_with_one() {
        let ctx = egui::Context::default();
        let mut state = state_with_tabs(&ctx, 1);
        let copy = find("edit.copy");
        assert!(!(copy.enabled)(&state), "a view without a selection");
        assert_eq!(copy.run(&state), None);
        let start = AppState::new();
        assert!(!(copy.enabled)(&start), "the start page");

        let tab = select_in_focused_tab(&mut state);
        assert!((copy.enabled)(&state));
        assert_eq!(copy.run(&state), Some(Action::CopySelection(tab)));
    }

    #[test]
    fn ctrl_c_runs_copy_as_a_key_press_and_as_the_copy_event_of_egui_winit() {
        let ctx = egui::Context::default();
        let mut state = state_with_tabs(&ctx, 1);
        assert_eq!(press(&ctx, &state, Modifiers::CTRL, Key::C), None);
        // Without a selection the event is left for other widgets.
        assert_eq!(copy_event_frame(&ctx, &state), (None, true));

        let tab = select_in_focused_tab(&mut state);
        assert_eq!(
            press(&ctx, &state, Modifiers::CTRL, Key::C),
            Some(Action::CopySelection(tab))
        );
        // egui-winit reports Ctrl+C as `Event::Copy`; it is taken when the command can run.
        assert_eq!(
            copy_event_frame(&ctx, &state),
            (Some(Action::CopySelection(tab)), false)
        );
    }

    #[test]
    fn copy_event_is_left_alone_while_a_text_field_has_focus() {
        let ctx = egui::Context::default();
        let mut state = state_with_tabs(&ctx, 1);
        select_in_focused_tab(&mut state);
        let mut text = String::new();
        run_frame(&ctx, egui::RawInput::default(), |ui| {
            ui.text_edit_singleline(&mut text).request_focus();
        });
        run_frame(&ctx, egui::RawInput::default(), |ui| {
            ui.text_edit_singleline(&mut text);
        });
        assert!(ctx.text_edit_focused());
        assert_eq!(copy_event_frame(&ctx, &state), (None, true));
    }

    #[test]
    fn selection_menu_entries_have_texts_known_commands_and_follow_the_selection() {
        assert!(!SELECTION_MENU.is_empty());
        for entry in SELECTION_MENU {
            for lang in [Lang::Ko, Lang::En] {
                assert!(lookup(lang, entry.name_key).is_some(), "{}", entry.name_key);
            }
            if let Some(id) = entry.command_id {
                assert!(command(id).is_some(), "unknown command {id}");
            }
        }
        let copy = SELECTION_MENU
            .iter()
            .find(|e| e.command_id == Some("edit.copy"))
            .expect("copy entry");
        let ctx = egui::Context::default();
        let mut state = state_with_tabs(&ctx, 1);
        let tab = state.focus.expect("tab");
        let pdf = |s: &AppState| s.tabs.pdf(tab).cloned().expect("pdf tab");
        assert!(!(copy.enabled)(&pdf(&state)));
        select_in_focused_tab(&mut state);
        assert!((copy.enabled)(&pdf(&state)));
        assert_eq!((copy.action)(tab), Action::CopySelection(tab));
    }
}
