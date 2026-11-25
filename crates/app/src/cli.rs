//! Command-line parsing (`docs/SPEC.md` OPEN-2, DIFF-1). A pure function: it reads no
//! environment and touches no files, so the tests need no window.
//!
//! ```text
//! refdesk [--no-restore] [--no-single-instance] [file[#page=N]]...
//! refdesk --diff old.pdf new.pdf
//! ```
//!
//! Paths stay as typed here. `main` makes them absolute against the current directory before
//! they are sent to another instance.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use refdesk_core::{IpcRequest, OpenTarget};

use crate::i18n::tr;

/// What the command line asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cli {
    /// Start (or hand the request to a running instance).
    Run {
        /// What to open. A bare `refdesk` is an empty `Open`.
        request: IpcRequest,
        /// `false` with `--no-single-instance`: do not look for a running instance and do not
        /// open the socket.
        single_instance: bool,
        /// `false` with `--no-restore`: start without the last session (it is still saved).
        restore: bool,
    },
    /// Print the usage text and exit with status 0.
    Help,
    /// Print the version and exit with status 0.
    Version,
    /// A mistake in the arguments: print the message and the usage to stderr, exit with status 2.
    Error(String),
}

/// The usage text, in the language of the process.
pub fn usage() -> String {
    tr("cli.help").to_owned()
}

/// `refdesk <version>`.
pub fn version_text() -> String {
    format!("refdesk {}", env!("CARGO_PKG_VERSION"))
}

/// Interprets the arguments after the program name.
///
/// * `--help`/`-h` and `--version`/`-V` win at the point they are met.
/// * `--diff A B` takes the next two arguments as they are (no `#page=` handling). It cannot be
///   combined with files or repeated.
/// * `--` ends the options: everything after it is a file, even if it starts with `-`.
/// * An argument that starts with `-` and is not a known option is an error (a lone `-` is a file
///   name). An argument that is not valid UTF-8 is a file without a page request.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Cli {
    let mut args = args.into_iter();
    let mut targets: Vec<OpenTarget> = Vec::new();
    let mut diff: Option<(PathBuf, PathBuf)> = None;
    let mut single_instance = true;
    let mut restore = true;
    let mut options_ended = false;

    while let Some(arg) = args.next() {
        if !options_ended && let Some(text) = arg.to_str() {
            match text {
                "--" => {
                    options_ended = true;
                    continue;
                }
                "--help" | "-h" => return Cli::Help,
                "--version" | "-V" => return Cli::Version,
                "--no-single-instance" => {
                    single_instance = false;
                    continue;
                }
                "--no-restore" => {
                    restore = false;
                    continue;
                }
                "--diff" => {
                    if diff.is_some() {
                        return Cli::Error(tr("cli.diff_twice").to_owned());
                    }
                    let (Some(left), Some(right)) = (args.next(), args.next()) else {
                        return Cli::Error(tr("cli.diff_needs_two").to_owned());
                    };
                    diff = Some((left.into(), right.into()));
                    continue;
                }
                _ if text.starts_with('-') && text != "-" => {
                    return Cli::Error(tr("cli.unknown_option").replace("{}", text));
                }
                _ => {}
            }
        }
        targets.push(file_target(&arg));
    }

    let request = match diff {
        Some(_) if !targets.is_empty() => {
            return Cli::Error(tr("cli.diff_with_files").to_owned());
        }
        Some((left, right)) => IpcRequest::Diff { left, right },
        None => IpcRequest::Open { targets },
    };
    Cli::Run {
        request,
        single_instance,
        restore,
    }
}

/// A file argument. One that is not valid UTF-8 cannot carry a `#page=` suffix.
fn file_target(arg: &OsStr) -> OpenTarget {
    match arg.to_str() {
        Some(text) => OpenTarget::parse(text),
        None => OpenTarget {
            path: PathBuf::from(arg),
            page: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn parse_strs(args: &[&str]) -> Cli {
        parse(args.iter().map(OsString::from))
    }

    fn target(path: &str, page: Option<u32>) -> OpenTarget {
        OpenTarget {
            path: PathBuf::from(path),
            page,
        }
    }

    fn open(targets: Vec<OpenTarget>, single_instance: bool) -> Cli {
        Cli::Run {
            request: IpcRequest::Open { targets },
            single_instance,
            restore: true,
        }
    }

    #[test]
    fn no_arguments_is_an_empty_open() {
        assert_eq!(parse_strs(&[]), open(vec![], true));
    }

    #[test]
    fn several_files_keep_their_order() {
        assert_eq!(
            parse_strs(&["b.pdf", "a.pdf", "c.pdf"]),
            open(
                vec![
                    target("b.pdf", None),
                    target("a.pdf", None),
                    target("c.pdf", None)
                ],
                true
            )
        );
    }

    #[test]
    fn a_page_fragment_is_one_based_in_and_zero_based_out() {
        assert_eq!(
            parse_strs(&["a.pdf#page=12", "b.pdf"]),
            open(vec![target("a.pdf", Some(11)), target("b.pdf", None)], true)
        );
    }

    #[test]
    fn diff_takes_two_files() {
        assert_eq!(
            parse_strs(&["--diff", "old.pdf", "new.pdf"]),
            Cli::Run {
                request: IpcRequest::Diff {
                    left: PathBuf::from("old.pdf"),
                    right: PathBuf::from("new.pdf"),
                },
                single_instance: true,
                restore: true,
            }
        );
    }

    #[test]
    fn diff_paths_are_taken_as_typed_without_page_handling() {
        let Cli::Run {
            request: IpcRequest::Diff { left, right },
            ..
        } = parse_strs(&["--diff", "a.pdf#page=3", "b.pdf"])
        else {
            panic!("expected a diff request");
        };
        assert_eq!(left, PathBuf::from("a.pdf#page=3"));
        assert_eq!(right, PathBuf::from("b.pdf"));
    }

    #[test]
    fn diff_with_too_few_arguments_is_an_error() {
        assert!(matches!(parse_strs(&["--diff"]), Cli::Error(_)));
        assert!(matches!(parse_strs(&["--diff", "a.pdf"]), Cli::Error(_)));
    }

    #[test]
    fn diff_cannot_be_combined_with_files_or_repeated() {
        for args in [
            &["--diff", "a.pdf", "b.pdf", "c.pdf"][..],
            &["c.pdf", "--diff", "a.pdf", "b.pdf"][..],
            &["--diff", "a.pdf", "b.pdf", "--diff", "c.pdf", "d.pdf"][..],
        ] {
            assert!(matches!(parse_strs(args), Cli::Error(_)), "{args:?}");
        }
    }

    #[test]
    fn an_unknown_option_is_an_error_naming_it() {
        let Cli::Error(message) = parse_strs(&["--bogus", "a.pdf"]) else {
            panic!("expected an error");
        };
        assert!(message.contains("--bogus"), "{message}");
        assert!(matches!(parse_strs(&["-x"]), Cli::Error(_)));
        // `--diff=a` is not a supported spelling.
        assert!(matches!(parse_strs(&["--diff=a.pdf"]), Cli::Error(_)));
    }

    #[test]
    fn help_and_version() {
        assert_eq!(parse_strs(&["--help"]), Cli::Help);
        assert_eq!(parse_strs(&["-h"]), Cli::Help);
        assert_eq!(parse_strs(&["a.pdf", "--help"]), Cli::Help);
        assert_eq!(parse_strs(&["--version"]), Cli::Version);
        assert_eq!(parse_strs(&["-V"]), Cli::Version);
    }

    #[test]
    fn no_single_instance_is_a_flag_anywhere() {
        assert_eq!(parse_strs(&["--no-single-instance"]), open(vec![], false));
        assert_eq!(
            parse_strs(&["a.pdf", "--no-single-instance", "b.pdf"]),
            open(vec![target("a.pdf", None), target("b.pdf", None)], false)
        );
        assert_eq!(
            parse_strs(&["--no-single-instance", "--diff", "a.pdf", "b.pdf"]),
            Cli::Run {
                request: IpcRequest::Diff {
                    left: PathBuf::from("a.pdf"),
                    right: PathBuf::from("b.pdf"),
                },
                single_instance: false,
                restore: true,
            }
        );
    }

    #[test]
    fn no_restore_is_a_flag_anywhere() {
        let Cli::Run {
            request: IpcRequest::Open { targets },
            single_instance,
            restore,
        } = parse_strs(&["a.pdf", "--no-restore", "b.pdf"])
        else {
            panic!("expected an open request");
        };
        assert!(!restore);
        assert!(single_instance);
        assert_eq!(targets, vec![target("a.pdf", None), target("b.pdf", None)]);
        assert!(matches!(
            parse_strs(&["--no-restore", "--no-single-instance"]),
            Cli::Run {
                restore: false,
                single_instance: false,
                ..
            }
        ));
        // After `--` it is a file name.
        assert!(matches!(
            parse_strs(&["--", "--no-restore"]),
            Cli::Run { restore: true, .. }
        ));
    }

    #[test]
    fn options_and_files_may_be_mixed() {
        assert_eq!(
            parse_strs(&["a.pdf#page=2", "--no-single-instance", "b.pdf"]),
            open(vec![target("a.pdf", Some(1)), target("b.pdf", None)], false)
        );
    }

    #[test]
    fn everything_after_a_double_dash_is_a_file() {
        assert_eq!(
            parse_strs(&["--", "--help", "-x", "--diff", "--"]),
            open(
                vec![
                    target("--help", None),
                    target("-x", None),
                    target("--diff", None),
                    target("--", None)
                ],
                true
            )
        );
        // Options before the `--` still count.
        assert_eq!(
            parse_strs(&["--no-single-instance", "--", "-a.pdf"]),
            open(vec![target("-a.pdf", None)], false)
        );
    }

    #[test]
    fn a_lone_dash_is_a_file_name() {
        assert_eq!(parse_strs(&["-"]), open(vec![target("-", None)], true));
    }

    #[test]
    fn an_argument_that_is_not_utf8_is_a_plain_path() {
        let raw = OsString::from_vec(vec![b'a', 0xff, b'.', b'p', b'd', b'f']);
        let Cli::Run {
            request: IpcRequest::Open { targets },
            ..
        } = parse([raw.clone()])
        else {
            panic!("expected an open request");
        };
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].path, PathBuf::from(raw));
        assert_eq!(targets[0].page, None);
    }

    #[test]
    fn usage_names_every_option() {
        let text = usage();
        for needle in [
            "--diff",
            "--no-single-instance",
            "--no-restore",
            "--help",
            "--version",
            "#page=",
        ] {
            assert!(text.contains(needle), "usage lacks {needle}");
        }
        assert!(version_text().starts_with("refdesk "));
    }
}
