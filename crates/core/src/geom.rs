//! Basic geometry in page space (points, origin at the top-left of the page, y grows downwards).
//!
//! See `docs/ARCHITECTURE.md` §3 and §4.1.

use serde::{Deserialize, Serialize};

/// A point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate (grows downwards).
    pub y: f32,
}

impl Point {
    /// Creates a point.
    pub fn new(x: f32, y: f32) -> Point {
        Point { x, y }
    }
}

/// A width and a height.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Size {
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Size {
    /// Creates a size.
    pub fn new(w: f32, h: f32) -> Size {
        Size { w, h }
    }
}

/// An axis-aligned rectangle. Invariant: `x0 <= x1` and `y0 <= y1`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    /// Left edge.
    pub x0: f32,
    /// Top edge.
    pub y0: f32,
    /// Right edge.
    pub x1: f32,
    /// Bottom edge.
    pub y1: f32,
}

impl Rect {
    /// Creates a rectangle from its edges. The arguments are used as given; use
    /// [`Rect::from_points`] when the order of the corners is not known.
    pub fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    /// Creates the smallest rectangle that contains both points, in any order.
    pub fn from_points(a: Point, b: Point) -> Rect {
        Rect {
            x0: a.x.min(b.x),
            y0: a.y.min(b.y),
            x1: a.x.max(b.x),
            y1: a.y.max(b.y),
        }
    }

    /// Horizontal extent.
    pub fn width(&self) -> f32 {
        self.x1 - self.x0
    }

    /// Vertical extent.
    pub fn height(&self) -> f32 {
        self.y1 - self.y0
    }

    /// Whether the point lies inside the rectangle. The edges count as inside.
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x0 && p.x <= self.x1 && p.y >= self.y0 && p.y <= self.y1
    }

    /// Whether the two rectangles share at least one point. Touching edges count.
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x0 <= other.x1 && other.x0 <= self.x1 && self.y0 <= other.y1 && other.y0 <= self.y1
    }

    /// The smallest rectangle that contains both rectangles.
    pub fn union(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }
}

/// A quadrilateral, used for glyphs of rotated or skewed text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Quad {
    /// Upper left corner.
    pub ul: Point,
    /// Upper right corner.
    pub ur: Point,
    /// Lower left corner.
    pub ll: Point,
    /// Lower right corner.
    pub lr: Point,
}

impl Quad {
    /// The axis-aligned bounding rectangle of the four corners.
    pub fn bounds(&self) -> Rect {
        let pts = [self.ul, self.ur, self.ll, self.lr];
        let mut r = Rect::from_points(self.ul, self.ul);
        for p in &pts[1..] {
            r = r.union(&Rect::from_points(*p, *p));
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::new(x0, y0, x1, y1)
    }

    #[test]
    fn quad_bounds_of_axis_aligned_quad() {
        let q = Quad {
            ul: Point::new(10.0, 20.0),
            ur: Point::new(30.0, 20.0),
            ll: Point::new(10.0, 40.0),
            lr: Point::new(30.0, 40.0),
        };
        assert_eq!(q.bounds(), r(10.0, 20.0, 30.0, 40.0));
    }

    #[test]
    fn quad_bounds_of_skewed_quad() {
        let q = Quad {
            ul: Point::new(12.0, 18.0),
            ur: Point::new(34.0, 22.0),
            ll: Point::new(8.0, 41.0),
            lr: Point::new(30.0, 45.0),
        };
        assert_eq!(q.bounds(), r(8.0, 18.0, 34.0, 45.0));
    }

    #[test]
    fn rect_size() {
        let a = r(1.0, 2.0, 4.0, 8.0);
        assert_eq!(a.width(), 3.0);
        assert_eq!(a.height(), 6.0);
    }

    #[test]
    fn rect_contains_includes_edges() {
        let a = r(0.0, 0.0, 10.0, 10.0);
        assert!(a.contains(Point::new(5.0, 5.0)));
        assert!(a.contains(Point::new(0.0, 0.0)));
        assert!(a.contains(Point::new(10.0, 10.0)));
        assert!(!a.contains(Point::new(10.01, 5.0)));
        assert!(!a.contains(Point::new(5.0, -0.01)));
    }

    #[test]
    fn rect_intersects() {
        let a = r(0.0, 0.0, 10.0, 10.0);
        assert!(a.intersects(&r(5.0, 5.0, 15.0, 15.0)));
        assert!(a.intersects(&r(10.0, 10.0, 20.0, 20.0)), "touching corners");
        assert!(a.intersects(&r(2.0, 2.0, 3.0, 3.0)), "contained");
        assert!(!a.intersects(&r(11.0, 0.0, 20.0, 10.0)));
        assert!(!a.intersects(&r(0.0, -5.0, 10.0, -1.0)));
    }

    #[test]
    fn rect_union() {
        let a = r(0.0, 5.0, 10.0, 10.0);
        let b = r(-3.0, 6.0, 4.0, 20.0);
        assert_eq!(a.union(&b), r(-3.0, 5.0, 10.0, 20.0));
        assert_eq!(a.union(&b), b.union(&a));
    }

    #[test]
    fn rect_from_points_orders_corners() {
        let a = Rect::from_points(Point::new(10.0, 2.0), Point::new(4.0, 9.0));
        assert_eq!(a, r(4.0, 2.0, 10.0, 9.0));
    }
}
