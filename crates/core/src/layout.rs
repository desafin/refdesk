//! Continuous vertical page layout: where each page sits in content space, which pages are
//! visible, and the conversions between content space and page space.
//!
//! See `docs/ARCHITECTURE.md` §3 and §4.3. Everything here is pure arithmetic; there is no
//! MuPDF, file IO or threading.
//!
//! Terms: *content space* is the plane in logical pixels on which the whole document is laid out
//! top to bottom (origin at its top-left corner). *Page space* is per page, in points, with the
//! origin at the top-left corner of the page.

use std::ops::Range;

use crate::geom::{Point, Rect, Size};
use crate::model::{Location, ZoomMode};

/// Logical pixels per point at 100% zoom (96 dpi / 72 pt per inch).
pub const PIXELS_PER_POINT_AT_100: f32 = 96.0 / 72.0;

/// Smallest zoom factor (`Custom(z)`, 1.0 = 100%).
pub const MIN_ZOOM: f32 = 0.1;

/// Largest zoom factor (`Custom(z)`, 1.0 = 100%).
pub const MAX_ZOOM: f32 = 8.0;

/// Smallest `scale` (logical pixels per point) any zoom mode can produce.
pub const MIN_SCALE: f32 = MIN_ZOOM * PIXELS_PER_POINT_AT_100;

/// Largest `scale` (logical pixels per point) any zoom mode can produce.
pub const MAX_SCALE: f32 = MAX_ZOOM * PIXELS_PER_POINT_AT_100;

/// Inputs of a layout. All lengths are in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutParams {
    /// Size of the area the document is shown in.
    pub viewport: Size,
    /// How the scale is chosen.
    pub zoom: ZoomMode,
    /// Vertical space between two consecutive pages.
    pub gap: f32,
    /// Space above the first page, below the last page, and on both sides of the widest page
    /// (when fitting).
    pub margin: f32,
    /// The page `ZoomMode::FitPage` fits into the viewport. Ignored by the other modes.
    /// An index past the last page means the last page.
    pub fit_page: u32,
}

/// The layout of one document at one scale and viewport size.
///
/// Page positions are computed once in [`PageLayout::new`] and kept as running sums, so that
/// [`PageLayout::visible_pages`] and [`PageLayout::location_at`] are binary searches.
#[derive(Clone, Debug)]
pub struct PageLayout {
    scale: f32,
    /// Page sizes in points (sanitized: finite and non-negative).
    sizes: Vec<Size>,
    /// Top edge of every page in content space.
    tops: Vec<f32>,
    /// Bottom edge of every page in content space.
    bottoms: Vec<f32>,
    content: Size,
}

/// Maps non-finite and negative lengths to 0.
fn nonneg(v: f32) -> f32 {
    if v.is_finite() { v.max(0.0) } else { 0.0 }
}

fn clamp_scale(s: f32) -> f32 {
    if s.is_nan() {
        MIN_SCALE
    } else {
        s.clamp(MIN_SCALE, MAX_SCALE)
    }
}

/// Clamps `v` into `0..=max`; NaN becomes 0. `max` must be finite and non-negative.
fn clamp_axis(v: f32, max: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, max) }
}

impl PageLayout {
    /// Computes the layout of pages of the given sizes (in points).
    ///
    /// Never panics: negative or non-finite sizes, gaps, margins and viewport sizes count as 0,
    /// and the scale is always within [`MIN_SCALE`]..=[`MAX_SCALE`].
    pub fn new(page_sizes: &[Size], params: LayoutParams) -> PageLayout {
        let viewport = Size::new(nonneg(params.viewport.w), nonneg(params.viewport.h));
        let gap = nonneg(params.gap);
        let margin = nonneg(params.margin);
        let sizes: Vec<Size> = page_sizes
            .iter()
            .map(|s| Size::new(nonneg(s.w), nonneg(s.h)))
            .collect();

        let avail_w = nonneg(viewport.w - 2.0 * margin);
        let avail_h = nonneg(viewport.h - 2.0 * margin);
        let widest = sizes.iter().map(|s| s.w).fold(0.0_f32, f32::max);
        let scale = match params.zoom {
            ZoomMode::Custom(z) => clamp_scale(z * PIXELS_PER_POINT_AT_100),
            ZoomMode::FitWidth => {
                if widest > 0.0 {
                    clamp_scale(avail_w / widest)
                } else {
                    MIN_SCALE
                }
            }
            ZoomMode::FitPage => match sizes.len().checked_sub(1) {
                None => MIN_SCALE,
                Some(last) => {
                    let s = sizes[(params.fit_page as usize).min(last)];
                    let by_w = (s.w > 0.0).then(|| avail_w / s.w);
                    let by_h = (s.h > 0.0).then(|| avail_h / s.h);
                    match (by_w, by_h) {
                        (Some(a), Some(b)) => clamp_scale(a.min(b)),
                        (Some(a), None) | (None, Some(a)) => clamp_scale(a),
                        (None, None) => MIN_SCALE,
                    }
                }
            },
        };

        let mut tops = Vec::with_capacity(sizes.len());
        let mut bottoms = Vec::with_capacity(sizes.len());
        // Sum in f64 so that thousands of pages do not accumulate visible rounding error.
        let mut y = f64::from(margin);
        for s in &sizes {
            tops.push(y as f32);
            y += f64::from(s.h) * f64::from(scale);
            bottoms.push(y as f32);
            y += f64::from(gap);
        }

        let content = if sizes.is_empty() {
            viewport
        } else {
            let height = (y - f64::from(gap) + f64::from(margin)) as f32;
            let width = viewport.w.max(widest * scale + 2.0 * margin);
            Size::new(width, height)
        };

        PageLayout {
            scale,
            sizes,
            tops,
            bottoms,
            content,
        }
    }

    /// Logical pixels per point.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Number of pages.
    pub fn page_count(&self) -> u32 {
        self.sizes.len() as u32
    }

    /// Size of the whole laid-out document in content space. For a document without pages this
    /// is the viewport size the layout was made for.
    pub fn content_size(&self) -> Size {
        self.content
    }

    /// The rectangle of a page in content space. A page number past the last page means the
    /// last page. A document without pages gives an empty rectangle at the origin.
    pub fn page_rect(&self, page: u32) -> Rect {
        let Some(i) = self.page_index(page) else {
            return Rect::default();
        };
        let w = self.sizes[i].w * self.scale;
        let x0 = (self.content.w - w) / 2.0;
        Rect::new(x0, self.tops[i], x0 + w, self.bottoms[i])
    }

    /// The pages that overlap the viewport by more than a zero-width line, for a viewport whose
    /// top-left corner is at `scroll` in content space.
    ///
    /// When only the gap between two pages (or the margin) is visible the range is empty and
    /// starts and ends at the next page (or at the page count if there is none).
    pub fn visible_pages(&self, scroll: Point, viewport: Size) -> Range<u32> {
        let view_top = scroll.y;
        let view_bottom = scroll.y + nonneg(viewport.h);
        // First page that ends below the top of the viewport.
        let start = self.bottoms.partition_point(|&b| b <= view_top);
        // Pages that start above the bottom of the viewport.
        let end = self.tops.partition_point(|&t| t < view_bottom).max(start);
        start as u32..end as u32
    }

    /// The page at a point in content space and the position on it. A point between pages or
    /// outside the document belongs to the nearest page (the earlier one on a tie), and the
    /// position is clamped into that page. A document without pages gives page 0 at (0, 0).
    pub fn location_at(&self, content: Point) -> Location {
        let n = self.sizes.len();
        if n == 0 {
            return Location::new(0, 0.0, 0.0);
        }
        // First page that ends below the point.
        let mut i = self.bottoms.partition_point(|&b| b <= content.y).min(n - 1);
        if i > 0 && content.y < self.tops[i] {
            let from_prev = content.y - self.bottoms[i - 1];
            let from_next = self.tops[i] - content.y;
            if from_prev <= from_next {
                i -= 1;
            }
        }
        let rect = self.page_rect(i as u32);
        let size = self.sizes[i];
        let x = clamp_axis((content.x - rect.x0) / self.scale, size.w);
        let y = clamp_axis((content.y - rect.y0) / self.scale, size.h);
        Location::new(i as u32, x, y)
    }

    /// The point in content space of a document location; the inverse of
    /// [`PageLayout::location_at`] for points inside a page. A page past the last page means the
    /// last one; the position is not clamped to the page. A document without pages gives the
    /// origin.
    pub fn content_pos(&self, loc: Location) -> Point {
        if self.sizes.is_empty() {
            return Point::default();
        }
        let rect = self.page_rect(loc.page);
        Point::new(rect.x0 + loc.x * self.scale, rect.y0 + loc.y * self.scale)
    }

    /// The largest scroll offset for a viewport of the given size. Zero on an axis where the
    /// content is not larger than the viewport.
    pub fn max_scroll(&self, viewport: Size) -> Point {
        Point::new(
            nonneg(self.content.w - nonneg(viewport.w)),
            nonneg(self.content.h - nonneg(viewport.h)),
        )
    }

    /// Limits a scroll offset to the range of valid offsets.
    pub fn clamp_scroll(&self, scroll: Point, viewport: Size) -> Point {
        let max = self.max_scroll(viewport);
        Point::new(clamp_axis(scroll.x, max.x), clamp_axis(scroll.y, max.y))
    }

    /// The scroll offset that puts the document location `loc` at `viewport_pos` inside the
    /// viewport, limited to the valid range. After a zoom change, calling this on the new
    /// layout with the location that was under the cursor keeps that spot under the cursor.
    pub fn scroll_to_keep(&self, loc: Location, viewport_pos: Point, viewport: Size) -> Point {
        let p = self.content_pos(loc);
        self.clamp_scroll(
            Point::new(p.x - viewport_pos.x, p.y - viewport_pos.y),
            viewport,
        )
    }

    /// Index into the page vectors for a page number, or `None` if there are no pages.
    fn page_index(&self, page: u32) -> Option<usize> {
        self.sizes
            .len()
            .checked_sub(1)
            .map(|last| (page as usize).min(last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: f32 = PIXELS_PER_POINT_AT_100;

    fn params(zoom: ZoomMode) -> LayoutParams {
        LayoutParams {
            viewport: Size::new(800.0, 600.0),
            zoom,
            gap: 10.0,
            margin: 20.0,
            fit_page: 0,
        }
    }

    fn letter(n: usize) -> Vec<Size> {
        vec![Size::new(612.0, 792.0); n]
    }

    fn near(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-2, "{a} != {b}");
    }

    fn near_loc(a: Location, b: Location) {
        assert_eq!(a.page, b.page);
        near(a.x, b.x);
        near(a.y, b.y);
    }

    #[test]
    fn fit_width_scale_matches_formula() {
        let l = PageLayout::new(&letter(3), params(ZoomMode::FitWidth));
        near(l.scale(), (800.0 - 40.0) / 612.0);
    }

    #[test]
    fn fit_width_uses_the_widest_page() {
        let sizes = [
            Size::new(400.0, 600.0),
            Size::new(900.0, 600.0),
            Size::new(500.0, 600.0),
        ];
        let l = PageLayout::new(&sizes, params(ZoomMode::FitWidth));
        near(l.scale(), (800.0 - 40.0) / 900.0);
        // The widest page fills the width between the margins.
        let r = l.page_rect(1);
        near(r.width(), 760.0);
        near(r.x0, 20.0);
    }

    #[test]
    fn fit_page_fits_a_tall_page() {
        // Tall page: the height limits. Viewport 800x600, margin 20 -> 760x560 available.
        let sizes = [Size::new(300.0, 1000.0)];
        let l = PageLayout::new(&sizes, params(ZoomMode::FitPage));
        near(l.scale(), 560.0 / 1000.0);
        let r = l.page_rect(0);
        assert!(r.width() <= 760.0 + 1e-3 && r.height() <= 560.0 + 1e-3);
        near(r.height(), 560.0);
    }

    #[test]
    fn fit_page_fits_a_wide_page() {
        let sizes = [Size::new(1200.0, 400.0)];
        let l = PageLayout::new(&sizes, params(ZoomMode::FitPage));
        near(l.scale(), 760.0 / 1200.0);
        let r = l.page_rect(0);
        near(r.width(), 760.0);
        assert!(r.height() <= 560.0);
    }

    #[test]
    fn fit_page_uses_the_reference_page_and_clamps_the_index() {
        let sizes = [Size::new(300.0, 1000.0), Size::new(1200.0, 400.0)];
        let mut p = params(ZoomMode::FitPage);
        p.fit_page = 1;
        near(PageLayout::new(&sizes, p).scale(), 760.0 / 1200.0);
        p.fit_page = 99;
        near(PageLayout::new(&sizes, p).scale(), 760.0 / 1200.0);
        p.fit_page = 0;
        near(PageLayout::new(&sizes, p).scale(), 560.0 / 1000.0);
    }

    #[test]
    fn custom_100_percent_is_96_over_72() {
        let l = PageLayout::new(&letter(1), params(ZoomMode::Custom(1.0)));
        near(l.scale(), 96.0 / 72.0);
        near(
            PageLayout::new(&letter(1), params(ZoomMode::Custom(2.0))).scale(),
            2.0 * 96.0 / 72.0,
        );
    }

    #[test]
    fn custom_zoom_is_clamped() {
        for z in [0.0, -3.0, 0.01, f32::NAN, f32::NEG_INFINITY] {
            let l = PageLayout::new(&letter(1), params(ZoomMode::Custom(z)));
            near(l.scale(), MIN_SCALE);
        }
        for z in [8.5, 100.0, f32::INFINITY] {
            let l = PageLayout::new(&letter(1), params(ZoomMode::Custom(z)));
            near(l.scale(), MAX_SCALE);
        }
        near(MIN_SCALE, 0.1 * 96.0 / 72.0);
        near(MAX_SCALE, 8.0 * 96.0 / 72.0);
    }

    #[test]
    fn fit_modes_are_clamped_to_the_same_range() {
        // A tiny page in a big viewport would need a huge scale.
        let tiny = [Size::new(1.0, 1.0)];
        near(
            PageLayout::new(&tiny, params(ZoomMode::FitWidth)).scale(),
            MAX_SCALE,
        );
        near(
            PageLayout::new(&tiny, params(ZoomMode::FitPage)).scale(),
            MAX_SCALE,
        );
        // A huge page needs less than the minimum.
        let huge = [Size::new(1.0e6, 1.0e6)];
        near(
            PageLayout::new(&huge, params(ZoomMode::FitWidth)).scale(),
            MIN_SCALE,
        );
        near(
            PageLayout::new(&huge, params(ZoomMode::FitPage)).scale(),
            MIN_SCALE,
        );
    }

    #[test]
    fn pages_stack_with_gaps_and_do_not_overlap() {
        let l = PageLayout::new(&letter(4), params(ZoomMode::Custom(1.0)));
        let h = 792.0 * K;
        let mut prev_bottom = 20.0 - 10.0; // so that page 0 starts at the margin
        for i in 0..4 {
            let r = l.page_rect(i);
            near(r.y0 - prev_bottom, 10.0);
            near(r.height(), h);
            prev_bottom = r.y1;
        }
        near(l.page_rect(0).y0, 20.0);
        near(l.content_size().h, 20.0 + 4.0 * h + 3.0 * 10.0 + 20.0);
    }

    #[test]
    fn narrow_pages_are_centered() {
        let sizes = [
            Size::new(600.0, 100.0),
            Size::new(300.0, 100.0),
            Size::new(450.0, 100.0),
        ];
        let l = PageLayout::new(&sizes, params(ZoomMode::Custom(1.0)));
        let cw = l.content_size().w;
        for i in 0..3 {
            let r = l.page_rect(i);
            near(r.x0 + r.x1, cw); // symmetric around the center
            near(r.width(), sizes[i as usize].w * K);
        }
        let wide = l.page_rect(0);
        let narrow = l.page_rect(1);
        near(narrow.x0 - wide.x0, (600.0 - 300.0) * K / 2.0);
    }

    #[test]
    fn content_is_wider_than_viewport_when_zoomed_in() {
        let l = PageLayout::new(&letter(1), params(ZoomMode::Custom(2.0)));
        near(l.content_size().w, 612.0 * 2.0 * K + 40.0);
        assert!(l.max_scroll(Size::new(800.0, 600.0)).x > 0.0);
        // Narrow content is as wide as the viewport and the page is centered in it.
        let l = PageLayout::new(&letter(1), params(ZoomMode::Custom(0.5)));
        near(l.content_size().w, 800.0);
        near(l.max_scroll(Size::new(800.0, 600.0)).x, 0.0);
    }

    #[test]
    fn page_rect_past_the_end_is_the_last_page() {
        let l = PageLayout::new(&letter(3), params(ZoomMode::Custom(1.0)));
        assert_eq!(l.page_rect(2), l.page_rect(3));
        assert_eq!(l.page_rect(2), l.page_rect(u32::MAX));
    }

    /// Layout of ten letter pages at 100%; page `i` spans `top(i)..top(i) + h`.
    fn ten() -> (PageLayout, f32, f32) {
        let l = PageLayout::new(&letter(10), params(ZoomMode::Custom(1.0)));
        (l, 792.0 * K, 10.0)
    }

    fn view(y: f32) -> Point {
        Point::new(0.0, y)
    }

    const VP: Size = Size { w: 800.0, h: 600.0 };

    #[test]
    fn visible_pages_at_the_top() {
        let (l, h, _) = ten();
        // 600 px viewport from the top: page 0 spans 20..20+1056, which covers it all.
        assert_eq!(l.visible_pages(view(0.0), VP), 0..1);
        assert!(h > 600.0);
    }

    #[test]
    fn visible_pages_in_the_middle() {
        let (l, h, g) = ten();
        // Start in the middle of page 3 and look across the boundary to page 4.
        let y = l.page_rect(3).y0 + h - 100.0;
        assert_eq!(l.visible_pages(view(y), VP), 3..5);
        // A viewport inside a single page.
        let y = l.page_rect(5).y0 + 100.0;
        assert_eq!(l.visible_pages(view(y), VP), 5..6);
        assert!(g > 0.0);
    }

    #[test]
    fn visible_pages_at_the_bottom() {
        let (l, _, _) = ten();
        let max = l.max_scroll(VP);
        assert_eq!(l.visible_pages(max, VP), 9..10);
        // Scrolled far past the end: nothing is visible.
        assert_eq!(l.visible_pages(view(1.0e9), VP), 10..10);
    }

    #[test]
    fn visible_pages_at_page_boundaries() {
        let (l, h, g) = ten();
        let top1 = l.page_rect(1).y0;
        // The viewport ends exactly where page 1 starts: page 1 is not visible.
        assert_eq!(l.visible_pages(view(top1 - 600.0), VP), 0..1);
        // One pixel more and page 1 shows.
        assert_eq!(l.visible_pages(view(top1 - 599.0), VP), 0..2);
        // The viewport starts exactly where page 0 ends: page 0 is not visible.
        let bottom0 = l.page_rect(0).y0 + h;
        assert_eq!(l.visible_pages(view(bottom0), VP), 1..2);
        // One pixel above that and page 0 still shows.
        assert_eq!(l.visible_pages(view(bottom0 - 1.0), VP), 0..2);
        assert!(g > 0.0);
    }

    #[test]
    fn visible_pages_with_only_a_gap_visible_is_empty() {
        let (l, h, g) = ten();
        let bottom0 = l.page_rect(0).y0 + h;
        // A 4 px viewport in the middle of the 10 px gap between pages 0 and 1.
        let r = l.visible_pages(view(bottom0 + 3.0), Size::new(800.0, 4.0));
        assert!(r.is_empty());
        assert_eq!(r, 1..1);
        assert!(g > 4.0 + 3.0);
        // Only the top margin visible.
        let r = l.visible_pages(view(0.0), Size::new(800.0, 10.0));
        assert_eq!(r, 0..0);
    }

    #[test]
    fn visible_pages_before_the_start_and_with_negative_scroll() {
        let (l, _, _) = ten();
        assert_eq!(l.visible_pages(view(-1000.0), VP), 0..0);
        assert_eq!(l.visible_pages(view(-300.0), VP), 0..1);
    }

    #[test]
    fn location_and_content_pos_roundtrip_inside_pages() {
        let sizes = [
            Size::new(612.0, 792.0),
            Size::new(300.0, 400.0),
            Size::new(842.0, 595.0),
        ];
        for zoom in [
            ZoomMode::Custom(1.0),
            ZoomMode::Custom(2.5),
            ZoomMode::FitWidth,
        ] {
            let l = PageLayout::new(&sizes, params(zoom));
            for (page, s) in sizes.iter().enumerate() {
                for (fx, fy) in [(0.0, 0.0), (0.5, 0.5), (1.0, 1.0), (0.1, 0.9), (1.0, 0.0)] {
                    let loc = Location::new(page as u32, s.w * fx, s.h * fy);
                    let back = l.location_at(l.content_pos(loc));
                    near_loc(back, loc);
                }
            }
        }
    }

    #[test]
    fn content_pos_and_location_at_agree_with_page_rect() {
        let (l, _, _) = ten();
        let r = l.page_rect(4);
        let p = l.content_pos(Location::new(4, 0.0, 0.0));
        near(p.x, r.x0);
        near(p.y, r.y0);
        let loc = l.location_at(Point::new(r.x0 + K * 72.0, r.y0 + K * 144.0));
        near_loc(loc, Location::new(4, 72.0, 144.0));
    }

    #[test]
    fn points_between_pages_go_to_the_nearest_page() {
        let (l, h, _) = ten();
        let r0 = l.page_rect(0);
        let mid_x = r0.x0 + 100.0;
        let bottom0 = r0.y0 + h;
        // Gap is 10 px: 2 px below page 0 is nearer to page 0, 8 px below is nearer to page 1.
        let a = l.location_at(Point::new(mid_x, bottom0 + 2.0));
        assert_eq!(a.page, 0);
        near(a.y, 792.0); // clamped to the bottom edge
        let b = l.location_at(Point::new(mid_x, bottom0 + 8.0));
        assert_eq!(b.page, 1);
        near(b.y, 0.0); // clamped to the top edge
        // Exactly in the middle of the gap: the earlier page.
        assert_eq!(l.location_at(Point::new(mid_x, bottom0 + 5.0)).page, 0);
    }

    #[test]
    fn points_outside_the_document_are_clamped_into_the_nearest_page() {
        let (l, h, _) = ten();
        let r0 = l.page_rect(0);
        // Left margin of page 0 (the page is narrower than the content here).
        let left = l.location_at(Point::new(-50.0, r0.y0 + 100.0));
        assert_eq!(left.page, 0);
        near(left.x, 0.0);
        near(left.y, 100.0 / K);
        // Right of the page.
        let right = l.location_at(Point::new(1.0e6, r0.y0 + 100.0));
        near(right.x, 612.0);
        // Above the document (top margin).
        let above = l.location_at(Point::new(r0.x0 + 50.0, -500.0));
        assert_eq!(above.page, 0);
        near(above.y, 0.0);
        near(above.x, 50.0 / K);
        // Below the document.
        let r9 = l.page_rect(9);
        let below = l.location_at(Point::new(r9.x0 + 50.0, r9.y0 + h + 5000.0));
        assert_eq!(below.page, 9);
        near(below.y, 792.0);
        near(below.x, 50.0 / K);
    }

    #[test]
    fn anchor_keeps_pointing_at_the_same_place_after_a_zoom_change() {
        let sizes = [
            Size::new(612.0, 792.0),
            Size::new(400.0, 500.0),
            Size::new(612.0, 792.0),
        ];
        let anchor = Location::new(1, 123.0, 321.0);
        for z in [0.5, 1.0, 1.7, 4.0] {
            let l = PageLayout::new(&sizes, params(ZoomMode::Custom(z)));
            let scroll = l.content_pos(anchor);
            near_loc(l.location_at(scroll), anchor);
        }
        let fit = PageLayout::new(&sizes, params(ZoomMode::FitWidth));
        near_loc(fit.location_at(fit.content_pos(anchor)), anchor);
    }

    #[test]
    fn scroll_to_keep_holds_the_spot_under_the_cursor_across_zoom() {
        let sizes = letter(30);
        let before = PageLayout::new(&sizes, params(ZoomMode::Custom(1.0)));
        let cursor = Point::new(300.0, 250.0);
        let scroll = Point::new(0.0, 5000.0);
        let loc = before.location_at(Point::new(scroll.x + cursor.x, scroll.y + cursor.y));
        for z in [1.5, 3.0] {
            let after = PageLayout::new(&sizes, params(ZoomMode::Custom(z)));
            let s2 = after.scroll_to_keep(loc, cursor, VP);
            assert_eq!(s2, after.clamp_scroll(s2, VP), "within the scroll range");
            let under = after.location_at(Point::new(s2.x + cursor.x, s2.y + cursor.y));
            near_loc(under, loc);
        }
        // Zoomed out so far that the content is narrower than the viewport: the vertical
        // position still holds; horizontally the scroll range is empty.
        let after = PageLayout::new(&sizes, params(ZoomMode::Custom(0.5)));
        let s2 = after.scroll_to_keep(loc, cursor, VP);
        assert_eq!(s2.x, 0.0);
        let under = after.location_at(Point::new(s2.x + cursor.x, s2.y + cursor.y));
        assert_eq!(under.page, loc.page);
        near(under.y, loc.y);
    }

    #[test]
    fn scroll_to_keep_clamps_when_the_spot_cannot_stay() {
        let l = PageLayout::new(&letter(2), params(ZoomMode::Custom(1.0)));
        // The top of the document cannot be moved down into the viewport.
        let s = l.scroll_to_keep(Location::new(0, 0.0, 0.0), Point::new(100.0, 200.0), VP);
        assert_eq!(s.y, 0.0);
        // The very end cannot be moved up above the bottom of the scroll range.
        let s = l.scroll_to_keep(Location::new(1, 612.0, 792.0), Point::new(0.0, 0.0), VP);
        assert_eq!(s, l.max_scroll(VP));
    }

    #[test]
    fn clamp_scroll_limits_both_axes() {
        let l = PageLayout::new(&letter(5), params(ZoomMode::Custom(2.0)));
        let max = l.max_scroll(VP);
        assert!(max.x > 0.0 && max.y > 0.0);
        assert_eq!(
            l.clamp_scroll(Point::new(-5.0, -5.0), VP),
            Point::new(0.0, 0.0)
        );
        assert_eq!(l.clamp_scroll(Point::new(1e9, 1e9), VP), max);
        let mid = Point::new(max.x / 2.0, max.y / 2.0);
        assert_eq!(l.clamp_scroll(mid, VP), mid);
        assert_eq!(
            l.clamp_scroll(Point::new(f32::NAN, f32::NAN), VP),
            Point::new(0.0, 0.0)
        );
        near(max.y, l.content_size().h - 600.0);
    }

    #[test]
    fn no_pages() {
        let l = PageLayout::new(&[], params(ZoomMode::FitWidth));
        assert_eq!(l.content_size(), Size::new(800.0, 600.0));
        assert_eq!(l.page_count(), 0);
        assert_eq!(l.visible_pages(Point::default(), VP), 0..0);
        assert_eq!(
            l.location_at(Point::new(10.0, 10.0)),
            Location::new(0, 0.0, 0.0)
        );
        assert_eq!(l.content_pos(Location::new(3, 5.0, 5.0)), Point::default());
        assert_eq!(l.page_rect(0), Rect::default());
        assert_eq!(l.max_scroll(VP), Point::default());
        assert_eq!(
            l.scroll_to_keep(Location::default(), Point::new(5.0, 5.0), VP),
            Point::default()
        );
        for zoom in [ZoomMode::FitPage, ZoomMode::Custom(1.0)] {
            let l = PageLayout::new(&[], params(zoom));
            assert!(l.scale() >= MIN_SCALE);
        }
    }

    #[test]
    fn one_page() {
        let l = PageLayout::new(&letter(1), params(ZoomMode::Custom(1.0)));
        assert_eq!(l.page_count(), 1);
        let r = l.page_rect(0);
        near(r.y0, 20.0);
        near(l.content_size().h, 20.0 + 792.0 * K + 20.0);
        assert_eq!(l.visible_pages(Point::default(), VP), 0..1);
        assert_eq!(l.visible_pages(l.max_scroll(VP), VP), 0..1);
        near_loc(
            l.location_at(Point::new(r.x0 + 10.0, r.y0 + 10.0)),
            Location::new(0, 10.0 / K, 10.0 / K),
        );
    }

    #[test]
    fn zero_or_negative_viewport_does_not_panic() {
        for vp in [
            Size::new(0.0, 0.0),
            Size::new(-10.0, -10.0),
            Size::new(f32::NAN, f32::INFINITY),
            Size::new(30.0, 30.0), // smaller than twice the margin
        ] {
            for zoom in [ZoomMode::FitWidth, ZoomMode::FitPage, ZoomMode::Custom(1.0)] {
                let mut p = params(zoom);
                p.viewport = vp;
                let l = PageLayout::new(&letter(3), p);
                assert!((MIN_SCALE..=MAX_SCALE).contains(&l.scale()));
                if !matches!(zoom, ZoomMode::Custom(_)) {
                    near(l.scale(), MIN_SCALE);
                }
                let _ = l.visible_pages(Point::default(), vp);
                let _ = l.location_at(Point::new(5.0, 5.0));
                let _ = l.scroll_to_keep(Location::new(1, 10.0, 10.0), Point::default(), vp);
                let c = l.content_size();
                assert!(c.w.is_finite() && c.h.is_finite() && c.w >= 0.0);
            }
        }
        // Empty document and zero viewport.
        let mut p = params(ZoomMode::FitWidth);
        p.viewport = Size::new(0.0, 0.0);
        assert_eq!(PageLayout::new(&[], p).content_size(), Size::new(0.0, 0.0));
    }

    #[test]
    fn degenerate_page_sizes_do_not_panic() {
        let sizes = [
            Size::new(0.0, 0.0),
            Size::new(-5.0, 10.0),
            Size::new(f32::NAN, 100.0),
        ];
        for zoom in [ZoomMode::FitWidth, ZoomMode::FitPage, ZoomMode::Custom(1.0)] {
            let l = PageLayout::new(&sizes, params(zoom));
            assert!((MIN_SCALE..=MAX_SCALE).contains(&l.scale()));
            let _ = l.location_at(Point::new(1.0, 1.0));
            let _ = l.visible_pages(Point::default(), VP);
        }
    }

    #[test]
    fn five_thousand_pages_match_linear_search() {
        // Mixed sizes so that positions are not a regular grid.
        let sizes: Vec<Size> = (0..5000)
            .map(|i| {
                Size::new(
                    400.0 + (i % 7) as f32 * 50.0,
                    500.0 + (i % 13) as f32 * 40.0,
                )
            })
            .collect();
        let l = PageLayout::new(&sizes, params(ZoomMode::Custom(1.0)));
        let n = l.page_count();
        assert_eq!(n, 5000);
        let rects: Vec<Rect> = (0..n).map(|i| l.page_rect(i)).collect();

        let linear_visible = |scroll: Point, vp: Size| -> Range<u32> {
            let (top, bottom) = (scroll.y, scroll.y + vp.h);
            let hit: Vec<u32> = (0..n)
                .filter(|&i| rects[i as usize].y1 > top && rects[i as usize].y0 < bottom)
                .collect();
            match (hit.first(), hit.last()) {
                (Some(&a), Some(&b)) => a..b + 1,
                _ => {
                    let next = (0..n).find(|&i| rects[i as usize].y1 > top).unwrap_or(n);
                    next..next
                }
            }
        };
        let linear_page_at = |y: f32| -> u32 {
            let mut best = (f32::MAX, 0);
            for i in 0..n {
                let r = &rects[i as usize];
                let d = if y < r.y0 {
                    r.y0 - y
                } else if y > r.y1 {
                    y - r.y1
                } else {
                    0.0
                };
                if d < best.0 {
                    best = (d, i); // strict: the earlier page wins a tie
                }
            }
            best.1
        };

        let total = l.content_size().h;
        let mut checked = 0;
        let mut y = -50.0;
        while y < total + 50.0 {
            let scroll = view(y);
            assert_eq!(
                l.visible_pages(scroll, VP),
                linear_visible(scroll, VP),
                "visible at {y}"
            );
            let loc = l.location_at(Point::new(300.0, y));
            assert_eq!(loc.page, linear_page_at(y), "page at {y}");
            checked += 1;
            y += 313.7;
        }
        assert!(checked > 4000);
        // Page boundaries and gaps exactly.
        for i in (0..n).step_by(97) {
            let r = &rects[i as usize];
            for y in [r.y0 - 5.0, r.y0, r.y0 + 0.5, r.y1 - 0.5, r.y1, r.y1 + 5.0] {
                assert_eq!(
                    l.location_at(Point::new(300.0, y)).page,
                    linear_page_at(y),
                    "edge {y}"
                );
                let s = view(y);
                assert_eq!(
                    l.visible_pages(s, VP),
                    linear_visible(s, VP),
                    "edge visible {y}"
                );
            }
        }
        // No overlap anywhere in the stack.
        for w in rects.windows(2) {
            assert!(w[1].y0 >= w[0].y1 + 10.0 - 0.5);
        }
    }
}
