//! `RefdeskApp`: the `eframe::App` implementation (`docs/ARCHITECTURE.md` §5.2).
//!
//! One frame:
//! 1. `logic()` turns outside events (the file chooser's result) into actions.
//! 2. `ui()` draws; widgets only push actions onto the queue.
//! 3. The end of `ui()` dispatches the queue. Structure changes happen only there (rule R-6).

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use egui::{CentralPanel, Panel, ScrollArea, Ui};
use refdesk_core::{DocStatus, DocumentHandle};

use crate::action::{Action, OpenTarget, dispatch_queued};
use crate::commands::{self, MenuGroup};
use crate::i18n::tr;
use crate::state::{AppState, PdfTab, Tab, TabId};
use crate::theme;

/// The application.
pub struct RefdeskApp {
    state: AppState,
    /// Receives the files chosen in the dialog. `Some` while a dialog is open.
    dialog: Option<Receiver<Vec<PathBuf>>>,
}

impl RefdeskApp {
    /// Creates the app and queues `initial` files (from the command line) for opening.
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Vec<OpenTarget>) -> RefdeskApp {
        theme::install(&cc.egui_ctx);
        let mut state = AppState::new();
        if !initial.is_empty() {
            state.actions.push(Action::OpenFiles(initial));
        }
        RefdeskApp {
            state,
            dialog: None,
        }
    }
}

impl eframe::App for RefdeskApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.state.file_dialog_requested) && self.dialog.is_none() {
            self.dialog = Some(spawn_file_dialog(ctx));
        }
        poll_file_dialog(&mut self.dialog, &mut self.state.actions);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut queue = std::mem::take(&mut self.state.actions);

        queue.extend(commands::handle_shortcuts(&ctx, &self.state));

        Panel::top("menu_bar").show(ui, |ui| menu_bar(ui, &self.state, &mut queue));
        Panel::top("tab_strip").show(ui, |ui| tab_strip(ui, &self.state, &mut queue));
        CentralPanel::default().show(ui, |ui| {
            tab_content(ui, &mut self.state, &mut queue);
        });

        self.state.actions.append(&mut queue);
        dispatch_queued(&mut self.state, &ctx);
    }
}

/// Opens the file chooser on its own thread so the UI never waits for it (rule R-1). The result
/// arrives on the returned channel; the thread then asks for a new frame.
fn spawn_file_dialog(ctx: &egui::Context) -> Receiver<Vec<PathBuf>> {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("file-dialog".into())
        .spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title(tr("dialog.title"))
                .add_filter(tr("dialog.filter"), &["pdf", "PDF"])
                .pick_files()
                .unwrap_or_default();
            // The receiver is gone only when the app is shutting down.
            let _ = tx.send(picked);
            ctx.request_repaint();
        });
    if let Err(err) = spawned {
        // `tx` was dropped with the closure; the poll sees a closed channel and gives up.
        log::error!("could not start the file dialog thread: {err}");
    }
    rx
}

/// Checks (without blocking) whether the dialog has finished and queues `OpenFiles` for its
/// result. Clears `dialog` once it is finished or its thread is gone.
fn poll_file_dialog(dialog: &mut Option<Receiver<Vec<PathBuf>>>, actions: &mut Vec<Action>) {
    let Some(rx) = dialog else {
        return;
    };
    match rx.try_recv() {
        Ok(paths) => {
            *dialog = None;
            if !paths.is_empty() {
                let targets = paths
                    .into_iter()
                    .map(|path| OpenTarget { path, page: None })
                    .collect();
                actions.push(Action::OpenFiles(targets));
            }
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => *dialog = None,
    }
}

/// The menu bar. Its entries are the rows of the command table.
fn menu_bar(ui: &mut Ui, state: &AppState, queue: &mut Vec<Action>) {
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
            });
        }
    });
}

/// The title shown on a tab.
fn tab_title(state: &AppState, tab: TabId) -> String {
    match state.tabs.get(tab) {
        Some(Tab::Pdf(pdf)) => state
            .docs
            .get(pdf.doc)
            .and_then(|doc| doc.path().file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| tr("tab.unknown").to_owned()),
        Some(Tab::Start) => tr("tab.start").to_owned(),
        None => tr("tab.unknown").to_owned(),
    }
}

/// The tab strip: click a title to switch, click × (or middle-click a title) to close.
fn tab_strip(ui: &mut Ui, state: &AppState, queue: &mut Vec<Action>) {
    ScrollArea::horizontal().show(ui, |ui| {
        ui.horizontal(|ui| {
            let window = state.main_window();
            for &tab in &window.tabs {
                let selected = window.active == Some(tab);
                let title = ui.selectable_label(selected, tab_title(state, tab));
                if title.clicked() {
                    queue.push(Action::FocusTab(tab));
                }
                if title.middle_clicked() {
                    queue.push(Action::CloseTab(tab));
                }
                // The start page is not closable.
                if matches!(state.tabs.get(tab), Some(Tab::Pdf(_)))
                    && ui
                        .small_button("×")
                        .on_hover_text(tr("cmd.tab.close"))
                        .clicked()
                {
                    queue.push(Action::CloseTab(tab));
                }
                ui.add_space(6.0);
            }
        });
    });
}

/// The content of the active tab.
fn tab_content(ui: &mut Ui, state: &mut AppState, queue: &mut Vec<Action>) {
    let Some(active) = state.main_window().active else {
        return;
    };
    // Disjoint fields: the tab is borrowed mutably, the document handle immutably.
    let AppState { docs, tabs, .. } = state;
    match tabs.get_mut(active) {
        Some(Tab::Pdf(tab)) => match docs.get(tab.doc) {
            Some(doc) => draw_pdf_tab(ui, tab, doc, queue),
            None => {
                ui.label(tr("tab.unknown"));
            }
        },
        Some(Tab::Start) => draw_start_page(ui, queue),
        None => {}
    }
}

/// Draws the content of a PDF tab: the file name and the state of the document.
///
/// **WP-15 replaces the body of this function with the PDF view.** It gets the tab (whose view
/// state it may change), the document handle, and the action queue.
pub fn draw_pdf_tab(ui: &mut Ui, tab: &mut PdfTab, doc: &DocumentHandle, queue: &mut Vec<Action>) {
    let _ = (&tab.view, &queue);
    let name = doc
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    ui.heading(name);
    ui.label(format!("{}: {}", tr("doc.path"), doc.path().display()));
    ui.add_space(8.0);
    match doc.status() {
        DocStatus::Loading => {
            ui.label(tr("doc.loading"));
        }
        DocStatus::Ready => {
            let pages = doc.info().map_or(0, |info| info.page_sizes.len());
            ui.label(tr("doc.pages").replace("{}", &pages.to_string()));
        }
        DocStatus::NeedsPassword => {
            ui.label(tr("doc.needs_password"));
        }
        DocStatus::Failed(message) => {
            ui.colored_label(
                ui.visuals().error_fg_color,
                tr("doc.failed").replace("{}", &message),
            );
        }
    }
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

    /// Draws every widget of the main window for a few frames without a window; nothing may panic
    /// and the widgets must not change the structure themselves.
    #[test]
    fn widgets_draw_for_start_page_and_pdf_tabs() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        for step in 0..2 {
            for _ in 0..3 {
                let mut queue = Vec::new();
                ctx.run_ui(egui::RawInput::default(), |ui| {
                    Panel::top("menu_bar").show(ui, |ui| menu_bar(ui, &state, &mut queue));
                    Panel::top("tab_strip").show(ui, |ui| tab_strip(ui, &state, &mut queue));
                    CentralPanel::default().show(ui, |ui| tab_content(ui, &mut state, &mut queue));
                })
                .drop_without_applying_deltas();
                assert!(queue.is_empty(), "nothing was clicked");
            }
            if step == 0 {
                crate::action::dispatch(
                    &mut state,
                    Action::OpenFiles(vec![OpenTarget::parse("/refdesk-test/missing.pdf")]),
                    &ctx,
                );
            }
        }
        assert!(state.focused_pdf().is_some());
    }

    #[test]
    fn a_finished_dialog_becomes_an_open_files_action() {
        let (tx, rx) = mpsc::channel();
        let mut dialog = Some(rx);
        let mut actions = Vec::new();

        // Nothing yet: still waiting, nothing queued.
        poll_file_dialog(&mut dialog, &mut actions);
        assert!(dialog.is_some() && actions.is_empty());

        tx.send(vec![PathBuf::from("/refdesk-test/a.pdf")])
            .expect("receiver alive");
        poll_file_dialog(&mut dialog, &mut actions);
        assert!(dialog.is_none());
        assert_eq!(
            actions,
            vec![Action::OpenFiles(vec![OpenTarget {
                path: PathBuf::from("/refdesk-test/a.pdf"),
                page: None
            }])]
        );
    }

    #[test]
    fn a_cancelled_dialog_queues_nothing() {
        let (tx, rx) = mpsc::channel();
        let mut dialog = Some(rx);
        let mut actions = Vec::new();
        tx.send(Vec::new()).expect("receiver alive");
        poll_file_dialog(&mut dialog, &mut actions);
        assert!(dialog.is_none() && actions.is_empty());
    }

    #[test]
    fn a_dead_dialog_thread_clears_the_dialog() {
        let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
        drop(tx);
        let mut dialog = Some(rx);
        let mut actions = Vec::new();
        poll_file_dialog(&mut dialog, &mut actions);
        assert!(dialog.is_none() && actions.is_empty());
    }
}
