//! Page colours (VIEW-7): the "dark" rendering of a page and the choice between the colours.
//!
//! The dark rendering is not a plain inversion. Per pixel only the lightness is turned around and
//! hue and saturation are kept (the HSL lightness `L` becomes `1 - L`): a white background turns
//! black, black text turns white, and a red warning stays red. Then the range is pressed a little
//! so the background is `#1e1e1e` rather than pure black and the text `#e6e6e6` rather than pure
//! white. Pictures are left in their own colours: the caller passes the pixel rectangles of the
//! page's images ([`image_keep_rects`]) and [`recolor_dark`] skips them.
//!
//! Everything here is pure and works on plain pixel buffers, so it is tested without MuPDF.

use serde::{Deserialize, Serialize};

use crate::geom::{Rect, Size};

/// The colours a page is rendered in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PageColors {
    /// As the document is.
    #[default]
    Original,
    /// Lightness turned around, hues kept, pictures as they are.
    Dark,
}

/// The page colours setting (`prefs.json` item `page_colors`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageColorsPref {
    /// Always [`PageColors::Original`].
    #[default]
    Original,
    /// Always [`PageColors::Dark`].
    Dark,
    /// Dark pages while the interface is dark.
    FollowTheme,
}

impl PageColorsPref {
    /// Every value, in menu order.
    pub const ALL: [PageColorsPref; 3] = [
        PageColorsPref::Original,
        PageColorsPref::Dark,
        PageColorsPref::FollowTheme,
    ];
}

/// The page colours to render with for a setting, given whether the interface is dark now.
pub fn effective_page_colors(pref: PageColorsPref, ui_dark: bool) -> PageColors {
    match pref {
        PageColorsPref::Original => PageColors::Original,
        PageColorsPref::Dark => PageColors::Dark,
        PageColorsPref::FollowTheme if ui_dark => PageColors::Dark,
        PageColorsPref::FollowTheme => PageColors::Original,
    }
}

/// The setting after "toggle page colours": original and dark swap; "follow theme" is fixed to
/// the opposite of what is `shown` now.
pub fn toggled_page_colors(pref: PageColorsPref, shown: PageColors) -> PageColorsPref {
    match pref {
        PageColorsPref::Original => PageColorsPref::Dark,
        PageColorsPref::Dark => PageColorsPref::Original,
        PageColorsPref::FollowTheme => match shown {
            PageColors::Original => PageColorsPref::Dark,
            PageColors::Dark => PageColorsPref::Original,
        },
    }
}

/// The darkest value a dark page has: what a white background becomes.
pub const DARK_BACKGROUND: u8 = 0x1e;
/// The lightest value a dark page has: what black text becomes.
pub const DARK_FOREGROUND: u8 = 0xe6;

/// An image covering at least this share of the page's area is recoloured like the rest: it is a
/// scanned page, and keeping it would leave the page white.
pub const FULL_PAGE_IMAGE: f32 = 0.85;

/// Maps a turned-around channel value `0..=255` linearly onto
/// [`DARK_BACKGROUND`]..=[`DARK_FOREGROUND`], rounded.
const LEVELS: [u8; 256] = levels();

const fn levels() -> [u8; 256] {
    let mut out = [0u8; 256];
    let span = (DARK_FOREGROUND - DARK_BACKGROUND) as u32;
    let mut v = 0u32;
    while v < 256 {
        out[v as usize] = (DARK_BACKGROUND as u32 + (v * span + 127) / 255) as u8;
        v += 1;
    }
    out
}

/// One pixel in dark colours.
#[inline]
pub fn dark_pixel(rgb: [u8; 3]) -> [u8; 3] {
    let [r, g, b] = rgb.map(u16::from);
    let m = r.max(g).max(b) + r.min(g).min(b);
    // c + 255 - m stays in 0..=255: min <= c <= max and max <= 255.
    let f = |c: u16| LEVELS[usize::from(c + 255 - m)];
    [f(r), f(g), f(b)]
}

/// Recolours `pixels` (each `n` bytes; the first three are R, G, B, a fourth is alpha and kept).
/// Written as a plain index loop: it is the hot loop of a dark page, and this form is also
/// reasonably fast in unoptimized builds.
#[inline]
fn recolor_span(pixels: &mut [u8], n: usize) {
    let end = pixels.len() - pixels.len() % n;
    let mut i = 0;
    while i < end {
        let (r, g, b) = (pixels[i], pixels[i + 1], pixels[i + 2]);
        let (hi, lo) = if r > g { (r, g) } else { (g, r) };
        let (hi, lo) = (hi.max(b), lo.min(b));
        // (c + 255) - (hi + lo) stays in 0..=255: lo <= c <= hi <= 255.
        let m = u16::from(hi) + u16::from(lo);
        pixels[i] = LEVELS[usize::from(u16::from(r) + 255 - m)];
        pixels[i + 1] = LEVELS[usize::from(u16::from(g) + 255 - m)];
        pixels[i + 2] = LEVELS[usize::from(u16::from(b) + 255 - m)];
        i += n;
    }
}

/// A rectangle of pixels: columns `x0..x1`, rows `y0..y1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    /// First column.
    pub x0: u32,
    /// First row.
    pub y0: u32,
    /// One past the last column.
    pub x1: u32,
    /// One past the last row.
    pub y1: u32,
}

/// Turns `samples` into dark colours in place, except the pixels inside `keep`.
///
/// `samples` holds `height` rows of `width` pixels of `n` bytes each (`n >= 3`; R, G, B, and an
/// alpha byte that is left alone when `n == 4`), without padding. Rows past the end of the buffer
/// and parts of `keep` outside the image are ignored.
pub fn recolor_dark(samples: &mut [u8], width: u32, height: u32, n: usize, keep: &[PixelRect]) {
    if n < 3 || width == 0 {
        return;
    }
    let row_len = width as usize * n;
    let rows = (height as usize).min(samples.len() / row_len);
    let mut keep: Vec<PixelRect> = keep
        .iter()
        .map(|r| PixelRect {
            x0: r.x0.min(width),
            y0: r.y0,
            x1: r.x1.min(width),
            y1: r.y1,
        })
        .filter(|r| r.x0 < r.x1 && r.y0 < r.y1)
        .collect();
    keep.sort_by_key(|r| r.x0);
    for (y, row) in samples.chunks_exact_mut(row_len).take(rows).enumerate() {
        let y = y as u32;
        // The rectangles are sorted by their left edge: walk them once, recolouring the gaps.
        let mut x = 0u32;
        for r in keep.iter().filter(|r| r.y0 <= y && y < r.y1) {
            if r.x0 > x {
                recolor_span(&mut row[x as usize * n..r.x0 as usize * n], n);
            }
            x = x.max(r.x1);
        }
        if x < width {
            recolor_span(&mut row[x as usize * n..], n);
        }
    }
}

/// The pixel rectangles of a page's images that keep their colours, in a buffer of
/// `width` × `height` pixels whose top-left pixel is pixel `origin` of the whole page rendered at
/// `scale` pixels per point (`(0, 0)` for a whole page; the region's corner for a region).
///
/// `images` are the image rectangles in page space. An image that covers at least
/// [`FULL_PAGE_IMAGE`] of the page is left out: it is recoloured. Edges are rounded to the
/// nearest pixel and the rectangles are clipped to the buffer; empty ones are dropped.
pub fn image_keep_rects(
    images: &[Rect],
    page: Size,
    scale: f32,
    origin: (u32, u32),
    width: u32,
    height: u32,
) -> Vec<PixelRect> {
    let page_area = page.w * page.h;
    let to_px = |v: f32, offset: u32, max: u32| -> u32 {
        let p = (v * scale).round() - offset as f32;
        if p.is_nan() || p <= 0.0 {
            0
        } else {
            (p as u32).min(max)
        }
    };
    images
        .iter()
        .filter(|r| {
            let w = (r.x1.min(page.w) - r.x0.max(0.0)).max(0.0);
            let h = (r.y1.min(page.h) - r.y0.max(0.0)).max(0.0);
            !(page_area > 0.0 && w * h >= FULL_PAGE_IMAGE * page_area)
        })
        .map(|r| PixelRect {
            x0: to_px(r.x0, origin.0, width),
            y0: to_px(r.y0, origin.1, height),
            x1: to_px(r.x1, origin.0, width),
            y1: to_px(r.y1, origin.1, height),
        })
        .filter(|r| r.x0 < r.x1 && r.y0 < r.y1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BG: u8 = DARK_BACKGROUND;
    const FG: u8 = DARK_FOREGROUND;

    fn argmax(p: [u8; 3]) -> usize {
        (0..3).max_by_key(|i| p[*i]).unwrap_or(0)
    }

    #[test]
    fn white_becomes_the_dark_background_and_black_the_light_text() {
        assert_eq!(dark_pixel([255, 255, 255]), [BG, BG, BG]);
        assert_eq!(dark_pixel([0, 0, 0]), [FG, FG, FG]);
        assert_eq!(BG, 0x1e);
        assert_eq!(FG, 0xe6);
    }

    #[test]
    fn pure_colours_keep_their_hue() {
        for (i, p) in [
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [200, 30, 30],
            [20, 90, 200],
        ]
        .into_iter()
        .enumerate()
        {
            let d = dark_pixel(p);
            assert_eq!(argmax(d), argmax(p), "case {i}: {p:?} -> {d:?}");
        }
        // A saturated red stays as saturated as the pressed range allows.
        assert_eq!(dark_pixel([255, 0, 0]), [FG, BG, BG]);
        // A light pastel becomes a dark shade of the same hue.
        let pastel = dark_pixel([255, 200, 200]);
        assert!(
            pastel[0] > pastel[1] && pastel[1] == pastel[2],
            "{pastel:?}"
        );
        assert!(pastel[0] < 128, "{pastel:?}");
    }

    #[test]
    fn the_order_of_greys_is_reversed() {
        let mut last = 0u8;
        for g in (0..=255u8).rev() {
            let d = dark_pixel([g, g, g]);
            assert!(d[0] == d[1] && d[1] == d[2]);
            assert!(d[0] >= last, "grey {g}");
            last = d[0];
        }
        assert!(dark_pixel([200, 200, 200])[0] < dark_pixel([50, 50, 50])[0]);
    }

    #[test]
    fn applying_twice_stays_in_range() {
        for r in (0..=255u8).step_by(15) {
            for g in (0..=255u8).step_by(17) {
                for b in (0..=255u8).step_by(51) {
                    let once = dark_pixel([r, g, b]);
                    let twice = dark_pixel(once);
                    for v in once.into_iter().chain(twice) {
                        assert!(
                            (BG..=FG).contains(&v),
                            "{r},{g},{b} -> {once:?} -> {twice:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_buffer_loop_matches_the_pixel_function() {
        let mut buf = Vec::new();
        for r in (0..=255u8).step_by(5) {
            for g in (0..=255u8).step_by(15) {
                for b in (0..=255u8).step_by(51) {
                    buf.extend_from_slice(&[r, g, b]);
                }
            }
        }
        let original = buf.clone();
        let count = (buf.len() / 3) as u32;
        recolor_dark(&mut buf, count, 1, 3, &[]);
        for (o, d) in original.chunks_exact(3).zip(buf.chunks_exact(3)) {
            assert_eq!(dark_pixel([o[0], o[1], o[2]]), [d[0], d[1], d[2]], "{o:?}");
        }
    }

    #[test]
    fn alpha_is_kept_and_rgb_buffers_work() {
        let mut rgba = vec![255, 255, 255, 7, 0, 0, 0, 200];
        recolor_dark(&mut rgba, 2, 1, 4, &[]);
        assert_eq!(rgba, vec![BG, BG, BG, 7, FG, FG, FG, 200]);
        let mut rgb = vec![255, 255, 255, 0, 0, 0];
        recolor_dark(&mut rgb, 2, 1, 3, &[]);
        assert_eq!(rgb, vec![BG, BG, BG, FG, FG, FG]);
    }

    #[test]
    fn empty_and_short_buffers_are_harmless() {
        let mut empty: Vec<u8> = Vec::new();
        recolor_dark(&mut empty, 0, 0, 3, &[]);
        recolor_dark(&mut empty, 10, 10, 3, &[]);
        assert!(empty.is_empty());
        // One full row and a partial second row: only the full row changes.
        let mut short = vec![255u8; 2 * 3 + 2];
        recolor_dark(&mut short, 2, 2, 3, &[]);
        assert_eq!(&short[..6], &[BG; 6]);
        assert_eq!(&short[6..], &[255, 255]);
        // Fewer than three channels: nothing to do.
        let mut grey = vec![255u8; 4];
        recolor_dark(&mut grey, 2, 2, 1, &[]);
        assert_eq!(grey, vec![255; 4]);
    }

    /// A `w`×`h` white RGB image.
    fn white(w: u32, h: u32) -> Vec<u8> {
        vec![255; (w * h * 3) as usize]
    }

    fn px(buf: &[u8], w: u32, x: u32, y: u32) -> u8 {
        buf[((y * w + x) * 3) as usize]
    }

    #[test]
    fn pixels_inside_kept_rectangles_stay() {
        let (w, h) = (6, 4);
        let mut buf = white(w, h);
        let keep = [
            PixelRect {
                x0: 1,
                y0: 1,
                x1: 3,
                y1: 3,
            },
            // Overlapping the first one.
            PixelRect {
                x0: 2,
                y0: 2,
                x1: 4,
                y1: 4,
            },
        ];
        recolor_dark(&mut buf, w, h, 3, &keep);
        let kept = |x: u32, y: u32| {
            keep.iter()
                .any(|r| r.x0 <= x && x < r.x1 && r.y0 <= y && y < r.y1)
        };
        for y in 0..h {
            for x in 0..w {
                let want = if kept(x, y) { 255 } else { BG };
                assert_eq!(px(&buf, w, x, y), want, "({x}, {y})");
            }
        }
    }

    #[test]
    fn rectangles_over_the_edge_are_clipped() {
        let (w, h) = (4, 3);
        let mut buf = white(w, h);
        let keep = [PixelRect {
            x0: 2,
            y0: 1,
            x1: 99,
            y1: 99,
        }];
        recolor_dark(&mut buf, w, h, 3, &keep);
        assert_eq!(px(&buf, w, 1, 1), BG);
        assert_eq!(px(&buf, w, 3, 2), 255);
        assert_eq!(px(&buf, w, 3, 0), BG);
        // Entirely outside: everything is recoloured.
        let mut buf = white(w, h);
        recolor_dark(
            &mut buf,
            w,
            h,
            3,
            &[PixelRect {
                x0: 10,
                y0: 0,
                x1: 20,
                y1: 3,
            }],
        );
        assert!(buf.iter().all(|v| *v == BG));
    }

    const PAGE: Size = Size { w: 100.0, h: 200.0 };

    #[test]
    fn image_rectangles_become_pixels_at_the_render_scale() {
        let images = [Rect::new(10.0, 20.0, 30.0, 50.0)];
        let r = image_keep_rects(&images, PAGE, 2.0, (0, 0), 200, 400);
        assert_eq!(
            r,
            vec![PixelRect {
                x0: 20,
                y0: 40,
                x1: 60,
                y1: 100
            }]
        );
        // Over the page edge: clipped to the buffer.
        let images = [Rect::new(-5.0, 190.0, 120.0, 230.0)];
        let r = image_keep_rects(&images, PAGE, 1.0, (0, 0), 100, 200);
        assert_eq!(
            r,
            vec![PixelRect {
                x0: 0,
                y0: 190,
                x1: 100,
                y1: 200
            }]
        );
    }

    #[test]
    fn a_region_render_shifts_the_rectangles_by_its_corner() {
        let images = [Rect::new(10.0, 20.0, 30.0, 50.0)];
        // The region starts at page pixel (30, 50) and is 40 x 40 pixels (scale 2).
        let r = image_keep_rects(&images, PAGE, 2.0, (30, 50), 40, 40);
        assert_eq!(
            r,
            vec![PixelRect {
                x0: 0,
                y0: 0,
                x1: 30,
                y1: 40
            }]
        );
        // A region beside the image keeps nothing.
        assert!(image_keep_rects(&images, PAGE, 2.0, (100, 0), 40, 40).is_empty());
        // And the recolouring of the region follows.
        let mut buf = white(40, 40);
        recolor_dark(&mut buf, 40, 40, 3, &r);
        assert_eq!(px(&buf, 40, 29, 39), 255);
        assert_eq!(px(&buf, 40, 30, 0), BG);
    }

    #[test]
    fn an_image_covering_most_of_the_page_is_recoloured() {
        let scan = [Rect::new(0.0, 0.0, 100.0, 171.0)]; // 85.5 %
        assert!(image_keep_rects(&scan, PAGE, 1.0, (0, 0), 100, 200).is_empty());
        let big = [Rect::new(0.0, 0.0, 100.0, 160.0)]; // 80 %
        assert_eq!(image_keep_rects(&big, PAGE, 1.0, (0, 0), 100, 200).len(), 1);
        // Measured on the page: an image larger than the page counts as the whole page.
        let bleed = [Rect::new(-50.0, -50.0, 150.0, 250.0)];
        assert!(image_keep_rects(&bleed, PAGE, 1.0, (0, 0), 100, 200).is_empty());
    }

    #[test]
    fn the_effective_colours_follow_the_setting_and_the_interface() {
        use PageColorsPref as P;
        for (pref, ui_dark, want) in [
            (P::Original, false, PageColors::Original),
            (P::Original, true, PageColors::Original),
            (P::Dark, false, PageColors::Dark),
            (P::Dark, true, PageColors::Dark),
            (P::FollowTheme, false, PageColors::Original),
            (P::FollowTheme, true, PageColors::Dark),
        ] {
            assert_eq!(
                effective_page_colors(pref, ui_dark),
                want,
                "{pref:?} {ui_dark}"
            );
        }
    }

    #[test]
    fn toggling_swaps_and_fixes_follow_theme_to_the_opposite_of_what_is_shown() {
        use PageColorsPref as P;
        assert_eq!(
            toggled_page_colors(P::Original, PageColors::Original),
            P::Dark
        );
        assert_eq!(toggled_page_colors(P::Dark, PageColors::Dark), P::Original);
        assert_eq!(
            toggled_page_colors(P::FollowTheme, PageColors::Dark),
            P::Original
        );
        assert_eq!(
            toggled_page_colors(P::FollowTheme, PageColors::Original),
            P::Dark
        );
    }

    #[test]
    fn the_setting_is_written_in_snake_case() {
        let text = serde_json::to_string(&PageColorsPref::FollowTheme).unwrap_or_default();
        assert_eq!(text, "\"follow_theme\"");
        let back: PageColorsPref = serde_json::from_str("\"dark\"").unwrap_or_default();
        assert_eq!(back, PageColorsPref::Dark);
    }
}
