//! Test PDF generator (`docs/ARCHITECTURE.md` §4.12).
//!
//! Writes small PDF files byte by byte so tests never need an external PDF: pages of a given
//! size, text at known positions, internal and URI links, a nested outline, and a flow layout
//! helper that wraps paragraphs over many pages.
//!
//! All coordinates taken by this module are **page space**: points, origin at the top left of
//! the page, y growing downwards (§3). They are converted to PDF user space internally.
//! Page numbers taken by this module are **1-based** (page 2 is the second page).
//!
//! Limits, by design: Helvetica only (a base-14 font, nothing embedded), ASCII only (any other
//! character is written as `?`), no stream compression. The cross-reference table and trailer
//! are exact, so no reader needs its repair path.
//!
//! This module is self-contained: it does not use `geom` or any other module of the crate.
//!
//! ```
//! use refdesk_core::testutil::PdfBuilder;
//! let pdf = PdfBuilder::new()
//!     .page(612.0, 792.0, |p| {
//!         p.text(72.0, 100.0, 12.0, "1 Introduction");
//!         p.link_to((72.0, 90.0, 200.0, 104.0), 2, Some((72.0, 300.0)));
//!     })
//!     .page(612.0, 792.0, |p| {
//!         p.text(72.0, 300.0, 12.0, "Target");
//!     })
//!     .outline("1 Introduction", 1, 100.0, |o| {
//!         o.child("1.1 Scope", 2, 300.0);
//!     })
//!     .build();
//! assert!(pdf.starts_with(b"%PDF-1.7"));
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A rectangle in page space: `(left, top, right, bottom)`, `top <= bottom`.
pub type PageRect = (f32, f32, f32, f32);

/// Width of one glyph as a fraction of the font size, used by the flow layout.
/// Helvetica's widest ASCII glyphs are well below this, so wrapped text never leaves the page.
pub const FLOW_CHAR_WIDTH: f32 = 0.6;

// ---------------------------------------------------------------------------------------------
// Page content
// ---------------------------------------------------------------------------------------------

struct TextRun {
    x: f32,
    baseline: f32,
    size: f32,
    text: String,
}

enum LinkTarget {
    Page { page: usize, at: Option<(f32, f32)> },
    Uri(String),
}

struct LinkSpec {
    rect: PageRect,
    target: LinkTarget,
}

struct ImageSpec {
    rect: PageRect,
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

/// Content of one page. Handed to the closure of [`PdfBuilder::page`].
pub struct PageBuilder {
    width: f32,
    height: f32,
    texts: Vec<TextRun>,
    links: Vec<LinkSpec>,
    images: Vec<ImageSpec>,
}

impl PageBuilder {
    /// Draws an RGB image of `width` × `height` pixels (`rgb` holds three bytes per pixel, rows
    /// top to bottom) stretched over `rect` (page space). Written as an inline image, before the
    /// text of the page.
    ///
    /// # Panics
    /// If `rgb` does not hold `width * height * 3` bytes.
    pub fn image(&mut self, rect: PageRect, width: u32, height: u32, rgb: &[u8]) -> &mut Self {
        assert_eq!(rgb.len(), (width * height * 3) as usize, "image size");
        self.images.push(ImageSpec {
            rect,
            width,
            height,
            rgb: rgb.to_vec(),
        });
        self
    }

    /// Puts `text` on the page. `(x, y)` is the left end of the baseline in page space.
    /// `size` is the font size in pt.
    pub fn text(&mut self, x: f32, y: f32, size: f32, text: &str) -> &mut Self {
        self.texts.push(TextRun {
            x,
            baseline: y,
            size,
            text: text.to_owned(),
        });
        self
    }

    /// Adds a link to another page of the same document. `page` is 1-based. `at` is the target
    /// point in the destination page's page space and is written as `/XYZ left top null`;
    /// `None` writes `/Fit`.
    pub fn link_to(&mut self, rect: PageRect, page: usize, at: Option<(f32, f32)>) -> &mut Self {
        self.links.push(LinkSpec {
            rect,
            target: LinkTarget::Page { page, at },
        });
        self
    }

    /// Adds an external link (`/A << /S /URI /URI (..) >>`).
    pub fn link_uri(&mut self, rect: PageRect, uri: &str) -> &mut Self {
        self.links.push(LinkSpec {
            rect,
            target: LinkTarget::Uri(uri.to_owned()),
        });
        self
    }

    /// Size of this page in pt (`width`, `height`).
    pub fn size(&self) -> (f32, f32) {
        (self.width, self.height)
    }
}

// ---------------------------------------------------------------------------------------------
// Outline
// ---------------------------------------------------------------------------------------------

struct OutlineItem {
    title: String,
    page: usize,
    y: f32,
    children: Vec<OutlineItem>,
}

/// Children of one outline entry. Handed to the closure of [`PdfBuilder::outline`].
#[derive(Default)]
pub struct OutlineBuilder {
    items: Vec<OutlineItem>,
}

impl OutlineBuilder {
    /// Adds a leaf entry. `page` is 1-based; `y` is the target's y in that page's page space.
    pub fn child(&mut self, title: &str, page: usize, y: f32) -> &mut Self {
        self.child_with(title, page, y, |_| {})
    }

    /// Adds an entry that has children of its own.
    pub fn child_with(
        &mut self,
        title: &str,
        page: usize,
        y: f32,
        f: impl FnOnce(&mut OutlineBuilder),
    ) -> &mut Self {
        let mut inner = OutlineBuilder::default();
        f(&mut inner);
        self.items.push(OutlineItem {
            title: title.to_owned(),
            page,
            y,
            children: inner.items,
        });
        self
    }
}

// ---------------------------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------------------------

/// Builds a whole PDF. See the module documentation for an example.
#[derive(Default)]
pub struct PdfBuilder {
    pages: Vec<PageBuilder>,
    outline: Vec<OutlineItem>,
}

impl PdfBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a page of `width` × `height` pt and fills it in `f`.
    pub fn page(mut self, width: f32, height: f32, f: impl FnOnce(&mut PageBuilder)) -> Self {
        let mut page = PageBuilder {
            width,
            height,
            texts: Vec::new(),
            links: Vec::new(),
            images: Vec::new(),
        };
        f(&mut page);
        self.pages.push(page);
        self
    }

    /// Appends a top-level outline entry; nested entries are added in `f` (use `|_| {}` or
    /// [`PdfBuilder::outline_leaf`] for none). `page` is 1-based; `y` is page space.
    pub fn outline(
        mut self,
        title: &str,
        page: usize,
        y: f32,
        f: impl FnOnce(&mut OutlineBuilder),
    ) -> Self {
        let mut inner = OutlineBuilder::default();
        f(&mut inner);
        self.outline.push(OutlineItem {
            title: title.to_owned(),
            page,
            y,
            children: inner.items,
        });
        self
    }

    /// Appends a top-level outline entry without children.
    pub fn outline_leaf(self, title: &str, page: usize, y: f32) -> Self {
        self.outline(title, page, y, |_| {})
    }

    /// Number of pages added so far.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Serialises the document.
    ///
    /// # Panics
    /// If a link or an outline entry points to a page that does not exist, or a page size is not
    /// positive. These are mistakes in the test, and failing loudly beats a silently odd PDF.
    pub fn build(&self) -> Vec<u8> {
        self.validate();
        let mut w = PdfWriter::new();

        // Object numbers. 1 catalog, 2 page tree, 3 font, then per page: page, content, annots.
        let font_id = 3;
        let mut next = 4;
        struct PageIds {
            page: usize,
            content: usize,
            annots: Vec<usize>,
        }
        let mut page_ids = Vec::with_capacity(self.pages.len());
        for p in &self.pages {
            let page = next;
            let content = next + 1;
            next += 2;
            let annots: Vec<usize> = (next..next + p.links.len()).collect();
            next += p.links.len();
            page_ids.push(PageIds {
                page,
                content,
                annots,
            });
        }
        let outline_root = next;
        let flat = flatten_outline(&self.outline);
        // Item i has object number `outline_root + 1 + i`.
        let item_id = |i: usize| outline_root + 1 + i;

        let page_dest = |page: usize, at: Option<(f32, f32)>| -> String {
            let id = page_ids[page - 1].page;
            match at {
                Some((x, y)) => {
                    let h = self.pages[page - 1].height;
                    format!("[{id} 0 R /XYZ {} {} null]", num(x), num(h - y))
                }
                None => format!("[{id} 0 R /Fit]"),
            }
        };

        w.begin_object(1);
        w.raw(&if flat.is_empty() {
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned()
        } else {
            format!("<< /Type /Catalog /Pages 2 0 R /Outlines {outline_root} 0 R /PageMode /UseOutlines >>")
        });
        w.end_object();

        w.begin_object(2);
        let kids: Vec<String> = page_ids.iter().map(|p| format!("{} 0 R", p.page)).collect();
        w.raw(&format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            self.pages.len()
        ));
        w.end_object();

        w.begin_object(font_id);
        w.raw("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>");
        w.end_object();

        for (p, ids) in self.pages.iter().zip(&page_ids) {
            let annots = if ids.annots.is_empty() {
                String::new()
            } else {
                let refs: Vec<String> = ids.annots.iter().map(|a| format!("{a} 0 R")).collect();
                format!(" /Annots [{}]", refs.join(" "))
            };
            w.begin_object(ids.page);
            w.raw(&format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << /Font << /F1 {font_id} 0 R >> >> /Contents {} 0 R{annots} >>",
                num(p.width),
                num(p.height),
                ids.content
            ));
            w.end_object();

            let mut stream = String::new();
            for img in &p.images {
                let (l, t, r, b) = img.rect;
                let mut hex = String::with_capacity(img.rgb.len() * 2);
                for v in &img.rgb {
                    let _ = write!(hex, "{v:02x}");
                }
                let _ = writeln!(
                    stream,
                    "q {} 0 0 {} {} {} cm BI /W {} /H {} /CS /RGB /BPC 8 /F /AHx ID {hex}> EI Q",
                    num(r - l),
                    num(b - t),
                    num(l),
                    num(p.height - b),
                    img.width,
                    img.height
                );
            }
            for t in &p.texts {
                let _ = writeln!(
                    stream,
                    "BT /F1 {} Tf {} {} Td ({}) Tj ET",
                    num(t.size),
                    num(t.x),
                    num(p.height - t.baseline),
                    escape(&t.text)
                );
            }
            w.begin_object(ids.content);
            w.raw(&format!(
                "<< /Length {} >>\nstream\n{stream}endstream",
                stream.len()
            ));
            w.end_object();

            for (link, id) in p.links.iter().zip(&ids.annots) {
                let (l, t, r, b) = link.rect;
                let action = match &link.target {
                    LinkTarget::Page { page, at } => format!("/Dest {}", page_dest(*page, *at)),
                    LinkTarget::Uri(uri) => {
                        format!("/A << /S /URI /URI ({}) >>", escape(uri))
                    }
                };
                w.begin_object(*id);
                w.raw(&format!(
                    "<< /Type /Annot /Subtype /Link /Rect [{} {} {} {}] /Border [0 0 0] {action} >>",
                    num(l),
                    num(p.height - b),
                    num(r),
                    num(p.height - t)
                ));
                w.end_object();
            }
        }

        if !flat.is_empty() {
            w.begin_object(outline_root);
            let roots: Vec<usize> = (0..flat.len())
                .filter(|i| flat[*i].parent.is_none())
                .collect();
            let total: usize = roots.iter().map(|i| 1 + flat[*i].count).sum();
            w.raw(&format!(
                "<< /Type /Outlines /First {} 0 R /Last {} 0 R /Count {total} >>",
                item_id(roots[0]),
                item_id(roots[roots.len() - 1])
            ));
            w.end_object();
            for (i, it) in flat.iter().enumerate() {
                let mut d = format!(
                    "<< /Title ({}) /Parent {} 0 R /Dest {}",
                    escape(it.title),
                    it.parent.map_or(outline_root, item_id),
                    // The outline target has a top but no left: `/XYZ null top null`.
                    {
                        let id = page_ids[it.page - 1].page;
                        let h = self.pages[it.page - 1].height;
                        format!("[{id} 0 R /XYZ null {} null]", num(h - it.y))
                    }
                );
                if let Some(p) = it.prev {
                    let _ = write!(d, " /Prev {} 0 R", item_id(p));
                }
                if let Some(n) = it.next {
                    let _ = write!(d, " /Next {} 0 R", item_id(n));
                }
                if let (Some(f), Some(l)) = (it.first, it.last) {
                    let _ = write!(
                        d,
                        " /First {} 0 R /Last {} 0 R /Count {}",
                        item_id(f),
                        item_id(l),
                        it.count
                    );
                }
                d.push_str(" >>");
                w.begin_object(item_id(i));
                w.raw(&d);
                w.end_object();
            }
        }

        w.finish()
    }

    /// Writes the document to a fresh file in the system temp directory and returns a guard
    /// that deletes the file when dropped. `stem` is only used to make the name readable.
    pub fn write_temp(&self, stem: &str) -> TempPdf {
        TempPdf::create(stem, &self.build())
    }

    fn validate(&self) {
        for (i, p) in self.pages.iter().enumerate() {
            assert!(
                p.width > 0.0 && p.height > 0.0,
                "page {} has a non-positive size",
                i + 1
            );
            for l in &p.links {
                if let LinkTarget::Page { page, .. } = l.target {
                    assert!(
                        (1..=self.pages.len()).contains(&page),
                        "link on page {} points to missing page {page}",
                        i + 1
                    );
                }
            }
        }
        fn check(items: &[OutlineItem], pages: usize) {
            for it in items {
                assert!(
                    (1..=pages).contains(&it.page),
                    "outline entry {:?} points to missing page {}",
                    it.title,
                    it.page
                );
                check(&it.children, pages);
            }
        }
        check(&self.outline, self.pages.len());
    }
}

/// One outline entry with its tree links resolved to indices into the flat list.
struct FlatItem<'a> {
    title: &'a str,
    page: usize,
    y: f32,
    parent: Option<usize>,
    prev: Option<usize>,
    next: Option<usize>,
    first: Option<usize>,
    last: Option<usize>,
    /// Number of descendants (every entry is written open, so the count is positive).
    count: usize,
}

/// Flattens the outline tree in pre-order.
fn flatten_outline(items: &[OutlineItem]) -> Vec<FlatItem<'_>> {
    fn walk<'a>(
        items: &'a [OutlineItem],
        parent: Option<usize>,
        out: &mut Vec<FlatItem<'a>>,
    ) -> Vec<usize> {
        let mut siblings = Vec::with_capacity(items.len());
        for it in items {
            let idx = out.len();
            out.push(FlatItem {
                title: &it.title,
                page: it.page,
                y: it.y,
                parent,
                prev: None,
                next: None,
                first: None,
                last: None,
                count: 0,
            });
            let kids = walk(&it.children, Some(idx), out);
            out[idx].first = kids.first().copied();
            out[idx].last = kids.last().copied();
            out[idx].count = kids.iter().map(|k| 1 + out[*k].count).sum();
            siblings.push(idx);
        }
        for (n, idx) in siblings.iter().enumerate() {
            out[*idx].prev = n.checked_sub(1).map(|p| siblings[p]);
            out[*idx].next = siblings.get(n + 1).copied();
        }
        siblings
    }
    let mut out = Vec::new();
    walk(items, None, &mut out);
    out
}

/// Collects objects and writes the cross-reference table with exact byte offsets.
struct PdfWriter {
    buf: String,
    /// `offsets[n - 1]` is the byte offset of object `n`.
    offsets: Vec<usize>,
}

impl PdfWriter {
    fn new() -> Self {
        Self {
            buf: String::from("%PDF-1.7\n"),
            offsets: Vec::new(),
        }
    }

    fn begin_object(&mut self, id: usize) {
        assert_eq!(
            id,
            self.offsets.len() + 1,
            "objects must be written in numeric order"
        );
        self.offsets.push(self.buf.len());
        let _ = writeln!(self.buf, "{id} 0 obj");
    }

    fn raw(&mut self, s: &str) {
        self.buf.push_str(s);
        self.buf.push('\n');
    }

    fn end_object(&mut self) {
        self.buf.push_str("endobj\n");
    }

    fn finish(mut self) -> Vec<u8> {
        let xref = self.buf.len();
        let n = self.offsets.len() + 1;
        let _ = writeln!(self.buf, "xref\n0 {n}");
        // Each entry is exactly 20 bytes, including the two-character line end.
        self.buf.push_str("0000000000 65535 f \n");
        for off in &self.offsets {
            let _ = writeln!(self.buf, "{off:010} 00000 n ");
        }
        let _ = write!(
            self.buf,
            "trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        );
        self.buf.into_bytes()
    }
}

/// Formats a number for a PDF content stream or object (no exponent, no NaN).
fn num(v: f32) -> String {
    assert!(v.is_finite(), "non-finite coordinate in test PDF");
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

/// Makes `s` a PDF literal string body: ASCII only, `( ) \` escaped.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '(' | ')' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            ' '..='~' => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Temp file
// ---------------------------------------------------------------------------------------------

/// A PDF file in the system temp directory, deleted on drop.
pub struct TempPdf {
    path: PathBuf,
}

impl TempPdf {
    fn create(stem: &str, bytes: &[u8]) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let stem: String = stem
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let name = format!(
            "refdesk-test-{}-{}-{stem}.pdf",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        if let Err(e) = std::fs::write(&path, bytes) {
            panic!("cannot write test PDF {}: {e}", path.display());
        }
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keeps the file and returns its path.
    pub fn into_path(self) -> PathBuf {
        let path = self.path.clone();
        std::mem::forget(self);
        path
    }
}

impl AsRef<Path> for TempPdf {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempPdf {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------------------------
// Flow layout
// ---------------------------------------------------------------------------------------------

/// Builds a page's header or footer text from its 1-based page number.
pub type PageTextFn = Box<dyn Fn(usize) -> String>;

/// Options of [`flow_document`]. All lengths are pt.
pub struct FlowOptions {
    pub page_width: f32,
    pub page_height: f32,
    /// Margin on all four sides. The header sits at half the margin from the top edge and the
    /// footer at half the margin from the bottom edge (baselines).
    pub margin: f32,
    pub font_size: f32,
    /// Distance between the baselines of consecutive lines.
    pub line_height: f32,
    /// Extra vertical space between paragraphs.
    pub paragraph_gap: f32,
    pub header: Option<PageTextFn>,
    pub footer: Option<PageTextFn>,
}

impl Default for FlowOptions {
    /// US Letter, 72 pt margins, 12 pt text on 16 pt lines, 8 pt between paragraphs, no header
    /// or footer.
    fn default() -> Self {
        Self {
            page_width: 612.0,
            page_height: 792.0,
            margin: 72.0,
            font_size: 12.0,
            line_height: 16.0,
            paragraph_gap: 8.0,
            header: None,
            footer: None,
        }
    }
}

impl FlowOptions {
    pub fn header(mut self, f: impl Fn(usize) -> String + 'static) -> Self {
        self.header = Some(Box::new(f));
        self
    }

    pub fn footer(mut self, f: impl Fn(usize) -> String + 'static) -> Self {
        self.footer = Some(Box::new(f));
        self
    }

    /// Characters that fit on one line.
    fn line_chars(&self) -> usize {
        let n =
            ((self.page_width - 2.0 * self.margin) / (self.font_size * FLOW_CHAR_WIDTH)).floor();
        (n as usize).max(1)
    }
}

/// Greedy word wrap at `width` characters. A word longer than a line is cut.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word;
        loop {
            let len = cur.chars().count();
            let wlen = word.chars().count();
            let needed = if len == 0 { wlen } else { len + 1 + wlen };
            if needed <= width {
                if len > 0 {
                    cur.push(' ');
                }
                cur.push_str(word);
                break;
            }
            if len > 0 {
                lines.push(std::mem::take(&mut cur));
                continue;
            }
            // Word alone is too long: cut it.
            let cut = word
                .char_indices()
                .nth(width)
                .map_or(word.len(), |(i, _)| i);
            lines.push(word[..cut].to_owned());
            word = &word[cut..];
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Lays `paragraphs` out over as many pages as needed and returns the builder, so the caller
/// may still add an outline or links (`.outline(..)`, then `.build()`).
///
/// Text starts at the top margin (first baseline one font size below it) and stops at the
/// bottom margin. A paragraph may be split between pages. Header and footer, when given, are
/// written on every page after the body text. There is always at least one page.
///
/// To get a pair of documents where an insertion pushes the rest onwards, call this twice with
/// paragraph lists that differ by one entry near the front.
pub fn flow_document<S: AsRef<str>>(paragraphs: &[S], opts: &FlowOptions) -> PdfBuilder {
    assert!(
        opts.page_height > 2.0 * opts.margin + opts.font_size + opts.line_height,
        "page too small for the margins"
    );
    let width = opts.line_chars();
    let top = opts.margin + opts.font_size;
    let bottom = opts.page_height - opts.margin;

    // Each page is a list of (baseline y, text).
    let mut pages: Vec<Vec<(f32, String)>> = vec![Vec::new()];
    let mut y = top;
    for (i, para) in paragraphs.iter().enumerate() {
        let lines = wrap_words(para.as_ref(), width);
        let at_page_top = pages.last().is_some_and(|p| p.is_empty());
        if i > 0 && !at_page_top {
            y += opts.paragraph_gap;
        }
        for line in lines {
            if y > bottom {
                pages.push(Vec::new());
                y = top;
            }
            if let Some(page) = pages.last_mut() {
                page.push((y, line));
            }
            y += opts.line_height;
        }
    }

    let mut doc = PdfBuilder::new();
    for (n, lines) in pages.iter().enumerate() {
        let number = n + 1;
        doc = doc.page(opts.page_width, opts.page_height, |p| {
            for (y, line) in lines {
                p.text(opts.margin, *y, opts.font_size, line);
            }
            if let Some(h) = &opts.header {
                p.text(opts.margin, opts.margin / 2.0, opts.font_size, &h(number));
            }
            if let Some(f) = &opts.footer {
                p.text(
                    opts.margin,
                    opts.page_height - opts.margin / 2.0,
                    opts.font_size,
                    &f(number),
                );
            }
        });
    }
    doc
}
