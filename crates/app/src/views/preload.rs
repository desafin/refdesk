//! Preloading every page at a low scale (VIEW-10, `docs/ARCHITECTURE.md` §5.6).
//!
//! When a document becomes ready, all its pages are rendered in the background at one low
//! "preview" scale and kept as textures for as long as the document is open, so wherever the
//! user scrolls there is something to draw at once. Sharp renders at the view's scale replace
//! them on screen as they arrive.
//!
//! Every frame (`logic()`), for every ready document, the pages still missing a preview are
//! requested at [`Priority::Background`], at most [`PRELOAD_BATCH`] of them, nearest to the page
//! the user looks at first. The engine drops pending requests that are not renewed, so the same
//! batch is requested again each frame until it is done. A page whose render failed counts as
//! done: the engine never retries it, so waiting for it would never end.
//!
//! The decisions (preview scale, which pages to request, progress) are plain functions tested
//! without egui (rule R-10); [`step`] connects them to the documents and the texture cache.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use refdesk_core::{DocStatus, PageColors, Priority, RenderKey, Size};

use crate::state::{AppState, DocId, Tab, TabStore};

const GIB: usize = 1024 * 1024 * 1024;

/// Bytes all documents' previews may take together. A candidate for a setting.
pub const PREVIEW_TOTAL_BUDGET: usize = 3 * GIB;
/// Bytes one document's previews may take at most. A candidate for a setting.
pub const PREVIEW_DOC_BUDGET: usize = GIB;
/// Narrowest preview of the widest page, in pixels: enough to recognize a page's layout.
pub const PREVIEW_MIN_WIDTH: f32 = 240.0;
/// Widest preview of the widest page, in pixels: nearly readable.
pub const PREVIEW_MAX_WIDTH: f32 = 900.0;
/// Most preview renders requested per document per frame.
pub const PRELOAD_BATCH: usize = 48;
/// While a preload is unfinished a frame is run at least this often, so that progress (and
/// failed renders, which do not wake the app) is noticed even when no result arrives.
pub const PRELOAD_POLL: Duration = Duration::from_millis(100);

/// The preview byte budget of one document when `open_docs` documents are open:
/// `min(1 GB, 3 GB / open_docs)`.
pub fn doc_budget(open_docs: usize) -> usize {
    PREVIEW_DOC_BUDGET.min(PREVIEW_TOTAL_BUDGET / open_docs.max(1))
}

/// Pages with a usable size (positive and finite).
fn usable(page_sizes: &[Size]) -> impl Iterator<Item = &Size> {
    page_sizes
        .iter()
        .filter(|s| s.w.is_finite() && s.h.is_finite() && s.w > 0.0 && s.h > 0.0)
}

/// The preview width of the widest page, in pixels, for a byte budget:
/// `clamp(sqrt(budget / (pages × 4 × mean height/width)), 240, 900)`. `None` without pages.
pub fn preview_width(page_sizes: &[Size], budget: usize) -> Option<f32> {
    let (count, aspect_sum) = usable(page_sizes).fold((0usize, 0.0f64), |(n, sum), s| {
        (n + 1, sum + f64::from(s.h) / f64::from(s.w))
    });
    if count == 0 {
        return None;
    }
    let aspect = aspect_sum / count as f64;
    let width = (budget as f64 / (count as f64 * 4.0 * aspect)).sqrt();
    Some((width as f32).clamp(PREVIEW_MIN_WIDTH, PREVIEW_MAX_WIDTH))
}

/// The preview rendering scale of a document, times 1000: [`preview_width`] over the width of
/// the widest page, lowered so no page's long side exceeds `max_texture_side`. `None` for a
/// document without pages.
pub fn preview_scale_milli(
    page_sizes: &[Size],
    open_docs: usize,
    max_texture_side: usize,
) -> Option<u32> {
    let width = preview_width(page_sizes, doc_budget(open_docs))?;
    let widest = usable(page_sizes).map(|s| s.w).fold(0.0f32, f32::max);
    let longest = usable(page_sizes)
        .map(|s| s.w.max(s.h))
        .fold(0.0f32, f32::max);
    let mut scale = width / widest;
    let limit = max_texture_side as f32 / longest;
    if scale > limit {
        scale = limit;
    }
    // Round down so no side grows past the limit.
    Some(((scale * 1000.0).floor() as u32).max(1))
}

/// How far a document's preload is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Pages with a preview (or whose preview cannot be rendered).
    pub done: u32,
    /// Pages in the document.
    pub total: u32,
}

impl Progress {
    /// Every page is done.
    pub fn is_complete(&self) -> bool {
        self.done >= self.total
    }
}

/// Counts the done pages.
pub fn progress(page_count: u32, is_done: impl Fn(u32) -> bool) -> Progress {
    Progress {
        done: (0..page_count).filter(|p| is_done(*p)).count() as u32,
        total: page_count,
    }
}

/// The next pages to request: those not done, nearest to `interest` first (on a tie the later
/// page), at most `max`.
pub fn next_requests(
    page_count: u32,
    interest: u32,
    max: usize,
    is_done: impl Fn(u32) -> bool,
) -> Vec<u32> {
    let mut out = Vec::new();
    if page_count == 0 || max == 0 {
        return out;
    }
    let interest = interest.min(page_count - 1);
    let consider = |p: u32, out: &mut Vec<u32>| {
        if !is_done(p) {
            out.push(p);
        }
        out.len() >= max
    };
    if consider(interest, &mut out) {
        return out;
    }
    for d in 1..page_count {
        let after = interest.checked_add(d).filter(|p| *p < page_count);
        let before = interest.checked_sub(d);
        if after.is_none() && before.is_none() {
            break;
        }
        for p in after.into_iter().chain(before) {
            if consider(p, &mut out) {
                return out;
            }
        }
    }
    out
}

/// The preload plan of one document for this frame: the pages to request and the progress.
/// `None` unless the document is ready (nothing can be rendered before).
pub fn plan_doc(
    status: &DocStatus,
    page_count: Option<u32>,
    interest: u32,
    is_done: impl Fn(u32) -> bool,
) -> Option<(Vec<u32>, Progress)> {
    if *status != DocStatus::Ready {
        return None;
    }
    let count = page_count?;
    let requests = next_requests(count, interest, PRELOAD_BATCH, &is_done);
    Some((requests, progress(count, is_done)))
}

/// The page the preload of a document starts from: the current page of its focused view, else
/// of its first view, else page 0. `views` lists `(focused, current page)` per view.
pub fn interest_page(views: impl IntoIterator<Item = (bool, u32)>) -> u32 {
    let mut first = None;
    for (focused, page) in views {
        if focused {
            return page;
        }
        first.get_or_insert(page);
    }
    first.unwrap_or(0)
}

/// The views of `doc` as `(focused, current page)`, in tab order.
fn views_of(tabs: &TabStore, focus: Option<crate::state::TabId>, doc: DocId) -> Vec<(bool, u32)> {
    let mut views: Vec<_> = tabs
        .iter()
        .filter_map(|(id, tab)| match tab {
            Tab::Pdf(p) if p.doc == doc => Some((
                id,
                focus == Some(id),
                p.ui.current_page.unwrap_or(p.view.anchor.page),
            )),
            _ => None,
        })
        .collect();
    views.sort_by_key(|(id, _, _)| *id);
    views.into_iter().map(|(_, f, p)| (f, p)).collect()
}

/// The preload of one document.
#[derive(Clone, Debug, Default)]
struct DocPreload {
    generation: u32,
    scale_milli: u32,
    /// The page colours the previews are made in; a change starts the preload over (VIEW-7).
    colors: PageColors,
    /// Pages whose preview render failed.
    failed: HashSet<u32>,
    /// The pages requested last frame.
    requested: Vec<u32>,
    progress: Progress,
}

/// The preload state of all documents. Lives in `AppState`.
#[derive(Clone, Debug, Default)]
pub struct Preloader {
    docs: HashMap<DocId, DocPreload>,
}

impl Preloader {
    /// The progress of `doc`'s preload while it is unfinished; `None` once it is finished (or
    /// before it started).
    pub fn progress(&self, doc: DocId) -> Option<Progress> {
        self.docs
            .get(&doc)
            .map(|d| d.progress)
            .filter(|p| !p.is_complete())
    }

    /// The preview scale chosen for `doc`, once its preload started.
    #[cfg(test)]
    pub fn scale_milli(&self, doc: DocId) -> Option<u32> {
        self.docs.get(&doc).map(|d| d.scale_milli)
    }
}

/// Runs the preload for one frame: picks each ready document's preview scale, requests the next
/// batch of missing previews and updates the progress. Called from `logic()` after
/// `textures::collect_rendered`.
pub fn step(ctx: &egui::Context, state: &mut AppState) {
    let AppState {
        docs,
        tabs,
        focus,
        textures,
        preload,
        ..
    } = state;
    let max_side = ctx.input(|i| i.max_texture_side);
    let ready: Vec<_> = docs
        .iter()
        .filter(|(_, d)| d.status() == DocStatus::Ready)
        .filter_map(|(id, d)| d.info().map(|info| (id, d, info)))
        .collect();
    preload
        .docs
        .retain(|id, _| ready.iter().any(|(r, _, _)| r == id));
    let mut busy = false;
    for (id, doc, info) in &ready {
        let (id, generation) = (*id, info.generation);
        let entry = preload.docs.entry(id).or_default();
        let Some(scale_milli) = preview_scale_milli(&info.page_sizes, ready.len(), max_side) else {
            *entry = DocPreload::default();
            continue;
        };
        let colors = textures.page_colors();
        if entry.generation != generation
            || entry.scale_milli != scale_milli
            || entry.colors != colors
        {
            *entry = DocPreload {
                generation,
                scale_milli,
                colors,
                ..DocPreload::default()
            };
        }
        textures.set_preview_scale(id, generation, scale_milli);
        let key = |page| RenderKey {
            page,
            scale_milli,
            generation,
        };
        for page in std::mem::take(&mut entry.requested) {
            if !textures.has_preview(id, key(page)) && doc.render_failed(key(page)) {
                entry.failed.insert(page);
            }
        }
        let pending: HashSet<u32> = textures
            .pending_keys(id)
            .filter(|k| k.scale_milli == scale_milli && k.generation == generation)
            .map(|k| k.page)
            .collect();
        let failed = &entry.failed;
        let is_done = |p: u32| {
            textures.has_preview(id, key(p)) || pending.contains(&p) || failed.contains(&p)
        };
        let interest = interest_page(views_of(tabs, *focus, id));
        let count = info.page_sizes.len() as u32;
        let Some((requests, progress)) =
            plan_doc(&DocStatus::Ready, Some(count), interest, is_done)
        else {
            continue;
        };
        // Within one priority the engine serves the most recent request first: the nearest
        // page is requested last.
        for page in requests.iter().rev() {
            doc.request_render(key(*page), Priority::Background);
        }
        entry.requested = requests;
        entry.progress = progress;
        busy |= !progress.is_complete();
    }
    if busy {
        ctx.request_repaint_after(PRELOAD_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A4: Size = Size { w: 595.0, h: 842.0 };

    fn width_px(sizes: &[Size], scale_milli: u32) -> f32 {
        let widest = sizes.iter().map(|s| s.w).fold(0.0, f32::max);
        widest * scale_milli as f32 / 1000.0
    }

    #[test]
    fn preview_width_follows_the_page_count() {
        let few = vec![A4; 30];
        let many = vec![A4; 660];
        let huge = vec![A4; 5000];
        let s30 = preview_scale_milli(&few, 1, 8192).expect("scale");
        let s660 = preview_scale_milli(&many, 1, 8192).expect("scale");
        let s5000 = preview_scale_milli(&huge, 1, 8192).expect("scale");
        // A few dozen pages: the upper limit.
        assert!((width_px(&few, s30) - PREVIEW_MAX_WIDTH).abs() < 1.0);
        // Hundreds of pages: in between.
        let w660 = width_px(&many, s660);
        assert!(
            w660 > PREVIEW_MIN_WIDTH + 50.0 && w660 < PREVIEW_MAX_WIDTH - 50.0,
            "{w660}"
        );
        // Thousands of pages: the lower limit.
        assert!((width_px(&huge, s5000) - PREVIEW_MIN_WIDTH).abs() < 1.0);
        // 660 A4 pages fit the 1 GB budget.
        let h = A4.h * s660 as f32 / 1000.0;
        assert!((w660 * h * 4.0 * 660.0) as usize <= GIB);
    }

    #[test]
    fn more_open_documents_get_smaller_budgets() {
        assert_eq!(doc_budget(0), GIB);
        assert_eq!(doc_budget(1), GIB);
        assert_eq!(doc_budget(3), GIB);
        assert_eq!(doc_budget(4), 3 * GIB / 4);
        assert_eq!(doc_budget(12), GIB / 4);
        let pages = vec![A4; 660];
        let one = preview_scale_milli(&pages, 1, 8192).expect("scale");
        let six = preview_scale_milli(&pages, 6, 8192).expect("scale");
        let many = preview_scale_milli(&pages, 100, 8192).expect("scale");
        assert!(six < one, "{six} < {one}");
        assert!(many <= six);
        assert!(width_px(&pages, many) >= PREVIEW_MIN_WIDTH - 1.0);
    }

    #[test]
    fn no_page_exceeds_the_texture_limit() {
        // A wide page among portrait ones, and a small texture limit.
        let mut sizes = vec![A4; 10];
        sizes.push(Size::new(3000.0, 400.0));
        let s = preview_scale_milli(&sizes, 1, 512).expect("scale");
        for p in &sizes {
            assert!(p.w.max(p.h) * s as f32 / 1000.0 <= 512.0, "{p:?} at {s}");
        }
        // A very tall page: the widest page alone would allow far more.
        let mut sizes = vec![Size::new(300.0, 400.0); 3];
        sizes.push(Size::new(300.0, 20000.0));
        let s = preview_scale_milli(&sizes, 1, 2048).expect("scale");
        for p in &sizes {
            assert!(p.w.max(p.h) * s as f32 / 1000.0 <= 2048.0, "{p:?} at {s}");
        }
    }

    #[test]
    fn no_pages_means_no_preview() {
        assert_eq!(preview_scale_milli(&[], 1, 2048), None);
        assert_eq!(preview_width(&[], GIB), None);
        assert_eq!(
            preview_scale_milli(&[Size::new(0.0, 0.0)], 1, 2048),
            None,
            "a page without size is not usable"
        );
        assert!(next_requests(0, 5, 48, |_| false).is_empty());
        assert_eq!(progress(0, |_| false), Progress { done: 0, total: 0 });
        assert!(progress(0, |_| false).is_complete());
    }

    #[test]
    fn requests_start_at_the_interest_page_and_spread_out() {
        assert_eq!(
            next_requests(100, 50, 5, |_| false),
            vec![50, 51, 49, 52, 48]
        );
        // At the start and end of the document only one side is left.
        assert_eq!(next_requests(10, 0, 4, |_| false), vec![0, 1, 2, 3]);
        assert_eq!(next_requests(10, 9, 3, |_| false), vec![9, 8, 7]);
        // An interest page past the end counts as the last page.
        assert_eq!(next_requests(10, 99, 2, |_| false), vec![9, 8]);
        // At most the batch size.
        assert_eq!(next_requests(1000, 0, PRELOAD_BATCH, |_| false).len(), 48);
    }

    #[test]
    fn requests_skip_done_and_failed_pages_and_end_empty() {
        let done: HashSet<u32> = [50, 51, 49].into_iter().collect();
        let failed: HashSet<u32> = [52].into_iter().collect();
        let is_done = |p: u32| done.contains(&p) || failed.contains(&p);
        assert_eq!(next_requests(100, 50, 3, is_done), vec![48, 53, 47]);
        let p = progress(100, is_done);
        assert_eq!(
            p,
            Progress {
                done: 4,
                total: 100
            }
        );
        assert!(!p.is_complete());

        // Everything done (some of it failed): nothing more to ask, and complete.
        let failed: HashSet<u32> = [7].into_iter().collect();
        let all = |p: u32| p != 7 || failed.contains(&p);
        assert!(next_requests(10, 3, 48, all).is_empty());
        assert!(progress(10, all).is_complete());
    }

    #[test]
    fn only_ready_documents_are_planned() {
        let none = |_| false;
        assert!(plan_doc(&DocStatus::Loading, None, 0, none).is_none());
        assert!(plan_doc(&DocStatus::NeedsPassword, None, 0, none).is_none());
        assert!(plan_doc(&DocStatus::Failed("x".into()), Some(3), 0, none).is_none());
        let (requests, p) = plan_doc(&DocStatus::Ready, Some(3), 1, none).expect("plan");
        assert_eq!(requests, vec![1, 2, 0]);
        assert_eq!(p, Progress { done: 0, total: 3 });
        let (requests, p) = plan_doc(&DocStatus::Ready, Some(3), 1, |_| true).expect("plan");
        assert!(requests.is_empty());
        assert!(p.is_complete());
    }

    #[test]
    fn the_focused_view_decides_the_interest_page() {
        assert_eq!(interest_page([]), 0);
        assert_eq!(interest_page([(false, 7)]), 7);
        assert_eq!(interest_page([(false, 7), (true, 30), (false, 2)]), 30);
        assert_eq!(interest_page([(false, 7), (false, 2)]), 7);
    }
}

/// The preload driven frame by frame without a window, on real documents.
#[cfg(test)]
mod preload_tests {
    use super::*;
    use std::path::Path;
    use std::time::Instant;

    use crate::action::{Action, OpenTarget, dispatch};
    use crate::views::test_support::{TIMEOUT, watch_repaints};
    use crate::views::textures::collect_rendered;

    /// Opens `path`, runs `logic()`-only frames until every page has its preview, and prints
    /// what it took. Measures the 660-page sample of the reference sources.
    #[test]
    #[ignore = "measurement on a large reference document; run with --ignored"]
    fn measure_preload_of_a_660_page_document() {
        let path = Path::new(
            "/home/oscar/orca/projects/pdfToolCreation/references/viewers/tdf/benches/example_dictionary.pdf",
        );
        assert!(path.exists(), "missing {}", path.display());
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        let target = OpenTarget {
            path: path.to_path_buf(),
            page: None,
        };
        dispatch(&mut state, Action::OpenFiles(vec![target]), &ctx);
        let doc_id = state.focused_pdf().map(|(_, p)| p.doc).expect("tab");
        // The longest time one frame spent uploading and planning, and the number of frames.
        let mut slowest = Duration::ZERO;
        let mut frames = 0u32;
        let mut frame = |state: &mut AppState| {
            ctx.run_ui(egui::RawInput::default(), |ui| {
                let started = Instant::now();
                collect_rendered(ui.ctx(), &state.docs, &mut state.textures);
                step(ui.ctx(), state);
                slowest = slowest.max(started.elapsed());
                frames += 1;
            })
            .drop_without_applying_deltas();
        };
        let deadline = Instant::now() + TIMEOUT * 10;
        let mut ready_at = None;
        loop {
            frame(&mut state);
            let doc = state.docs.get(doc_id).expect("doc");
            if ready_at.is_none() && doc.info().is_some() {
                ready_at = Some(Instant::now());
            }
            let count = doc.info().map_or(0, |i| i.page_sizes.len());
            if count > 0 && state.textures.preview_count(doc_id) == count {
                break;
            }
            repaints.wait_one("previews", deadline);
        }
        let took = ready_at.map(|t| t.elapsed()).unwrap_or_default();
        let info = state.docs.get(doc_id).and_then(|d| d.info()).expect("info");
        let scale = state.preload.scale_milli(doc_id).unwrap_or(0);
        let widest = info.page_sizes.iter().map(|s| s.w).fold(0.0, f32::max);
        let mean_aspect =
            info.page_sizes.iter().map(|s| s.h / s.w).sum::<f32>() / info.page_sizes.len() as f32;
        println!(
            "pages {}, widest {widest:.1} pt, mean h/w {mean_aspect:.3}, preview scale {scale}, \
             width {:.0} px, all previews in {took:?}, preview bytes {} ({:.1} MiB), \
             {frames} frames, slowest frame {slowest:?}",
            info.page_sizes.len(),
            widest * scale as f32 / 1000.0,
            state.textures.preview_bytes(),
            state.textures.preview_bytes() as f64 / (1024.0 * 1024.0),
        );
    }
}

/// The preload starts over in new page colours (VIEW-7), on a real document without a window.
#[cfg(test)]
mod page_colors_tests {
    use super::*;

    use crate::action::{Action, OpenTarget, dispatch};
    use crate::views::test_support::watch_repaints;
    use crate::views::textures::collect_rendered;
    use refdesk_core::testutil::PdfBuilder;

    #[test]
    fn the_preload_runs_again_in_new_page_colours() {
        let pdf = PdfBuilder::new()
            .page(300.0, 400.0, |p| {
                p.text(20.0, 50.0, 20.0, "ONE");
            })
            .page(300.0, 400.0, |p| {
                p.text(20.0, 50.0, 20.0, "TWO");
            })
            .page(300.0, 400.0, |_| {})
            .write_temp("preload-colors");
        let ctx = egui::Context::default();
        let repaints = watch_repaints(&ctx);
        let mut state = AppState::new();
        let target = OpenTarget {
            path: pdf.path().to_path_buf(),
            page: None,
        };
        dispatch(&mut state, Action::OpenFiles(vec![target]), &ctx);
        let doc_id = state.focused_pdf().map(|(_, p)| p.doc).expect("tab");
        let frame = |state: &mut AppState| {
            ctx.run_ui(egui::RawInput::default(), |ui| {
                collect_rendered(ui.ctx(), &state.docs, &mut state.textures);
                step(ui.ctx(), state);
            })
            .drop_without_applying_deltas();
        };
        let all_previews = |state: &AppState| {
            let Some(scale) = state.preload.scale_milli(doc_id) else {
                return false;
            };
            (0..3).all(|page| {
                state.textures.has_preview(
                    doc_id,
                    RenderKey {
                        page,
                        scale_milli: scale,
                        generation: 0,
                    },
                )
            })
        };
        repaints.wait_until("original previews", || {
            frame(&mut state);
            all_previews(&state) && state.preload.progress(doc_id).is_none()
        });
        let doc = state.docs.get(doc_id).expect("doc").clone();
        assert_eq!(doc.page_colors(), PageColors::Original);

        state.textures.set_page_colors(PageColors::Dark);
        assert!(
            !all_previews(&state),
            "the old previews are not the new colours"
        );
        assert_eq!(state.textures.preview_count(doc_id), 3, "but still drawn");
        frame(&mut state);
        assert_eq!(doc.page_colors(), PageColors::Dark, "requests go out dark");
        repaints.wait_until("dark previews", || {
            frame(&mut state);
            all_previews(&state) && state.preload.progress(doc_id).is_none()
        });
        assert_eq!(state.textures.preview_count(doc_id), 3);
    }
}
