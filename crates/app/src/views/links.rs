//! Links in the PDF view: what is under the pointer, what it says, what a click does, the mark
//! that shows where a jump landed, and the rest timer that keeps the history entry current.
//!
//! Everything here is a plain function or plain data, tested without egui (rule R-10). The view
//! (`pdf_view.rs`) reads the pointer, asks these functions, and paints or queues the result.
//! Links are in page space (R-5); only the helpers that return `*_rects` / `*_band` produce
//! viewport positions, and they do not store them.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use refdesk_core::link::hit_test;
use refdesk_core::nav::dest::resolve;
use refdesk_core::{Link, LinkTarget, Location, Outline, PageLayout, Point, Rect, Size};

use crate::action::OpenHow;
use crate::i18n::tr;
use crate::overlays::link_hints::LinkHints;
use crate::overlays::link_preview::PreviewState;
use crate::overlays::pinned_preview::PinnedPreviews;
use crate::views::selection::{self, Hit};

/// How long the mark of a jump's landing place takes to fade out.
pub const ARRIVAL_SECS: f64 = 0.8;
/// The scroll position counts as at rest after this long without change; then the history entry
/// of the view takes it over (LINK-3).
pub const REST_SECS: f64 = 1.0;
/// The mark starts this many logical pixels above the landing place...
pub const ARRIVAL_ABOVE: f32 = 4.0;
/// ...and is this high.
pub const ARRIVAL_HEIGHT: f32 = 24.0;
/// Schemes of external links that are opened with the default program.
pub const OPENABLE_SCHEMES: &[&str] = &["http", "https", "mailto"];

// ------------------------------------------------------------------------------------------------
// Where the pointer is
// ------------------------------------------------------------------------------------------------

/// The links of a page, or `None` while they have not arrived. The function queues the request.
pub type PageLinks<'a> = &'a dyn Fn(u32) -> Option<Arc<[Link]>>;

/// A link found under a position.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkHit {
    /// The page the link is on (zero-based).
    pub page: u32,
    /// The index of the link in the list of that page.
    pub index: usize,
    /// The link.
    pub link: Link,
}

/// The link at a document position found by the view's hit test. `None` off the page, while the
/// links of the page have not arrived, and where there is none.
pub fn link_at_hit(hit: &Hit, links_of: PageLinks) -> Option<LinkHit> {
    if !hit.on_page {
        return None;
    }
    let links = links_of(hit.loc.page)?;
    let index = hit_test(&links, Point::new(hit.loc.x, hit.loc.y))?;
    Some(LinkHit {
        page: hit.loc.page,
        index,
        link: links[index].clone(),
    })
}

/// The link under `viewport_pos`, a position relative to the top-left corner of the view, with
/// the view scrolled to `scroll` (content space).
pub fn link_at(
    layout: &PageLayout,
    scroll: Point,
    viewport_pos: Point,
    links_of: PageLinks,
) -> Option<LinkHit> {
    let hit = selection::locate(layout, scroll, viewport_pos)?;
    link_at_hit(&hit, links_of)
}

/// The rectangles of a link on `page` as positions relative to the top-left corner of the view.
pub fn screen_rects(layout: &PageLayout, scroll: Point, page: u32, rects: &[Rect]) -> Vec<Rect> {
    let origin = layout.page_rect(page);
    let s = layout.scale();
    rects
        .iter()
        .map(|r| {
            Rect::new(
                origin.x0 + r.x0 * s - scroll.x,
                origin.y0 + r.y0 * s - scroll.y,
                origin.x0 + r.x1 * s - scroll.x,
                origin.y0 + r.y1 * s - scroll.y,
            )
        })
        .collect()
}

// ------------------------------------------------------------------------------------------------
// What a link says
// ------------------------------------------------------------------------------------------------

/// What to show about a link (LINK-5, LINK-7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkInfo {
    /// A place in this document: the page as numbered on it (its label when it has one) and the
    /// title of the section there, when the document has a table of contents that covers it.
    Page {
        /// The page label, or the page number from 1.
        label: String,
        /// The section title.
        title: Option<String>,
    },
    /// An external address, as written.
    Url(String),
    /// Another document, with the page in it (from 1) when the link names one.
    File {
        /// The file name.
        name: String,
        /// The page number.
        page: Option<u32>,
    },
}

impl LinkInfo {
    /// The text in the current language: "12쪽 — 7.3.2 제목", the address, or "name — 3쪽".
    pub fn text(&self) -> String {
        match self {
            LinkInfo::Page {
                label,
                title: Some(title),
            } => tr("link.page_title")
                .replace("{page}", label)
                .replace("{title}", title),
            LinkInfo::Page { label, title: None } => tr("link.page").replace("{page}", label),
            LinkInfo::Url(url) => url.clone(),
            LinkInfo::File {
                name,
                page: Some(page),
            } => tr("link.file_page")
                .replace("{name}", name)
                .replace("{page}", &page.to_string()),
            LinkInfo::File { name, page: None } => name.clone(),
        }
    }
}

/// The page as numbered on it: its label if the document has one, else the number from 1.
pub fn page_label(page: u32, labels: &[String]) -> String {
    match labels.get(page as usize) {
        Some(l) if !l.trim().is_empty() => l.clone(),
        _ => (page + 1).to_string(),
    }
}

/// What to show about the target of a link in a document with these pages, page labels and
/// outline. The section is the one the outline says contains the place the link leads to.
pub fn describe(
    target: &LinkTarget,
    page_sizes: &[Size],
    page_labels: &[String],
    outline: &Outline,
) -> LinkInfo {
    match target {
        LinkTarget::Internal(dest) => {
            let size = page_sizes
                .get(dest.page as usize)
                .copied()
                .unwrap_or_default();
            let mut at = resolve(dest, size);
            // A destination past the last page shows as the last page.
            if let Some(last) = page_sizes.len().checked_sub(1) {
                at.page = at.page.min(last as u32);
            }
            let title = outline
                .current(at)
                .and_then(|i| outline.items.get(i as usize))
                .map(|item| item.title.trim().to_owned())
                .filter(|t| !t.is_empty());
            LinkInfo::Page {
                label: page_label(at.page, page_labels),
                title,
            }
        }
        LinkTarget::External(url) => LinkInfo::Url(url.clone()),
        LinkTarget::OtherDocument { path, dest } => LinkInfo::File {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            page: dest.map(|d| d.page + 1),
        },
    }
}

// ------------------------------------------------------------------------------------------------
// What a click does
// ------------------------------------------------------------------------------------------------

/// The mouse button of a click on a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickButton {
    /// The main button.
    Primary,
    /// The wheel button.
    Middle,
}

/// The modifier keys held at a click.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClickMods {
    /// Ctrl.
    pub ctrl: bool,
    /// Shift.
    pub shift: bool,
}

/// What following a link makes happen.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkAction {
    /// Go to a place in this document, here or in a new view.
    Navigate {
        /// The place.
        to: Location,
        /// Which view shows it.
        how: OpenHow,
    },
    /// Open an address with the default program.
    OpenUrl(String),
    /// Open another document as a tab, at a page (zero-based) when the link names one.
    OpenFile {
        /// The file.
        path: PathBuf,
        /// The page.
        page: Option<u32>,
    },
    /// An address that is not opened; the status bar shows it while the pointer is on the link.
    Blocked(String),
}

/// Where a click on an in-document link opens: a plain click here; Ctrl or the wheel button in a
/// new tab; Ctrl+Shift in a split beside (LINK-6).
pub fn open_how(button: ClickButton, mods: ClickMods) -> OpenHow {
    if mods.ctrl && mods.shift {
        OpenHow::SplitSide
    } else if mods.ctrl || button == ClickButton::Middle {
        OpenHow::NewTab
    } else {
        OpenHow::Here
    }
}

/// Whether an address is opened with the default program: only the schemes of
/// [`OPENABLE_SCHEMES`], compared without regard to case.
pub fn is_openable(url: &str) -> bool {
    let Some((scheme, _)) = url.trim().split_once(':') else {
        return false;
    };
    OPENABLE_SCHEMES
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
}

/// What a click with `button` and `mods` on a link to `target` does. `page_sizes` are the pages
/// of the document the link is in. The zoom a destination asks for is ignored (LINK-1).
pub fn follow(
    target: &LinkTarget,
    button: ClickButton,
    mods: ClickMods,
    page_sizes: &[Size],
) -> LinkAction {
    match target {
        LinkTarget::Internal(dest) => {
            let size = page_sizes
                .get(dest.page as usize)
                .copied()
                .unwrap_or_default();
            LinkAction::Navigate {
                to: resolve(dest, size),
                how: open_how(button, mods),
            }
        }
        LinkTarget::External(url) if is_openable(url) => LinkAction::OpenUrl(url.trim().to_owned()),
        LinkTarget::External(url) => LinkAction::Blocked(url.clone()),
        LinkTarget::OtherDocument { path, dest } => LinkAction::OpenFile {
            path: path.clone(),
            page: dest.map(|d| d.page),
        },
    }
}

// ------------------------------------------------------------------------------------------------
// The mark where a jump landed (LINK-9)
// ------------------------------------------------------------------------------------------------

/// How strong the mark is `now`, for one that started at `started` (seconds): 1 at the start,
/// falling to 0 over [`ARRIVAL_SECS`]. `None` once it is over.
pub fn arrival_strength(started: f64, now: f64) -> Option<f32> {
    let t = ((now - started) / ARRIVAL_SECS).max(0.0);
    (t < 1.0).then_some((1.0 - t) as f32)
}

/// A landing place being marked. The clock starts at the first frame that asks for the strength,
/// so an action that has no clock can start it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Arrival {
    /// The place that was jumped to.
    pub loc: Location,
    started: Option<f64>,
}

impl Arrival {
    /// A mark for `loc` whose clock has not started.
    pub fn new(loc: Location) -> Arrival {
        Arrival { loc, started: None }
    }

    /// The strength at `now` (seconds); the first call starts the clock. `None` when over.
    pub fn strength(&mut self, now: f64) -> Option<f32> {
        let started = *self.started.get_or_insert(now);
        arrival_strength(started, now)
    }
}

/// The band that marks `loc`, relative to the top-left corner of the view: as wide as the page,
/// from [`ARRIVAL_ABOVE`] above the place to [`ARRIVAL_HEIGHT`] high. A place above the page
/// (the margin) is marked at the top of the page.
pub fn arrival_band(layout: &PageLayout, scroll: Point, loc: Location) -> Option<Rect> {
    if loc.page >= layout.page_count() {
        return None;
    }
    let page = layout.page_rect(loc.page);
    let top = page.y0 + loc.y.max(0.0) * layout.scale() - ARRIVAL_ABOVE - scroll.y;
    Some(Rect::new(
        page.x0 - scroll.x,
        top,
        page.x1 - scroll.x,
        top + ARRIVAL_HEIGHT,
    ))
}

// ------------------------------------------------------------------------------------------------
// Scroll at rest (LINK-3)
// ------------------------------------------------------------------------------------------------

/// Notices when the view has stood still for [`REST_SECS`], once per standstill.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollRest {
    last: Option<Location>,
    since: f64,
    reported: bool,
}

impl ScrollRest {
    /// Records the place the view is at in a frame at `now` (seconds). True once when it has not
    /// changed for [`REST_SECS`]: the caller then updates the history entry (`History::touch`).
    pub fn note(&mut self, at: Location, now: f64) -> bool {
        if self.last != Some(at) {
            self.last = Some(at);
            self.since = now;
            self.reported = false;
            false
        } else if !self.reported && now - self.since >= REST_SECS {
            self.reported = true;
            true
        } else {
            false
        }
    }

    /// Time until [`ScrollRest::note`] reports; `None` when it already did or nothing was noted.
    pub fn remaining(&self, now: f64) -> Option<Duration> {
        if self.reported || self.last.is_none() {
            return None;
        }
        let left = REST_SECS - (now - self.since);
        Some(Duration::from_secs_f64(left.max(0.0)))
    }
}

// ------------------------------------------------------------------------------------------------
// The link state of one view
// ------------------------------------------------------------------------------------------------

/// What a view remembers about links between frames. Not saved.
#[derive(Clone, Debug, Default)]
pub struct LinkState {
    /// What the link under the pointer says, for the status bar.
    pub hover_text: Option<String>,
    /// The landing place being marked.
    pub arrival: Option<Arrival>,
    /// Scroll standstill, for the history entry.
    pub rest: ScrollRest,
    /// The wheel button went down in the view.
    pub middle_down: bool,
    /// The hover preview popup of a link (LINK-8).
    pub preview: PreviewState,
    /// The keyboard link hints (LINK-13), while they are shown.
    pub hints: LinkHints,
    /// The pinned previews (LINK-14).
    pub pinned: PinnedPreviews,
}

impl LinkState {
    /// Sets the text of the link under the pointer. True when it changed.
    pub fn set_hover_text(&mut self, text: Option<String>) -> bool {
        if self.hover_text == text {
            return false;
        }
        self.hover_text = text;
        true
    }

    /// Starts the mark of a landing place.
    pub fn mark_arrival(&mut self, loc: Location) {
        self.arrival = Some(Arrival::new(loc));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Instant;

    use crate::i18n::{current_lang, lookup};
    use crate::views::test_support::TIMEOUT;
    use refdesk_core::layout::PIXELS_PER_POINT_AT_100;
    use refdesk_core::testutil::{PdfBuilder, TempPdf};
    use refdesk_core::{DestKind, Destination, DocumentHandle, LayoutParams, ZoomMode};

    use crate::views::pdf_view::{MARGIN, PAGE_GAP};

    fn template(key: &str) -> &'static str {
        lookup(current_lang(), key).expect("key exists")
    }

    fn dest(page: u32, top: f32) -> Destination {
        Destination {
            page,
            kind: DestKind::Xyz {
                left: Some(72.0),
                top: Some(top),
            },
        }
    }

    // ---- hit test on a real document ----

    struct Opened {
        doc: DocumentHandle,
        sizes: Vec<Size>,
        _pdf: TempPdf,
    }

    /// Calls `check` until it holds, re-checking after every notification of the document.
    fn wait_until(rx: &Receiver<()>, what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !check() {
            let left = deadline.saturating_duration_since(Instant::now());
            rx.recv_timeout(left)
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
        }
    }

    fn open(pdf: TempPdf) -> Opened {
        let (tx, rx) = mpsc::channel();
        // `Sender` is not `Sync`; the notifier must be.
        let tx = Mutex::new(tx);
        let doc = DocumentHandle::open(
            pdf.path(),
            Arc::new(move || {
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send(());
                }
            }),
        );
        wait_until(&rx, "the document", || doc.info().is_some());
        let sizes = doc.info().expect("info").page_sizes.clone();
        for page in 0..sizes.len() as u32 {
            wait_until(&rx, "the links", || doc.page_links(page).is_some());
        }
        Opened {
            doc,
            sizes,
            _pdf: pdf,
        }
    }

    fn layout(sizes: &[Size], zoom: ZoomMode) -> PageLayout {
        PageLayout::new(
            sizes,
            LayoutParams {
                viewport: Size::new(800.0, 600.0),
                zoom,
                gap: PAGE_GAP,
                margin: MARGIN,
                fit_page: 0,
            },
        )
    }

    /// The viewport position of a page-space point.
    fn viewport_of(layout: &PageLayout, scroll: Point, page: u32, x: f32, y: f32) -> Point {
        let r = layout.page_rect(page);
        let s = layout.scale();
        Point::new(r.x0 + x * s - scroll.x, r.y0 + y * s - scroll.y)
    }

    fn doc_with_links() -> Opened {
        open(
            PdfBuilder::new()
                .page(612.0, 792.0, |p| {
                    p.text(72.0, 100.0, 12.0, "see section 4.2 on the next");
                    p.link_to((72.0, 90.0, 200.0, 104.0), 2, Some((72.0, 300.0)));
                    // One link over two lines: the end of one line and the start of the next.
                    p.link_to((400.0, 200.0, 540.0, 212.0), 3, None);
                    p.link_to((72.0, 214.0, 150.0, 226.0), 3, None);
                })
                .page(612.0, 792.0, |_| {})
                .page(612.0, 792.0, |_| {})
                .write_temp("links-hit"),
        )
    }

    #[test]
    fn a_point_on_a_link_hits_it_at_any_zoom_and_scroll() {
        let o = doc_with_links();
        let links_of = |p: u32| o.doc.page_links(p);
        let mut checked = 0;
        for zoom in [
            ZoomMode::FitWidth,
            ZoomMode::Custom(0.5),
            ZoomMode::Custom(1.0),
            ZoomMode::Custom(3.0),
        ] {
            let layout = layout(&o.sizes, zoom);
            for scroll in [
                Point::new(0.0, 0.0),
                Point::new(0.0, 5.0),
                // Scrolled so that the first page's link is still in the viewport.
                Point::new(0.0, layout.page_rect(0).y0 + 60.0 * layout.scale()),
            ] {
                let scroll = layout.clamp_scroll(scroll, Size::new(800.0, 600.0));
                let at = viewport_of(&layout, scroll, 0, 100.0, 97.0);
                if at.x < 0.0 || at.y < 0.0 || at.x > 800.0 || at.y > 600.0 {
                    continue; // outside the viewport at this zoom and scroll
                }
                let hit = link_at(&layout, scroll, at, &links_of)
                    .unwrap_or_else(|| panic!("a hit at {zoom:?}, {scroll:?}"));
                checked += 1;
                assert_eq!((hit.page, hit.index), (0, 0));
                assert_eq!(
                    hit.link.target,
                    LinkTarget::Internal(Destination {
                        page: 1,
                        kind: DestKind::Xyz {
                            left: Some(72.0),
                            top: Some(300.0)
                        }
                    })
                );
                // Just outside the link, on the page: nothing.
                let off = viewport_of(&layout, scroll, 0, 100.0, 120.0);
                if off.y >= 0.0 && off.y <= 600.0 {
                    assert_eq!(link_at(&layout, scroll, off, &links_of), None);
                }
            }
        }
        assert!(
            checked >= 8,
            "most combinations were really checked: {checked}"
        );
    }

    #[test]
    fn the_second_line_of_a_two_line_link_hits_the_same_link() {
        let o = doc_with_links();
        let links_of = |p: u32| o.doc.page_links(p);
        let layout = layout(&o.sizes, ZoomMode::Custom(1.0));
        let scroll = layout.clamp_scroll(
            Point::new(0.0, layout.page_rect(0).y0 + 150.0),
            Size::new(800.0, 600.0),
        );
        let first = link_at(
            &layout,
            scroll,
            viewport_of(&layout, scroll, 0, 470.0, 206.0),
            &links_of,
        )
        .expect("first line");
        let second = link_at(
            &layout,
            scroll,
            viewport_of(&layout, scroll, 0, 100.0, 220.0),
            &links_of,
        )
        .expect("second line");
        assert_eq!(first.link.rects.len(), 2, "merged into one link");
        assert_eq!((first.page, first.index), (second.page, second.index));
        assert_eq!(first.link, second.link);
        // Between the lines' ends, to the right of the second line: no link.
        assert_eq!(
            link_at(
                &layout,
                scroll,
                viewport_of(&layout, scroll, 0, 300.0, 220.0),
                &links_of
            ),
            None
        );
    }

    #[test]
    fn nothing_is_hit_in_a_gap_off_the_page_or_before_the_links_arrive() {
        let o = doc_with_links();
        let layout = layout(&o.sizes, ZoomMode::Custom(1.0));
        let scroll = Point::new(0.0, 0.0);
        let links_of = |p: u32| o.doc.page_links(p);
        // The margin above the first page.
        assert_eq!(
            link_at(&layout, scroll, Point::new(300.0, 2.0), &links_of),
            None
        );
        // A pending page: the closure has nothing yet.
        let at = viewport_of(&layout, scroll, 0, 100.0, 97.0);
        assert_eq!(link_at(&layout, scroll, at, &|_| None), None);
        // A position on the link's coordinates but past the page's edge.
        let hit = Hit {
            loc: Location::new(0, 100.0, 97.0),
            on_page: false,
        };
        assert_eq!(link_at_hit(&hit, &links_of), None);
    }

    #[test]
    fn screen_rects_follow_zoom_and_scroll() {
        let o = doc_with_links();
        let layout = layout(&o.sizes, ZoomMode::Custom(2.0));
        let scroll = Point::new(0.0, 100.0);
        let rects = screen_rects(&layout, scroll, 0, &[Rect::new(72.0, 90.0, 200.0, 104.0)]);
        let s = 2.0 * PIXELS_PER_POINT_AT_100;
        let page = layout.page_rect(0);
        assert_eq!(rects.len(), 1);
        assert!((rects[0].x0 - (page.x0 + 72.0 * s)).abs() < 0.01);
        assert!((rects[0].y0 - (page.y0 + 90.0 * s - 100.0)).abs() < 0.01);
        assert!((rects[0].width() - 128.0 * s).abs() < 0.01);
    }

    // ---- what a link says ----

    fn outline() -> Outline {
        // Built through the engine's own constructor so that `current` works.
        Outline::from_tree(vec![refdesk_core::OutlineNode {
            title: "7 Interfaces".into(),
            target: Some(Location::new(1, 0.0, 50.0)),
            uri: None,
            children: vec![refdesk_core::OutlineNode {
                title: "7.3 Timing".into(),
                target: Some(Location::new(3, 0.0, 200.0)),
                uri: None,
                children: Vec::new(),
            }],
        }])
    }

    fn sizes(n: usize) -> Vec<Size> {
        vec![Size::new(595.0, 842.0); n]
    }

    #[test]
    fn an_in_document_link_shows_its_page_and_section() {
        let info = describe(
            &LinkTarget::Internal(dest(3, 300.0)),
            &sizes(10),
            &[],
            &outline(),
        );
        assert_eq!(
            info,
            LinkInfo::Page {
                label: "4".into(),
                title: Some("7.3 Timing".into())
            }
        );
        let expected = template("link.page_title")
            .replace("{page}", "4")
            .replace("{title}", "7.3 Timing");
        assert_eq!(info.text(), expected);
        // Before the section starts on its page, the section before it.
        let before = describe(
            &LinkTarget::Internal(dest(3, 100.0)),
            &sizes(10),
            &[],
            &outline(),
        );
        assert!(matches!(&before, LinkInfo::Page { title: Some(t), .. } if t == "7 Interfaces"));
        // Before the first entry: no section.
        let first = describe(
            &LinkTarget::Internal(dest(0, 10.0)),
            &sizes(10),
            &[],
            &outline(),
        );
        assert_eq!(
            first,
            LinkInfo::Page {
                label: "1".into(),
                title: None
            }
        );
    }

    #[test]
    fn without_an_outline_only_the_page_is_shown_and_a_label_wins_over_the_number() {
        let info = describe(
            &LinkTarget::Internal(dest(11, 0.0)),
            &sizes(20),
            &[],
            &Outline::new(),
        );
        assert_eq!(info.text(), template("link.page").replace("{page}", "12"));
        // With page labels: the label of that page.
        let labels: Vec<String> = (0..20)
            .map(|i| {
                if i < 4 {
                    roman(i).to_owned()
                } else {
                    String::new()
                }
            })
            .collect();
        let with_label = describe(
            &LinkTarget::Internal(dest(2, 0.0)),
            &sizes(20),
            &labels,
            &Outline::new(),
        );
        assert_eq!(
            with_label,
            LinkInfo::Page {
                label: "iii".into(),
                title: None
            }
        );
        // An empty label falls back to the number.
        let unlabeled = describe(
            &LinkTarget::Internal(dest(7, 0.0)),
            &sizes(20),
            &labels,
            &Outline::new(),
        );
        assert!(matches!(unlabeled, LinkInfo::Page { label, .. } if label == "8"));
        // A destination past the end shows the last page.
        let past = describe(
            &LinkTarget::Internal(dest(99, 0.0)),
            &sizes(20),
            &[],
            &Outline::new(),
        );
        assert!(matches!(past, LinkInfo::Page { label, .. } if label == "20"));
    }

    fn roman(i: usize) -> &'static str {
        ["i", "ii", "iii", "iv"][i]
    }

    #[test]
    fn an_external_link_shows_its_address_and_another_document_its_file_name() {
        let url = LinkTarget::External("https://example.com/a?b=c#d".into());
        let info = describe(&url, &sizes(1), &[], &Outline::new());
        assert_eq!(info.text(), "https://example.com/a?b=c#d");

        let other = LinkTarget::OtherDocument {
            path: PathBuf::from("/docs/spec/sws_os.pdf"),
            dest: Some(Destination {
                page: 4,
                kind: DestKind::Fit,
            }),
        };
        let info = describe(&other, &sizes(1), &[], &Outline::new());
        assert_eq!(
            info,
            LinkInfo::File {
                name: "sws_os.pdf".into(),
                page: Some(5)
            }
        );
        assert_eq!(
            info.text(),
            template("link.file_page")
                .replace("{name}", "sws_os.pdf")
                .replace("{page}", "5")
        );
        let no_page = LinkTarget::OtherDocument {
            path: PathBuf::from("/docs/spec/sws_os.pdf"),
            dest: None,
        };
        assert_eq!(
            describe(&no_page, &sizes(1), &[], &Outline::new()).text(),
            "sws_os.pdf"
        );
    }

    // ---- what a click does ----

    const NONE: ClickMods = ClickMods {
        ctrl: false,
        shift: false,
    };
    const CTRL: ClickMods = ClickMods {
        ctrl: true,
        shift: false,
    };
    const CTRL_SHIFT: ClickMods = ClickMods {
        ctrl: true,
        shift: true,
    };

    fn internal(page: u32, top: f32) -> LinkTarget {
        LinkTarget::Internal(dest(page, top))
    }

    #[test]
    fn clicks_on_an_in_document_link_open_where_the_modifiers_say() {
        let pages = sizes(10);
        let to = |how| LinkAction::Navigate {
            to: Location::new(4, 72.0, 300.0),
            how,
        };
        let target = internal(4, 300.0);
        assert_eq!(
            follow(&target, ClickButton::Primary, NONE, &pages),
            to(OpenHow::Here)
        );
        assert_eq!(
            follow(&target, ClickButton::Primary, CTRL, &pages),
            to(OpenHow::NewTab)
        );
        assert_eq!(
            follow(&target, ClickButton::Middle, NONE, &pages),
            to(OpenHow::NewTab)
        );
        assert_eq!(
            follow(&target, ClickButton::Primary, CTRL_SHIFT, &pages),
            to(OpenHow::SplitSide)
        );
        // Shift alone changes nothing.
        let shift = ClickMods {
            ctrl: false,
            shift: true,
        };
        assert_eq!(
            follow(&target, ClickButton::Primary, shift, &pages),
            to(OpenHow::Here)
        );
    }

    #[test]
    fn the_zoom_a_destination_asks_for_is_ignored_and_fit_goes_to_the_top() {
        let pages = sizes(3);
        let fit_r = LinkTarget::Internal(Destination {
            page: 2,
            kind: DestKind::FitR(Rect::new(10.0, 20.0, 300.0, 400.0)),
        });
        assert_eq!(
            follow(&fit_r, ClickButton::Primary, NONE, &pages),
            LinkAction::Navigate {
                to: Location::new(2, 10.0, 20.0),
                how: OpenHow::Here
            }
        );
        let fit = LinkTarget::Internal(Destination {
            page: 1,
            kind: DestKind::Fit,
        });
        assert_eq!(
            follow(&fit, ClickButton::Primary, NONE, &pages),
            LinkAction::Navigate {
                to: Location::new(1, 0.0, 0.0),
                how: OpenHow::Here
            }
        );
    }

    #[test]
    fn only_http_https_and_mailto_are_opened() {
        let pages = sizes(1);
        for url in [
            "http://example.com",
            "https://example.com/x",
            "HTTPS://EXAMPLE.COM",
            "mailto:someone@example.com",
        ] {
            assert!(
                matches!(
                    follow(
                        &LinkTarget::External(url.into()),
                        ClickButton::Primary,
                        NONE,
                        &pages
                    ),
                    LinkAction::OpenUrl(_)
                ),
                "{url}"
            );
        }
        for url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "ftp://example.com",
            "data:text/html,hi",
            "custom-scheme://x",
            "no-scheme",
            "",
        ] {
            assert_eq!(
                follow(
                    &LinkTarget::External(url.into()),
                    ClickButton::Primary,
                    NONE,
                    &pages
                ),
                LinkAction::Blocked(url.into()),
                "{url}"
            );
        }
        // Modifiers do not matter for an address.
        assert!(matches!(
            follow(
                &LinkTarget::External("https://example.com".into()),
                ClickButton::Middle,
                CTRL_SHIFT,
                &pages
            ),
            LinkAction::OpenUrl(_)
        ));
    }

    #[test]
    fn a_link_to_another_document_opens_that_file_at_its_page() {
        let target = LinkTarget::OtherDocument {
            path: PathBuf::from("/docs/other.pdf"),
            dest: Some(Destination {
                page: 6,
                kind: DestKind::Fit,
            }),
        };
        for (button, mods) in [
            (ClickButton::Primary, NONE),
            (ClickButton::Primary, CTRL),
            (ClickButton::Middle, NONE),
        ] {
            assert_eq!(
                follow(&target, button, mods, &sizes(1)),
                LinkAction::OpenFile {
                    path: Path::new("/docs/other.pdf").to_path_buf(),
                    page: Some(6)
                }
            );
        }
        assert_eq!(
            follow(
                &LinkTarget::OtherDocument {
                    path: PathBuf::from("/docs/other.pdf"),
                    dest: None
                },
                ClickButton::Primary,
                NONE,
                &sizes(1)
            ),
            LinkAction::OpenFile {
                path: PathBuf::from("/docs/other.pdf"),
                page: None
            }
        );
    }

    // ---- the arrival mark ----

    #[test]
    fn the_arrival_mark_is_on_at_the_start_and_off_after_eight_tenths_of_a_second() {
        assert_eq!(arrival_strength(10.0, 10.0), Some(1.0));
        let half = arrival_strength(10.0, 10.4).expect("still on");
        assert!((half - 0.5).abs() < 1e-3, "{half}");
        assert!(arrival_strength(10.0, 10.79).is_some());
        assert_eq!(arrival_strength(10.0, 10.8), None);
        assert_eq!(arrival_strength(10.0, 20.0), None);
        // A clock that went back does not make it stronger than full.
        assert_eq!(arrival_strength(10.0, 9.0), Some(1.0));
    }

    #[test]
    fn an_arrival_starts_its_clock_at_the_first_frame_that_asks() {
        let mut a = Arrival::new(Location::new(2, 0.0, 50.0));
        assert_eq!(a.strength(100.0), Some(1.0));
        assert!(a.strength(100.5).is_some());
        assert_eq!(a.strength(100.9), None);
    }

    #[test]
    fn the_arrival_band_is_page_wide_around_the_landing_place() {
        let layout = layout(&sizes(5), ZoomMode::Custom(1.0));
        let scroll = layout.content_pos(Location::new(2, 0.0, 100.0));
        let band = arrival_band(
            &layout,
            Point::new(scroll.x, scroll.y),
            Location::new(2, 0.0, 100.0),
        )
        .expect("band");
        let page = layout.page_rect(2);
        assert!((band.x0 - (page.x0 - scroll.x)).abs() < 0.01);
        assert!((band.width() - page.width()).abs() < 0.01);
        // The place is at the top of the viewport; the band starts a little above it.
        assert!((band.y0 + ARRIVAL_ABOVE).abs() < 0.01, "{band:?}");
        assert!((band.height() - ARRIVAL_HEIGHT).abs() < 0.01);
        // A place in the margin above the page is marked at the top of the page.
        let top = arrival_band(&layout, Point::new(0.0, 0.0), Location::new(2, 0.0, -10.0))
            .expect("band");
        assert!((top.y0 - (page.y0 - ARRIVAL_ABOVE)).abs() < 0.01);
        // A page that does not exist: no band.
        assert_eq!(
            arrival_band(&layout, Point::new(0.0, 0.0), Location::new(9, 0.0, 0.0)),
            None
        );
    }

    // ---- scroll at rest ----

    #[test]
    fn standing_still_for_a_second_is_reported_once() {
        let mut rest = ScrollRest::default();
        let at = Location::new(3, 0.0, 40.0);
        assert!(!rest.note(at, 10.0));
        assert!(!rest.note(at, 10.5));
        assert!(rest.remaining(10.5).is_some());
        assert!(!rest.note(at, 10.99));
        assert!(rest.note(at, 11.0));
        assert!(!rest.note(at, 12.0), "only once");
        assert_eq!(rest.remaining(12.0), None);
    }

    #[test]
    fn moving_restarts_the_wait() {
        let mut rest = ScrollRest::default();
        assert!(!rest.note(Location::new(3, 0.0, 40.0), 10.0));
        assert!(!rest.note(Location::new(3, 0.0, 90.0), 10.9));
        assert!(!rest.note(Location::new(3, 0.0, 90.0), 11.5));
        assert!(rest.note(Location::new(3, 0.0, 90.0), 11.9));
        // Moving again re-arms it.
        assert!(!rest.note(Location::new(4, 0.0, 0.0), 12.0));
        assert!(rest.note(Location::new(4, 0.0, 0.0), 13.0));
    }

    #[test]
    fn the_remaining_time_counts_down() {
        let mut rest = ScrollRest::default();
        assert_eq!(rest.remaining(0.0), None, "nothing noted yet");
        rest.note(Location::new(0, 0.0, 0.0), 5.0);
        let left = rest.remaining(5.25).expect("waiting");
        assert!((left.as_secs_f64() - 0.75).abs() < 1e-6);
        assert_eq!(rest.remaining(7.0), Some(Duration::ZERO));
    }

    #[test]
    fn the_hover_text_reports_changes() {
        let mut s = LinkState::default();
        assert!(!s.set_hover_text(None));
        assert!(s.set_hover_text(Some("a".into())));
        assert!(!s.set_hover_text(Some("a".into())));
        assert!(s.set_hover_text(None));
    }

    /// A measurement on a real document: how many links the first pages of the geotopo manual have
    /// and how many lead inside the document. Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "reads a large reference document; run with --ignored"]
    fn geotopo_links_of_the_first_twenty_pages() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../references/viewers/tdf/benches/geotopo.pdf"
        ));
        assert!(path.exists(), "the reference document is missing");
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let doc = DocumentHandle::open(
            path,
            Arc::new(move || {
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send(());
                }
            }),
        );
        wait_until(&rx, "the document", || doc.info().is_some());
        let info = doc.info().expect("info");
        let count = info.page_sizes.len() as u32;
        let (mut total, mut internal, mut in_range, mut external, mut other) = (0, 0, 0, 0, 0);
        let mut titled = 0;
        for page in 0..20.min(count) {
            wait_until(&rx, "the links", || doc.page_links(page).is_some());
            for link in doc.page_links(page).expect("links").iter() {
                total += 1;
                match &link.target {
                    LinkTarget::Internal(d) => {
                        internal += 1;
                        if d.page < count {
                            in_range += 1;
                        }
                        let text = describe(
                            &link.target,
                            &info.page_sizes,
                            &info.page_labels,
                            &info.outline,
                        );
                        if matches!(text, LinkInfo::Page { title: Some(_), .. }) {
                            titled += 1;
                        }
                    }
                    LinkTarget::External(_) => external += 1,
                    LinkTarget::OtherDocument { .. } => other += 1,
                }
            }
        }
        println!(
            "geotopo.pdf: {count} pages; first 20 pages: {total} links ({internal} internal, \
             {external} external, {other} other document); {in_range} internal links lead to a \
             page inside the document; {titled} of them show a section title"
        );
        assert!(total > 0);
        assert_eq!(internal, in_range, "every internal destination is in range");
    }
}
