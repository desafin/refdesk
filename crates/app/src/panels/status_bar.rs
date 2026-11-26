//! The status bar at the bottom of a window: current page / page count (with the page label),
//! the zoom controls, the preload progress and the document state.
//!
//! Clicking the page number (or Ctrl+G) opens a page number field; Enter jumps to the top of
//! that page, Esc cancels. The zoom controls are `[−] [137% ⏷] [+]` (VIEW-11): − and + run the
//! zoom commands, the percentage opens the zoom menu. Names, shortcuts and actions come from the
//! command table.

use egui::{Key, Ui};
use refdesk_core::{DocStatus, ZoomMode};

use crate::action::Action;
use crate::commands::{self, Command, ZOOM_PRESETS};
use crate::i18n::tr;
use crate::state::{AppState, Tab, TabId};
use crate::views::diff_view;
use crate::views::preload::Progress;

/// The open page number field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GotoInput {
    /// What has been typed.
    pub text: String,
    /// The field has been given the keyboard focus.
    pub focused_once: bool,
}

/// "12 / 300", with the page label when the document defines one: "12 / 300 (xii)".
pub fn page_text(current: Option<u32>, count: u32, labels: &[String]) -> String {
    let Some(page) = current.filter(|_| count > 0) else {
        return tr("doc.pages").replace("{}", &count.to_string());
    };
    let page = page.min(count - 1);
    let base = tr("status.page")
        .replace("{page}", &(page + 1).to_string())
        .replace("{count}", &count.to_string());
    match labels.get(page as usize) {
        Some(label) if !label.is_empty() && *label != (page + 1).to_string() => {
            format!("{base} ({label})")
        }
        _ => base,
    }
}

/// The zoom in percent as shown: the actual factor of the last layout (so the fit modes show
/// what they amount to), else the factor of a custom zoom. `None` for a fit mode before its
/// first layout.
pub fn zoom_percent(effective: Option<f32>, mode: ZoomMode) -> Option<u32> {
    match (effective, mode) {
        (Some(z), _) | (None, ZoomMode::Custom(z)) if z.is_finite() && z > 0.0 => {
            Some((z * 100.0).round() as u32)
        }
        _ => None,
    }
}

/// The text of the zoom menu button: "137% ⏷", or the fit mode's name before the first layout.
pub fn zoom_button_text(effective: Option<f32>, mode: ZoomMode) -> String {
    let text = match zoom_percent(effective, mode) {
        Some(p) => format!("{p}%"),
        None => ZoomChoice::for_mode(mode).map_or_else(String::new, ZoomChoice::label),
    };
    format!("{text} ⏷")
}

/// One entry of the zoom menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoomChoice {
    /// Fit the widest page to the width.
    FitWidth,
    /// Fit the current page.
    FitPage,
    /// A fixed zoom, in percent.
    Percent(u32),
}

/// The zoom menu from top to bottom; `None` is a separator.
pub fn zoom_menu() -> Vec<Option<ZoomChoice>> {
    let mut items = vec![Some(ZoomChoice::FitWidth), Some(ZoomChoice::FitPage), None];
    items.extend(ZOOM_PRESETS.iter().map(|p| Some(ZoomChoice::Percent(*p))));
    items
}

impl ZoomChoice {
    /// The fit choice of a fit mode.
    fn for_mode(mode: ZoomMode) -> Option<ZoomChoice> {
        match mode {
            ZoomMode::FitWidth => Some(ZoomChoice::FitWidth),
            ZoomMode::FitPage => Some(ZoomChoice::FitPage),
            ZoomMode::Custom(_) => None,
        }
    }

    /// The row of the command table this choice runs, if it has one.
    pub fn command(self) -> Option<&'static Command> {
        match self {
            ZoomChoice::FitWidth => commands::command("view.fit_width"),
            ZoomChoice::FitPage => commands::command("view.fit_page"),
            ZoomChoice::Percent(p) => commands::preset_command(p),
        }
    }

    /// The displayed name: the command's name for the fit modes, the number for a preset.
    pub fn label(self) -> String {
        match (self, self.command()) {
            (ZoomChoice::Percent(p), _) => format!("{p}%"),
            (_, Some(c)) => tr(c.name_key).to_owned(),
            (_, None) => String::new(),
        }
    }

    /// The displayed shortcut, from the command table.
    pub fn shortcut(self, ctx: &egui::Context) -> Option<String> {
        self.command()
            .and_then(|c| c.shortcut)
            .map(|s| ctx.format_shortcut(&s))
    }

    /// The action of choosing it for the focused view.
    pub fn action(self, state: &AppState) -> Option<Action> {
        match (self, self.command()) {
            (_, Some(c)) => c.run(state),
            (ZoomChoice::Percent(p), None) => commands::zoom_preset(state, p),
            (_, None) => None,
        }
    }

    /// Whether it is the current mode or the current zoom (a fit mode that happens to give
    /// 100% marks both).
    pub fn is_current(self, effective: Option<f32>, mode: ZoomMode) -> bool {
        match self {
            ZoomChoice::FitWidth => mode == ZoomMode::FitWidth,
            ZoomChoice::FitPage => mode == ZoomMode::FitPage,
            ZoomChoice::Percent(p) => zoom_percent(effective, mode) == Some(p),
        }
    }
}

/// "Preloading 123 / 660".
pub fn preload_text(progress: Progress) -> String {
    tr("status.preloading")
        .replace("{done}", &progress.done.to_string())
        .replace("{total}", &progress.total.to_string())
}

/// The document state, for states other than ready.
pub fn status_text(status: &DocStatus) -> Option<String> {
    match status {
        DocStatus::Ready => None,
        DocStatus::Loading => Some(tr("doc.loading").to_owned()),
        DocStatus::NeedsPassword => Some(tr("doc.needs_password").to_owned()),
        DocStatus::Failed(message) => Some(tr("doc.failed").replace("{}", message)),
    }
}

/// The zero-based page for what was typed: a 1-based number, limited to the document, or a page
/// label. `None` for anything else.
pub fn parse_page_input(text: &str, count: u32, labels: &[String]) -> Option<u32> {
    let text = text.trim();
    if count == 0 || text.is_empty() {
        return None;
    }
    if let Ok(n) = text.parse::<i64>() {
        return Some((n.clamp(1, i64::from(count)) - 1) as u32);
    }
    labels
        .iter()
        .position(|l| l == text)
        .map(|i| (i as u32).min(count - 1))
}

/// A command's name with its shortcut: "Zoom In (Ctrl+=)".
fn command_hint(ctx: &egui::Context, command: &Command) -> String {
    match command.shortcut {
        Some(s) => format!("{} ({})", tr(command.name_key), ctx.format_shortcut(&s)),
        None => tr(command.name_key).to_owned(),
    }
}

/// Draws the status bar for `tab` (the focused tab of the window it belongs to).
pub fn show(ui: &mut Ui, state: &mut AppState, tab: Option<TabId>, queue: &mut Vec<Action>) {
    let Some(tab_id) = tab else {
        return;
    };
    if let Some(Tab::Diff(diff)) = state.tabs.get(tab_id) {
        // A comparison tab: where each side is, and the state of the comparison (WP-62).
        let side_text = |pdf: &crate::state::PdfTab| {
            let count = state
                .docs
                .get(pdf.doc)
                .and_then(|d| d.info())
                .map_or(0, |i| i.page_sizes.len() as u32);
            page_text(pdf.ui.current_page, count, &[])
        };
        let text = format!("{} | {}", side_text(&diff.left), side_text(&diff.right));
        ui.horizontal(|ui| {
            ui.label(text);
            ui.separator();
            ui.label(diff_view::status_text(diff, &state.docs));
        });
        return;
    }
    let Some(Tab::Pdf(tab)) = state.tabs.get(tab_id) else {
        return;
    };
    let doc_id = tab.doc;
    let Some(doc) = state.docs.get(doc_id) else {
        return;
    };
    let status = doc.status();
    let info = doc.info();
    let (count, labels) = info.as_ref().map_or((0, &[][..]), |i| {
        (i.page_sizes.len() as u32, i.page_labels.as_slice())
    });

    ui.horizontal(|ui| {
        history_buttons(ui, state, tab_id, queue);
        ui.separator();
        let editing = page_number(ui, state, tab_id, count, labels, queue, info.is_some());
        if !editing && info.is_some() {
            ui.separator();
            zoom_controls(ui, state, tab_id, queue);
            ui.separator();
            crate::overlays::translate::status_button(ui, state, queue);
            if let Some(progress) = state.preload.progress(doc_id) {
                ui.separator();
                ui.weak(preload_text(progress));
            }
        }
        if let Some(text) = status_text(&status) {
            ui.separator();
            ui.label(text);
        }
        // What the link under the pointer leads to (LINK-5, LINK-7).
        let hover = state
            .tabs
            .pdf(tab_id)
            .and_then(|t| t.ui.links.hover_text.clone());
        if let Some(text) = hover {
            ui.separator();
            ui.add(egui::Label::new(text).truncate());
        }
    });
}

/// `[←] [→]`: back and forward in the history of `tab_id` (LINK-2). Disabled where there is
/// nowhere to go; the hover text names the shortcut.
fn history_buttons(ui: &mut Ui, state: &AppState, tab_id: TabId, queue: &mut Vec<Action>) {
    let Some(tab) = state.tabs.pdf(tab_id) else {
        return;
    };
    let history = &tab.view.history;
    let steps = [
        ("nav.back", "←", history.can_back(), Action::Back(tab_id)),
        (
            "nav.forward",
            "→",
            history.can_forward(),
            Action::Forward(tab_id),
        ),
    ];
    for (id, symbol, enabled, action) in steps {
        let Some(command) = commands::command(id) else {
            continue;
        };
        let hint = command_hint(ui.ctx(), command);
        let response = ui
            .add_enabled(enabled, egui::Button::new(symbol))
            .on_hover_text(hint.clone())
            .on_disabled_hover_text(hint);
        if response.clicked() {
            queue.push(action);
        }
    }
}

/// The page number, or the page number field while it is open. Returns whether the field is
/// open.
fn page_number(
    ui: &mut Ui,
    state: &mut AppState,
    tab_id: TabId,
    count: u32,
    labels: &[String],
    queue: &mut Vec<Action>,
    ready: bool,
) -> bool {
    let Some(Tab::Pdf(tab)) = state.tabs.get_mut(tab_id) else {
        return false;
    };
    match &mut tab.ui.goto {
        Some(input) => {
            ui.label(tr("status.goto"));
            let field = ui.add(
                egui::TextEdit::singleline(&mut input.text)
                    .desired_width(64.0)
                    .hint_text(format!("1–{count}")),
            );
            if !input.focused_once {
                field.request_focus();
                input.focused_once = true;
            }
            let (enter, escape) =
                ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
            if field.lost_focus() || escape {
                if enter && let Some(page) = parse_page_input(&input.text, count, labels) {
                    queue.push(Action::GoToPage { tab: tab_id, page });
                }
                tab.ui.goto = None;
            }
            true
        }
        None => {
            if ready {
                let text = page_text(tab.ui.current_page, count, labels);
                let hint = match commands::shortcut_text(ui.ctx(), "view.goto_page") {
                    Some(keys) => format!("{} ({keys})", tr("cmd.view.goto_page")),
                    None => tr("cmd.view.goto_page").to_owned(),
                };
                if ui.link(text).on_hover_text(hint).clicked() {
                    tab.ui.goto = Some(GotoInput::default());
                }
            }
            false
        }
    }
}

/// `[−] [137% ⏷] [+]`.
fn zoom_controls(ui: &mut Ui, state: &AppState, tab_id: TabId, queue: &mut Vec<Action>) {
    let Some(tab) = state.tabs.pdf(tab_id) else {
        return;
    };
    let (effective, mode) = (tab.ui.effective_zoom, tab.view.zoom);
    command_button(ui, state, "view.zoom_out", "−", queue);
    ui.menu_button(zoom_button_text(effective, mode), |ui| {
        for item in zoom_menu() {
            let Some(choice) = item else {
                ui.separator();
                continue;
            };
            let mut button =
                egui::Button::new(choice.label()).selected(choice.is_current(effective, mode));
            if let Some(keys) = choice.shortcut(ui.ctx()) {
                button = button.shortcut_text(keys);
            }
            if ui.add(button).clicked() {
                queue.extend(choice.action(state));
                ui.close();
            }
        }
        ui.separator();
        ui.label(egui::RichText::new(tr("status.zoom_hint")).small().weak());
    })
    .response
    .on_hover_text(tr("status.zoom_choose"));
    command_button(ui, state, "view.zoom_in", "+", queue);
}

/// A small button that runs a command; its name and shortcut show on hover.
fn command_button(ui: &mut Ui, state: &AppState, id: &str, symbol: &str, queue: &mut Vec<Action>) {
    let Some(command) = commands::command(id) else {
        return;
    };
    let hint = command_hint(ui.ctx(), command);
    let response = ui
        .add_enabled((command.enabled)(state), egui::Button::new(symbol))
        .on_hover_text(hint.clone())
        .on_disabled_hover_text(hint);
    if response.clicked() {
        queue.extend(command.run(state));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, lookup};

    fn labels(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// The expected text in the current language for a template.
    fn template(key: &str) -> &'static str {
        lookup(crate::i18n::current_lang(), key).expect("key exists")
    }

    #[test]
    fn page_text_shows_current_and_count() {
        let expected = template("status.page")
            .replace("{page}", "3")
            .replace("{count}", "10");
        assert_eq!(page_text(Some(2), 10, &[]), expected);
        // Out of range current page is limited to the last one.
        let last = template("status.page")
            .replace("{page}", "10")
            .replace("{count}", "10");
        assert_eq!(page_text(Some(20), 10, &[]), last);
        // Unknown current page: the count alone.
        assert_eq!(
            page_text(None, 10, &[]),
            template("doc.pages").replace("{}", "10")
        );
    }

    #[test]
    fn page_text_adds_a_label_that_differs_from_the_number() {
        let l = labels(&["i", "ii", "3", "4"]);
        assert!(page_text(Some(1), 4, &l).ends_with(" (ii)"));
        assert!(!page_text(Some(2), 4, &l).contains('('));
    }

    #[test]
    fn zoom_button_shows_the_actual_percent() {
        assert_eq!(zoom_button_text(None, ZoomMode::Custom(1.5)), "150% ⏷");
        assert_eq!(zoom_button_text(Some(2.0), ZoomMode::Custom(2.0)), "200% ⏷");
        // A fit mode shows what it amounts to.
        assert_eq!(zoom_button_text(Some(1.374), ZoomMode::FitWidth), "137% ⏷");
        // Before its first layout, its name.
        let fit = lookup(crate::i18n::current_lang(), "cmd.view.fit_page").expect("key");
        assert_eq!(
            zoom_button_text(None, ZoomMode::FitPage),
            format!("{fit} ⏷")
        );
        assert_eq!(zoom_percent(Some(f32::NAN), ZoomMode::FitWidth), None);
    }

    #[test]
    fn zoom_percent_matches_the_laid_out_scale_in_fit_modes() {
        use crate::views::pdf_view::PdfViewUiState;
        use refdesk_core::layout::PIXELS_PER_POINT_AT_100;
        use refdesk_core::{Size, ViewState};
        let sizes = vec![Size::new(595.0, 842.0); 5];
        for mode in [
            ZoomMode::FitWidth,
            ZoomMode::FitPage,
            ZoomMode::Custom(1.25),
        ] {
            let mut view = ViewState::new(std::path::PathBuf::from("/refdesk-test/a.pdf"));
            view.zoom = mode;
            let mut ui = PdfViewUiState::default();
            ui.sync(&view, &sizes, 0, Size::new(1000.0, 700.0));
            ui.commit(&mut view);
            let scale = ui.layout().expect("layout").scale();
            let expected = (scale / PIXELS_PER_POINT_AT_100 * 100.0).round() as u32;
            assert_eq!(
                zoom_percent(ui.effective_zoom, mode),
                Some(expected),
                "{mode:?}"
            );
            assert_eq!(
                zoom_button_text(ui.effective_zoom, mode),
                format!("{expected}% ⏷")
            );
        }
    }

    #[test]
    fn zoom_menu_lists_fit_modes_then_presets() {
        let menu = zoom_menu();
        assert_eq!(
            menu,
            vec![
                Some(ZoomChoice::FitWidth),
                Some(ZoomChoice::FitPage),
                None,
                Some(ZoomChoice::Percent(50)),
                Some(ZoomChoice::Percent(75)),
                Some(ZoomChoice::Percent(100)),
                Some(ZoomChoice::Percent(125)),
                Some(ZoomChoice::Percent(150)),
                Some(ZoomChoice::Percent(200)),
                Some(ZoomChoice::Percent(400)),
            ]
        );
        // Names and shortcuts come from the command table.
        let ctx = egui::Context::default();
        let lang = crate::i18n::current_lang();
        assert_eq!(
            ZoomChoice::FitWidth.label(),
            lookup(lang, "cmd.view.fit_width").expect("key")
        );
        assert_eq!(
            ZoomChoice::FitPage.label(),
            lookup(lang, "cmd.view.fit_page").expect("key")
        );
        assert_eq!(ZoomChoice::Percent(75).label(), "75%");
        assert_eq!(
            ZoomChoice::Percent(100).shortcut(&ctx),
            commands::shortcut_text(&ctx, "view.zoom_reset")
        );
        assert!(ZoomChoice::Percent(100).shortcut(&ctx).is_some());
        assert_eq!(ZoomChoice::Percent(50).shortcut(&ctx), None);
        assert_eq!(ZoomChoice::FitWidth.shortcut(&ctx), None);
    }

    #[test]
    fn every_zoom_menu_entry_sets_its_zoom() {
        use crate::action::{OpenTarget, dispatch};
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        // Nothing to zoom on the start page.
        assert!(
            zoom_menu()
                .into_iter()
                .flatten()
                .all(|c| c.action(&state).is_none())
        );
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/a.pdf")]),
            &ctx,
        );
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        let set = |zoom| Some(Action::SetZoom { tab, zoom });
        assert_eq!(ZoomChoice::FitWidth.action(&state), set(ZoomMode::FitWidth));
        assert_eq!(ZoomChoice::FitPage.action(&state), set(ZoomMode::FitPage));
        for p in ZOOM_PRESETS {
            assert_eq!(
                ZoomChoice::Percent(*p).action(&state),
                set(ZoomMode::Custom(*p as f32 / 100.0)),
                "{p}%"
            );
        }
    }

    #[test]
    fn the_current_mode_and_zoom_are_marked() {
        let marked = |effective, mode| -> Vec<ZoomChoice> {
            zoom_menu()
                .into_iter()
                .flatten()
                .filter(|c| c.is_current(effective, mode))
                .collect()
        };
        assert_eq!(
            marked(Some(1.37), ZoomMode::FitWidth),
            vec![ZoomChoice::FitWidth]
        );
        assert_eq!(
            marked(Some(1.0), ZoomMode::FitPage),
            vec![ZoomChoice::FitPage, ZoomChoice::Percent(100)]
        );
        assert_eq!(
            marked(Some(1.5), ZoomMode::Custom(1.5)),
            vec![ZoomChoice::Percent(150)]
        );
        assert_eq!(
            marked(None, ZoomMode::Custom(0.75)),
            vec![ZoomChoice::Percent(75)]
        );
        assert!(marked(Some(1.1), ZoomMode::Custom(1.1)).is_empty());
    }

    #[test]
    fn preload_text_shows_done_and_total() {
        let expected = template("status.preloading")
            .replace("{done}", "123")
            .replace("{total}", "660");
        assert_eq!(
            preload_text(Progress {
                done: 123,
                total: 660
            }),
            expected
        );
    }

    #[test]
    fn the_zoom_buttons_and_menu_draw_without_a_window() {
        use crate::action::{OpenTarget, dispatch};
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        dispatch(
            &mut state,
            Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/a.pdf")]),
            &ctx,
        );
        let (tab, _) = state.focused_pdf().expect("pdf tab");
        for _ in 0..2 {
            let mut queue = Vec::new();
            ctx.run_ui(egui::RawInput::default(), |ui| {
                zoom_controls(ui, &state, tab, &mut queue);
            })
            .drop_without_applying_deltas();
            assert!(queue.is_empty(), "nothing was clicked");
        }
    }

    #[test]
    fn status_text_only_for_states_other_than_ready() {
        assert_eq!(status_text(&DocStatus::Ready), None);
        assert!(status_text(&DocStatus::Loading).is_some());
        let failed = status_text(&DocStatus::Failed("broken xref".into())).expect("text");
        assert!(failed.contains("broken xref"));
    }

    #[test]
    fn page_input_is_one_based_and_limited_to_the_document() {
        assert_eq!(parse_page_input("1", 10, &[]), Some(0));
        assert_eq!(parse_page_input(" 7 ", 10, &[]), Some(6));
        assert_eq!(parse_page_input("99", 10, &[]), Some(9));
        assert_eq!(parse_page_input("0", 10, &[]), Some(0));
        assert_eq!(parse_page_input("-4", 10, &[]), Some(0));
        assert_eq!(parse_page_input("", 10, &[]), None);
        assert_eq!(parse_page_input("abc", 10, &[]), None);
        assert_eq!(parse_page_input("3", 0, &[]), None);
    }

    #[test]
    fn page_input_accepts_page_labels() {
        let l = labels(&["i", "ii", "A-1"]);
        assert_eq!(parse_page_input("A-1", 3, &l), Some(2));
        assert_eq!(parse_page_input("ii", 3, &l), Some(1));
        assert_eq!(parse_page_input("B-1", 3, &l), None);
    }

    #[test]
    fn all_status_texts_exist_in_both_languages() {
        for key in [
            "status.page",
            "doc.pages",
            "status.goto",
            "cmd.view.goto_page",
            "doc.password_submit",
            "doc.password_wrong",
            "status.preloading",
            "status.zoom_choose",
            "status.zoom_hint",
        ] {
            for lang in [Lang::Ko, Lang::En] {
                assert!(lookup(lang, key).is_some(), "{key} {lang:?}");
            }
        }
    }
}
