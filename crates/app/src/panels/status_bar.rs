//! The status bar at the bottom of a window: current page / page count (with the page label),
//! zoom, and the document state. Clicking the page number (or Ctrl+G) opens a page number field;
//! Enter jumps to the top of that page, Esc cancels.

use egui::{Key, Ui};
use refdesk_core::{DocStatus, ZoomMode};

use crate::action::Action;
use crate::i18n::tr;
use crate::state::{AppState, Tab};

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

/// "125%" plus the fit mode name, when there is one.
pub fn zoom_text(effective: Option<f32>, mode: ZoomMode) -> String {
    let percent = match (effective, mode) {
        (Some(z), _) | (None, ZoomMode::Custom(z)) => format!("{}%", (z * 100.0).round()),
        (None, _) => String::new(),
    };
    let mode = match mode {
        ZoomMode::FitWidth => tr("cmd.view.fit_width"),
        ZoomMode::FitPage => tr("cmd.view.fit_page"),
        ZoomMode::Custom(_) => "",
    };
    match (percent.is_empty(), mode.is_empty()) {
        (_, true) => percent,
        (true, false) => mode.to_owned(),
        (false, false) => format!("{percent} · {mode}"),
    }
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

/// Draws the status bar for the focused tab.
pub fn show(ui: &mut Ui, state: &mut AppState, queue: &mut Vec<Action>) {
    let AppState {
        docs, tabs, focus, ..
    } = state;
    let Some(tab_id) = *focus else {
        return;
    };
    let Some(Tab::Pdf(tab)) = tabs.get_mut(tab_id) else {
        return;
    };
    let Some(doc) = docs.get(tab.doc) else {
        return;
    };
    let status = doc.status();
    let info = doc.info();
    let (count, labels) = info.as_ref().map_or((0, &[][..]), |i| {
        (i.page_sizes.len() as u32, i.page_labels.as_slice())
    });

    ui.horizontal(|ui| {
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
            }
            None => {
                if info.is_some() {
                    let text = page_text(tab.ui.current_page, count, labels);
                    let hint = match crate::commands::shortcut_text(ui.ctx(), "view.goto_page") {
                        Some(keys) => format!("{} ({keys})", tr("cmd.view.goto_page")),
                        None => tr("cmd.view.goto_page").to_owned(),
                    };
                    if ui.link(text).on_hover_text(hint).clicked() {
                        tab.ui.goto = Some(GotoInput::default());
                    }
                    ui.separator();
                    ui.label(zoom_text(tab.ui.effective_zoom, tab.view.zoom));
                }
            }
        }
        if let Some(text) = status_text(&status) {
            ui.separator();
            ui.label(text);
        }
    });
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
    fn zoom_text_shows_percent_and_mode() {
        let fit = lookup(crate::i18n::current_lang(), "cmd.view.fit_width").expect("key");
        assert_eq!(
            zoom_text(Some(1.234), ZoomMode::FitWidth),
            format!("123% · {fit}")
        );
        assert_eq!(zoom_text(None, ZoomMode::Custom(1.5)), "150%");
        assert_eq!(zoom_text(Some(2.0), ZoomMode::Custom(2.0)), "200%");
        assert_eq!(zoom_text(None, ZoomMode::FitWidth), fit);
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
        ] {
            for lang in [Lang::Ko, Lang::En] {
                assert!(lookup(lang, key).is_some(), "{key} {lang:?}");
            }
        }
    }
}
