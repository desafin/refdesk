//! refdesk-core: document engine and view logic for refdesk.
//!
//! This crate holds everything that is not drawing or input handling. It must not depend on
//! egui, eframe or winit (rule R-4 in `docs/ARCHITECTURE.md`). Modules are added by later
//! work packages.

pub mod engine;
pub mod geom;
pub mod layout;
pub mod link;
pub mod model;
pub mod nav;
pub mod outline;
#[cfg(any(test, feature = "testutil"))]
pub mod testutil;
pub mod text;

pub use engine::{
    DocStatus, DocumentHandle, DocumentInfo, EngineError, Notifier, Priority, RenderKey,
    RenderedImage, RenderedPage,
};
pub use geom::{Point, Quad, Rect, Size};
pub use layout::{LayoutParams, PageLayout};
pub use link::{Link, LinkTarget, RawLink};
pub use model::{Location, ViewState, ZoomMode};
pub use nav::dest::{DestKind, Destination};
pub use nav::history::{History, Place};
pub use outline::{Outline, OutlineItem, OutlineNode};
pub use text::{CopyOptions, PageText, Selection, TextBlock, TextChar, TextLine, TextPos};
