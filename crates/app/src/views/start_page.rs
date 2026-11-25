//! The start page (SES-7) and the page shown in place of a document that could not be opened
//! (SES-6).
//!
//! Both only draw and queue actions. The data comes from `session::start_page`, which checks
//! whether the recent files exist when it builds the list, not every frame.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{RichText, Ui};

use crate::action::{Action, OpenTarget};
use crate::i18n::tr;
use crate::state::TabId;

/// Width of the column the start page is laid out in, in points.
const COLUMN_WIDTH: f32 = 560.0;

/// One recently opened document.
#[derive(Clone, Debug, PartialEq)]
pub struct RecentEntry {
    /// The file.
    pub path: PathBuf,
    /// Its file name.
    pub name: String,
    /// The folder it is in.
    pub folder: String,
    /// When it was last opened (Unix seconds).
    pub last_opened: i64,
    /// Whether the file existed when the list was built.
    pub exists: bool,
}

impl RecentEntry {
    /// An entry for `path`.
    pub fn new(path: &Path, last_opened: i64, exists: bool) -> RecentEntry {
        RecentEntry {
            path: path.to_path_buf(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            folder: path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            last_opened,
            exists,
        }
    }
}

/// Everything the start page shows.
#[derive(Clone, Debug, Default)]
pub struct StartPage {
    /// Recent documents, newest first.
    pub recent: Arc<Vec<RecentEntry>>,
    /// Recent workspace folders, newest first (WS-1).
    pub recent_folders: Vec<PathBuf>,
    /// Command name and shortcut text of a few common commands.
    pub shortcuts: Vec<(String, String)>,
    /// Unix seconds now, for the "3 days ago" texts.
    pub now: i64,
}

/// "just now", "5 min ago", "3 days ago", … for a time `then` seen at `now` (Unix seconds).
pub fn relative_time(now: i64, then: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    let ago = now.saturating_sub(then).max(0);
    let (key, n) = if ago < MINUTE {
        return tr("time.just_now").to_owned();
    } else if ago < HOUR {
        ("time.minutes", ago / MINUTE)
    } else if ago < DAY {
        ("time.hours", ago / HOUR)
    } else if ago < 30 * DAY {
        ("time.days", ago / DAY)
    } else if ago < 365 * DAY {
        ("time.months", ago / (30 * DAY))
    } else {
        ("time.years", ago / (365 * DAY))
    };
    tr(key).replace("{n}", &n.to_string())
}

/// The second line of a recent entry: its folder and when it was opened, or that it is gone.
pub fn entry_detail(entry: &RecentEntry, now: i64) -> String {
    let when = if entry.exists {
        relative_time(now, entry.last_opened)
    } else {
        tr("start.missing").to_owned()
    };
    format!("{} · {when}", entry.folder)
}

/// Draws the start page: the name, an open button, the recent documents and a few shortcuts.
pub fn show(ui: &mut Ui, page: &StartPage, queue: &mut Vec<Action>) {
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.add_space(32.0);
            ui.vertical_centered(|ui| {
                ui.heading("refdesk");
                ui.label(tr("start.hint"));
                ui.add_space(8.0);
                // Two buttons side by side, centred.
                let width = ui.available_width();
                ui.horizontal(|ui| {
                    ui.add_space((width / 2.0 - 110.0).max(0.0));
                    if ui.button(tr("start.open_button")).clicked() {
                        queue.push(Action::OpenFileDialog);
                    }
                    if ui.button(tr("start.open_folder_button")).clicked() {
                        queue.push(Action::OpenFolderDialog);
                    }
                });
            });
            ui.add_space(24.0);
            let width = ui.available_width();
            let column = width.min(COLUMN_WIDTH);
            ui.horizontal(|ui| {
                ui.add_space((width - column) / 2.0);
                ui.vertical(|ui| {
                    ui.set_width(column);
                    if !page.recent_folders.is_empty() {
                        folder_list(ui, &page.recent_folders, queue);
                        ui.add_space(16.0);
                    }
                    recent_list(ui, page, queue);
                    if !page.shortcuts.is_empty() {
                        ui.add_space(24.0);
                        ui.strong(tr("start.shortcuts"));
                        egui::Grid::new("start-shortcuts")
                            .num_columns(2)
                            .spacing([24.0, 4.0])
                            .show(ui, |ui| {
                                for (name, keys) in &page.shortcuts {
                                    ui.label(name);
                                    ui.weak(keys);
                                    ui.end_row();
                                }
                            });
                    }
                });
            });
        });
}

/// The name and the parent folder a recent folder is listed with.
fn folder_label(path: &Path) -> (String, String) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let parent = path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    (name, parent)
}

fn folder_list(ui: &mut Ui, folders: &[PathBuf], queue: &mut Vec<Action>) {
    ui.strong(tr("start.recent_folders"));
    ui.add_space(4.0);
    for folder in folders {
        let (name, parent) = folder_label(folder);
        let clicked = ui
            .add(egui::Button::new(RichText::new(name)).frame(false))
            .on_hover_text(folder.display().to_string())
            .clicked();
        ui.label(RichText::new(parent).small().weak());
        ui.add_space(4.0);
        if clicked {
            queue.push(Action::OpenFolder(folder.clone()));
        }
    }
}

fn recent_list(ui: &mut Ui, page: &StartPage, queue: &mut Vec<Action>) {
    ui.strong(tr("start.recent"));
    ui.add_space(4.0);
    if page.recent.is_empty() {
        ui.weak(tr("start.no_recent"));
        return;
    }
    for entry in page.recent.iter() {
        let mut name = RichText::new(&entry.name);
        if !entry.exists {
            name = name.weak();
        }
        let clicked = ui
            .add(egui::Button::new(name).frame(false))
            .on_hover_text(entry.path.display().to_string())
            .clicked();
        ui.label(RichText::new(entry_detail(entry, page.now)).small().weak());
        ui.add_space(4.0);
        if clicked {
            queue.push(Action::OpenFiles(vec![OpenTarget {
                path: entry.path.clone(),
                page: None,
            }]));
        }
    }
}

/// Draws a PDF tab whose document could not be opened: the error, the path, and buttons to try
/// again (the file may be back) or close the tab.
pub fn show_failed_document(
    ui: &mut Ui,
    tab: TabId,
    path: &Path,
    message: &str,
    queue: &mut Vec<Action>,
) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.3);
        ui.colored_label(
            ui.visuals().error_fg_color,
            tr("doc.failed").replace("{}", message),
        );
        ui.label(format!("{}: {}", tr("doc.path"), path.display()));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            // Centre the two buttons.
            let width = ui.available_width();
            ui.add_space((width / 2.0 - 100.0).max(0.0));
            if ui.button(tr("doc.retry")).clicked() {
                queue.push(Action::RetryOpen(tab));
            }
            if ui.button(tr("cmd.tab.close")).clicked() {
                queue.push(Action::CloseTab(tab));
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{current_lang, lookup};

    fn text(key: &str) -> &'static str {
        lookup(current_lang(), key).expect("key")
    }

    #[test]
    fn relative_times_pick_the_largest_whole_unit() {
        let now = 1_000_000_000;
        assert_eq!(relative_time(now, now), text("time.just_now"));
        assert_eq!(relative_time(now, now - 59), text("time.just_now"));
        // A time in the future (clock changed) is "just now", not negative.
        assert_eq!(relative_time(now, now + 500), text("time.just_now"));
        assert_eq!(
            relative_time(now, now - 5 * 60),
            text("time.minutes").replace("{n}", "5")
        );
        assert_eq!(
            relative_time(now, now - 3 * 3600 - 10),
            text("time.hours").replace("{n}", "3")
        );
        assert_eq!(
            relative_time(now, now - 2 * 86400),
            text("time.days").replace("{n}", "2")
        );
        assert_eq!(
            relative_time(now, now - 65 * 86400),
            text("time.months").replace("{n}", "2")
        );
        assert_eq!(
            relative_time(now, now - 800 * 86400),
            text("time.years").replace("{n}", "2")
        );
    }

    #[test]
    fn an_entry_names_its_file_and_folder_and_marks_missing_files() {
        let entry = RecentEntry::new(Path::new("/docs/specs/a.pdf"), 100, true);
        assert_eq!(entry.name, "a.pdf");
        assert_eq!(entry.folder, "/docs/specs");
        assert_eq!(
            entry_detail(&entry, 100 + 120),
            format!("/docs/specs · {}", text("time.minutes").replace("{n}", "2"))
        );
        let gone = RecentEntry::new(Path::new("/docs/b.pdf"), 100, false);
        assert_eq!(
            entry_detail(&gone, 200),
            format!("/docs · {}", text("start.missing"))
        );
    }
}
