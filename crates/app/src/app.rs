//! `RefdeskApp`: the `eframe::App` implementation (`docs/ARCHITECTURE.md` §5.2).
//!
//! One frame:
//! 1. `logic()` turns outside events (the file chooser's result) into actions.
//! 2. `ui()` draws every window (`windows::show_all`); widgets only push actions onto the queue.
//! 3. The end of `ui()` dispatches the queue. Structure changes happen only there (rule R-6).

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use egui::Ui;
use refdesk_core::{IpcListener, IpcRequest};

use crate::action::{Action, OpenTarget, dispatch_queued};
use crate::i18n::tr;
use crate::state::AppState;
use crate::theme;
use crate::views::{preload, textures};
use crate::windows;

/// The application.
pub struct RefdeskApp {
    state: AppState,
    /// Receives the files chosen in the dialog. `Some` while a dialog is open.
    dialog: Option<Receiver<Vec<PathBuf>>>,
    /// Requests from other `refdesk` processes (`refdesk_core::ipc`).
    remote: Receiver<IpcRequest>,
    /// Keeps the socket open for as long as the app lives.
    _listener: Option<IpcListener>,
}

impl RefdeskApp {
    /// Creates the app. `initial` is this process's own command line; it is handled like a
    /// request from another process, but the window is not raised. `remote` delivers the requests
    /// of other processes; `listener` is the open socket they arrive on, if there is one.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        initial: IpcRequest,
        remote: Receiver<IpcRequest>,
        listener: Option<IpcListener>,
    ) -> RefdeskApp {
        theme::install(&cc.egui_ctx);
        let mut state = AppState::new();
        state.actions.extend(actions_for_request(initial));
        RefdeskApp {
            state,
            dialog: None,
            remote,
            _listener: listener,
        }
    }
}

impl eframe::App for RefdeskApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.state.file_dialog_requested) && self.dialog.is_none() {
            self.dialog = Some(spawn_file_dialog(ctx));
        }
        poll_file_dialog(&mut self.dialog, &mut self.state.actions);
        if poll_remote_requests(&self.remote, &mut self.state.actions) {
            bring_to_front(ctx);
        }
        // WP-15: upload finished renders; drop textures of closed documents.
        textures::collect_rendered(ctx, &self.state.docs, &mut self.state.textures);
        // WP-17: render every page's preview in the background.
        preload::step(ctx, &mut self.state);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut queue = std::mem::take(&mut self.state.actions);

        // Every window, with its menu bar; shortcuts go to the window that has the keyboard.
        windows::show_all(ui, &mut self.state, &mut queue);

        self.state.actions.append(&mut queue);
        dispatch_queued(&mut self.state, &ctx);
    }
}

/// What a request asks the application to do.
fn actions_for_request(request: IpcRequest) -> Vec<Action> {
    match request {
        IpcRequest::Open { targets } if targets.is_empty() => Vec::new(),
        IpcRequest::Open { targets } => vec![Action::OpenFiles(targets)],
        IpcRequest::Diff { left, right } => vec![Action::StartDiff { left, right }],
    }
}

/// Moves the requests that other processes have sent so far into `actions`. Returns whether there
/// was any (even an empty `Open`, which only asks for the window to come forward).
fn poll_remote_requests(remote: &Receiver<IpcRequest>, actions: &mut Vec<Action>) -> bool {
    let mut any = false;
    while let Ok(request) = remote.try_recv() {
        any = true;
        actions.extend(actions_for_request(request));
    }
    any
}

/// Asks for the window to be shown. Wayland compositors may refuse a focus request from a window
/// that was not just activated, so the user's attention is requested too (GNOME then shows its
/// "is ready" notice).
fn bring_to_front(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
        egui::UserAttentionType::Informational,
    ));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Draws every widget of every window for a few frames without a window; nothing may panic
    /// and the widgets must not change the structure themselves.
    #[test]
    fn widgets_draw_for_start_page_and_pdf_tabs() {
        let ctx = egui::Context::default();
        let mut state = AppState::new();
        for step in 0..2 {
            for _ in 0..3 {
                let mut queue = Vec::new();
                ctx.run_ui(egui::RawInput::default(), |ui| {
                    windows::show_all(ui, &mut state, &mut queue);
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
                crate::action::dispatch(&mut state, Action::NewWindow, &ctx);
            }
        }
        assert_eq!(state.windows.len(), 2);
        assert!(
            state
                .main_window()
                .dock
                .tabs()
                .iter()
                .any(|t| state.tabs.pdf(*t).is_some())
        );
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

    fn target(path: &str, page: Option<u32>) -> OpenTarget {
        OpenTarget {
            path: PathBuf::from(path),
            page,
        }
    }

    #[test]
    fn an_open_request_becomes_open_files() {
        let targets = vec![
            target("/refdesk-test/a.pdf", Some(4)),
            target("/b.pdf", None),
        ];
        assert_eq!(
            actions_for_request(IpcRequest::Open {
                targets: targets.clone()
            }),
            vec![Action::OpenFiles(targets)]
        );
    }

    #[test]
    fn an_empty_open_request_queues_nothing() {
        assert!(actions_for_request(IpcRequest::Open { targets: vec![] }).is_empty());
    }

    #[test]
    fn a_diff_request_becomes_start_diff() {
        assert_eq!(
            actions_for_request(IpcRequest::Diff {
                left: PathBuf::from("/old.pdf"),
                right: PathBuf::from("/new.pdf"),
            }),
            vec![Action::StartDiff {
                left: PathBuf::from("/old.pdf"),
                right: PathBuf::from("/new.pdf"),
            }]
        );
    }

    #[test]
    fn remote_requests_are_drained_in_order_and_report_that_the_window_is_wanted() {
        let (tx, rx) = mpsc::channel();
        let mut actions = Vec::new();

        // Nothing sent: nothing happens and the window is left alone.
        assert!(!poll_remote_requests(&rx, &mut actions));
        assert!(actions.is_empty());

        tx.send(IpcRequest::Open {
            targets: vec![target("/a.pdf", None)],
        })
        .expect("receiver alive");
        tx.send(IpcRequest::Diff {
            left: PathBuf::from("/b.pdf"),
            right: PathBuf::from("/c.pdf"),
        })
        .expect("receiver alive");
        assert!(poll_remote_requests(&rx, &mut actions));
        assert_eq!(
            actions,
            vec![
                Action::OpenFiles(vec![target("/a.pdf", None)]),
                Action::StartDiff {
                    left: PathBuf::from("/b.pdf"),
                    right: PathBuf::from("/c.pdf"),
                },
            ]
        );
        assert!(
            !poll_remote_requests(&rx, &mut actions),
            "channel was emptied"
        );

        // An empty open request queues nothing but still asks for the window.
        let mut actions = Vec::new();
        tx.send(IpcRequest::Open { targets: vec![] })
            .expect("receiver alive");
        assert!(poll_remote_requests(&rx, &mut actions));
        assert!(actions.is_empty());
    }

    #[test]
    fn a_closed_remote_channel_is_not_a_request() {
        let (tx, rx) = mpsc::channel::<IpcRequest>();
        drop(tx);
        let mut actions = Vec::new();
        assert!(!poll_remote_requests(&rx, &mut actions));
        assert!(actions.is_empty());
    }

    #[test]
    fn bringing_the_window_to_the_front_sends_focus_and_attention_commands() {
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |_ui| bring_to_front(&ctx));
        let commands: Vec<_> = output
            .viewport_output
            .values()
            .flat_map(|v| v.commands.iter().cloned())
            .collect();
        output.drop_without_applying_deltas();
        assert!(commands.contains(&egui::ViewportCommand::Focus));
        assert!(commands.iter().any(|c| matches!(
            c,
            egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational)
        )));
    }
}
