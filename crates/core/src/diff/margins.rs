//! Excluded page margins of a comparison (`docs/SPEC.md` DIFF-11).
//!
//! Running headers whose text changes from page to page (a chapter title, a document number with
//! the page number, a date) are not found by [`noise`](super::noise). The user can instead name
//! the edges of the page that are left out: each [`Margins`] value is a fraction of the page
//! height (top, bottom) or of the page width (left, right), so pages of different sizes are
//! treated alike. A word is left out when the centre of its box lies inside an excluded band.

use serde::{Deserialize, Serialize};

use crate::geom::{Point, Rect, Size};

/// The largest fraction of a page one margin can take.
pub const MAX_MARGIN: f32 = 0.4;

/// How many steps of the margin controls make a whole page (a step is 0.5 %).
pub const MARGIN_STEPS: f32 = 200.0;

/// The step of the margin controls, as a fraction (0.5 %).
pub const MARGIN_STEP: f32 = 1.0 / MARGIN_STEPS;

/// One edge of a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarginSide {
    /// The top edge; the margin is a fraction of the page height.
    Top,
    /// The bottom edge; a fraction of the page height.
    Bottom,
    /// The left edge; a fraction of the page width.
    Left,
    /// The right edge; a fraction of the page width.
    Right,
}

impl MarginSide {
    /// Every side, in the order the controls and the summary use.
    pub const ALL: [MarginSide; 4] = [
        MarginSide::Top,
        MarginSide::Bottom,
        MarginSide::Left,
        MarginSide::Right,
    ];
}

/// How much of each page edge a comparison leaves out. All zero (the default) leaves nothing
/// out. Values are fractions in `0.0 ..= MAX_MARGIN`; use [`Margins::clamped`] on anything that
/// comes from outside.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Margins {
    /// Fraction of the page height at the top.
    pub top: f32,
    /// Fraction of the page height at the bottom.
    pub bottom: f32,
    /// Fraction of the page width at the left.
    pub left: f32,
    /// Fraction of the page width at the right.
    pub right: f32,
}

impl Margins {
    /// The value of one side.
    pub fn get(&self, side: MarginSide) -> f32 {
        match side {
            MarginSide::Top => self.top,
            MarginSide::Bottom => self.bottom,
            MarginSide::Left => self.left,
            MarginSide::Right => self.right,
        }
    }

    /// Sets one side as it is (see [`Margins::clamped`]).
    pub fn set(&mut self, side: MarginSide, value: f32) {
        match side {
            MarginSide::Top => self.top = value,
            MarginSide::Bottom => self.bottom = value,
            MarginSide::Left => self.left = value,
            MarginSide::Right => self.right = value,
        }
    }

    /// Whether nothing is left out.
    pub fn is_zero(&self) -> bool {
        MarginSide::ALL.iter().all(|s| self.get(*s) <= 0.0)
    }

    /// The margins with every value limited to `0.0 ..= MAX_MARGIN` (a value that is not a number
    /// becomes 0). Two margins on opposite sides therefore never add up to more than twice
    /// [`MAX_MARGIN`], 0.8 of the page, and a fifth of it is always left in the middle.
    pub fn clamped(self) -> Margins {
        let fix = |v: f32| {
            if v.is_finite() {
                v.clamp(0.0, MAX_MARGIN)
            } else {
                0.0
            }
        };
        Margins {
            top: fix(self.top),
            bottom: fix(self.bottom),
            left: fix(self.left),
            right: fix(self.right),
        }
    }

    /// The margins with every value rounded to the nearest [`MARGIN_STEP`] and then
    /// [clamped](Margins::clamped).
    pub fn snapped(self) -> Margins {
        let snap = |v: f32| {
            if v.is_finite() {
                (v * MARGIN_STEPS).round() / MARGIN_STEPS
            } else {
                0.0
            }
        };
        Margins {
            top: snap(self.top),
            bottom: snap(self.bottom),
            left: snap(self.left),
            right: snap(self.right),
        }
        .clamped()
    }

    /// Whether a word whose box has its centre at `centre` (page space) is left out on a page of
    /// `size`. A centre exactly on the inner edge of a band is kept. A side whose margin is zero
    /// leaves nothing out, not even what lies beyond the page edge. A page without a usable size
    /// leaves nothing out.
    pub fn excludes(&self, size: Size, centre: Point) -> bool {
        if !(size.w.is_finite() && size.h.is_finite() && size.w > 0.0 && size.h > 0.0) {
            return false;
        }
        let m = self.clamped();
        (m.top > 0.0 && centre.y < size.h * m.top)
            || (m.bottom > 0.0 && centre.y > size.h * (1.0 - m.bottom))
            || (m.left > 0.0 && centre.x < size.w * m.left)
            || (m.right > 0.0 && centre.x > size.w * (1.0 - m.right))
    }

    /// The excluded bands of a page of `size`, in page space, for drawing. A side with margin 0
    /// has no band. The top and bottom bands run over the whole width; the left and right bands
    /// only between them, so no area is in two bands.
    pub fn bands(&self, size: Size) -> Vec<(MarginSide, Rect)> {
        let m = self.clamped();
        let (w, h) = (size.w, size.h);
        let (top, bottom) = (h * m.top, h * m.bottom);
        let mut out = Vec::new();
        if m.top > 0.0 {
            out.push((MarginSide::Top, Rect::new(0.0, 0.0, w, top)));
        }
        if m.bottom > 0.0 {
            out.push((MarginSide::Bottom, Rect::new(0.0, h - bottom, w, h)));
        }
        if m.left > 0.0 {
            out.push((
                MarginSide::Left,
                Rect::new(0.0, top, w * m.left, h - bottom),
            ));
        }
        if m.right > 0.0 {
            out.push((
                MarginSide::Right,
                Rect::new(w * (1.0 - m.right), top, w, h - bottom),
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: Size = Size { w: 400.0, h: 800.0 };

    fn m(top: f32, bottom: f32, left: f32, right: f32) -> Margins {
        Margins {
            top,
            bottom,
            left,
            right,
        }
    }

    #[test]
    fn the_default_leaves_nothing_out_even_beyond_the_page() {
        let none = Margins::default();
        assert!(none.is_zero());
        for p in [
            Point::new(-50.0, -50.0),
            Point::new(1000.0, 5000.0),
            Point::new(200.0, 400.0),
        ] {
            assert!(!none.excludes(PAGE, p));
        }
    }

    #[test]
    fn a_centre_just_inside_a_band_is_left_out_and_just_outside_is_kept() {
        let top = m(0.25, 0.0, 0.0, 0.0);
        assert!(top.excludes(PAGE, Point::new(100.0, 199.5)));
        assert!(!top.excludes(PAGE, Point::new(100.0, 200.0)), "on the edge");
        assert!(!top.excludes(PAGE, Point::new(100.0, 200.5)));
        let bottom = m(0.0, 0.25, 0.0, 0.0);
        assert!(bottom.excludes(PAGE, Point::new(100.0, 600.5)));
        assert!(!bottom.excludes(PAGE, Point::new(100.0, 600.0)));
        assert!(!bottom.excludes(PAGE, Point::new(100.0, 599.5)));
        let left = m(0.0, 0.0, 0.25, 0.0);
        assert!(left.excludes(PAGE, Point::new(99.5, 400.0)));
        assert!(!left.excludes(PAGE, Point::new(100.5, 400.0)));
        let right = m(0.0, 0.0, 0.0, 0.25);
        assert!(right.excludes(PAGE, Point::new(300.5, 400.0)));
        assert!(!right.excludes(PAGE, Point::new(299.5, 400.0)));
    }

    #[test]
    fn the_fraction_is_of_the_size_of_each_page() {
        let top = m(0.25, 0.0, 0.0, 0.0);
        let small = Size::new(200.0, 400.0);
        assert!(top.excludes(small, Point::new(10.0, 99.0)));
        assert!(!top.excludes(small, Point::new(10.0, 101.0)));
        assert!(!top.excludes(Size::new(0.0, 0.0), Point::new(10.0, 1.0)));
        assert!(!top.excludes(Size::new(f32::NAN, 800.0), Point::new(10.0, 1.0)));
    }

    #[test]
    fn values_are_limited_and_no_pair_can_pass_eight_tenths() {
        let c = m(-1.0, 0.9, f32::NAN, f32::INFINITY).clamped();
        assert_eq!(c, m(0.0, 0.4, 0.0, 0.0));
        let c = m(0.9, 0.9, 0.9, 0.9).clamped();
        assert_eq!(c, m(0.4, 0.4, 0.4, 0.4));
        assert!(c.top + c.bottom <= 0.8 + f32::EPSILON);
        assert!(c.left + c.right <= 0.8 + f32::EPSILON);
        // The middle fifth of the page is never left out.
        assert!(!c.excludes(PAGE, Point::new(200.0, 400.0)));
        assert!(Margins::default().clamped().is_zero());
    }

    #[test]
    fn snapping_rounds_to_half_percents() {
        let s = m(0.0612, 0.0537, 0.1, 0.7).snapped();
        // Exactly the nearest number to what a user would type.
        assert_eq!((s.top, s.bottom, s.left), (0.06, 0.055, 0.1));
        assert_eq!(s.right, MAX_MARGIN);
    }

    #[test]
    fn bands_cover_each_nonzero_side_without_overlap() {
        assert!(Margins::default().bands(PAGE).is_empty());
        let bands = m(0.25, 0.125, 0.25, 0.0).bands(PAGE);
        let sides: Vec<MarginSide> = bands.iter().map(|b| b.0).collect();
        assert_eq!(
            sides,
            [MarginSide::Top, MarginSide::Bottom, MarginSide::Left]
        );
        assert_eq!(bands[0].1, Rect::new(0.0, 0.0, 400.0, 200.0));
        assert_eq!(bands[1].1, Rect::new(0.0, 700.0, 400.0, 800.0));
        assert_eq!(bands[2].1, Rect::new(0.0, 200.0, 100.0, 700.0));
        let total: f32 = bands.iter().map(|b| b.1.width() * b.1.height()).sum();
        assert_eq!(total, 400.0 * 200.0 + 400.0 * 100.0 + 100.0 * 500.0);
    }

    #[test]
    fn get_and_set_address_the_sides() {
        let mut v = Margins::default();
        for (i, side) in MarginSide::ALL.into_iter().enumerate() {
            v.set(side, 0.1 * (i + 1) as f32);
            assert_eq!(v.get(side), 0.1 * (i + 1) as f32);
        }
        assert!(!v.is_zero());
    }
}
