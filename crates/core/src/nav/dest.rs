//! Link destinations and their resolution to a [`Location`]. See `docs/ARCHITECTURE.md` §4.4.

use serde::{Deserialize, Serialize};

use crate::geom::{Rect, Size};
use crate::model::Location;

/// The kind of a destination. Coordinates are in page space (top-left origin, y downwards).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum DestKind {
    /// A position, either coordinate may be unspecified.
    Xyz {
        /// Horizontal position.
        left: Option<f32>,
        /// Vertical position.
        top: Option<f32>,
    },
    /// Fit the whole page.
    Fit,
    /// Fit the page width, starting at `top`.
    FitH {
        /// Vertical position.
        top: Option<f32>,
    },
    /// Fit the page height, starting at `left`.
    FitV {
        /// Horizontal position.
        left: Option<f32>,
    },
    /// Fit a rectangle.
    FitR(Rect),
}

/// A destination inside a document, normalized to page space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Destination {
    /// Zero-based page index.
    pub page: u32,
    /// What part of the page.
    pub kind: DestKind,
}

/// Turns a destination into the location the view should scroll to.
///
/// Missing values become 0 and negative or non-finite values are raised to 0, so that a slightly
/// off destination does not show the previous page. The zoom a destination asks for is ignored
/// (LINK-1). `page_size` is part of the contract for callers that know it; the current rules do
/// not need it, values beyond the page are passed through unchanged.
pub fn resolve(dest: &Destination, _page_size: Size) -> Location {
    let (x, y) = match dest.kind {
        DestKind::Xyz { left, top } => (left, top),
        DestKind::Fit => (None, None),
        DestKind::FitH { top } => (None, top),
        DestKind::FitV { left } => (left, None),
        DestKind::FitR(r) => (Some(r.x0), Some(r.y0)),
    };
    Location {
        page: dest.page,
        x: clamp(x),
        y: clamp(y),
    }
}

fn clamp(v: Option<f32>) -> f32 {
    match v {
        Some(v) if v.is_finite() && v > 0.0 => v,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: Size = Size { w: 612.0, h: 792.0 };

    fn d(kind: DestKind) -> Destination {
        Destination { page: 3, kind }
    }

    #[test]
    fn resolve_xyz() {
        let loc = resolve(
            &d(DestKind::Xyz {
                left: Some(72.0),
                top: Some(100.0),
            }),
            PAGE,
        );
        assert_eq!(loc, Location::new(3, 72.0, 100.0));
    }

    #[test]
    fn resolve_fit_is_page_origin() {
        assert_eq!(resolve(&d(DestKind::Fit), PAGE), Location::new(3, 0.0, 0.0));
    }

    #[test]
    fn resolve_fit_h_uses_top_only() {
        let loc = resolve(&d(DestKind::FitH { top: Some(200.0) }), PAGE);
        assert_eq!(loc, Location::new(3, 0.0, 200.0));
    }

    #[test]
    fn resolve_fit_v_uses_left_only() {
        let loc = resolve(&d(DestKind::FitV { left: Some(50.0) }), PAGE);
        assert_eq!(loc, Location::new(3, 50.0, 0.0));
    }

    #[test]
    fn resolve_fit_r_uses_upper_left_corner() {
        let loc = resolve(
            &d(DestKind::FitR(Rect::new(10.0, 20.0, 300.0, 400.0))),
            PAGE,
        );
        assert_eq!(loc, Location::new(3, 10.0, 20.0));
    }

    #[test]
    fn resolve_missing_values_are_zero() {
        let loc = resolve(
            &d(DestKind::Xyz {
                left: None,
                top: None,
            }),
            PAGE,
        );
        assert_eq!(loc, Location::new(3, 0.0, 0.0));
        let loc = resolve(
            &d(DestKind::Xyz {
                left: None,
                top: Some(5.0),
            }),
            PAGE,
        );
        assert_eq!(loc, Location::new(3, 0.0, 5.0));
        assert_eq!(
            resolve(&d(DestKind::FitH { top: None }), PAGE),
            Location::new(3, 0.0, 0.0)
        );
        assert_eq!(
            resolve(&d(DestKind::FitV { left: None }), PAGE),
            Location::new(3, 0.0, 0.0)
        );
    }

    #[test]
    fn resolve_negative_values_become_zero() {
        let loc = resolve(
            &d(DestKind::Xyz {
                left: Some(-4.0),
                top: Some(-0.5),
            }),
            PAGE,
        );
        assert_eq!(loc, Location::new(3, 0.0, 0.0));
        let loc = resolve(&d(DestKind::FitR(Rect::new(-1.0, -2.0, 10.0, 10.0))), PAGE);
        assert_eq!(loc, Location::new(3, 0.0, 0.0));
    }

    #[test]
    fn resolve_non_finite_values_become_zero() {
        let loc = resolve(
            &d(DestKind::Xyz {
                left: Some(f32::NAN),
                top: Some(f32::INFINITY),
            }),
            PAGE,
        );
        assert_eq!(loc, Location::new(3, 0.0, 0.0));
    }
}
