//! refdesk application entry point.

mod action;
mod app;
mod cli;
mod commands;
mod i18n;
mod state;
mod theme;

use std::path::Path;
use std::sync::mpsc;

use eframe::egui;
use refdesk_core::{IpcListener, IpcRequest, Paths, ipc};

use crate::app::RefdeskApp;
use crate::cli::Cli;

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

fn main() -> eframe::Result {
    let (request, single_instance) = match cli::parse(std::env::args_os().skip(1)) {
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
        } => (request, single_instance),
    };

    // A running instance has another current directory: send it absolute paths.
    let request = match std::env::current_dir() {
        Ok(cwd) => request.with_absolute_paths(&cwd),
        Err(err) => {
            log::warn!("cannot read the current directory: {err}");
            request
        }
    };

    let socket = Paths::from_env().socket_path;
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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("refdesk")
            // Lets GNOME match the window to `refdesk.desktop` (StartupWMClass=refdesk).
            .with_app_id("refdesk"),
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
            )))
        }),
    )
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
