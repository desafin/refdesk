//! refdesk application entry point.

mod action;
mod app;
mod commands;
mod i18n;
mod state;
mod theme;

use eframe::egui;

use crate::action::OpenTarget;
use crate::app::RefdeskApp;

/// Turns command-line arguments into files to open. An argument that is not valid UTF-8 is a
/// plain path (it cannot carry a `#page=` suffix).
fn targets_from_args(args: impl Iterator<Item = std::ffi::OsString>) -> Vec<OpenTarget> {
    args.map(|arg| match arg.to_str() {
        Some(text) => OpenTarget::parse(text),
        None => OpenTarget {
            path: arg.into(),
            page: None,
        },
    })
    .collect()
}

fn main() -> eframe::Result {
    let initial = targets_from_args(std::env::args_os().skip(1));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("refdesk"),
        ..Default::default()
    };
    eframe::run_native(
        "refdesk",
        options,
        Box::new(move |cc| Ok(Box::new(RefdeskApp::new(cc, initial)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn arguments_become_open_targets() {
        let args = ["a.pdf", "b.pdf#page=3"].map(OsString::from);
        let targets = targets_from_args(args.into_iter());
        assert_eq!(
            targets,
            vec![
                OpenTarget {
                    path: PathBuf::from("a.pdf"),
                    page: None
                },
                OpenTarget {
                    path: PathBuf::from("b.pdf"),
                    page: Some(2)
                },
            ]
        );
    }
}
