//! Locations, zoom and the saved state of one view. See `docs/ARCHITECTURE.md` §4.1.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::nav::history::{History, Place};

/// A point in a document, in page space (R-5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Location {
    /// Zero-based page index.
    pub page: u32,
    /// Horizontal position in points from the left edge of the page.
    pub x: f32,
    /// Vertical position in points from the top edge of the page.
    pub y: f32,
}

impl Location {
    /// Creates a location.
    pub fn new(page: u32, x: f32, y: f32) -> Location {
        Location { page, x, y }
    }
}

/// How the zoom of a view is determined.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum ZoomMode {
    /// Fit the widest page to the viewport width.
    FitWidth,
    /// Fit the page at the anchor into the viewport.
    FitPage,
    /// A fixed factor. `Custom(1.0)` is 100%.
    Custom(f32),
}

/// The saveable state of one view.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewState {
    /// The document location the top-left corner of the viewport points at.
    pub anchor: Location,
    /// Zoom of the view.
    pub zoom: ZoomMode,
    /// Places visited by long jumps.
    pub history: History,
}

impl ViewState {
    /// The state of a freshly opened document: top of the first page, fit to width, and a
    /// history that starts at that spot.
    pub fn new(doc: PathBuf) -> ViewState {
        let anchor = Location::new(0, 0.0, 0.0);
        ViewState {
            anchor,
            zoom: ZoomMode::FitWidth,
            history: History::new(Place { doc, loc: anchor }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_view_state_starts_at_top_with_fit_width() {
        let v = ViewState::new(PathBuf::from("/tmp/a.pdf"));
        assert_eq!(v.anchor, Location::new(0, 0.0, 0.0));
        assert_eq!(v.zoom, ZoomMode::FitWidth);
        assert_eq!(v.history.current().doc, PathBuf::from("/tmp/a.pdf"));
        assert_eq!(v.history.current().loc, v.anchor);
        assert!(!v.history.can_back());
        assert!(!v.history.can_forward());
    }

    #[test]
    fn view_state_serde_roundtrip() {
        let mut v = ViewState::new(PathBuf::from("/tmp/a.pdf"));
        v.zoom = ZoomMode::Custom(1.5);
        let s = serde_json::to_string(&v).unwrap();
        let back: ViewState = serde_json::from_str(&s).unwrap();
        assert_eq!(back, v);
    }
}
