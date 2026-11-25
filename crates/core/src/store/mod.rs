//! Persistent user data: session, per-document memory and configuration.
//! See `docs/ARCHITECTURE.md` §4.10.
//!
//! Rules shared by every file in this module:
//!
//! * Functions take the file path as an argument. The only function that reads the environment
//!   is [`Paths::from_env`]. Timestamps come from the caller (Unix seconds); nothing here reads
//!   the clock.
//! * Reading never fails. A missing or empty file gives the default value. A damaged file gives
//!   the default value and is moved to `<name>.bad` (R-8).
//! * Writing goes through [`atomic_write`] only.
//! * Every file has a top-level `version`. Unknown fields are ignored, missing fields take their
//!   default, and a file with a higher version is read as far as it can be understood.
//! * No function here starts a thread. Running them in the background is the caller's job.

pub mod atomic;
pub mod config;
pub mod docstate;
pub mod paths;
pub mod session;

pub use atomic::{atomic_write, bad_path, quarantine, read_json_or_default};
pub use config::{CaptureConfig, Config, DefaultZoom, Language, Theme};
pub use docstate::{DocRecord, DocStateDb, MAX_RECENT_FOLDERS, MAX_RECORDS, RecentFolder};
pub use paths::Paths;
pub use session::{LayoutNode, Session, SessionTab, SessionWindow, SessionWorkspace};

/// The format version written to every stored file.
pub const FORMAT_VERSION: u32 = 1;

/// The most history entries a saved view keeps (see `History::trimmed`).
pub const SAVED_HISTORY_ENTRIES: usize = 50;

/// Parses the items of a JSON array one by one, dropping the ones that do not parse so that one
/// damaged entry does not cost the rest. Each drop is described in `dropped`.
pub(crate) fn lenient_items<T: serde::de::DeserializeOwned>(
    value: Option<&serde_json::Value>,
    what: &str,
    dropped: &mut Vec<String>,
) -> Vec<T> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        dropped.push(format!("{what}: expected an array"));
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match serde_json::from_value::<T>(item.clone()) {
            Ok(v) => out.push(v),
            Err(e) => dropped.push(format!("{what}[{i}]: {e}")),
        }
    }
    out
}

/// The `version` of a parsed JSON object (1 when absent or not a number).
pub(crate) fn json_version(obj: &serde_json::Map<String, serde_json::Value>) -> u64 {
    obj.get("version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1)
}

/// Logs a warning when a file is newer than this build understands.
pub(crate) fn warn_if_newer(what: &str, version: u64) {
    if version > u64::from(FORMAT_VERSION) {
        log::warn!(
            "{what}: file version {version} is newer than {FORMAT_VERSION}; reading what is understood"
        );
    }
}
