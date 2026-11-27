//! `RefdeskApp`: the `eframe::App` implementation (`docs/ARCHITECTURE.md` §5.2).
//!
//! One frame:
//! 1. `logic()` turns outside events (the file chooser's result) into actions and runs the
//!    session save timer.
//! 2. `ui()` draws every window (`windows::show_all`); widgets only push actions onto the queue.
//! 3. The end of `ui()` dispatches the queue. Structure changes happen only there (rule R-6).
//!
//! At start the last session is restored (`session::start`); on exit it is saved.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use egui::Ui;
use refdesk_core::{IpcListener, IpcRequest, Prefs, Theme};

use crate::action::{Action, OpenTarget, apply_scroll_speed, dispatch_queued};
use crate::i18n::tr;
use crate::panels::explorer;
use crate::panels::global_search;
use crate::session::{self, Saver, Startup};
use crate::state::AppState;
use crate::theme;
use crate::views::{preload, textures};
use crate::windows;

/// The application.
pub struct RefdeskApp {
    state: AppState,
    /// Receives the files chosen in the dialog. `Some` while a dialog is open.
    dialog: Option<Receiver<Vec<PathBuf>>>,
    /// Receives the folder chosen in the folder dialog. `Some` while it is open (WS-1).
    folder_dialog: Option<Receiver<Option<PathBuf>>>,
    /// Requests from other `refdesk` processes (`refdesk_core::ipc`).
    remote: Receiver<IpcRequest>,
    /// Keeps the socket open for as long as the app lives.
    _listener: Option<IpcListener>,
    /// Saves the session and the document records (SES-5).
    saver: Saver,
}

impl RefdeskApp {
    /// Creates the app. `initial` is this process's own command line; it is handled like a
    /// request from another process, but the window is not raised. `remote` delivers the requests
    /// of other processes; `listener` is the open socket they arrive on, if there is one.
    /// `startup` holds the settings and the stored session.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        initial: IpcRequest,
        remote: Receiver<IpcRequest>,
        listener: Option<IpcListener>,
        startup: Startup,
    ) -> RefdeskApp {
        theme::install(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx, startup.config.theme);
        let (state, saver) = initial_state(startup, initial, &cc.egui_ctx);
        RefdeskApp {
            state,
            dialog: None,
            folder_dialog: None,
            remote,
            _listener: listener,
            saver,
        }
    }
}

/// The state the app starts with: the last session restored (when allowed), then the files of
/// the command line added as tabs (SES-2).
fn initial_state(startup: Startup, initial: IpcRequest, ctx: &egui::Context) -> (AppState, Saver) {
    let mut state = AppState::new();
    let saver = session::start(&mut state, startup, ctx);
    start_prefs(&mut state, ctx);
    state.actions.extend(actions_for_request(initial));
    (state, saver)
}

/// Reads `prefs.json` (when there is a place to read it from) and gives egui the scroll speed in
/// effect. egui has one setting for every window, so once is enough.
fn start_prefs(state: &mut AppState, ctx: &egui::Context) {
    if let Some(paths) = &state.paths {
        state.prefs = Prefs::load(&paths.prefs_file());
    }
    crate::panels::search_history::load_into(state);
    crate::panels::marks::load_into(state);
    apply_scroll_speed(state, ctx);
    crate::overlays::translate::start(state, ctx);
    theme::apply_theme_setting(state, ctx);
}

/// Applies the `theme` setting.
fn apply_theme(ctx: &egui::Context, theme: Theme) {
    ctx.set_theme(match theme {
        Theme::System => egui::ThemePreference::System,
        Theme::Light => egui::ThemePreference::Light,
        Theme::Dark => egui::ThemePreference::Dark,
    });
}

impl RefdeskApp {
    /// Runs the save timer and schedules the frame a pending save needs.
    fn tick_saver(&mut self, ctx: &egui::Context) {
        session::poll_fingerprints(&mut self.state, ctx);
        let now = ctx.input(|i| i.time);
        if let Some(wait) = self.saver.tick(&mut self.state, now) {
            ctx.request_repaint_after(wait);
        }
    }
}

impl eframe::App for RefdeskApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.state.file_dialog_requested) && self.dialog.is_none() {
            self.dialog = Some(spawn_file_dialog(ctx));
        }
        poll_file_dialog(&mut self.dialog, &mut self.state.actions);
        if std::mem::take(&mut self.state.folder_dialog_requested) && self.folder_dialog.is_none() {
            self.folder_dialog = Some(spawn_folder_dialog(ctx));
        }
        poll_folder_dialog(&mut self.folder_dialog, &mut self.state.actions);
        // WP-71: the workspace watcher saw a change; scan again.
        explorer::poll_workspace(&self.state);
        // WP-91: chooser and clipboard results, the running import, the folder picker.
        crate::panels::project::tick(&mut self.state, ctx);
        if poll_remote_requests(&self.remote, &mut self.state.actions) {
            bring_to_front(ctx);
        }
        // WP-19: the page colours in effect (the system theme may change at any time).
        theme::sync_page_colors(&mut self.state, ctx);
        // WP-15: upload finished renders; drop textures of closed documents.
        textures::collect_rendered(ctx, &self.state.docs, &mut self.state.textures);
        // WP-17: render every page's preview in the background.
        preload::step(ctx, &mut self.state);
        // WP-55: the search in all open documents follows the open documents and its jobs.
        global_search::tick(&mut self.state, ctx);
        // WP-81: the model warm-up, the model list and the delayed save of translations.
        crate::overlays::translate::tick(&mut self.state, ctx);
        // WP-74: highlights for the views, the mark letter's wait, the delayed save of marks.
        crate::panels::marks::tick(&mut self.state, ctx);
        // WP-41: save the session two seconds after it last changed.
        self.tick_saver(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut queue = std::mem::take(&mut self.state.actions);

        // Every window, with its menu bar; shortcuts go to the window that has the keyboard.
        windows::show_all(ui, &mut self.state, &mut queue);

        self.state.actions.append(&mut queue);
        let dispatched = !self.state.actions.is_empty();
        dispatch_queued(&mut self.state, &ctx);
        if dispatched {
            // The save timer runs in `logic()`; make sure it sees what just changed.
            ctx.request_repaint();
        }
    }

    fn on_exit(&mut self) {
        self.saver.save_now(&mut self.state);
        crate::overlays::translate::save_now(&mut self.state);
        crate::panels::marks::save_now(&mut self.state);
    }
}

/// What a request asks the application to do. A target that is a folder opens as the workspace
/// (WS-1), before the files, so the files get the focus. Telling a folder from a file is one
/// look at the disk per target, done here, by the instance that handles the request. A text to
/// find (`--find`) is searched for once the files are open.
fn actions_for_request(request: IpcRequest) -> Vec<Action> {
    match request {
        IpcRequest::Open { targets, find } => {
            let (folders, files): (Vec<OpenTarget>, Vec<OpenTarget>) = targets
                .into_iter()
                .partition(|t| t.page.is_none() && t.path.is_dir());
            let mut actions: Vec<Action> = folders
                .into_iter()
                .map(|t| Action::OpenFolder(t.path))
                .collect();
            if !files.is_empty() {
                actions.push(Action::OpenFiles(files));
            }
            if let Some(text) = find.filter(|t| !t.trim().is_empty()) {
                actions.push(Action::GlobalSearchFor(text));
            }
            actions
        }
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

/// Opens the folder chooser on its own thread (rule R-1), like [`spawn_file_dialog`].
fn spawn_folder_dialog(ctx: &egui::Context) -> Receiver<Option<PathBuf>> {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("folder-dialog".into())
        .spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title(tr("dialog.folder_title"))
                .pick_folder();
            // The receiver is gone only when the app is shutting down.
            let _ = tx.send(picked);
            ctx.request_repaint();
        });
    if let Err(err) = spawned {
        log::error!("could not start the folder dialog thread: {err}");
    }
    rx
}

/// Checks (without blocking) whether the folder dialog has finished and queues `OpenFolder` for
/// its result. Clears `dialog` once it is finished or its thread is gone.
fn poll_folder_dialog(dialog: &mut Option<Receiver<Option<PathBuf>>>, actions: &mut Vec<Action>) {
    let Some(rx) = dialog else {
        return;
    };
    match rx.try_recv() {
        Ok(picked) => {
            *dialog = None;
            if let Some(path) = picked {
                actions.push(Action::OpenFolder(path));
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

    /// A start-up whose session shows `/refdesk-test/restored.pdf`.
    fn startup_with_one_tab() -> Startup {
        let path = PathBuf::from("/refdesk-test/restored.pdf");
        let mut startup = Startup::empty();
        startup.session.tabs.insert(
            "t0".into(),
            refdesk_core::SessionTab::Pdf {
                path: path.clone(),
                view: refdesk_core::ViewState::new(path),
                preview: false,
            },
        );
        startup.session.windows.push(refdesk_core::SessionWindow {
            main: true,
            layout: refdesk_core::LayoutNode::Tabs {
                tabs: vec!["t0".into()],
                active: 0,
            },
            ..Default::default()
        });
        startup
    }

    fn open_request(paths: &[&str]) -> IpcRequest {
        IpcRequest::Open {
            targets: paths.iter().map(|p| target(p, None)).collect(),
            find: None,
        }
    }

    fn pdf_paths(state: &AppState) -> Vec<PathBuf> {
        state
            .windows
            .iter()
            .flat_map(|w| w.dock.tabs())
            .filter_map(|t| state.tabs.pdf(t))
            .filter_map(|p| state.docs.get(p.doc))
            .map(|d| d.path().to_path_buf())
            .collect()
    }

    #[test]
    fn command_line_files_are_added_after_the_restored_session() {
        let ctx = egui::Context::default();
        let (mut state, _saver) = initial_state(
            startup_with_one_tab(),
            open_request(&["/refdesk-test/new.pdf"]),
            &ctx,
        );
        assert_eq!(pdf_paths(&state).len(), 1, "restored before the files");
        dispatch_queued(&mut state, &ctx);
        assert_eq!(
            pdf_paths(&state),
            vec![
                PathBuf::from("/refdesk-test/restored.pdf"),
                PathBuf::from("/refdesk-test/new.pdf")
            ]
        );
        let focused = state.focused_pdf().expect("focus").1.doc;
        assert_eq!(
            state.docs.get(focused).expect("doc").path(),
            std::path::Path::new("/refdesk-test/new.pdf")
        );
    }

    #[test]
    fn a_command_line_file_already_in_the_session_only_gets_the_focus() {
        let ctx = egui::Context::default();
        let mut startup = startup_with_one_tab();
        startup.session.tabs.insert(
            "t1".into(),
            refdesk_core::SessionTab::Pdf {
                path: PathBuf::from("/refdesk-test/other.pdf"),
                view: refdesk_core::ViewState::new(PathBuf::from("/refdesk-test/other.pdf")),
                preview: false,
            },
        );
        if let refdesk_core::LayoutNode::Tabs { tabs, .. } = &mut startup.session.windows[0].layout
        {
            tabs.push("t1".into());
        }
        let (mut state, _saver) =
            initial_state(startup, open_request(&["/refdesk-test/other.pdf"]), &ctx);
        let restored = pdf_paths(&state);
        let focused_before = state.focus;
        dispatch_queued(&mut state, &ctx);
        assert_eq!(pdf_paths(&state), restored, "no new tab");
        assert_ne!(state.focus, focused_before);
        let focused = state.focused_pdf().expect("focus").1.doc;
        assert_eq!(
            state.docs.get(focused).expect("doc").path(),
            std::path::Path::new("/refdesk-test/other.pdf")
        );
    }

    #[test]
    fn no_restore_starts_with_only_the_command_line_files() {
        let ctx = egui::Context::default();
        let mut startup = startup_with_one_tab();
        startup.restore = false;
        let (mut state, _saver) =
            initial_state(startup, open_request(&["/refdesk-test/new.pdf"]), &ctx);
        dispatch_queued(&mut state, &ctx);
        assert_eq!(
            pdf_paths(&state),
            vec![PathBuf::from("/refdesk-test/new.pdf")]
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

    fn line_speed(ctx: &egui::Context) -> f32 {
        ctx.options(|o| o.input_options.line_scroll_speed)
    }

    fn startup_at(dir: &crate::action::scroll_tests::ScratchDir) -> Startup {
        let mut startup = Startup::empty();
        startup.paths = Some(dir.paths());
        startup
    }

    #[test]
    fn the_scroll_speed_starts_at_the_default_and_follows_the_config_file() {
        let ctx = egui::Context::default();
        let (state, _saver) = initial_state(Startup::empty(), open_request(&[]), &ctx);
        assert_eq!(state.scroll_speed(), 2.5);
        assert_eq!(line_speed(&ctx), 100.0);

        let ctx = egui::Context::default();
        let mut startup = Startup::empty();
        startup.config.scroll_speed = 4.0;
        let (state, _saver) = initial_state(startup, open_request(&[]), &ctx);
        assert_eq!(state.scroll_speed(), 4.0);
        assert_eq!(line_speed(&ctx), 160.0);
    }

    #[test]
    fn a_changed_scroll_speed_is_there_again_after_a_restart() {
        let dir = crate::action::scroll_tests::ScratchDir::new("restart");
        let ctx = egui::Context::default();
        let (mut state, _saver) = initial_state(startup_at(&dir), open_request(&[]), &ctx);
        crate::action::dispatch(&mut state, Action::SetScrollSpeed(Some(3.0)), &ctx);
        drop(state);

        // A new process: a new egui context and a new state over the same files.
        let ctx = egui::Context::default();
        assert_eq!(line_speed(&ctx), 40.0, "egui starts with its own default");
        let (state, _saver) = initial_state(startup_at(&dir), open_request(&[]), &ctx);
        assert_eq!(state.scroll_speed(), 3.0);
        assert_eq!(line_speed(&ctx), 120.0);
    }

    #[test]
    fn a_changed_theme_and_page_colours_are_there_again_after_a_restart() {
        let dir = crate::action::scroll_tests::ScratchDir::new("restart-theme");
        let ctx = egui::Context::default();
        let (mut state, _saver) = initial_state(startup_at(&dir), open_request(&[]), &ctx);
        crate::action::dispatch(&mut state, Action::SetTheme(Theme::Light), &ctx);
        let pref = refdesk_core::PageColorsPref::Dark;
        crate::action::dispatch(&mut state, Action::SetPageColors(pref), &ctx);
        drop(state);

        let ctx = egui::Context::default();
        let mut startup = startup_at(&dir);
        startup.config.theme = Theme::Dark;
        let (mut state, _saver) = initial_state(startup, open_request(&[]), &ctx);
        assert_eq!(
            theme::theme(&state),
            Theme::Light,
            "prefs.json wins over config.toml"
        );
        assert_eq!(
            ctx.options(|o| o.theme_preference),
            egui::ThemePreference::Light
        );
        assert_eq!(theme::page_colors_pref(&state), pref);
        theme::sync_page_colors(&mut state, &ctx);
        assert_eq!(state.textures.page_colors(), refdesk_core::PageColors::Dark);
    }

    #[test]
    fn a_damaged_prefs_file_starts_with_the_config_speed_and_keeps_a_bad_copy() {
        let dir = crate::action::scroll_tests::ScratchDir::new("damaged");
        let prefs_file = dir.paths().prefs_file();
        std::fs::create_dir_all(prefs_file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&prefs_file, b"{ not json").expect("write");
        let ctx = egui::Context::default();
        let (state, _saver) = initial_state(startup_at(&dir), open_request(&[]), &ctx);
        assert_eq!(state.scroll_speed(), 2.5);
        assert!(refdesk_core::store::bad_path(&prefs_file).exists());
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
                targets: targets.clone(),
                find: None,
            }),
            vec![Action::OpenFiles(targets)]
        );
    }

    /// A fresh folder under the temporary directory with an empty `inside.pdf`.
    fn temp_folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("refdesk-wp71-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("inside.pdf"), b"").expect("write file");
        dir
    }

    #[test]
    fn a_folder_in_an_open_request_opens_as_the_workspace_before_the_files() {
        let dir = temp_folder("request");
        let file = dir.join("inside.pdf");
        let request = IpcRequest::Open {
            targets: vec![
                OpenTarget {
                    path: file.clone(),
                    page: Some(2),
                },
                OpenTarget {
                    path: dir.clone(),
                    page: None,
                },
            ],
            find: None,
        };
        assert_eq!(
            actions_for_request(request),
            vec![
                Action::OpenFolder(dir.clone()),
                Action::OpenFiles(vec![OpenTarget {
                    path: file,
                    page: Some(2)
                }])
            ]
        );
        // A folder alone.
        assert_eq!(
            actions_for_request(IpcRequest::Open {
                targets: vec![OpenTarget {
                    path: dir.clone(),
                    page: None
                }],
                find: None,
            }),
            vec![Action::OpenFolder(dir.clone())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_sent_by_another_instance_opens_as_the_workspace() {
        let dir = temp_folder("remote");
        let (tx, rx) = mpsc::channel();
        let mut actions = Vec::new();
        tx.send(IpcRequest::Open {
            targets: vec![OpenTarget {
                path: dir.clone(),
                page: None,
            }],
            find: None,
        })
        .expect("receiver alive");
        assert!(poll_remote_requests(&rx, &mut actions));
        assert_eq!(actions, vec![Action::OpenFolder(dir.clone())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_on_the_command_line_is_open_after_start() {
        let dir = temp_folder("cli");
        let ctx = egui::Context::default();
        let (mut state, _saver) = initial_state(
            Startup::empty(),
            IpcRequest::Open {
                targets: vec![OpenTarget {
                    path: dir.clone(),
                    page: None,
                }],
                find: None,
            },
            &ctx,
        );
        dispatch_queued(&mut state, &ctx);
        let ws = state
            .workspace
            .as_ref()
            .expect("the folder is the workspace");
        assert_eq!(ws.root, crate::state::normalize_path(&dir));
        assert!(state.main_window().toc_open && state.main_window().explorer_open);
        drop(state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_finished_folder_dialog_becomes_an_open_folder_action() {
        let (tx, rx) = mpsc::channel();
        let mut dialog = Some(rx);
        let mut actions = Vec::new();
        poll_folder_dialog(&mut dialog, &mut actions);
        assert!(dialog.is_some() && actions.is_empty());
        tx.send(Some(PathBuf::from("/refdesk-test/folder")))
            .expect("receiver alive");
        poll_folder_dialog(&mut dialog, &mut actions);
        assert!(dialog.is_none());
        assert_eq!(
            actions,
            vec![Action::OpenFolder(PathBuf::from("/refdesk-test/folder"))]
        );
        // Cancelled, and a dead thread.
        let (tx, rx) = mpsc::channel();
        let mut dialog = Some(rx);
        tx.send(None).expect("receiver alive");
        poll_folder_dialog(&mut dialog, &mut actions);
        assert!(dialog.is_none() && actions.len() == 1);
        let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
        drop(tx);
        let mut dialog = Some(rx);
        poll_folder_dialog(&mut dialog, &mut actions);
        assert!(dialog.is_none() && actions.len() == 1);
    }

    #[test]
    fn an_empty_open_request_queues_nothing() {
        assert!(
            actions_for_request(IpcRequest::Open {
                targets: vec![],
                find: None
            })
            .is_empty()
        );
    }

    #[test]
    fn a_find_text_is_searched_after_the_files_and_folders_open() {
        let dir = temp_folder("find");
        let file = OpenTarget {
            path: dir.join("inside.pdf"),
            page: None,
        };
        let folder = OpenTarget {
            path: dir.clone(),
            page: None,
        };
        let request = IpcRequest::Open {
            targets: vec![file.clone(), folder],
            find: Some("hello".into()),
        };
        assert_eq!(
            actions_for_request(request),
            vec![
                Action::OpenFolder(dir.clone()),
                Action::OpenFiles(vec![file]),
                Action::GlobalSearchFor("hello".into()),
            ]
        );
        // Alone it searches what is open; an empty text searches nothing.
        assert_eq!(
            actions_for_request(IpcRequest::Open {
                targets: vec![],
                find: Some("hello".into()),
            }),
            vec![Action::GlobalSearchFor("hello".into())]
        );
        assert!(
            actions_for_request(IpcRequest::Open {
                targets: vec![],
                find: Some(" ".into()),
            })
            .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_find_text_from_another_instance_shows_the_search_view_and_searches() {
        let ctx = egui::Context::default();
        let (tx, rx) = mpsc::channel();
        let (mut state, _saver) = initial_state(
            Startup::empty(),
            IpcRequest::Open {
                targets: vec![],
                find: None,
            },
            &ctx,
        );
        tx.send(IpcRequest::Open {
            targets: vec![],
            find: Some("needle".into()),
        })
        .expect("receiver alive");
        assert!(poll_remote_requests(&rx, &mut state.actions));
        dispatch_queued(&mut state, &ctx);
        assert_eq!(state.global_search.query.text, "needle");
        assert!(state.main_window().toc_open);
        assert_eq!(
            state.main_window().sidebar_view,
            crate::panels::sidebar::SidebarView::Search
        );
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
            find: None,
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
        tx.send(IpcRequest::Open {
            targets: vec![],
            find: None,
        })
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
