//! refdesk-core: document engine and view logic for refdesk.
//!
//! This crate holds everything that is not drawing or input handling. It must not depend on
//! egui, eframe or winit (rule R-4 in `docs/ARCHITECTURE.md`). Modules are added by later
//! work packages.

pub mod diff;
pub mod engine;
pub mod fingerprint;
pub mod geom;
pub mod ipc;
pub mod layout;
pub mod link;
pub mod model;
pub mod nav;
pub mod outline;
pub mod search;
pub mod store;
#[cfg(any(test, feature = "testutil"))]
pub mod testutil;
pub mod text;
pub mod workspace;

pub use diff::{
    Change, ChangeKind, DiffJob, DiffLimits, DiffOptions, DiffResult, Mark, MarkKind, Side, Span,
};
pub use engine::{
    DocStatus, DocumentHandle, DocumentInfo, EngineError, EngineOptions, Notifier, Priority,
    RenderKey, RenderedImage, RenderedPage,
};
pub use fingerprint::{FileStamp, fingerprint_file, stamp};
pub use geom::{Point, Quad, Rect, Size};
pub use ipc::{IpcListener, IpcRequest, OpenTarget};
pub use layout::{LayoutParams, PageLayout};
pub use link::{Link, LinkTarget, RawLink};
pub use model::{Location, ViewState, ZoomMode};
pub use nav::dest::{DestKind, Destination};
pub use nav::history::{History, Place};
pub use outline::{Outline, OutlineItem, OutlineNode};
pub use search::{
    MatchPart, SearchError, SearchJob, SearchMatch, SearchOptions, SearchQuery, SearchSnapshot,
};
pub use store::{
    CaptureConfig, Config, DefaultZoom, DocRecord, DocStateDb, Language, LayoutNode, Paths, Prefs,
    RecentFolder, ScrollSpeedItem, Session, SessionTab, SessionWindow, SessionWorkspace, Theme,
    atomic_write, clamp_scroll_speed, effective_scroll_speed, line_scroll_speed,
    multiplier_of_line_scroll_speed, scroll_speed_label, scroll_speed_menu, step_scroll_speed,
};
pub use text::{
    CopyOptions, PageText, Selection, TextBlock, TextChar, TextLine, TextPos, selection_text,
};
pub use workspace::{
    EntryKind, ScanLimits, WorkspaceEntry, WorkspaceIndex, WorkspaceScan, WorkspaceWatcher,
    natural_cmp,
};
