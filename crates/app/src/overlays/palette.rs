//! The command palette (Ctrl+Shift+P, CMD-1) and quick open (Ctrl+P, CMD-2).
//!
//! One overlay with two lists. Which list, what is typed and which row is selected live in
//! [`PaletteState`] on the `AppState`. Everything that decides something is a plain function over
//! the state and tested without a window: matching and ranking ([`command_items`],
//! [`quick_items`]), moving the selection ([`move_selection`]), opening and closing
//! ([`toggle`]) and choosing a row ([`choose`]). [`show`] only reads the keys, draws and
//! calls those.
//!
//! The palette runs nothing itself: choosing a command queues what the command table says
//! ([`Command::run`]), choosing a tab queues `FocusTab`, choosing a recent document queues
//! `OpenFiles`. Structure changes therefore still happen only in `dispatch` (rule R-6).

use std::cmp::Reverse;
use std::path::{Path, PathBuf};

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align2, Color32, Context, CornerRadius, Id, Key, KeyboardShortcut, Modifiers, Response,
    ScrollArea, Sense, Stroke, TextEdit, TextStyle, Ui, vec2,
};

use crate::action::{Action, OpenTarget};
use crate::commands::{self, COMMANDS, Command};
use crate::i18n::{Lang, current_lang, lookup, tr};
use crate::overlays::fuzzy::{fuzzy_match, highlight_ranges};
use crate::state::{AppState, TabId, WindowId};
use crate::windows::tab_title;

/// Identifier of the command that opens the command palette.
pub const COMMANDS_COMMAND_ID: &str = "palette.commands";
/// Identifier of the command that opens quick open.
pub const QUICK_OPEN_COMMAND_ID: &str = "palette.quick_open";

/// How many executed commands are remembered for the empty-query order.
pub const MAX_RECENT_COMMANDS: usize = 10;
/// How many recent documents quick open lists at most.
pub const MAX_RECENT_DOCS: usize = 100;
/// How many rows PgUp / PgDn move.
pub const PAGE_ROWS: usize = 8;
/// How many focused tabs are remembered.
const MAX_FOCUS_HISTORY: usize = 200;
/// A match on the id of a command counts for less than one on its name.
const ID_PENALTY: i32 = 10;
/// A match on the file name counts for more than one on the rest of the path.
const NAME_BONUS: i32 = 30;

/// Which list the palette shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteMode {
    /// All commands of the command table.
    Commands,
    /// Open tabs and recent documents.
    QuickOpen,
}

/// The palette while it is shown.
#[derive(Clone, Debug)]
pub struct OpenPalette {
    /// Which list it shows.
    pub mode: PaletteMode,
    /// The window it is drawn in.
    pub window: WindowId,
    /// What the user typed.
    pub query: String,
    /// Index of the selected row in the filtered list.
    pub selected: usize,
    /// Set when the selection moved by key: the list scrolls it into view once.
    scroll_to_selected: bool,
}

/// The palette state kept in `AppState`.
#[derive(Debug, Default)]
pub struct PaletteState {
    /// `Some` while the palette is shown.
    pub open: Option<OpenPalette>,
    /// Identifiers of the commands run from the palette, the latest first. Not saved.
    recent_commands: Vec<&'static str>,
    /// The tabs that had the focus, the latest first. Not saved.
    focus_history: Vec<TabId>,
}

impl PaletteState {
    /// Notes which tab has the focus now (for the order of quick open).
    pub fn note_focus(&mut self, focus: Option<TabId>) {
        let Some(tab) = focus else {
            return;
        };
        if self.focus_history.first() == Some(&tab) {
            return;
        }
        self.focus_history.retain(|t| *t != tab);
        self.focus_history.insert(0, tab);
        self.focus_history.truncate(MAX_FOCUS_HISTORY);
    }

    /// Notes that a command was run from the palette.
    fn note_command(&mut self, id: &'static str) {
        self.recent_commands.retain(|c| *c != id);
        self.recent_commands.insert(0, id);
        self.recent_commands.truncate(MAX_RECENT_COMMANDS);
    }

    /// Commands run from the palette, the latest first.
    pub fn recent_commands(&self) -> &[&'static str] {
        &self.recent_commands
    }
}

/// Where quick open takes the recent documents from. Called with the state each time the list is
/// built, so it should be cheap (no file access).
pub type RecentDocuments<'a> = &'a dyn Fn(&AppState) -> Vec<PathBuf>;

/// The recent-documents source until the session work provides one: there are none.
///
/// To attach the real list, pass a function that returns the paths of the recently opened
/// documents, most recent first, in `windows::show_window` where this one is passed.
pub fn no_recent_documents(_: &AppState) -> Vec<PathBuf> {
    Vec::new()
}

// ------------------------------------------------------------------------------------------------
// Opening and closing
// ------------------------------------------------------------------------------------------------

/// What the commands that open the palette do (`Action::ShowPalette`): open it on the focused
/// window, or close it when it already shows this list, or switch it to this list.
pub fn toggle(state: &mut AppState, mode: PaletteMode, ctx: &Context) {
    match &mut state.palette.open {
        Some(open) if open.mode == mode => close(state, ctx),
        Some(open) => {
            open.mode = mode;
            open.query.clear();
            open.selected = 0;
            open.scroll_to_selected = true;
        }
        None => {
            let window = state.focused_window().id;
            state.palette.open = Some(OpenPalette {
                mode,
                window,
                query: String::new(),
                selected: 0,
                scroll_to_selected: true,
            });
        }
    }
}

/// Closes the palette and lets go of the keyboard: the text field of the palette is not drawn any
/// more, but egui would keep it focused for one more frame and the shortcuts would not work.
fn close(state: &mut AppState, ctx: &Context) {
    state.palette.open = None;
    ctx.memory_mut(|m| m.stop_text_input());
}

/// Keeps the palette state in step with the rest of the state; `dispatch` calls it after every
/// action. Remembers the focus, and closes a palette whose window is gone.
pub fn after_dispatch(state: &mut AppState) {
    state.palette.note_focus(state.focus);
    let gone = state
        .palette
        .open
        .as_ref()
        .is_some_and(|open| !state.windows.iter().any(|w| w.id == open.window));
    if gone {
        state.palette.open = None;
    }
}

// ------------------------------------------------------------------------------------------------
// Selection
// ------------------------------------------------------------------------------------------------

/// A movement of the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// One row up.
    Up,
    /// One row down.
    Down,
    /// [`PAGE_ROWS`] rows up.
    PageUp,
    /// [`PAGE_ROWS`] rows down.
    PageDown,
    /// The first row.
    Home,
    /// The last row.
    End,
}

/// The selection after a step in a list of `len` rows. It stops at both ends; it does not wrap.
pub fn move_selection(selected: usize, len: usize, step: Step) -> usize {
    if len == 0 {
        return 0;
    }
    let last = len - 1;
    let current = selected.min(last);
    match step {
        Step::Up => current.saturating_sub(1),
        Step::Down => (current + 1).min(last),
        Step::PageUp => current.saturating_sub(PAGE_ROWS),
        Step::PageDown => (current + PAGE_ROWS).min(last),
        Step::Home => 0,
        Step::End => last,
    }
}

/// The selection brought inside a list of `len` rows (after the list got shorter).
pub fn clamp_selection(selected: usize, len: usize) -> usize {
    selected.min(len.saturating_sub(1))
}

// ------------------------------------------------------------------------------------------------
// The lists
// ------------------------------------------------------------------------------------------------

/// One row of the command list.
pub struct CommandItem {
    /// The row of the command table.
    pub command: &'static Command,
    /// The name in the language of the list.
    pub name: String,
    /// The name of the menu the command is in, in the same language.
    pub menu: String,
    /// Whether the command can run now; disabled ones are shown dimmed.
    pub enabled: bool,
    /// Characters of `name` that matched the query (indices into `name.chars()`).
    pub name_positions: Vec<usize>,
    /// How well the row matches; the list is sorted by it.
    pub score: i32,
}

fn text(lang: Lang, key: &'static str) -> &'static str {
    lookup(lang, key).unwrap_or(key)
}

/// Every command of the table that matches `query`, best first. The query is matched against the
/// name in `lang`, the English name and the identifier, so `zoom` finds "확대" in the Korean UI.
/// With an empty query the commands run most recently (`recent`, latest first) come first and the
/// others follow in table order; otherwise equal scores keep table order.
pub fn command_items(
    state: &AppState,
    lang: Lang,
    query: &str,
    recent: &[&'static str],
) -> Vec<CommandItem> {
    let mut items: Vec<CommandItem> = COMMANDS
        .iter()
        .filter_map(|command| {
            let name = text(lang, command.name_key);
            let english = text(Lang::En, command.name_key);
            let on_name = fuzzy_match(query, name);
            let scores = [
                on_name.as_ref().map(|m| m.score),
                fuzzy_match(query, english).map(|m| m.score),
                fuzzy_match(query, command.id).map(|m| m.score - ID_PENALTY),
            ];
            let score = scores.into_iter().flatten().max()?;
            Some(CommandItem {
                command,
                name: name.to_owned(),
                menu: text(lang, command.menu.name_key()).to_owned(),
                enabled: (command.enabled)(state),
                name_positions: on_name.map(|m| m.positions).unwrap_or_default(),
                score,
            })
        })
        .collect();
    if query.trim().is_empty() {
        // Stable: commands that were not run recently stay in table order.
        items.sort_by_key(|item| {
            recent
                .iter()
                .position(|id| *id == item.command.id)
                .unwrap_or(usize::MAX)
        });
    } else {
        items.sort_by_key(|item| Reverse(item.score));
    }
    items
}

/// What a row of quick open stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuickTarget {
    /// A tab that is open.
    Tab(TabId),
    /// A document that is not open; choosing it opens it.
    Recent(PathBuf),
}

/// One row of quick open.
#[derive(Clone, Debug)]
pub struct QuickItem {
    /// What choosing the row does.
    pub target: QuickTarget,
    /// The file name (the tab title for a tab).
    pub name: String,
    /// The full path.
    pub path: String,
    /// Index in `AppState::windows` of the window holding the tab; `None` for a recent document.
    pub window: Option<usize>,
    /// Characters of `name` that matched (indices into `name.chars()`).
    pub name_positions: Vec<usize>,
    /// Characters of `path` that matched.
    pub path_positions: Vec<usize>,
    /// How well the row matches; the list is sorted by it.
    pub score: i32,
}

/// The PDF tabs in the order of an empty query: those that had the focus most recently first, then
/// the ones that never had it (window order), and the tab with the focus now last. With the
/// focus on tab B and A before it, the top row is A: Ctrl+P, Enter switches back and forth.
fn tabs_by_recent_focus(state: &AppState) -> Vec<(usize, TabId)> {
    let all: Vec<(usize, TabId)> = state
        .windows
        .iter()
        .enumerate()
        .flat_map(|(w, window)| window.dock.tabs().into_iter().map(move |t| (w, t)))
        .filter(|(_, t)| state.tabs.pdf(*t).is_some())
        .collect();
    let rank = |tab: TabId| state.palette.focus_history.iter().position(|t| *t == tab);
    let mut ordered: Vec<(usize, TabId)> = all
        .iter()
        .copied()
        .filter(|(_, t)| Some(*t) != state.focus)
        .collect();
    // Stable: tabs without a place in the history keep window order, after the others.
    ordered.sort_by_key(|(_, t)| rank(*t).unwrap_or(usize::MAX));
    ordered.extend(all.into_iter().filter(|(_, t)| Some(*t) == state.focus));
    ordered
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Scores one row. The file name counts for more than the rest of the path.
fn quick_item(
    target: QuickTarget,
    window: Option<usize>,
    name: String,
    path: String,
    query: &str,
) -> Option<QuickItem> {
    let on_name = fuzzy_match(query, &name);
    let on_path = fuzzy_match(query, &path);
    let score = [
        on_name.as_ref().map(|m| m.score + NAME_BONUS),
        on_path.as_ref().map(|m| m.score),
    ]
    .into_iter()
    .flatten()
    .max()?;
    Some(QuickItem {
        target,
        name,
        path,
        window,
        name_positions: on_name.map(|m| m.positions).unwrap_or_default(),
        path_positions: on_path.map(|m| m.positions).unwrap_or_default(),
        score,
    })
}

/// The rows of quick open that match `query`: the open tabs, then the recent documents that are
/// not open. With an empty query the order is "latest focus first, current tab last" (see
/// [`tabs_by_recent_focus`]) and `recent` as given; otherwise best score first with equal scores in
/// that same order. At most [`MAX_RECENT_DOCS`] recent documents are listed.
pub fn quick_items(state: &AppState, query: &str, recent: &[PathBuf]) -> Vec<QuickItem> {
    let mut items: Vec<QuickItem> = Vec::new();
    for (window, tab) in tabs_by_recent_focus(state) {
        let path = state
            .tabs
            .pdf(tab)
            .and_then(|pdf| state.docs.get(pdf.doc))
            .map(|doc| doc.path().display().to_string())
            .unwrap_or_default();
        items.extend(quick_item(
            QuickTarget::Tab(tab),
            Some(window),
            tab_title(state, tab),
            path,
            query,
        ));
    }
    let open_paths: Vec<&Path> = state
        .tabs
        .iter()
        .filter_map(|(_, tab)| match tab {
            crate::state::Tab::Pdf(pdf) => state.docs.get(pdf.doc).map(|d| d.path()),
            crate::state::Tab::Start => None,
        })
        .collect();
    for path in recent
        .iter()
        .filter(|p| !open_paths.contains(&p.as_path()))
        .take(MAX_RECENT_DOCS)
    {
        items.extend(quick_item(
            QuickTarget::Recent(path.clone()),
            None,
            file_name_of(path),
            path.display().to_string(),
            query,
        ));
    }
    if !query.trim().is_empty() {
        items.sort_by_key(|item| Reverse(item.score));
    }
    items
}

/// The rows of whichever list is shown.
enum Items {
    Commands(Vec<CommandItem>),
    Quick(Vec<QuickItem>),
}

impl Items {
    fn len(&self) -> usize {
        match self {
            Items::Commands(items) => items.len(),
            Items::Quick(items) => items.len(),
        }
    }
}

fn build_items(
    state: &AppState,
    lang: Lang,
    mode: PaletteMode,
    query: &str,
    recent: RecentDocuments<'_>,
) -> Items {
    match mode {
        PaletteMode::Commands => Items::Commands(command_items(
            state,
            lang,
            query,
            state.palette.recent_commands(),
        )),
        PaletteMode::QuickOpen => Items::Quick(quick_items(state, query, &recent(state))),
    }
}

// ------------------------------------------------------------------------------------------------
// Choosing
// ------------------------------------------------------------------------------------------------

/// Runs row `index` of the list the palette shows: closes the palette and queues the row's action.
/// Returns false, leaving the palette open, when there is no such row or the command cannot run
/// now.
pub fn choose(
    state: &mut AppState,
    ctx: &Context,
    lang: Lang,
    recent: RecentDocuments<'_>,
    index: usize,
    queue: &mut Vec<Action>,
) -> bool {
    let Some(open) = &state.palette.open else {
        return false;
    };
    let items = build_items(state, lang, open.mode, &open.query, recent);
    let action = match items {
        Items::Commands(items) => {
            let Some(item) = items.get(index) else {
                return false;
            };
            if !item.enabled {
                return false;
            }
            state.palette.note_command(item.command.id);
            item.command.run(state)
        }
        Items::Quick(items) => match items.get(index).map(|item| &item.target) {
            None => return false,
            Some(QuickTarget::Tab(tab)) => Some(Action::FocusTab(*tab)),
            Some(QuickTarget::Recent(path)) => Some(Action::OpenFiles(vec![OpenTarget {
                path: path.clone(),
                page: None,
            }])),
        },
    };
    close(state, ctx);
    queue.extend(action);
    true
}

// ------------------------------------------------------------------------------------------------
// Keys and drawing
// ------------------------------------------------------------------------------------------------

/// The keys of one frame that the palette reacts to.
#[derive(Default)]
struct Keys {
    /// A shortcut that opens a palette: the same one closes it, the other switches to its list.
    toggle: Option<PaletteMode>,
    close: bool,
    run: bool,
    steps: Vec<Step>,
}

/// Takes the palette's keys out of this frame's input, so the text field and the views behind do
/// not see them. Home and End move the selection here, not the text cursor.
fn read_keys(ctx: &Context) -> Keys {
    let mut bindings: Vec<(KeyboardShortcut, PaletteMode)> = [
        (COMMANDS_COMMAND_ID, PaletteMode::Commands),
        (QUICK_OPEN_COMMAND_ID, PaletteMode::QuickOpen),
    ]
    .into_iter()
    .filter_map(|(id, mode)| commands::command(id).map(|c| (c, mode)))
    .flat_map(|(c, mode)| c.all_shortcuts().map(move |s| (s, mode)))
    .collect();
    // `consume_shortcut` ignores an extra Shift: Ctrl+Shift+P must be asked before Ctrl+P.
    bindings.sort_by_key(|(s, _)| Reverse(u8::from(s.modifiers.shift) + u8::from(s.modifiers.alt)));

    ctx.input_mut(|input| {
        let mut keys = Keys::default();
        for (shortcut, mode) in &bindings {
            if input.consume_shortcut(shortcut) {
                keys.toggle = Some(*mode);
                break;
            }
        }
        keys.close = input.consume_key(Modifiers::NONE, Key::Escape);
        keys.run = input.consume_key(Modifiers::NONE, Key::Enter);
        for (key, step) in [
            (Key::ArrowUp, Step::Up),
            (Key::ArrowDown, Step::Down),
            (Key::PageUp, Step::PageUp),
            (Key::PageDown, Step::PageDown),
            (Key::Home, Step::Home),
            (Key::End, Step::End),
        ] {
            if input.consume_key(Modifiers::NONE, key) {
                keys.steps.push(step);
            }
        }
        keys
    })
}

const ROW_HEIGHT: f32 = 26.0;
const ROW_PADDING: f32 = 8.0;
const PALETTE_WIDTH: f32 = 600.0;
const LIST_MAX_HEIGHT: f32 = 360.0;
const TOP_OFFSET: f32 = 70.0;

/// A row ready to draw.
struct Row {
    name: String,
    name_positions: Vec<usize>,
    /// Dimmed text after the name (the path in quick open) and its matched characters.
    detail: Option<(String, Vec<usize>)>,
    /// Dimmed text at the right edge.
    right: String,
    enabled: bool,
}

fn rows(ctx: &Context, state: &AppState, items: &Items) -> Vec<Row> {
    match items {
        Items::Commands(items) => items
            .iter()
            .map(|item| {
                let right = match item.command.shortcut {
                    Some(shortcut) => format!("{}  {}", item.menu, ctx.format_shortcut(&shortcut)),
                    None => item.menu.clone(),
                };
                Row {
                    name: item.name.clone(),
                    name_positions: item.name_positions.clone(),
                    detail: None,
                    right,
                    enabled: item.enabled,
                }
            })
            .collect(),
        Items::Quick(items) => items
            .iter()
            .map(|item| {
                let right = match (&item.target, item.window) {
                    (QuickTarget::Recent(_), _) => tr("palette.recent").to_owned(),
                    (QuickTarget::Tab(_), Some(w)) if state.windows.len() > 1 => {
                        tr("palette.window_n").replace("{n}", &(w + 1).to_string())
                    }
                    _ => String::new(),
                };
                Row {
                    name: item.name.clone(),
                    name_positions: item.name_positions.clone(),
                    detail: Some((item.path.clone(), item.path_positions.clone())),
                    right,
                    enabled: true,
                }
            })
            .collect(),
    }
}

fn append_highlighted(
    job: &mut LayoutJob,
    text: &str,
    positions: &[usize],
    plain: &TextFormat,
    lit: &TextFormat,
) {
    for (range, highlighted) in highlight_ranges(text, positions) {
        let part = text.get(range).unwrap_or_default();
        job.append(part, 0.0, if highlighted { lit } else { plain }.clone());
    }
}

/// Draws one row and returns its click response.
fn draw_row(ui: &mut Ui, row: &Row, selected: bool, scroll_to_it: bool) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if scroll_to_it {
        response.scroll_to_me(None);
    }
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let visuals = ui.visuals();
    let weak = visuals.weak_text_color();
    let plain_color = if row.enabled {
        visuals.text_color()
    } else {
        weak
    };
    let lit_color = if row.enabled {
        visuals.strong_text_color()
    } else {
        weak
    };
    let underline = Stroke::new(1.0, visuals.hyperlink_color);
    let background = if selected {
        Some(visuals.selection.bg_fill)
    } else if response.hovered() {
        Some(visuals.widgets.hovered.weak_bg_fill)
    } else {
        None
    };
    let font = TextStyle::Body.resolve(ui.style());

    let painter = ui.painter();
    if let Some(fill) = background {
        painter.rect_filled(rect, CornerRadius::same(4), fill);
    }
    let right = painter.layout_no_wrap(row.right.clone(), font.clone(), weak);
    let name_width = (rect.width() - right.size().x - 3.0 * ROW_PADDING).max(40.0);

    let format = |color: Color32, underline: Stroke| TextFormat {
        font_id: font.clone(),
        color,
        underline,
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = name_width;
    job.wrap.max_rows = 1;
    append_highlighted(
        &mut job,
        &row.name,
        &row.name_positions,
        &format(plain_color, Stroke::NONE),
        &format(lit_color, underline),
    );
    if let Some((detail, positions)) = &row.detail {
        job.append("  ", 0.0, format(weak, Stroke::NONE));
        append_highlighted(
            &mut job,
            detail,
            positions,
            &format(weak, Stroke::NONE),
            &format(lit_color, underline),
        );
    }
    let galley = painter.layout_job(job);
    let left_top = rect.left_center() + vec2(ROW_PADDING, -galley.size().y / 2.0);
    painter.galley(left_top, galley, plain_color);
    let right_top =
        rect.right_center() + vec2(-ROW_PADDING - right.size().x, -right.size().y / 2.0);
    painter.galley(right_top, right, weak);
    response
}

/// Draws the palette if it is open on window `window_index`, and handles its keys. The window
/// calls this before it draws its own content, so the palette takes its keys first.
pub fn show(
    ctx: &Context,
    state: &mut AppState,
    window_index: usize,
    queue: &mut Vec<Action>,
    recent: RecentDocuments<'_>,
) {
    let Some(window_id) = state.windows.get(window_index).map(|w| w.id) else {
        return;
    };
    let Some(open) = state
        .palette
        .open
        .as_ref()
        .filter(|o| o.window == window_id)
    else {
        return;
    };
    let lang = current_lang();
    let mode = open.mode;
    let mut query = open.query.clone();
    let mut selected = open.selected;
    let mut scroll_to_selected = open.scroll_to_selected;

    let keys = read_keys(ctx);
    if let Some(mode) = keys.toggle
        && let Some(action) = palette_command_action(state, mode)
    {
        queue.push(action);
    }
    if keys.close {
        close(state, ctx);
        return;
    }
    let mut items = build_items(state, lang, mode, &query, recent);
    for step in &keys.steps {
        selected = move_selection(selected, items.len(), *step);
        scroll_to_selected = true;
    }
    if keys.run {
        let index = clamp_selection(selected, items.len());
        if choose(state, ctx, lang, recent, index, queue) {
            return;
        }
    }

    let id = Id::unique(("palette", window_id));
    let width = PALETTE_WIDTH
        .min(ctx.content_rect().width() - 32.0)
        .max(200.0);
    let modal = egui::Modal::new(id)
        .area(egui::Modal::default_area(id).anchor(Align2::CENTER_TOP, vec2(0.0, TOP_OFFSET)));
    let response = modal.show(ctx, |ui| {
        ui.set_width(width);
        let hint = match mode {
            PaletteMode::Commands => tr("palette.hint_commands"),
            PaletteMode::QuickOpen => tr("palette.hint_quick_open"),
        };
        let edit = ui.add(
            TextEdit::singleline(&mut query)
                .hint_text(hint)
                .desired_width(f32::INFINITY),
        );
        // The text field keeps the keyboard for as long as the palette is open.
        edit.request_focus();
        if edit.changed() {
            selected = 0;
            scroll_to_selected = true;
            items = build_items(state, lang, mode, &query, recent);
        }
        let selected_now = clamp_selection(selected, items.len());
        let rows = rows(ctx, state, &items);
        ui.add_space(4.0);
        let mut clicked = None;
        if rows.is_empty() {
            ui.weak(tr("palette.no_match"));
        }
        ScrollArea::vertical()
            .max_height(LIST_MAX_HEIGHT)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (index, row) in rows.iter().enumerate() {
                    let is_selected = index == selected_now;
                    let response =
                        draw_row(ui, row, is_selected, is_selected && scroll_to_selected);
                    if response.clicked() {
                        clicked = Some(index);
                    }
                }
            });
        clicked
    });

    let clicked = response.inner;
    let outside = response.backdrop_response.clicked();
    // Write the typed text and the selection back before anything reads them.
    if let Some(open) = &mut state.palette.open {
        open.query = query;
        open.selected = clicked.unwrap_or(selected);
        open.scroll_to_selected = false;
    }
    if outside {
        close(state, ctx);
    } else if let Some(index) = clicked {
        choose(state, ctx, lang, recent, index, queue);
    }
}

/// The action of the command that opens the palette in `mode`.
fn palette_command_action(state: &AppState, mode: PaletteMode) -> Option<Action> {
    let id = match mode {
        PaletteMode::Commands => COMMANDS_COMMAND_ID,
        PaletteMode::QuickOpen => QUICK_OPEN_COMMAND_ID,
    };
    commands::command(id)?.run(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{dispatch, dispatch_queued};
    use crate::state::WindowState;

    fn open_pdf(state: &mut AppState, ctx: &Context, path: &str) -> TabId {
        dispatch(state, Action::OpenFiles(vec![OpenTarget::parse(path)]), ctx);
        state.focus.expect("focused tab")
    }

    fn state_with(ctx: &Context, names: &[&str]) -> (AppState, Vec<TabId>) {
        let mut state = AppState::new();
        let tabs = names
            .iter()
            .map(|n| open_pdf(&mut state, ctx, &format!("/refdesk-test/{n}.pdf")))
            .collect();
        (state, tabs)
    }

    fn ids(items: &[CommandItem]) -> Vec<&'static str> {
        items.iter().map(|i| i.command.id).collect()
    }

    fn names(items: &[QuickItem]) -> Vec<&str> {
        items.iter().map(|i| i.name.as_str()).collect()
    }

    fn open_palette(state: &mut AppState, mode: PaletteMode) {
        toggle(state, mode, &Context::default());
        assert!(state.palette.open.is_some());
    }

    fn set_query(state: &mut AppState, query: &str) {
        if let Some(open) = &mut state.palette.open {
            open.query = query.to_owned();
        }
    }

    // ---- matching and ranking of commands ----

    #[test]
    fn korean_english_and_identifier_queries_find_the_same_command() {
        let state = AppState::new();
        for (lang, query) in [
            (Lang::Ko, "분할"),
            (Lang::En, "split"),
            (Lang::Ko, "split"),
            (Lang::En, "tab.split"),
        ] {
            let found = ids(&command_items(&state, lang, query, &[]));
            assert!(found.contains(&"tab.split_right"), "{lang:?} {query}");
            assert!(found.contains(&"tab.split_down"), "{lang:?} {query}");
            assert!(!found.contains(&"app.quit"), "{lang:?} {query}");
        }
        // The identifier works in both languages.
        for lang in [Lang::Ko, Lang::En] {
            let found = ids(&command_items(&state, lang, "view.zoom", &[]));
            for id in ["view.zoom_in", "view.zoom_out", "view.zoom_reset"] {
                assert!(found.contains(&id), "{lang:?} {id}");
            }
        }
    }

    #[test]
    fn an_empty_query_lists_every_command_in_table_order() {
        let state = AppState::new();
        let items = command_items(&state, Lang::En, "", &[]);
        let table: Vec<&str> = COMMANDS.iter().map(|c| c.id).collect();
        assert_eq!(ids(&items), table);
        let items = command_items(&state, Lang::Ko, "  ", &[]);
        assert_eq!(ids(&items), table);
    }

    #[test]
    fn rows_carry_name_menu_and_the_matched_characters() {
        let state = AppState::new();
        let items = command_items(&state, Lang::Ko, "분할", &[]);
        let right = items
            .iter()
            .find(|i| i.command.id == "tab.split_right")
            .expect("row");
        assert_eq!(right.name, "오른쪽으로 분할");
        assert_eq!(right.menu, "탭");
        assert_eq!(right.name_positions, vec![6, 7]);
        let chars: Vec<char> = right.name.chars().collect();
        let lit: String = right.name_positions.iter().map(|i| chars[*i]).collect();
        assert_eq!(lit, "분할");
        // A match through the English name or the id highlights nothing in the Korean name.
        let items = command_items(&state, Lang::Ko, "zoom", &[]);
        assert!(items.iter().all(|i| i.name_positions.is_empty()));
        assert!(!items.is_empty());
        let items = command_items(&state, Lang::En, "zoom", &[]);
        let zoom_in = items
            .iter()
            .find(|i| i.command.id == "view.zoom_in")
            .expect("row");
        assert_eq!(zoom_in.name_positions, vec![0, 1, 2, 3]);
        assert_eq!(zoom_in.menu, "View");
    }

    #[test]
    fn a_word_start_match_ranks_above_a_middle_match() {
        let state = AppState::new();
        let items = command_items(&state, Lang::En, "ct", &[]);
        let found = ids(&items);
        assert_eq!(found.first(), Some(&"tab.close"), "{found:?}");
    }

    #[test]
    fn contiguous_matches_rank_above_scattered_ones_and_ties_keep_table_order() {
        let state = AppState::new();
        let items = command_items(&state, Lang::En, "zoom", &[]);
        // "Zoom In" and "Zoom Out" match identically: table order. "Actual Size" has no zoom.
        let found = ids(&items);
        let pos = |id: &str| found.iter().position(|i| *i == id);
        assert!(pos("view.zoom_in") < pos("view.zoom_out"));
        assert!(pos("view.zoom_in").is_some());
        // Scattered "ptn" is a weaker match than contiguous "pane".
        let scattered = command_items(&state, Lang::En, "fnp", &[]);
        assert!(ids(&scattered).contains(&"tab.focus_next_pane"));
    }

    #[test]
    fn disabled_commands_are_listed_but_have_no_action() {
        let ctx = Context::default();
        let state = AppState::new();
        let items = command_items(&state, Lang::En, "close tab", &[]);
        let close = items
            .iter()
            .find(|i| i.command.id == "tab.close")
            .expect("listed");
        assert!(!close.enabled);
        assert!(close.command.run(&state).is_none());

        let (state, _) = state_with(&ctx, &["a"]);
        let items = command_items(&state, Lang::En, "close tab", &[]);
        let close = items
            .iter()
            .find(|i| i.command.id == "tab.close")
            .expect("listed");
        assert!(close.enabled);
        assert!(close.command.run(&state).is_some());
    }

    #[test]
    fn recently_run_commands_come_first_for_an_empty_query_only() {
        let state = AppState::new();
        let recent = ["app.quit", "file.open"];
        let items = command_items(&state, Lang::En, "", &recent);
        assert_eq!(&ids(&items)[..2], &["app.quit", "file.open"]);
        // The rest keeps table order.
        let rest: Vec<&str> = ids(&items)[2..].to_vec();
        let table: Vec<&str> = COMMANDS
            .iter()
            .map(|c| c.id)
            .filter(|id| !recent.contains(id))
            .collect();
        assert_eq!(rest, table);
        // With a query the score decides.
        let items = command_items(&state, Lang::En, "open", &recent);
        assert_eq!(ids(&items).first(), Some(&"file.open"));
    }

    #[test]
    fn the_commands_run_from_the_palette_are_remembered_latest_first() {
        let mut state = AppState::new();
        for id in ["file.open", "app.quit", "file.open"] {
            state
                .palette
                .note_command(commands::command(id).expect("command").id);
        }
        assert_eq!(state.palette.recent_commands(), &["file.open", "app.quit"]);
        for id in COMMANDS.iter().map(|c| c.id).cycle().take(30) {
            state.palette.note_command(id);
        }
        assert_eq!(state.palette.recent_commands().len(), MAX_RECENT_COMMANDS);
    }

    // ---- quick open ----

    #[test]
    fn quick_open_lists_every_open_tab_with_its_window() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b", "c"]);
        // Move tab `c` into a second window.
        let second = state.new_window_id();
        state.windows.push(WindowState::new(second));
        state.windows[0].dock.remove_tab(tabs[2]);
        state.windows[1].dock.add_tab(tabs[2], None);

        let items = quick_items(&state, "", &[]);
        assert_eq!(items.len(), 3);
        for (tab, window) in [(tabs[0], 0), (tabs[1], 0), (tabs[2], 1)] {
            let item = items
                .iter()
                .find(|i| i.target == QuickTarget::Tab(tab))
                .expect("row");
            assert_eq!(item.window, Some(window));
        }
        let c = items
            .iter()
            .find(|i| i.target == QuickTarget::Tab(tabs[2]))
            .expect("row");
        assert_eq!(c.name, "c.pdf");
        assert_eq!(c.path, "/refdesk-test/c.pdf");
    }

    #[test]
    fn the_start_page_is_not_listed() {
        let state = AppState::new();
        assert!(quick_items(&state, "", &[]).is_empty());
    }

    #[test]
    fn quick_open_orders_by_recent_focus_with_the_current_tab_last() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b", "c"]);
        // Focus history: c (now), b, a.
        assert_eq!(
            names(&quick_items(&state, "", &[])),
            ["b.pdf", "a.pdf", "c.pdf"]
        );

        dispatch(&mut state, Action::FocusTab(tabs[0]), &ctx);
        assert_eq!(
            names(&quick_items(&state, "", &[])),
            ["c.pdf", "b.pdf", "a.pdf"]
        );

        // Ctrl+P, Enter: the top row is the tab we came from.
        dispatch(&mut state, Action::FocusTab(tabs[2]), &ctx);
        assert_eq!(names(&quick_items(&state, "", &[]))[0], "a.pdf");
    }

    #[test]
    fn tabs_that_never_had_the_focus_follow_the_others_in_window_order() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b", "c"]);
        state.palette = PaletteState::default();
        state.palette.note_focus(Some(tabs[1]));
        state.focus = Some(tabs[2]);
        // b was focused; a never was; c is current.
        assert_eq!(
            names(&quick_items(&state, "", &[])),
            ["b.pdf", "a.pdf", "c.pdf"]
        );
    }

    #[test]
    fn recent_documents_come_after_the_open_tabs_and_skip_open_ones() {
        let ctx = Context::default();
        let (state, _) = state_with(&ctx, &["a", "b"]);
        let recent = vec![
            PathBuf::from("/docs/r1.pdf"),
            PathBuf::from("/refdesk-test/a.pdf"),
            PathBuf::from("/docs/r2.pdf"),
        ];
        let items = quick_items(&state, "", &recent);
        assert_eq!(names(&items), ["a.pdf", "b.pdf", "r1.pdf", "r2.pdf"]);
        assert_eq!(
            items[2].target,
            QuickTarget::Recent(PathBuf::from("/docs/r1.pdf"))
        );
        assert_eq!(items[2].window, None);
        // The source is asked with the state; it decides what is recent.
        let source = |_: &AppState| vec![PathBuf::from("/x/y.pdf")];
        let provider: RecentDocuments<'_> = &source;
        let built = build_items(&state, Lang::En, PaletteMode::QuickOpen, "", provider);
        assert_eq!(built.len(), 3);
    }

    #[test]
    fn quick_open_matches_file_names_and_paths_and_prefers_names() {
        let ctx = Context::default();
        let (state, _) = state_with(&ctx, &["alpha", "beta"]);
        let recent = vec![PathBuf::from("/notes/alpha-notes/readme.pdf")];

        let items = quick_items(&state, "alpha", &recent);
        // The tab named alpha beats the document that only has alpha in its path.
        assert_eq!(names(&items), ["alpha.pdf", "readme.pdf"]);
        assert_eq!(items[0].name_positions, vec![0, 1, 2, 3, 4]);
        assert_eq!(items[1].name_positions, Vec::<usize>::new());
        assert!(!items[1].path_positions.is_empty());

        // Both live under /refdesk-test: the path finds both tabs.
        let items = quick_items(&state, "refdesk-test", &[]);
        assert_eq!(items.len(), 2);
        assert!(quick_items(&state, "zzz", &recent).is_empty());
    }

    // ---- selection ----

    #[test]
    fn the_selection_stops_at_both_ends() {
        assert_eq!(move_selection(0, 5, Step::Up), 0);
        assert_eq!(move_selection(4, 5, Step::Down), 4);
        assert_eq!(move_selection(1, 5, Step::Down), 2);
        assert_eq!(move_selection(3, 5, Step::Up), 2);
        assert_eq!(move_selection(2, 5, Step::PageDown), 4);
        assert_eq!(move_selection(2, 5, Step::PageUp), 0);
        assert_eq!(move_selection(2, 50, Step::PageDown), 2 + PAGE_ROWS);
        assert_eq!(move_selection(30, 50, Step::PageUp), 30 - PAGE_ROWS);
        assert_eq!(move_selection(2, 5, Step::Home), 0);
        assert_eq!(move_selection(2, 5, Step::End), 4);
        assert_eq!(move_selection(3, 0, Step::Down), 0);
        // A selection beyond the list counts as the last row.
        assert_eq!(move_selection(9, 5, Step::Up), 3);
    }

    #[test]
    fn a_shorter_list_brings_the_selection_back_inside() {
        assert_eq!(clamp_selection(7, 3), 2);
        assert_eq!(clamp_selection(1, 3), 1);
        assert_eq!(clamp_selection(5, 0), 0);
    }

    // ---- opening, closing, choosing ----

    #[test]
    fn the_shortcut_opens_closes_and_switches_the_list() {
        let mut state = AppState::new();
        assert!(state.palette.open.is_none());
        toggle(&mut state, PaletteMode::Commands, &Context::default());
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::Commands)
        );
        // Typing something, then the other shortcut: the other list, nothing typed.
        set_query(&mut state, "abc");
        toggle(&mut state, PaletteMode::QuickOpen, &Context::default());
        let open = state.palette.open.as_ref().expect("still open");
        assert_eq!(open.mode, PaletteMode::QuickOpen);
        assert_eq!(open.query, "");
        // The same shortcut again closes.
        toggle(&mut state, PaletteMode::QuickOpen, &Context::default());
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn the_palette_opens_on_the_window_of_the_focused_tab() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b"]);
        let second = state.new_window_id();
        state.windows.push(WindowState::new(second));
        state.windows[0].dock.remove_tab(tabs[1]);
        state.windows[1].dock.add_tab(tabs[1], None);
        dispatch(&mut state, Action::FocusTab(tabs[1]), &ctx);
        toggle(&mut state, PaletteMode::Commands, &Context::default());
        assert_eq!(state.palette.open.as_ref().map(|o| o.window), Some(second));
    }

    #[test]
    fn the_show_palette_action_toggles_through_dispatch() {
        let ctx = Context::default();
        let mut state = AppState::new();
        dispatch(&mut state, Action::ShowPalette(PaletteMode::Commands), &ctx);
        assert!(state.palette.open.is_some());
        dispatch(&mut state, Action::ShowPalette(PaletteMode::Commands), &ctx);
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn a_palette_whose_window_is_gone_is_closed() {
        let ctx = Context::default();
        let mut state = AppState::new();
        toggle(&mut state, PaletteMode::Commands, &Context::default());
        let missing = state.new_window_id();
        if let Some(open) = &mut state.palette.open {
            open.window = missing;
        }
        dispatch(&mut state, Action::OpenFileDialog, &ctx);
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn running_a_command_closes_the_palette_and_queues_its_action() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a"]);
        open_palette(&mut state, PaletteMode::Commands);
        set_query(&mut state, "duplicate");
        let mut queue = Vec::new();
        assert!(choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &no_recent_documents,
            0,
            &mut queue
        ));
        assert_eq!(queue, vec![Action::DuplicateTab(tabs[0])]);
        assert!(state.palette.open.is_none());
        assert_eq!(state.palette.recent_commands(), &["tab.duplicate"]);
        // The queued action is the ordinary one.
        state.actions.append(&mut queue);
        dispatch_queued(&mut state, &ctx);
        assert_eq!(state.main_window().dock.tabs().len(), 2);
    }

    #[test]
    fn running_a_disabled_command_does_nothing_and_keeps_the_palette_open() {
        let mut state = AppState::new();
        open_palette(&mut state, PaletteMode::Commands);
        set_query(&mut state, "close tab");
        let mut queue = Vec::new();
        assert!(!choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &no_recent_documents,
            0,
            &mut queue
        ));
        assert!(queue.is_empty());
        assert!(state.palette.open.is_some());
        assert!(state.palette.recent_commands().is_empty());
        // A row that does not exist is not a command either.
        assert!(!choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &no_recent_documents,
            99,
            &mut queue
        ));
    }

    #[test]
    fn choosing_the_command_that_opens_the_palette_reopens_it() {
        let ctx = Context::default();
        let mut state = AppState::new();
        open_palette(&mut state, PaletteMode::Commands);
        set_query(&mut state, "quick open");
        let mut queue = Vec::new();
        assert!(choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &no_recent_documents,
            0,
            &mut queue
        ));
        assert_eq!(queue, vec![Action::ShowPalette(PaletteMode::QuickOpen)]);
        state.actions.append(&mut queue);
        dispatch_queued(&mut state, &ctx);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::QuickOpen)
        );
    }

    #[test]
    fn choosing_a_tab_of_another_window_focuses_it() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b"]);
        let second = state.new_window_id();
        state.windows.push(WindowState::new(second));
        state.windows[0].dock.remove_tab(tabs[0]);
        state.windows[1].dock.add_tab(tabs[0], None);
        assert_eq!(state.focus, Some(tabs[1]));

        open_palette(&mut state, PaletteMode::QuickOpen);
        set_query(&mut state, "a.pdf");
        let mut queue = Vec::new();
        assert!(choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &no_recent_documents,
            0,
            &mut queue
        ));
        assert_eq!(queue, vec![Action::FocusTab(tabs[0])]);
        state.actions.append(&mut queue);
        dispatch_queued(&mut state, &ctx);
        assert_eq!(state.focus, Some(tabs[0]));
        assert_eq!(state.windows[1].dock.focused_tab(), Some(tabs[0]));
    }

    #[test]
    fn choosing_a_recent_document_opens_it() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        let source = |_: &AppState| vec![PathBuf::from("/docs/old.pdf")];
        open_palette(&mut state, PaletteMode::QuickOpen);
        let mut queue = Vec::new();
        // Row 0 is the open tab, row 1 the recent document.
        assert!(choose(
            &mut state,
            &Context::default(),
            Lang::En,
            &source,
            1,
            &mut queue
        ));
        assert_eq!(
            queue,
            vec![Action::OpenFiles(vec![OpenTarget {
                path: PathBuf::from("/docs/old.pdf"),
                page: None
            }])]
        );
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn the_palette_commands_are_in_the_view_menu_with_their_shortcuts() {
        let view: Vec<&str> = commands::in_menu(commands::MenuGroup::View)
            .map(|c| c.id)
            .collect();
        for (id, shortcut) in [
            (
                COMMANDS_COMMAND_ID,
                KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::P),
            ),
            (
                QUICK_OPEN_COMMAND_ID,
                KeyboardShortcut::new(Modifiers::CTRL, Key::P),
            ),
        ] {
            assert!(view.contains(&id), "{id}");
            let command = commands::command(id).expect("command");
            assert_eq!(command.shortcut, Some(shortcut));
            assert!(command.run(&AppState::new()).is_some(), "{id} always runs");
        }
    }

    // ---- frames without a window ----

    const SCREEN: egui::Vec2 = egui::vec2(1000.0, 700.0);

    fn input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN)),
            events,
            ..Default::default()
        }
    }

    fn key(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// One frame the way `RefdeskApp::ui` runs it: shortcuts first, then the window. Dispatches
    /// the queue at the end, like the app does.
    fn frame(ctx: &Context, state: &mut AppState, events: Vec<egui::Event>) {
        let mut queue = Vec::new();
        ctx.run_ui(input(events), |ui| {
            queue.extend(commands::handle_shortcuts(ui.ctx(), state));
            crate::windows::show_window(ui, state, 0, &mut queue);
        })
        .drop_without_applying_deltas();
        state.actions.append(&mut queue);
        dispatch_queued(state, ctx);
    }

    fn settle(ctx: &Context, state: &mut AppState) {
        for _ in 0..3 {
            frame(ctx, state, vec![]);
        }
    }

    #[test]
    fn typing_and_enter_run_the_command_without_panicking() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        open_palette(&mut state, PaletteMode::Commands);
        settle(&ctx, &mut state);

        frame(&ctx, &mut state, vec![egui::Event::Text("duplic".into())]);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.query.as_str()),
            Some("duplic")
        );
        assert_eq!(state.main_window().dock.tabs().len(), 1);

        frame(&ctx, &mut state, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(state.palette.open.is_none());
        assert_eq!(state.main_window().dock.tabs().len(), 2, "the command ran");
        settle(&ctx, &mut state);
    }

    #[test]
    fn arrow_keys_move_the_selection_and_escape_closes() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a"]);
        open_palette(&mut state, PaletteMode::Commands);
        settle(&ctx, &mut state);
        let selected = |s: &AppState| s.palette.open.as_ref().map(|o| o.selected);
        assert_eq!(selected(&state), Some(0));
        frame(&ctx, &mut state, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        frame(&ctx, &mut state, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        assert_eq!(selected(&state), Some(2));
        frame(&ctx, &mut state, vec![key(Key::ArrowUp, Modifiers::NONE)]);
        assert_eq!(selected(&state), Some(1));
        frame(&ctx, &mut state, vec![key(Key::End, Modifiers::NONE)]);
        assert_eq!(selected(&state), Some(COMMANDS.len() - 1));
        frame(&ctx, &mut state, vec![key(Key::ArrowDown, Modifiers::NONE)]);
        assert_eq!(selected(&state), Some(COMMANDS.len() - 1), "no wrap");
        frame(&ctx, &mut state, vec![key(Key::Home, Modifiers::NONE)]);
        assert_eq!(selected(&state), Some(0));

        frame(&ctx, &mut state, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(state.palette.open.is_none());
        assert_eq!(state.focus, Some(tabs[0]));
    }

    #[test]
    fn other_shortcuts_do_nothing_while_the_palette_is_open() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        let ctrl_w = key(Key::W, Modifiers::CTRL);
        // Without the palette Ctrl+W closes the tab.
        let mut probe = Vec::new();
        ctx.run_ui(input(vec![ctrl_w.clone()]), |ui| {
            probe.extend(commands::handle_shortcuts(ui.ctx(), &state));
        })
        .drop_without_applying_deltas();
        assert!(matches!(probe.as_slice(), [Action::CloseTab(_)]));

        open_palette(&mut state, PaletteMode::Commands);
        settle(&ctx, &mut state);
        frame(&ctx, &mut state, vec![ctrl_w.clone()]);
        let mut probe = Vec::new();
        ctx.run_ui(input(vec![ctrl_w]), |ui| {
            probe.extend(commands::handle_shortcuts(ui.ctx(), &state));
        })
        .drop_without_applying_deltas();
        assert!(probe.is_empty());
        assert_eq!(
            state.main_window().dock.tabs().len(),
            1,
            "the tab is still open"
        );
        assert!(state.palette.open.is_some());
    }

    #[test]
    fn shortcuts_are_blocked_from_the_very_first_frame_of_the_palette() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        open_palette(&mut state, PaletteMode::Commands);
        // No frame has run, so the text field has not taken the focus yet.
        assert!(!ctx.text_edit_focused());
        let mut probe = Vec::new();
        ctx.run_ui(input(vec![key(Key::W, Modifiers::CTRL)]), |ui| {
            probe.extend(commands::handle_shortcuts(ui.ctx(), &state));
        })
        .drop_without_applying_deltas();
        assert!(probe.is_empty());
    }

    #[test]
    fn the_shortcuts_close_and_switch_while_the_palette_is_open() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        let ctrl_shift_p = key(Key::P, Modifiers::CTRL.plus(Modifiers::SHIFT));
        let ctrl_p = key(Key::P, Modifiers::CTRL);

        // Closed: Ctrl+Shift+P opens the command palette.
        frame(&ctx, &mut state, vec![ctrl_shift_p.clone()]);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::Commands)
        );
        settle(&ctx, &mut state);
        // Ctrl+P switches to quick open.
        frame(&ctx, &mut state, vec![ctrl_p.clone()]);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::QuickOpen)
        );
        settle(&ctx, &mut state);
        // Ctrl+Shift+P switches back, Ctrl+Shift+P again closes.
        frame(&ctx, &mut state, vec![ctrl_shift_p.clone()]);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::Commands)
        );
        frame(&ctx, &mut state, vec![ctrl_shift_p]);
        assert!(state.palette.open.is_none());
        // Ctrl+P opens quick open and, pressed again, closes it.
        frame(&ctx, &mut state, vec![ctrl_p.clone()]);
        assert_eq!(
            state.palette.open.as_ref().map(|o| o.mode),
            Some(PaletteMode::QuickOpen)
        );
        frame(&ctx, &mut state, vec![ctrl_p]);
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn quick_open_with_enter_switches_between_two_tabs() {
        let ctx = Context::default();
        let (mut state, tabs) = state_with(&ctx, &["a", "b"]);
        assert_eq!(state.focus, Some(tabs[1]));
        for expected in [tabs[0], tabs[1], tabs[0]] {
            frame(&ctx, &mut state, vec![key(Key::P, Modifiers::CTRL)]);
            settle(&ctx, &mut state);
            frame(&ctx, &mut state, vec![key(Key::Enter, Modifiers::NONE)]);
            assert!(state.palette.open.is_none());
            assert_eq!(state.focus, Some(expected));
        }
    }

    #[test]
    fn a_click_outside_closes_the_palette() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a"]);
        open_palette(&mut state, PaletteMode::Commands);
        settle(&ctx, &mut state);
        // The bottom-left corner is far from the palette at the top centre.
        let at = egui::pos2(10.0, SCREEN.y - 10.0);
        let click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame(&ctx, &mut state, vec![egui::Event::PointerMoved(at)]);
        frame(&ctx, &mut state, vec![click(true)]);
        assert!(state.palette.open.is_some(), "closes on release");
        frame(&ctx, &mut state, vec![click(false)]);
        assert!(state.palette.open.is_none());
    }

    #[test]
    fn both_lists_draw_without_panicking_for_odd_input() {
        let ctx = Context::default();
        let (mut state, _) = state_with(&ctx, &["a", "b"]);
        for mode in [PaletteMode::Commands, PaletteMode::QuickOpen] {
            state.palette.open = None;
            open_palette(&mut state, mode);
            settle(&ctx, &mut state);
            for text in ["분할", "zzzz-no-match", "", "  ", "a.pdf"] {
                set_query(&mut state, text);
                frame(&ctx, &mut state, vec![]);
                frame(&ctx, &mut state, vec![key(Key::ArrowDown, Modifiers::NONE)]);
                frame(&ctx, &mut state, vec![key(Key::PageDown, Modifiers::NONE)]);
            }
        }
    }
}
