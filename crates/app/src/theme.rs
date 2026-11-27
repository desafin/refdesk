//! Look and feel: the interface theme, page colours (VIEW-7) and fonts.
//!
//! The theme (follow the system, light, dark) and the page colours (original, dark, follow the
//! theme) are picked in the View menu and kept in `prefs.json`; a theme there wins over the
//! `theme` of `config.toml`. The page colours in effect depend on whether the interface is dark
//! right now (the system may switch), so they are worked out every frame
//! ([`sync_page_colors`]) and handed to the texture cache, which hands them to the documents.
//!
//! The decisions (menus, cycling, which colours, highlight colours on dark pages) are plain
//! functions tested without a window (R-10).

use egui::{Color32, Ui};
use refdesk_core::recolor::DARK_BACKGROUND;
use refdesk_core::store::prefs::effective_theme;
use refdesk_core::{PageColors, PageColorsPref, Theme, effective_page_colors};

use crate::action::Action;
use crate::i18n::tr;
use crate::state::{AppState, StatusNotice};

/// Applies the application theme once at start-up.
pub fn install(ctx: &egui::Context) {
    configure_fonts(ctx);
}

/// Font setup. Intentionally empty: eframe's default behavior is used for now. Korean glyph
/// coverage is being verified separately; adjust here when the result is in.
pub fn configure_fonts(_ctx: &egui::Context) {}

/// The theme in effect: the menu's choice, else `config.toml`'s.
pub fn theme(state: &AppState) -> Theme {
    effective_theme(&state.prefs, &state.config)
}

/// The page colours setting in effect.
pub fn page_colors_pref(state: &AppState) -> PageColorsPref {
    state.prefs.page_colors.unwrap_or_default()
}

/// Whether the interface is dark now (for "follow the system", what the system says).
pub fn ui_is_dark(ctx: &egui::Context) -> bool {
    ctx.theme() == egui::Theme::Dark
}

/// egui's preference for a theme setting.
pub fn theme_preference(theme: Theme) -> egui::ThemePreference {
    match theme {
        Theme::System => egui::ThemePreference::System,
        Theme::Light => egui::ThemePreference::Light,
        Theme::Dark => egui::ThemePreference::Dark,
    }
}

/// Gives egui the theme in effect.
pub fn apply_theme_setting(state: &AppState, ctx: &egui::Context) {
    ctx.set_theme(theme_preference(theme(state)));
}

/// Works out the page colours in effect and hands them to the texture cache. Called every frame
/// from `logic()` (the system theme may change at any time) and after a change in `dispatch`.
pub fn sync_page_colors(state: &mut AppState, ctx: &egui::Context) {
    let colors = effective_page_colors(page_colors_pref(state), ui_is_dark(ctx));
    state.textures.set_page_colors(colors);
}

/// The themes in menu order.
pub const THEMES: [Theme; 3] = [Theme::System, Theme::Light, Theme::Dark];

/// The theme after "cycle theme": system → light → dark → system.
pub fn cycle_theme(theme: Theme) -> Theme {
    match theme {
        Theme::System => Theme::Light,
        Theme::Light => Theme::Dark,
        Theme::Dark => Theme::System,
    }
}

/// The i18n key of a theme's name.
pub fn theme_name_key(theme: Theme) -> &'static str {
    match theme {
        Theme::System => "theme.system",
        Theme::Light => "theme.light",
        Theme::Dark => "theme.dark",
    }
}

/// The i18n key of a page colours setting's name.
pub fn page_colors_name_key(pref: PageColorsPref) -> &'static str {
    match pref {
        PageColorsPref::Original => "page_colors.original",
        PageColorsPref::Dark => "page_colors.dark",
        PageColorsPref::FollowTheme => "page_colors.follow_theme",
    }
}

/// One row of a choice menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuRow<T> {
    /// The value the row sets.
    pub value: T,
    /// The row is the current value (shown with a check mark).
    pub checked: bool,
}

/// The rows of "View ▸ Theme".
pub fn theme_menu(current: Theme) -> Vec<MenuRow<Theme>> {
    THEMES
        .iter()
        .map(|t| MenuRow {
            value: *t,
            checked: *t == current,
        })
        .collect()
}

/// The rows of "View ▸ Page Colors".
pub fn page_colors_menu(current: PageColorsPref) -> Vec<MenuRow<PageColorsPref>> {
    PageColorsPref::ALL
        .iter()
        .map(|p| MenuRow {
            value: *p,
            checked: *p == current,
        })
        .collect()
}

/// The text of a choice row: a check mark before the current one.
pub fn row_text(label: &str, checked: bool) -> String {
    if checked {
        format!("✓ {label}")
    } else {
        // An em space keeps the names aligned with the checked row.
        format!("\u{2003} {label}")
    }
}

/// "Theme ▸" and "Page Colors ▸" of the View menu (VIEW-7). The rows are buttons named by their
/// text, so they are found in the accessibility tree.
pub fn theme_menus(ui: &mut Ui, state: &AppState, queue: &mut Vec<Action>) {
    ui.menu_button(tr("menu.theme"), |ui| {
        for row in theme_menu(theme(state)) {
            let text = row_text(tr(theme_name_key(row.value)), row.checked);
            if ui
                .add(egui::Button::new(text).selected(row.checked))
                .clicked()
            {
                queue.push(Action::SetTheme(row.value));
                ui.close();
            }
        }
    });
    ui.menu_button(tr("menu.page_colors"), |ui| {
        for row in page_colors_menu(page_colors_pref(state)) {
            let text = row_text(tr(page_colors_name_key(row.value)), row.checked);
            if ui
                .add(egui::Button::new(text).selected(row.checked))
                .clicked()
            {
                queue.push(Action::SetPageColors(row.value));
                ui.close();
            }
        }
    });
}

/// Seconds the "theme" and "page colours" notices stay in the status bar.
const NOTICE_SECS: f64 = 2.0;

/// Saves `prefs.json` when there is a place for it. A failed save is only logged: the change
/// still applies until exit.
fn save_prefs(state: &AppState) {
    if let Some(paths) = &state.paths
        && let Err(err) = state.prefs.save(&paths.prefs_file())
    {
        log::warn!("could not save {}: {err}", paths.prefs_file().display());
    }
}

fn notice(state: &mut AppState, text: String, ctx: &egui::Context) {
    state.status_notice = Some(StatusNotice {
        text,
        until: ctx.input(|i| i.time) + NOTICE_SECS,
    });
    ctx.request_repaint();
}

/// `Action::SetTheme`: applies the theme, keeps it in `prefs.json` and tells the user.
pub(crate) fn set_theme(state: &mut AppState, theme: Theme, ctx: &egui::Context) {
    state.prefs.theme = Some(theme);
    apply_theme_setting(state, ctx);
    sync_page_colors(state, ctx);
    save_prefs(state);
    let text = tr("status.theme").replace("{}", tr(theme_name_key(theme)));
    notice(state, text, ctx);
}

/// `Action::SetPageColors`: applies the page colours, keeps them in `prefs.json` and tells the
/// user. The views draw the old pictures until the new ones arrive.
pub(crate) fn set_page_colors(state: &mut AppState, pref: PageColorsPref, ctx: &egui::Context) {
    state.prefs.page_colors = Some(pref);
    sync_page_colors(state, ctx);
    save_prefs(state);
    let text = tr("status.page_colors").replace("{}", tr(page_colors_name_key(pref)));
    notice(state, text, ctx);
}

/// The colour behind the pages of a view (the gaps between them). On dark pages it is dark
/// whatever the theme, so no light band flashes between them.
pub fn page_backdrop(colors: PageColors, ui_dark: bool) -> Color32 {
    match colors {
        PageColors::Dark => Color32::from_gray(52),
        PageColors::Original if ui_dark => Color32::from_gray(52),
        PageColors::Original => Color32::from_gray(140),
    }
}

/// The colour of a page that has no picture yet.
pub fn blank_page(colors: PageColors) -> Color32 {
    match colors {
        PageColors::Original => Color32::WHITE,
        PageColors::Dark => Color32::from_gray(DARK_BACKGROUND),
    }
}

/// Alpha of the selection tint.
const SELECTION_ALPHA: u8 = 110;

/// The selection tint over the pages. `base` is the theme's selection colour. On dark pages the
/// dark theme's deep blue nearly vanishes, so a lighter blue is used there (see the tests).
pub fn selection_tint(base: Color32, colors: PageColors) -> Color32 {
    let base = match colors {
        PageColors::Original => base,
        PageColors::Dark => Color32::from_rgb(70, 150, 255),
    };
    Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), SELECTION_ALPHA)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_menu_lists_three_themes_and_checks_the_current_one() {
        let rows = theme_menu(Theme::Light);
        let values: Vec<Theme> = rows.iter().map(|r| r.value).collect();
        assert_eq!(values, vec![Theme::System, Theme::Light, Theme::Dark]);
        let checked: Vec<Theme> = rows.iter().filter(|r| r.checked).map(|r| r.value).collect();
        assert_eq!(checked, vec![Theme::Light]);
    }

    #[test]
    fn the_page_colours_menu_lists_three_values_and_checks_the_current_one() {
        let rows = page_colors_menu(PageColorsPref::FollowTheme);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].value, PageColorsPref::Original);
        let checked: Vec<_> = rows.iter().filter(|r| r.checked).map(|r| r.value).collect();
        assert_eq!(checked, vec![PageColorsPref::FollowTheme]);
    }

    #[test]
    fn row_texts_mark_only_the_current_row() {
        assert_eq!(row_text("Dark", true), "✓ Dark");
        assert_eq!(row_text("Dark", false), "\u{2003} Dark");
    }

    #[test]
    fn cycling_visits_every_theme_and_comes_back() {
        let mut t = Theme::System;
        let mut seen = Vec::new();
        for _ in 0..3 {
            t = cycle_theme(t);
            seen.push(t);
        }
        assert_eq!(seen, vec![Theme::Light, Theme::Dark, Theme::System]);
    }

    #[test]
    fn every_name_has_both_languages() {
        use crate::i18n::{Lang, lookup};
        let keys = THEMES
            .iter()
            .map(|t| theme_name_key(*t))
            .chain(PageColorsPref::ALL.iter().map(|p| page_colors_name_key(*p)))
            .chain([
                "menu.theme",
                "menu.page_colors",
                "status.theme",
                "status.page_colors",
            ]);
        for key in keys {
            for lang in [Lang::Ko, Lang::En] {
                assert!(lookup(lang, key).is_some(), "{key} {lang:?}");
            }
        }
        assert_eq!(
            lookup(Lang::En, "page_colors.follow_theme"),
            Some("Follow Theme")
        );
    }

    #[test]
    fn the_effective_page_colours_follow_the_interface() {
        // Three settings by a light and a dark interface, through egui's theme.
        for (pref, theme, want) in [
            (
                PageColorsPref::Original,
                egui::Theme::Light,
                PageColors::Original,
            ),
            (
                PageColorsPref::Original,
                egui::Theme::Dark,
                PageColors::Original,
            ),
            (PageColorsPref::Dark, egui::Theme::Light, PageColors::Dark),
            (PageColorsPref::Dark, egui::Theme::Dark, PageColors::Dark),
            (
                PageColorsPref::FollowTheme,
                egui::Theme::Light,
                PageColors::Original,
            ),
            (
                PageColorsPref::FollowTheme,
                egui::Theme::Dark,
                PageColors::Dark,
            ),
        ] {
            let ctx = egui::Context::default();
            ctx.set_theme(theme);
            let mut state = AppState::new();
            state.prefs.page_colors = Some(pref);
            sync_page_colors(&mut state, &ctx);
            assert_eq!(state.textures.page_colors(), want, "{pref:?} {theme:?}");
        }
    }

    #[test]
    fn the_backdrop_and_empty_pages_are_dark_on_dark_pages() {
        assert_eq!(blank_page(PageColors::Original), Color32::WHITE);
        assert_eq!(blank_page(PageColors::Dark), Color32::from_gray(0x1e));
        for ui_dark in [false, true] {
            let gap = page_backdrop(PageColors::Dark, ui_dark);
            assert!(gap.r() < 80, "{gap:?}");
        }
        assert_eq!(
            page_backdrop(PageColors::Original, false),
            Color32::from_gray(140)
        );
        assert_eq!(
            page_backdrop(PageColors::Original, true),
            Color32::from_gray(52)
        );
    }

    // Contrast of highlights. A highlight is a translucent colour over the page; it shows as
    // long as the tinted background differs enough from the plain one. The WCAG contrast ratio
    // of the two is measured; a highlight is "lost" on dark pages when it falls below both
    // 1.5 and what it reaches on a white page.

    fn linear(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn luminance(rgb: [f64; 3]) -> f64 {
        let l = |v: f64| linear(v.round().clamp(0.0, 255.0) as u8);
        0.2126 * l(rgb[0]) + 0.7152 * l(rgb[1]) + 0.0722 * l(rgb[2])
    }

    /// Contrast between `tint` painted over a page of grey `page` and the page itself.
    fn contrast_on(tint: Color32, page: u8) -> f64 {
        let [r, g, b, a] = tint.to_srgba_unmultiplied();
        let a = f64::from(a) / 255.0;
        let over = |c: u8| f64::from(c) * a + f64::from(page) * (1.0 - a);
        let lt = luminance([over(r), over(g), over(b)]);
        let lp = luminance([f64::from(page); 3]);
        (lt.max(lp) + 0.05) / (lt.min(lp) + 0.05)
    }

    fn lost_on_dark_pages(tint: Color32) -> bool {
        let dark = contrast_on(tint, DARK_BACKGROUND);
        dark < 1.5 && dark < contrast_on(tint, 255)
    }

    fn ui_visuals() -> [egui::Visuals; 2] {
        [egui::Visuals::light(), egui::Visuals::dark()]
    }

    #[test]
    fn the_selection_needs_its_own_colour_on_dark_pages_and_has_it() {
        let dark_theme_selection = egui::Visuals::dark().selection.bg_fill;
        // The theme's tint, as used on white pages, is lost on dark ones ...
        assert!(lost_on_dark_pages(selection_tint(
            dark_theme_selection,
            PageColors::Original
        )));
        // ... the dark-page tint is not, under either theme.
        for v in ui_visuals() {
            let tint = selection_tint(v.selection.bg_fill, PageColors::Dark);
            assert!(!lost_on_dark_pages(tint), "{tint:?}");
            assert!(contrast_on(tint, DARK_BACKGROUND) >= 1.5);
        }
    }

    #[test]
    fn the_other_highlights_still_show_on_dark_pages() {
        let (all, current, _) = crate::panels::search::highlight_colors();
        let (g_all, g_current, _) = crate::panels::search::global_highlight_colors();
        let mut tints = vec![all, current, g_all, g_current];
        // The link under the pointer (alpha 48 of the theme's link colour) and the band where a
        // jump lands (at its strongest).
        for v in ui_visuals() {
            let l = v.hyperlink_color;
            tints.push(Color32::from_rgba_unmultiplied(l.r(), l.g(), l.b(), 48));
        }
        tints.push(Color32::from_rgba_unmultiplied(255, 200, 40, 110));
        for tint in tints {
            assert!(!lost_on_dark_pages(tint), "{tint:?}");
        }
    }
}

/// Changing the theme and the page colours through `dispatch`, without a window.
#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::action::dispatch;
    use crate::action::scroll_tests::ScratchDir;
    use refdesk_core::Prefs;

    fn state_with_paths(dir: &ScratchDir) -> AppState {
        let mut state = AppState::new();
        state.paths = Some(dir.paths());
        state
    }

    #[test]
    fn setting_the_theme_changes_the_state_egui_and_the_saved_file() {
        let dir = ScratchDir::new("theme");
        let ctx = egui::Context::default();
        let mut state = state_with_paths(&dir);
        dispatch(&mut state, Action::SetTheme(Theme::Light), &ctx);
        assert_eq!(theme(&state), Theme::Light);
        assert_eq!(
            ctx.options(|o| o.theme_preference),
            egui::ThemePreference::Light
        );
        assert!(!ui_is_dark(&ctx));
        assert_eq!(
            Prefs::load(&dir.paths().prefs_file()).theme,
            Some(Theme::Light)
        );
        let notice = state.status_notice.clone().expect("a notice");
        assert!(notice.text.contains(tr("theme.light")), "{}", notice.text);

        dispatch(&mut state, Action::SetTheme(Theme::Dark), &ctx);
        assert!(ui_is_dark(&ctx));
        assert_eq!(
            Prefs::load(&dir.paths().prefs_file()).theme,
            Some(Theme::Dark)
        );
    }

    #[test]
    fn the_theme_preference_wins_over_the_config_file() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        state.config.theme = Theme::Dark;
        assert_eq!(theme(&state), Theme::Dark);
        dispatch(&mut state, Action::SetTheme(Theme::System), &ctx);
        assert_eq!(theme(&state), Theme::System);
        assert_eq!(
            ctx.options(|o| o.theme_preference),
            egui::ThemePreference::System
        );
    }

    #[test]
    fn setting_page_colours_changes_the_cache_and_the_saved_file() {
        let dir = ScratchDir::new("page-colors");
        let ctx = egui::Context::default();
        let mut state = state_with_paths(&dir);
        dispatch(
            &mut state,
            Action::SetPageColors(PageColorsPref::Dark),
            &ctx,
        );
        assert_eq!(state.textures.page_colors(), PageColors::Dark);
        assert_eq!(
            Prefs::load(&dir.paths().prefs_file()).page_colors,
            Some(PageColorsPref::Dark)
        );
        let notice = state.status_notice.clone().expect("a notice");
        assert!(
            notice.text.contains(tr("page_colors.dark")),
            "{}",
            notice.text
        );

        // Follow the theme: with a light theme the pages are original again, with a dark one dark.
        dispatch(
            &mut state,
            Action::SetPageColors(PageColorsPref::FollowTheme),
            &ctx,
        );
        dispatch(&mut state, Action::SetTheme(Theme::Light), &ctx);
        assert_eq!(state.textures.page_colors(), PageColors::Original);
        dispatch(&mut state, Action::SetTheme(Theme::Dark), &ctx);
        assert_eq!(state.textures.page_colors(), PageColors::Dark);
    }

    #[test]
    fn without_a_save_path_the_changes_still_apply() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        assert!(state.paths.is_none());
        dispatch(&mut state, Action::SetTheme(Theme::Light), &ctx);
        dispatch(
            &mut state,
            Action::SetPageColors(PageColorsPref::Dark),
            &ctx,
        );
        assert_eq!(theme(&state), Theme::Light);
        assert!(!ui_is_dark(&ctx));
        assert_eq!(state.textures.page_colors(), PageColors::Dark);
    }

    #[test]
    fn the_commands_toggle_page_colours_and_cycle_the_theme() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        let toggle = crate::commands::command("view.toggle_page_colors").expect("command");
        let cycle = crate::commands::command("view.cycle_theme").expect("command");
        assert_eq!(
            toggle.run(&state),
            Some(Action::SetPageColors(PageColorsPref::Dark))
        );
        // Following a dark theme: the pages are dark, so the toggle fixes them to original.
        dispatch(&mut state, Action::SetTheme(Theme::Dark), &ctx);
        dispatch(
            &mut state,
            Action::SetPageColors(PageColorsPref::FollowTheme),
            &ctx,
        );
        assert_eq!(
            toggle.run(&state),
            Some(Action::SetPageColors(PageColorsPref::Original))
        );
        assert_eq!(cycle.run(&state), Some(Action::SetTheme(Theme::System)));
        dispatch(&mut state, Action::SetTheme(Theme::System), &ctx);
        assert_eq!(cycle.run(&state), Some(Action::SetTheme(Theme::Light)));
    }
}

/// Captures stay in their own colours while the views are dark (VIEW-7).
#[cfg(test)]
mod capture_tests {
    use std::sync::{Arc, Mutex};

    use refdesk_core::testutil::PdfBuilder;
    use refdesk_core::{DocStatus, DocumentHandle, Rect, RenderedImage};

    use super::*;
    use crate::overlays::capture::{self, Clipboard, Event, JobParams};
    use crate::views::test_support::watch_repaints;

    /// Keeps what it is given; `wl-copy` is never there.
    #[derive(Default)]
    struct KeepingClipboard(Mutex<Option<RenderedImage>>);

    impl Clipboard for KeepingClipboard {
        fn wl_copy_available(&self) -> bool {
            false
        }
        fn copy_png_with_wl_copy(&self, _png: &[u8]) -> Result<(), String> {
            Err("not here".to_string())
        }
        fn copy_image_with_egui(&self, image: &RenderedImage) -> Result<(), String> {
            *self.0.lock().map_err(|e| e.to_string())? = Some(image.clone());
            Ok(())
        }
    }

    #[test]
    fn a_capture_is_in_original_colours_while_the_pages_are_dark() {
        let pdf = PdfBuilder::new()
            .page(200.0, 200.0, |p| {
                p.text(20.0, 50.0, 20.0, "TEXT");
            })
            .write_temp("capture-colors");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let doc = DocumentHandle::open(pdf.path(), crate::action::notifier(&ctx));
        repaints.wait_until("ready", || doc.status() != DocStatus::Loading);
        assert_eq!(doc.status(), DocStatus::Ready);
        // The views are dark: the cache hands that to the document.
        doc.set_page_colors(PageColors::Dark);
        let clipboard = Arc::new(KeepingClipboard::default());
        let params = JobParams {
            page: 0,
            region: Rect::new(0.0, 0.0, 100.0, 100.0),
            scale_milli: 1000,
            doc_name: "capture-colors".to_string(),
            save_dir: None,
            always_save: false,
            stamp: capture::Stamp::now(),
        };
        let event = capture::run_job(&doc, clipboard.as_ref(), &params);
        assert!(matches!(event, Event::Captured(_)), "{event:?}");
        let image = clipboard.0.lock().expect("lock").clone().expect("copied");
        // The corner is the page's own white, not the dark background.
        assert_eq!(&image.rgb[..3], &[255, 255, 255]);
        assert_eq!(doc.page_colors(), PageColors::Dark, "the views stay dark");
    }
}
