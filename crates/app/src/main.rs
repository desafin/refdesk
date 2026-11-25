//! refdesk application entry point.

mod action;
mod app;
mod cli;
mod commands;
mod i18n;
mod overlays;
mod panels;
mod session;
mod state;
mod theme;
mod views;
mod windows;

use std::path::Path;
use std::sync::mpsc;

use eframe::egui;
use refdesk_core::{Config, IpcListener, IpcRequest, Language, Paths, ipc};

use crate::app::RefdeskApp;
use crate::cli::Cli;
use crate::i18n::Lang;
use crate::session::Startup;

/// Opens the socket other instances send their requests to. Every request is queued on `tx` and
/// the UI is woken up. Failure (another instance owns the socket, no permission) is only a
/// warning: the application still starts, it just cannot receive requests.
fn start_listener(
    socket: &Path,
    tx: mpsc::Sender<IpcRequest>,
    ctx: egui::Context,
) -> Option<IpcListener> {
    let started = ipc::listen(socket, move |request| {
        // The receiver is gone only while the application shuts down.
        let _ = tx.send(request);
        ctx.request_repaint();
    });
    match started {
        Ok(listener) => Some(listener),
        Err(err) => {
            log::warn!(
                "single-instance socket {} unavailable: {err}",
                socket.display()
            );
            eprintln!(
                "refdesk: warning: cannot listen on {}: {err}",
                socket.display()
            );
            None
        }
    }
}

/// Writes log records to standard error. Records of refdesk itself pass at `level`; other
/// crates (eframe, wgpu, …) are capped at `warn` so `debug` stays readable.
struct StderrLogger {
    level: log::LevelFilter,
}

impl StderrLogger {
    fn allows(&self, metadata: &log::Metadata) -> bool {
        let limit = if metadata.target().starts_with("refdesk") {
            self.level
        } else {
            self.level.min(log::LevelFilter::Warn)
        };
        metadata.level() <= limit
    }
}

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.allows(metadata)
    }

    fn log(&self, record: &log::Record) {
        if self.allows(record.metadata()) {
            eprintln!(
                "refdesk [{} {}] {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

/// The log level named by `REFDESK_LOG`: error, warn, info or debug (any case). Anything else,
/// or nothing, is `warn`.
fn log_level(value: Option<&str>) -> log::LevelFilter {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("error") => log::LevelFilter::Error,
        Some("info") => log::LevelFilter::Info,
        Some("debug") => log::LevelFilter::Debug,
        _ => log::LevelFilter::Warn,
    }
}

/// Installs the standard error logger at the level of `REFDESK_LOG`.
fn init_logging() {
    let level = log_level(std::env::var("REFDESK_LOG").ok().as_deref());
    let logger = Box::leak(Box::new(StderrLogger { level }));
    // Fails only when a logger is already installed, which leaves that one in charge.
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}

/// Applies the `language` setting before any text is looked up.
fn apply_language(language: Language) {
    let lang = match language {
        Language::Auto => return,
        Language::Ko => Lang::Ko,
        Language::En => Lang::En,
    };
    i18n::force_lang(lang);
}

fn main() -> eframe::Result {
    init_logging();
    let paths = Paths::from_env();
    // Read first: the language setting applies to the help text too. Never written by the app.
    let config = Config::load(&paths.config_file());
    apply_language(config.language);
    let (request, single_instance, restore) = match cli::parse(std::env::args_os().skip(1)) {
        Cli::Help => {
            println!("{}", cli::usage());
            return Ok(());
        }
        Cli::Version => {
            println!("{}", cli::version_text());
            return Ok(());
        }
        Cli::Error(message) => {
            eprintln!("refdesk: {message}\n\n{}", cli::usage());
            std::process::exit(2);
        }
        Cli::Run {
            request,
            single_instance,
            restore,
        } => (request, single_instance, restore),
    };

    // A running instance has another current directory: send it absolute paths.
    let request = match std::env::current_dir() {
        Ok(cwd) => request.with_absolute_paths(&cwd),
        Err(err) => {
            log::warn!("cannot read the current directory: {err}");
            request
        }
    };

    let socket = paths.socket_path.clone();
    if single_instance {
        match ipc::send(&socket, &request) {
            // The running instance has it. Nothing left to do.
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => {
                log::warn!("could not hand the request to a running instance: {err}");
                eprintln!("refdesk: warning: could not reach a running instance: {err}");
            }
        }
    }

    // The session is read before the window exists: the main window opens at its saved size.
    let startup = Startup::load(paths, config, restore);
    let main_window = session::main_geometry(&startup.session);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("refdesk")
        // Lets GNOME match the window to `refdesk.desktop` (StartupWMClass=refdesk).
        .with_app_id("refdesk")
        .with_inner_size(session::sane_size(main_window.size));
    if main_window.maximized {
        viewport = viewport.with_maximized(true);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "refdesk",
        options,
        Box::new(move |cc| {
            let (tx, remote) = mpsc::channel();
            let listener =
                single_instance.then(|| start_listener(&socket, tx, cc.egui_ctx.clone()));
            Ok(Box::new(RefdeskApp::new(
                cc,
                request,
                remote,
                listener.flatten(),
                startup,
            )))
        }),
    )
}

#[cfg(test)]
mod log_tests {
    use super::*;
    use log::{Level, LevelFilter, Log, Metadata};

    #[test]
    fn log_level_comes_from_the_variable_with_warn_as_default() {
        assert_eq!(log_level(None), LevelFilter::Warn);
        assert_eq!(log_level(Some("error")), LevelFilter::Error);
        assert_eq!(log_level(Some("warn")), LevelFilter::Warn);
        assert_eq!(log_level(Some("INFO")), LevelFilter::Info);
        assert_eq!(log_level(Some(" debug ")), LevelFilter::Debug);
        assert_eq!(log_level(Some("trace?")), LevelFilter::Warn);
        assert_eq!(log_level(Some("")), LevelFilter::Warn);
    }

    #[test]
    fn other_crates_are_capped_at_warn() {
        let logger = StderrLogger {
            level: LevelFilter::Debug,
        };
        let meta = |level, target| Metadata::builder().level(level).target(target).build();
        assert!(logger.enabled(&meta(Level::Debug, "refdesk::action")));
        assert!(logger.enabled(&meta(Level::Debug, "refdesk_core::engine")));
        assert!(!logger.enabled(&meta(Level::Info, "wgpu_core")));
        assert!(logger.enabled(&meta(Level::Warn, "eframe")));
        let quiet = StderrLogger {
            level: LevelFilter::Error,
        };
        assert!(!quiet.enabled(&meta(Level::Warn, "refdesk::app")));
    }
}

#[cfg(test)]
mod script_tests {
    //! The install scripts change the user's home directory, so they are only ever run with
    //! `--dry-run` and a throwaway `HOME`.

    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn scripts_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts")
    }

    fn scratch_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("refdesk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create scratch home");
        home
    }

    fn entries(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .expect("read scratch home")
            .map(|e| e.expect("entry").path())
            .collect()
    }

    #[test]
    fn scripts_have_valid_bash_syntax() {
        for script in ["install.sh", "uninstall.sh"] {
            let status = Command::new("bash")
                .arg("-n")
                .arg(scripts_dir().join(script))
                .status()
                .expect("run bash");
            assert!(status.success(), "{script} has a syntax error");
        }
    }

    #[test]
    fn dry_runs_change_nothing() {
        let home = scratch_home("dry-run");
        for (script, args) in [
            ("install.sh", &["--dry-run"][..]),
            ("install.sh", &["--dry-run", "--set-default"][..]),
            ("uninstall.sh", &["--dry-run"][..]),
        ] {
            let output = Command::new("bash")
                .arg(scripts_dir().join(script))
                .args(args)
                .env("HOME", &home)
                .env_remove("XDG_DATA_HOME")
                .output()
                .expect("run script");
            assert!(
                output.status.success(),
                "{script} {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(entries(&home).is_empty(), "{script} {args:?} wrote files");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn dry_run_install_names_every_step_and_leaves_the_default_alone() {
        let home = scratch_home("dry-run-text");
        let run = |extra: &[&str]| {
            let output = Command::new("bash")
                .arg(scripts_dir().join("install.sh"))
                .arg("--dry-run")
                .args(extra)
                .env("HOME", &home)
                .output()
                .expect("run script");
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        let plain = run(&[]);
        for needle in [
            ".local/bin/refdesk",
            "refdesk.desktop",
            "refdesk.svg",
            "cargo build",
        ] {
            assert!(plain.contains(needle), "missing {needle}:\n{plain}");
        }
        assert!(
            !plain.contains("xdg-mime"),
            "the default must not change:\n{plain}"
        );
        assert!(
            run(&["--set-default"]).contains("xdg-mime default refdesk.desktop application/pdf")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn unknown_script_options_are_refused() {
        let output = Command::new("bash")
            .arg(scripts_dir().join("install.sh"))
            .arg("--bogus")
            .output()
            .expect("run script");
        assert_eq!(output.status.code(), Some(2));
    }
}
