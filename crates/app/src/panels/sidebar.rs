//! The sidebar (WS-7): the left panel of a window, with the explorer section above and the table
//! of contents section below (`docs/ARCHITECTURE.md` §5.9).
//!
//! Each section has a header (fold triangle, name, small buttons on the right); a click on the
//! header folds the section. While both are open the boundary between them can be dragged
//! (`WindowState::sidebar_split`); a folded section leaves its height to the other. How the
//! height is shared is the pure function [`layout`]. The sidebar's state is per window.
//!
//! A thin row at the top switches between two views (WP-55): "Files" (the two sections) and
//! "Search" (the search in all open documents, `panels/global_search.rs`).

use std::path::PathBuf;

use egui::{Align, CursorIcon, Layout, Pos2, Rect, RichText, Sense, TextStyle, Ui, UiBuilder};

use crate::action::Action;
use crate::i18n::tr;
use crate::panels::explorer::{self, WorkspaceView};
use crate::panels::global_search;
use crate::panels::toc::{self, title_job, triangle};
use crate::state::{AppState, DocStore, Tab, TabId, TabStore};

/// Share of the height the explorer takes in a window that was never adjusted.
pub const DEFAULT_SPLIT: f32 = 0.5;
/// The smallest and largest share the boundary can be dragged to (as in `SessionWindow`).
pub const SPLIT_RANGE: (f32, f32) = (0.1, 0.9);
/// Height of a section header.
pub const HEADER_H: f32 = 22.0;
/// Height of the draggable boundary between the two open sections.
pub const SPLITTER_H: f32 = 6.0;
/// Height of the row that switches between the views (WP-55).
pub const SWITCH_H: f32 = 24.0;

/// What the sidebar of a window shows (WP-55).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidebarView {
    /// The explorer and the table of contents.
    #[default]
    Files,
    /// The search in all open documents.
    Search,
}

/// The height left for the view below the switch row in a sidebar of height `total`.
pub fn below_switch(total: f32) -> f32 {
    (total - SWITCH_H).max(0.0)
}

/// One section of the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarSection {
    /// The file explorer, above.
    Explorer,
    /// The table of contents, below.
    Outline,
}

/// How the sidebar's height is shared, from the top: explorer header, explorer body, the
/// boundary, outline header, outline body. A folded section has a body of 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SectionLayout {
    /// Height of the explorer's body.
    pub explorer_body: f32,
    /// Height of the table of contents' body.
    pub outline_body: f32,
    /// Whether the boundary between the bodies is shown (both open, the explorer shows a tree).
    pub splitter: bool,
}

/// A share kept within [`SPLIT_RANGE`]; a value that is not a number becomes the default.
pub fn clamp_split(split: f32) -> f32 {
    if split.is_nan() {
        DEFAULT_SPLIT
    } else {
        split.clamp(SPLIT_RANGE.0, SPLIT_RANGE.1)
    }
}

/// Shares `total` (the sidebar's height) between the sections. `compact` is the height the
/// explorer's body needs when it has no tree to show (see `explorer::compact_body_height`):
/// then it takes only that and the table of contents the rest.
pub fn layout(
    total: f32,
    explorer_open: bool,
    outline_open: bool,
    split: f32,
    compact: Option<f32>,
) -> SectionLayout {
    let bodies = (total - 2.0 * HEADER_H).max(0.0);
    match (explorer_open, outline_open) {
        (true, true) => match compact {
            Some(h) => {
                let explorer_body = h.min(bodies);
                SectionLayout {
                    explorer_body,
                    outline_body: bodies - explorer_body,
                    splitter: false,
                }
            }
            None => {
                let shared = (bodies - SPLITTER_H).max(0.0);
                let explorer_body = shared * clamp_split(split);
                SectionLayout {
                    explorer_body,
                    outline_body: shared - explorer_body,
                    splitter: true,
                }
            }
        },
        (true, false) => SectionLayout {
            explorer_body: bodies,
            outline_body: 0.0,
            splitter: false,
        },
        (false, true) => SectionLayout {
            explorer_body: 0.0,
            outline_body: bodies,
            splitter: false,
        },
        (false, false) => SectionLayout {
            explorer_body: 0.0,
            outline_body: 0.0,
            splitter: false,
        },
    }
}

/// The share after dragging the boundary by `dy` points in a sidebar of height `total`.
pub fn dragged_split(split: f32, dy: f32, total: f32) -> f32 {
    let shared = total - 2.0 * HEADER_H - SPLITTER_H;
    if shared <= 1.0 {
        return clamp_split(split);
    }
    clamp_split(clamp_split(split) + dy / shared)
}

/// The document a window's focused tab shows (the side in use of a comparison), normalized like
/// the paths of the workspace index.
fn current_document(tabs: &TabStore, docs: &DocStore, tab: Option<TabId>) -> Option<PathBuf> {
    let doc = match tabs.get(tab?)? {
        Tab::Pdf(pdf) => pdf.doc,
        Tab::Diff(diff) => diff.side(diff.active).doc,
        Tab::Start => return None,
    };
    // Document paths are normalized when they are opened; this only copies.
    docs.get(doc).map(|d| d.path().to_path_buf())
}

/// Draws the sidebar of window `window_index` into `ui` (the inside of the left panel). The
/// table of contents shows `focus_tab`, the window's focused tab.
pub fn show(
    ui: &mut Ui,
    state: &mut AppState,
    window_index: usize,
    focus_tab: Option<TabId>,
    queue: &mut Vec<Action>,
) {
    let AppState {
        windows,
        tabs,
        docs,
        workspace,
        global_search,
        ..
    } = state;
    let Some(window) = windows.get_mut(window_index) else {
        return;
    };
    let window_id = window.id;

    // The view switch, and below it the search view or the two sections.
    let whole = ui.available_rect_before_wrap();
    let found = (global_search.searched() && global_search.has_query())
        .then(|| global_search.totals().matches)
        .filter(|n| *n > 0);
    let switch = Rect::from_min_size(whole.min, egui::vec2(whole.width(), SWITCH_H));
    view_switch(ui, switch, &mut window.sidebar_view, found);
    let full = Rect::from_min_max(
        Pos2::new(whole.left(), whole.bottom() - below_switch(whole.height())),
        whole.max,
    );
    if window.sidebar_view == SidebarView::Search {
        let any_docs = tabs.iter().any(|(_, t)| matches!(t, Tab::Pdf(_)));
        let mut body = body_ui(ui, full, "search_body");
        global_search::show_view(&mut body, global_search, docs, any_docs, window_id, queue);
        ui.allocate_rect(whole, Sense::hover());
        return;
    }

    let view = workspace.as_ref().map(WorkspaceView::of);
    let current = current_document(tabs, docs, focus_tab);
    let shares = layout(
        full.height(),
        window.explorer_open,
        window.outline_open,
        window.sidebar_split,
        explorer::compact_body_height(view.as_ref()),
    );
    let mut y = full.top();
    let band = |y: f32, h: f32| {
        Rect::from_min_size(Pos2::new(full.left(), y), egui::vec2(full.width(), h))
    };

    // Explorer.
    let (title, tooltip) = explorer::header_title(view.as_ref());
    let explorer_open = window.explorer_open;
    let clicked = header(
        ui,
        band(y, HEADER_H),
        "explorer",
        &title,
        tooltip.as_deref(),
        explorer_open,
        |ui| explorer::header_buttons(ui, view.as_ref(), &mut window.explorer, queue),
    );
    if clicked {
        queue.push(Action::ToggleSidebarSection {
            window: window_id,
            section: SidebarSection::Explorer,
        });
    }
    y += HEADER_H;
    if explorer_open {
        let rect = band(y, shares.explorer_body);
        let mut body = body_ui(ui, rect, "explorer_body");
        explorer::show_body(
            &mut body,
            view.as_ref(),
            &mut window.explorer,
            current.as_deref(),
            window_id,
            queue,
        );
        y += shares.explorer_body;
    }
    if shares.splitter {
        let rect = band(y, SPLITTER_H);
        let response = ui
            .interact(
                rect,
                ui.make_persistent_id("sidebar_splitter"),
                Sense::drag(),
            )
            .on_hover_and_drag_cursor(CursorIcon::ResizeVertical);
        let stroke = if response.hovered() || response.dragged() {
            ui.visuals().widgets.hovered.fg_stroke
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke
        };
        ui.painter().hline(rect.x_range(), rect.center().y, stroke);
        if response.dragged() {
            window.sidebar_split =
                dragged_split(window.sidebar_split, response.drag_delta().y, full.height());
        }
        y += SPLITTER_H;
    }

    // Table of contents.
    let outline_open = window.outline_open;
    let clicked = header(
        ui,
        band(y, HEADER_H),
        "outline",
        tr("toc.title"),
        None,
        outline_open,
        |ui| outline_buttons(ui, tabs, docs, focus_tab),
    );
    if clicked {
        queue.push(Action::ToggleSidebarSection {
            window: window_id,
            section: SidebarSection::Outline,
        });
    }
    y += HEADER_H;
    if outline_open {
        let mut body = body_ui(ui, band(y, shares.outline_body), "outline_body");
        toc::show(&mut body, tabs, docs, focus_tab, queue);
    }
    ui.allocate_rect(whole, Sense::hover());
}

/// Draws the row that switches the sidebar between its views into `rect`. `found` is the number
/// of results of the search in all documents, shown beside "Search".
fn view_switch(ui: &mut Ui, rect: Rect, view: &mut SidebarView, found: Option<usize>) {
    let mut row = ui.new_child(
        UiBuilder::new()
            .id_salt("sidebar_view_switch")
            .max_rect(rect.shrink2(egui::vec2(4.0, 1.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    let search = match found {
        Some(n) => tr("sidebar.search_count").replace("{n}", &n.to_string()),
        None => tr("sidebar.search").to_owned(),
    };
    let entries = [
        (
            SidebarView::Files,
            tr("sidebar.files").to_owned(),
            "sidebar.files_hint",
        ),
        (SidebarView::Search, search, "sidebar.search_hint"),
    ];
    for (which, label, hint) in entries {
        if row
            .selectable_label(*view == which, label)
            .on_hover_text(tr(hint))
            .clicked()
        {
            *view = which;
        }
    }
    ui.painter().hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
}

/// A child `Ui` that draws exactly into `rect`, clipped to it.
fn body_ui(ui: &mut Ui, rect: Rect, salt: &str) -> Ui {
    let mut body = ui.new_child(
        UiBuilder::new()
            .id_salt(("sidebar", salt))
            .max_rect(rect.shrink2(egui::vec2(0.0, 2.0)))
            .layout(Layout::top_down(Align::Min)),
    );
    body.set_clip_rect(rect.intersect(ui.clip_rect()));
    body
}

/// Draws a section header into `rect` and returns whether it was clicked (folding the section
/// is the caller's job). `buttons` draws the small buttons, right to left from the right edge;
/// they take their clicks before the header does.
fn header(
    ui: &mut Ui,
    rect: Rect,
    salt: &str,
    title: &str,
    tooltip: Option<&str>,
    open: bool,
    buttons: impl FnOnce(&mut Ui),
) -> bool {
    let response = ui.interact(
        rect,
        ui.make_persistent_id(("sidebar_header", salt)),
        Sense::click(),
    );
    let visuals = ui.visuals().clone();
    let fill = if response.hovered() {
        visuals.widgets.hovered.weak_bg_fill
    } else {
        visuals.faint_bg_color
    };
    ui.painter().rect_filled(rect, 0.0, fill);
    let fold_rect = Rect::from_min_size(rect.min, egui::vec2(HEADER_H, HEADER_H));
    ui.painter()
        .add(triangle(fold_rect, open, visuals.strong_text_color()));

    let mut row = ui.new_child(
        UiBuilder::new()
            .id_salt(("sidebar_header_buttons", salt))
            .max_rect(rect.shrink2(egui::vec2(4.0, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
    );
    buttons(&mut row);
    let buttons_left = row.min_rect().left().min(rect.right());

    let job = title_job(
        title,
        &[],
        ui.style().text_styles[&TextStyle::Button].clone(),
        visuals.strong_text_color(),
        visuals.strong_text_color(),
        (buttons_left - fold_rect.right() - 6.0).max(0.0),
    );
    let galley = ui.painter().layout_job(job);
    let pos = Pos2::new(fold_rect.right(), rect.center().y - galley.size().y / 2.0);
    ui.painter()
        .galley(pos, galley, visuals.strong_text_color());

    let response = match tooltip {
        Some(text) => response.on_hover_text(text),
        None => response,
    };
    response.clicked()
}

/// The buttons of the table of contents header: fold every entry of the focused document.
fn outline_buttons(ui: &mut Ui, tabs: &mut TabStore, docs: &DocStore, focus_tab: Option<TabId>) {
    let Some(Tab::Pdf(pdf)) = focus_tab.and_then(|t| tabs.get_mut(t)) else {
        return;
    };
    let Some(info) = docs.get(pdf.doc).and_then(|d| d.info()) else {
        return;
    };
    if info.outline.items.is_empty() {
        return;
    }
    let clicked = ui
        .add(egui::Button::new(RichText::new("⊟").small()).frame(false))
        .on_hover_text(tr("toc.collapse_all"))
        .clicked();
    if clicked {
        pdf.toc.collapse_all(&info.outline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sections_share_the_height_by_the_split() {
        let l = layout(600.0, true, true, 0.25, None);
        let shared = 600.0 - 2.0 * HEADER_H - SPLITTER_H;
        assert!(l.splitter);
        assert!((l.explorer_body - shared * 0.25).abs() < 1e-3);
        assert!((l.explorer_body + l.outline_body - shared).abs() < 1e-3);
    }

    #[test]
    fn the_view_switch_takes_its_row_and_the_sections_share_the_rest() {
        assert_eq!(below_switch(600.0), 600.0 - SWITCH_H);
        assert_eq!(below_switch(10.0), 0.0, "never negative");
        let rest = below_switch(600.0);
        let l = layout(rest, true, true, 0.25, None);
        let shared = 600.0 - SWITCH_H - 2.0 * HEADER_H - SPLITTER_H;
        assert!((l.explorer_body - shared * 0.25).abs() < 1e-3);
        assert!((l.explorer_body + l.outline_body - shared).abs() < 1e-3);
        assert_eq!(SidebarView::default(), SidebarView::Files);
    }

    #[test]
    fn a_folded_section_leaves_its_height_to_the_other() {
        let bodies = 600.0 - 2.0 * HEADER_H;
        assert_eq!(
            layout(600.0, false, true, 0.5, None),
            SectionLayout {
                explorer_body: 0.0,
                outline_body: bodies,
                splitter: false
            }
        );
        assert_eq!(
            layout(600.0, true, false, 0.5, None),
            SectionLayout {
                explorer_body: bodies,
                outline_body: 0.0,
                splitter: false
            }
        );
    }

    #[test]
    fn both_folded_shows_only_the_two_headers() {
        let l = layout(600.0, false, false, 0.5, None);
        assert_eq!(
            (l.explorer_body, l.outline_body, l.splitter),
            (0.0, 0.0, false)
        );
        // A sidebar lower than two headers gives no negative heights.
        let l = layout(10.0, true, true, 0.5, None);
        assert!(l.explorer_body >= 0.0 && l.outline_body >= 0.0);
    }

    #[test]
    fn an_explorer_without_a_tree_takes_only_what_it_needs() {
        let l = layout(600.0, true, true, 0.5, Some(28.0));
        assert_eq!(l.explorer_body, 28.0);
        assert_eq!(l.outline_body, 600.0 - 2.0 * HEADER_H - 28.0);
        assert!(!l.splitter);
        // Alone it still gets the whole height.
        assert_eq!(
            layout(600.0, true, false, 0.5, Some(28.0)).explorer_body,
            600.0 - 2.0 * HEADER_H
        );
    }

    #[test]
    fn the_split_is_kept_within_its_range() {
        assert_eq!(clamp_split(0.0), SPLIT_RANGE.0);
        assert_eq!(clamp_split(2.0), SPLIT_RANGE.1);
        assert_eq!(clamp_split(f32::NAN), DEFAULT_SPLIT);
        assert_eq!(clamp_split(0.3), 0.3);
        let shared = 600.0 - 2.0 * HEADER_H - SPLITTER_H;
        let moved = dragged_split(0.5, shared * 0.1, 600.0);
        assert!((moved - 0.6).abs() < 1e-4);
        assert_eq!(dragged_split(0.5, 10_000.0, 600.0), SPLIT_RANGE.1);
        assert_eq!(dragged_split(0.5, -10_000.0, 600.0), SPLIT_RANGE.0);
        assert_eq!(dragged_split(0.5, 50.0, 0.0), 0.5);
        let l = layout(600.0, true, true, 5.0, None);
        assert!((l.explorer_body - shared * SPLIT_RANGE.1).abs() < 1e-3);
    }

    // --- actions and drawing, without a window -------------------------------------------

    use crate::action::dispatch;
    use crate::state::WindowState;
    use crate::windows::show_window;

    /// A state with a second window that shows a start page, focused.
    fn two_windows() -> (AppState, TabId, TabId) {
        let mut state = AppState::new();
        let first = state.main_window().dock.tabs()[0];
        let id = state.new_window_id();
        let second = state.tabs.insert(Tab::Start);
        let mut window = WindowState::new(id);
        window.dock.add_tab(second, None);
        state.windows.push(window);
        state.focus = Some(second);
        (state, first, second)
    }

    fn flags(w: &WindowState) -> (bool, bool, bool) {
        (w.toc_open, w.explorer_open, w.outline_open)
    }

    #[test]
    fn f9_shows_and_hides_the_whole_sidebar_of_the_focused_window_only() {
        let ctx = egui::Context::default();
        let (mut state, first, _) = two_windows();
        dispatch(&mut state, Action::ToggleToc, &ctx);
        assert_eq!(flags(&state.windows[1]), (true, true, true));
        assert_eq!(flags(&state.windows[0]), (false, true, true));
        // The sections keep their state while the sidebar is hidden.
        state.windows[1].outline_open = false;
        dispatch(&mut state, Action::ToggleToc, &ctx);
        assert_eq!(flags(&state.windows[1]), (false, true, false));
        dispatch(&mut state, Action::ToggleToc, &ctx);
        assert_eq!(flags(&state.windows[1]), (true, true, false));
        state.focus = Some(first);
        dispatch(&mut state, Action::ToggleToc, &ctx);
        assert_eq!(flags(&state.windows[0]), (true, true, true));
    }

    #[test]
    fn show_explorer_opens_the_sidebar_and_the_section_and_asks_for_the_filter_box() {
        let ctx = egui::Context::default();
        let (mut state, _, _) = two_windows();
        state.windows[1].explorer_open = false;
        dispatch(&mut state, Action::ShowExplorer, &ctx);
        assert_eq!(flags(&state.windows[1]), (true, true, true));
        assert!(state.windows[1].explorer.focus_filter);
        assert!(!state.windows[0].explorer.focus_filter);
        assert!(!state.windows[0].toc_open);
        // Again while it shows: it stays shown.
        dispatch(&mut state, Action::ShowExplorer, &ctx);
        assert!(state.windows[1].toc_open && state.windows[1].explorer_open);
    }

    #[test]
    fn sections_fold_per_window_and_a_hidden_sidebar_comes_back_with_the_section() {
        let ctx = egui::Context::default();
        let (mut state, _, _) = two_windows();
        let (main, other) = (state.windows[0].id, state.windows[1].id);
        let toggle = |window, section| Action::ToggleSidebarSection { window, section };
        // Hidden: shown with the section open.
        state.windows[1].outline_open = false;
        dispatch(&mut state, toggle(other, SidebarSection::Outline), &ctx);
        assert_eq!(flags(&state.windows[1]), (true, true, true));
        // Shown: folds and unfolds.
        dispatch(&mut state, toggle(other, SidebarSection::Explorer), &ctx);
        assert_eq!(flags(&state.windows[1]), (true, false, true));
        dispatch(&mut state, toggle(other, SidebarSection::Outline), &ctx);
        assert_eq!(flags(&state.windows[1]), (true, false, false));
        dispatch(&mut state, toggle(other, SidebarSection::Explorer), &ctx);
        assert_eq!(flags(&state.windows[1]), (true, true, false));
        // The other window is untouched; a window that is gone is ignored.
        assert_eq!(flags(&state.windows[0]), (false, true, true));
        dispatch(&mut state, toggle(main, SidebarSection::Explorer), &ctx);
        assert_eq!(flags(&state.windows[0]), (true, true, true));
        let gone = state.new_window_id();
        dispatch(&mut state, toggle(gone, SidebarSection::Explorer), &ctx);
    }

    fn frame(ctx: &egui::Context, state: &mut AppState, events: Vec<egui::Event>) -> Vec<Action> {
        let mut queue = Vec::new();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1000.0, 700.0))),
            events,
            ..Default::default()
        };
        ctx.run_ui(input, |ui| show_window(ui, state, 0, &mut queue))
            .drop_without_applying_deltas();
        queue
    }

    fn click(ctx: &egui::Context, state: &mut AppState, pos: Pos2) -> Vec<Action> {
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut queue = frame(ctx, state, vec![egui::Event::PointerMoved(pos)]);
        queue.extend(frame(ctx, state, vec![button(true)]));
        queue.extend(frame(ctx, state, vec![button(false)]));
        queue
    }

    #[test]
    fn clicking_a_header_asks_to_fold_its_section_of_this_window() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(&mut state, Action::ToggleToc, &ctx);
        for _ in 0..2 {
            assert!(frame(&ctx, &mut state, Vec::new()).is_empty());
        }
        // The explorer header is the first line of the panel below the view switch.
        let queue = click(
            &ctx,
            &mut state,
            Pos2::new(60.0, 2.0 + SWITCH_H + HEADER_H / 2.0),
        );
        assert_eq!(
            queue,
            vec![Action::ToggleSidebarSection {
                window: state.main_window().id,
                section: SidebarSection::Explorer,
            }]
        );
        for action in queue {
            dispatch(&mut state, action, &ctx);
        }
        assert!(!state.main_window().explorer_open);
        // Folded, the outline header comes right below it.
        frame(&ctx, &mut state, Vec::new());
        let queue = click(
            &ctx,
            &mut state,
            Pos2::new(60.0, 2.0 + SWITCH_H + HEADER_H * 1.5),
        );
        assert_eq!(
            queue,
            vec![Action::ToggleSidebarSection {
                window: state.main_window().id,
                section: SidebarSection::Outline,
            }]
        );
        for action in queue {
            dispatch(&mut state, action, &ctx);
        }
        // Both folded: only the two headers; drawing goes on.
        assert_eq!(flags(state.main_window()), (true, false, false));
        for _ in 0..2 {
            assert!(frame(&ctx, &mut state, Vec::new()).is_empty());
        }
    }

    #[test]
    fn the_open_folder_button_of_an_empty_explorer_asks_for_the_folder_chooser() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(&mut state, Action::ToggleToc, &ctx);
        frame(&ctx, &mut state, Vec::new());
        let mut found = false;
        for y in (26..60).step_by(3) {
            let queue = click(&ctx, &mut state, Pos2::new(30.0, SWITCH_H + y as f32));
            if queue.contains(&Action::OpenFolderDialog) {
                found = true;
                break;
            }
        }
        assert!(found, "the button under the explorer header");
        dispatch(&mut state, Action::OpenFolderDialog, &ctx);
        assert!(state.folder_dialog_requested);
    }
}
