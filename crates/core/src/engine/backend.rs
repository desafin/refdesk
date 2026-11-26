//! Every call into MuPDF (rule R-2: the only file under `crates/*/src` that imports `mupdf`).
//!
//! A [`Backend`] wraps one open MuPDF document. MuPDF objects are bound to the thread that made
//! them, so a backend is created and used on its document's worker thread only, and everything
//! it returns is plain data in page space (§3): MuPDF coordinates minus the page's bounds origin.

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mupdf::pdf::PdfDocument;
use mupdf::{
    Colorspace, DestinationKind, Document, Matrix, MetadataName, Page, TextPageFlags,
    text_page::TextBlockType,
};

use super::uri;
use super::{DocumentInfo, EngineError, RenderedImage};
use crate::geom::{Point, Quad, Rect, Size};
use crate::link::{self, Link, LinkTarget, RawLink};
use crate::nav::dest::{self, DestKind, Destination};
use crate::outline::{Outline, OutlineNode};
use crate::recolor::{self, PageColors};
use crate::text::{PageText, TextBlock, TextChar, TextLine};

/// Upper bound on the pixels of one rendering, so a wild scale fails instead of exhausting memory.
const MAX_PIXELS: u64 = 120_000_000;

/// Used for a page whose bounds cannot be read (US Letter).
const FALLBACK_SIZE: Size = Size { w: 612.0, h: 792.0 };

/// Runs `f`, turning a panic inside the `mupdf` crate into an error (rule R-9). The crate has a
/// few `unwrap`s on paths a damaged document can reach (for example while iterating links).
pub(super) fn guarded<T>(f: impl FnOnce() -> Result<T, EngineError>) -> Result<T, EngineError> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        Err(EngineError::Failed(
            "internal error in the PDF library".to_string(),
        ))
    })
}

fn failed(context: &str, e: impl std::fmt::Display) -> EngineError {
    EngineError::Failed(format!("{context}: {e}"))
}

/// Geometry of one page in MuPDF's coordinates.
#[derive(Clone, Copy, Debug)]
struct PageGeom {
    /// Top-left corner of the page bounds. Subtracted from everything MuPDF returns.
    origin: (f32, f32),
    size: Size,
}

/// One open document.
pub(super) struct Backend {
    doc: Document,
    path: PathBuf,
    /// Filled by [`Backend::load_info`].
    pages: Vec<PageGeom>,
    /// The image rectangles (page space) of each page asked for so far, for dark page colours.
    /// A reload makes a new backend, so they never outlive the document they came from.
    image_rects: RefCell<HashMap<u32, Rc<[Rect]>>>,
}

impl Backend {
    /// Opens a file. Fails with a message for the user.
    pub(super) fn open(path: &Path) -> Result<Backend, String> {
        if !path.is_file() {
            return Err(format!("file not found: {}", path.display()));
        }
        let doc = guarded(|| Document::open(path).map_err(|e| failed("cannot open", e)))
            .map_err(|e| e.to_string())?;
        Ok(Backend {
            doc,
            path: path.to_path_buf(),
            pages: Vec::new(),
            image_rects: RefCell::new(HashMap::new()),
        })
    }

    /// Whether the document is encrypted and no password has been accepted yet.
    pub(super) fn needs_password(&self) -> bool {
        guarded(|| {
            self.doc
                .needs_password()
                .map_err(|e| failed("password check", e))
        })
        .unwrap_or(false)
    }

    /// Tries a password. `true` when it was accepted.
    pub(super) fn authenticate(&mut self, password: &str) -> bool {
        let doc = &mut self.doc;
        guarded(|| {
            doc.authenticate(password)
                .map_err(|e| failed("authenticate", e))
        })
        .unwrap_or(false)
    }

    /// Reads page sizes, labels, title and outline. Fails when the document has no usable pages.
    pub(super) fn load_info(&mut self, generation: u32) -> Result<DocumentInfo, String> {
        let count = guarded(|| self.doc.page_count().map_err(|e| failed("page count", e)))
            .map_err(|e| e.to_string())?;
        if count <= 0 {
            return Err("the document has no pages".to_string());
        }
        let mut pages = Vec::with_capacity(count as usize);
        let mut broken = 0usize;
        for i in 0..count {
            match guarded(|| self.page_geom(i)) {
                Ok(g) => pages.push(g),
                Err(e) => {
                    // A damaged page must not make the whole document unreadable: give it the
                    // size of its predecessor and let rendering it fail later.
                    log::warn!("{}: page {}: {e}", self.path.display(), i + 1);
                    broken += 1;
                    let size = pages.last().map_or(FALLBACK_SIZE, |g: &PageGeom| g.size);
                    pages.push(PageGeom {
                        origin: (0.0, 0.0),
                        size,
                    });
                }
            }
        }
        if broken == pages.len() {
            return Err("none of the pages can be read".to_string());
        }
        self.pages = pages;

        let title = guarded(|| {
            self.doc
                .metadata(MetadataName::Title)
                .map_err(|e| failed("title", e))
        })
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

        let outline = match guarded(|| self.outline()) {
            Ok(o) => o,
            Err(e) => {
                log::warn!("{}: outline: {e}", self.path.display());
                Outline::new()
            }
        };

        Ok(DocumentInfo {
            path: self.path.clone(),
            generation,
            page_sizes: self.pages.iter().map(|g| g.size).collect(),
            page_labels: self.page_labels(),
            title,
            outline,
        })
    }

    fn page_geom(&self, index: i32) -> Result<PageGeom, EngineError> {
        let page = self
            .doc
            .load_page(index)
            .map_err(|e| failed("cannot load page", e))?;
        let b = page.bounds().map_err(|e| failed("page bounds", e))?;
        let (w, h) = (b.x1 - b.x0, b.y1 - b.y0);
        if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
            return Err(EngineError::Failed(format!(
                "page has an empty size {w}x{h}"
            )));
        }
        Ok(PageGeom {
            origin: (b.x0, b.y0),
            size: Size::new(w, h),
        })
    }

    /// Labels of all pages when the PDF defines `/PageLabels`; otherwise empty.
    fn page_labels(&self) -> Vec<String> {
        let labels = guarded(|| {
            if !self.doc.is_pdf() {
                return Ok(Vec::new());
            }
            let pdf = PdfDocument::try_from(self.doc.clone()).map_err(|e| failed("pdf", e))?;
            let has_labels = pdf
                .catalog()
                .and_then(|c| c.get_dict("PageLabels"))
                .map_err(|e| failed("catalog", e))?
                .is_some();
            if !has_labels {
                return Ok(Vec::new());
            }
            (0..self.pages.len())
                .map(|i| pdf.page_label(i).map_err(|e| failed("page label", e)))
                .collect()
        });
        labels.unwrap_or_else(|e| {
            log::warn!("{}: page labels: {e}", self.path.display());
            Vec::new()
        })
    }

    fn outline(&self) -> Result<Outline, EngineError> {
        let roots = self.doc.outlines().map_err(|e| failed("outline", e))?;
        Ok(Outline::from_tree(self.outline_nodes(&roots)))
    }

    fn outline_nodes(&self, items: &[mupdf::Outline]) -> Vec<OutlineNode> {
        items
            .iter()
            .map(|o| {
                let target = o.dest.map(|d| {
                    let dest = self.destination(d.loc.page_number, d.kind);
                    dest::resolve(&dest, self.page_size(dest.page))
                });
                // An entry that resolved inside the document needs no URI; an unresolved
                // fragment ("#...") is not an address either.
                let uri = match (&target, &o.uri) {
                    (None, Some(u)) if !u.starts_with('#') && !u.is_empty() => Some(u.clone()),
                    _ => None,
                };
                OutlineNode {
                    title: o.title.clone(),
                    target,
                    uri,
                    children: self.outline_nodes(&o.down),
                }
            })
            .collect()
    }

    fn page_size(&self, page: u32) -> Size {
        self.pages
            .get(page as usize)
            .map_or(FALLBACK_SIZE, |g| g.size)
    }

    fn origin(&self, page: u32) -> (f32, f32) {
        self.pages
            .get(page as usize)
            .map_or((0.0, 0.0), |g| g.origin)
    }

    /// A destination on `page`, moved from MuPDF's coordinates into page space.
    ///
    /// MuPDF 0.8.0 already reports destination coordinates with a top-left origin (observed in
    /// WP-11, `docs/ARCHITECTURE.md` §3), so only the bounds origin is subtracted; nothing is
    /// flipped.
    fn destination(&self, page: u32, kind: DestinationKind) -> Destination {
        let (ox, oy) = self.origin(page);
        let x = |v: Option<f32>| v.map(|v| v - ox);
        let y = |v: Option<f32>| v.map(|v| v - oy);
        let kind = match kind {
            DestinationKind::Fit | DestinationKind::FitB => DestKind::Fit,
            DestinationKind::FitH { top } | DestinationKind::FitBH { top } => {
                DestKind::FitH { top: y(top) }
            }
            DestinationKind::FitV { left } | DestinationKind::FitBV { left } => {
                DestKind::FitV { left: x(left) }
            }
            DestinationKind::XYZ { left, top, .. } => DestKind::Xyz {
                left: x(left),
                top: y(top),
            },
            DestinationKind::FitR {
                left,
                bottom,
                right,
                top,
            } => DestKind::FitR(Rect::from_points(
                Point::new(left - ox, bottom - oy),
                Point::new(right - ox, top - oy),
            )),
        };
        Destination { page, kind }
    }

    fn load_page(&self, page: u32) -> Result<Page, EngineError> {
        let count = self.pages.len() as u32;
        if page >= count {
            return Err(EngineError::PageOutOfRange { page, count });
        }
        self.doc
            .load_page(page as i32)
            .map_err(|e| failed(&format!("cannot load page {}", page + 1), e))
    }

    /// Renders a whole page at `scale_milli / 1000` device pixels per point in `colors`: opaque
    /// RGB (on white for [`PageColors::Original`]).
    pub(super) fn render(
        &self,
        page: u32,
        scale_milli: u32,
        colors: PageColors,
    ) -> Result<(u32, u32, Vec<u8>), EngineError> {
        let (w, h, mut rgb) = self.render_original(page, scale_milli)?;
        self.recolor(page, scale_milli, colors, (0, 0), w, h, &mut rgb);
        Ok((w, h, rgb))
    }

    /// Turns a rendering of (part of) `page` into `colors`. `origin` is the buffer's top-left
    /// pixel within the whole page rendered at `scale_milli`.
    #[allow(clippy::too_many_arguments)]
    fn recolor(
        &self,
        page: u32,
        scale_milli: u32,
        colors: PageColors,
        origin: (u32, u32),
        w: u32,
        h: u32,
        rgb: &mut [u8],
    ) {
        if colors == PageColors::Original {
            return;
        }
        let images = match self.image_rects(page) {
            Ok(r) => r,
            Err(e) => {
                // Without the image positions the whole page is recoloured: still readable.
                log::warn!("{}: images of page {}: {e}", self.path.display(), page + 1);
                Rc::from(Vec::new())
            }
        };
        let scale = scale_milli as f32 / 1000.0;
        let keep = recolor::image_keep_rects(&images, self.page_size(page), scale, origin, w, h);
        recolor::recolor_dark(rgb, w, h, 3, &keep);
    }

    /// The rectangles (page space) of the image blocks of a page, from MuPDF's structured text
    /// extracted with images preserved. Remembered per page.
    fn image_rects(&self, page: u32) -> Result<Rc<[Rect]>, EngineError> {
        if let Some(r) = self.image_rects.borrow().get(&page) {
            return Ok(Rc::clone(r));
        }
        let p = self.load_page(page)?;
        let tp = p
            .to_text_page(TextPageFlags::PRESERVE_IMAGES)
            .map_err(|e| failed(&format!("cannot read the images of page {}", page + 1), e))?;
        let (ox, oy) = self.origin(page);
        let rects: Rc<[Rect]> = tp
            .blocks()
            .filter(|b| b.r#type() == TextBlockType::Image)
            .map(|b| {
                let r = b.bounds();
                Rect::new(r.x0 - ox, r.y0 - oy, r.x1 - ox, r.y1 - oy)
            })
            .filter(|r| [r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite()))
            .collect();
        self.image_rects
            .borrow_mut()
            .insert(page, Rc::clone(&rects));
        Ok(rects)
    }

    /// Renders a whole page in its own colours.
    fn render_original(
        &self,
        page: u32,
        scale_milli: u32,
    ) -> Result<(u32, u32, Vec<u8>), EngineError> {
        if scale_milli == 0 {
            return Err(EngineError::Failed("render scale is zero".to_string()));
        }
        let scale = scale_milli as f32 / 1000.0;
        let size = self.page_size(page);
        let pixels = (size.w * scale).ceil() as u64 * (size.h * scale).ceil() as u64;
        if pixels > MAX_PIXELS {
            return Err(EngineError::Failed(format!(
                "page {} at scale {scale} is too large to render",
                page + 1
            )));
        }
        let p = self.load_page(page)?;
        let pm = p
            .to_pixmap(
                &Matrix::new_scale(scale, scale),
                &Colorspace::device_rgb(),
                false,
                true,
            )
            .map_err(|e| failed(&format!("cannot render page {}", page + 1), e))?;
        let (w, h) = (pm.width(), pm.height());
        let n = pm.n() as usize;
        let stride = usize::try_from(pm.stride()).map_err(|e| failed("pixmap stride", e))?;
        let samples = pm.samples();
        if n < 3 || stride < w as usize * n || samples.len() < stride * h as usize {
            return Err(EngineError::Failed(format!(
                "unexpected pixmap layout (n={n}, stride={stride}, {w}x{h})"
            )));
        }
        let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
        for row in samples.chunks(stride).take(h as usize) {
            let row = &row[..w as usize * n];
            if n == 3 {
                rgb.extend_from_slice(row);
            } else {
                for px in row.chunks_exact(n) {
                    rgb.extend_from_slice(&px[..3]);
                }
            }
        }
        Ok((w, h, rgb))
    }

    /// Renders `region` (page space) of a page. The region is clipped to the page; an empty
    /// result is an error.
    pub(super) fn render_region(
        &self,
        page: u32,
        region: Rect,
        scale_milli: u32,
        colors: PageColors,
    ) -> Result<RenderedImage, EngineError> {
        let count = self.pages.len() as u32;
        if page >= count {
            return Err(EngineError::PageOutOfRange { page, count });
        }
        let size = self.page_size(page);
        let clip = clip_region(region, size).ok_or_else(|| {
            EngineError::Failed("the region does not overlap the page".to_string())
        })?;
        let (w, h, rgb) = self.render_original(page, scale_milli)?;
        let scale = scale_milli as f32 / 1000.0;
        let (mut image, origin) = crop(&rgb, w, h, clip, scale).ok_or_else(|| {
            EngineError::Failed("the region is smaller than one pixel".to_string())
        })?;
        drop(rgb);
        let (cw, ch) = (image.width, image.height);
        self.recolor(page, scale_milli, colors, origin, cw, ch, &mut image.rgb);
        Ok(image)
    }

    /// Extracts the text of a page in MuPDF's reading order. Image blocks are skipped.
    pub(super) fn page_text(&self, page: u32) -> Result<PageText, EngineError> {
        let p = self.load_page(page)?;
        let tp = p
            .to_text_page(TextPageFlags::empty())
            .map_err(|e| failed(&format!("cannot read the text of page {}", page + 1), e))?;
        let (ox, oy) = self.origin(page);
        let pt = |p: mupdf::Point| Point::new(p.x - ox, p.y - oy);
        let rect = |r: mupdf::Rect| Rect::new(r.x0 - ox, r.y0 - oy, r.x1 - ox, r.y1 - oy);
        let mut out = PageText::default();
        for block in tp.blocks() {
            if block.r#type() != TextBlockType::Text {
                continue;
            }
            let first_line = out.lines.len() as u32;
            for line in block.lines() {
                let first_char = out.chars.len() as u32;
                for c in line.chars() {
                    let q = c.quad();
                    out.chars.push(TextChar {
                        ch: c.char().unwrap_or(char::REPLACEMENT_CHARACTER),
                        quad: Quad {
                            ul: pt(q.ul),
                            ur: pt(q.ur),
                            ll: pt(q.ll),
                            lr: pt(q.lr),
                        },
                        size: c.size(),
                    });
                }
                out.lines.push(TextLine {
                    chars: first_char..out.chars.len() as u32,
                    bounds: rect(line.bounds()),
                });
            }
            out.blocks.push(TextBlock {
                lines: first_line..out.lines.len() as u32,
                bounds: rect(block.bounds()),
            });
        }
        Ok(out)
    }

    /// Reads the link annotations of a page and merges the ones broken over several lines.
    pub(super) fn page_links(&self, page: u32) -> Result<Vec<Link>, EngineError> {
        let p = self.load_page(page)?;
        let (ox, oy) = self.origin(page);
        let base_dir = self.path.parent();
        let mut raw = Vec::new();
        for l in p
            .links()
            .map_err(|e| failed(&format!("cannot read the links of page {}", page + 1), e))?
        {
            let b = l.bounds;
            let rect = Rect::from_points(
                Point::new(b.x0 - ox, b.y0 - oy),
                Point::new(b.x1 - ox, b.y1 - oy),
            );
            let target = match l.dest {
                Some(d) => Some(LinkTarget::Internal(
                    self.destination(d.loc.page_number, d.kind),
                )),
                None => uri::classify(&l.uri, base_dir),
            };
            if let Some(target) = target {
                raw.push(RawLink { rect, target });
            }
        }
        Ok(link::merge(raw))
    }
}

/// Clips `region` to a page of `size`. `None` when nothing is left.
fn clip_region(region: Rect, size: Size) -> Option<Rect> {
    let vals = [region.x0, region.y0, region.x1, region.y1];
    if vals.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let r = Rect::from_points(
        Point::new(region.x0, region.y0),
        Point::new(region.x1, region.y1),
    );
    let c = Rect::new(
        r.x0.max(0.0),
        r.y0.max(0.0),
        r.x1.min(size.w),
        r.y1.min(size.h),
    );
    (c.x1 > c.x0 && c.y1 > c.y0).then_some(c)
}

/// Cuts the pixels covering `clip` (page space) out of a `w`×`h` RGB image rendered at `scale`.
/// Also returns the position of the cut's top-left pixel in the image.
fn crop(rgb: &[u8], w: u32, h: u32, clip: Rect, scale: f32) -> Option<(RenderedImage, (u32, u32))> {
    // A small tolerance keeps float noise (96.00001) from adding a pixel row or column.
    let lo = |v: f32, max: u32| ((v * scale + 1e-3).floor().max(0.0) as u32).min(max);
    let hi = |v: f32, max: u32| ((v * scale - 1e-3).ceil().max(0.0) as u32).min(max);
    let (x0, y0) = (lo(clip.x0, w), lo(clip.y0, h));
    let (x1, y1) = (hi(clip.x1, w), hi(clip.y1, h));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (cw, ch) = (x1 - x0, y1 - y0);
    let mut out = Vec::with_capacity(cw as usize * ch as usize * 3);
    let row_len = w as usize * 3;
    for y in y0..y1 {
        let start = y as usize * row_len + x0 as usize * 3;
        out.extend_from_slice(rgb.get(start..start + cw as usize * 3)?);
    }
    Some((
        RenderedImage {
            width: cw,
            height: ch,
            rgb: out,
        },
        (x0, y0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: Size = Size { w: 100.0, h: 200.0 };

    #[test]
    fn region_is_clipped_to_the_page() {
        assert_eq!(
            clip_region(Rect::new(-10.0, 150.0, 50.0, 300.0), PAGE),
            Some(Rect::new(0.0, 150.0, 50.0, 200.0))
        );
        // Corners in any order.
        assert_eq!(
            clip_region(Rect::new(50.0, 60.0, 10.0, 20.0), PAGE),
            Some(Rect::new(10.0, 20.0, 50.0, 60.0))
        );
        assert_eq!(clip_region(Rect::new(120.0, 0.0, 150.0, 10.0), PAGE), None);
        assert_eq!(clip_region(Rect::new(10.0, 10.0, 10.0, 50.0), PAGE), None);
        assert_eq!(
            clip_region(Rect::new(f32::NAN, 0.0, 10.0, 10.0), PAGE),
            None
        );
    }

    #[test]
    fn crop_takes_the_covered_pixels() {
        // 4x2 image whose pixel value is its index.
        let rgb: Vec<u8> = (0..8u8).flat_map(|i| [i, i, i]).collect();
        let (img, origin) =
            crop(&rgb, 4, 2, Rect::new(1.0, 1.0, 3.0, 2.0), 1.0).expect("non-empty");
        assert_eq!(origin, (1, 1));
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.rgb, vec![5, 5, 5, 6, 6, 6]);
        // A fraction of a pixel still covers the whole pixel.
        let (img, _) = crop(&rgb, 4, 2, Rect::new(0.5, 0.0, 0.75, 0.25), 1.0).expect("non-empty");
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.rgb, vec![0, 0, 0]);
        // Scale 2: page-space (1,0)-(2,1) is pixels 2..4 x 0..2.
        assert_eq!(
            crop(&rgb, 4, 2, Rect::new(1.0, 0.0, 2.0, 1.0), 2.0).map(|(i, _)| i.width),
            Some(2)
        );
    }
}
