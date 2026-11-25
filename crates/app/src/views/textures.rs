//! The texture cache (`docs/ARCHITECTURE.md` §5.6).
//!
//! Rendered pages arrive from the document workers as RGB bytes (`poll_rendered`); `logic()`
//! queues them here and uploads them as textures, a bounded amount per frame; the PDF view draws
//! them. The cache owns the textures: core keeps no rendered pixels.
//!
//! Two layers (VIEW-10):
//! - **Previews**: every page of a document at one low scale per document (chosen by
//!   `views::preload`). Kept while the document is open, never evicted, counted apart from the
//!   budget. A render whose scale is the document's preview scale goes here.
//! - **Sharp**: everything else, under a byte budget with least-recently-used eviction.
//!
//! The bookkeeping (lookup, nearest scale, eviction) lives in [`Cache`], which is generic over
//! the texture handle so it is tested without egui. [`TextureCache`] is the egui instance.

use std::collections::{HashMap, VecDeque};

use refdesk_core::{RenderKey, RenderedPage};

use crate::state::{DocId, DocStore};

/// Default byte budget of the sharp layer: 2 GB (SPEC principle 2: memory is used generously).
pub const DEFAULT_BUDGET: usize = 2 * 1024 * 1024 * 1024;

/// Most texture bytes uploaded in one frame; the rest waits for the next frame so a burst of
/// finished renders does not stall scrolling. At least one page is uploaded per frame.
/// Converting and uploading measured about 1 ms per 1.6 MB page on the target machine, so this
/// keeps a frame's upload work near 10 ms while a 1 GB preload still takes about a second.
pub const UPLOAD_BYTES_PER_FRAME: usize = 16 * 1024 * 1024;

/// Bytes a texture of this size occupies. egui keeps textures as RGBA.
pub fn texture_bytes(width: u32, height: u32) -> usize {
    width as usize * height as usize * 4
}

/// One cached texture.
#[derive(Clone, Debug)]
pub struct Entry<H> {
    /// The texture.
    pub handle: H,
    /// Bytes it occupies.
    pub bytes: usize,
    /// The frame it was last inserted or looked up in.
    pub last_used: u64,
}

/// One page's preview texture.
#[derive(Clone, Debug)]
struct Preview<H> {
    key: RenderKey,
    handle: H,
    bytes: usize,
}

/// A texture chosen for drawing a page.
#[derive(Debug, PartialEq, Eq)]
pub enum Pick<'a, H> {
    /// Rendered at exactly the requested scale.
    Exact(&'a H),
    /// Rendered at another scale; it must be stretched to the page rectangle.
    Stretched(&'a H),
    /// Nothing for this page: draw a white page.
    Blank,
}

/// Texture bookkeeping, generic over the texture handle `H`.
#[derive(Debug)]
pub struct Cache<H> {
    /// The sharp layer.
    entries: HashMap<(DocId, RenderKey), Entry<H>>,
    /// Bytes of the sharp layer.
    bytes: usize,
    budget: usize,
    frame: u64,
    /// The preview layer: one texture per page.
    previews: HashMap<(DocId, u32), Preview<H>>,
    /// Bytes of the preview layer.
    preview_bytes: usize,
    /// Each document's preview scale: (generation, scale_milli).
    preview_scales: HashMap<DocId, (u32, u32)>,
    /// Rendered pages waiting to be uploaded.
    pending: VecDeque<(DocId, RenderedPage)>,
}

impl<H> Cache<H> {
    /// An empty cache whose sharp layer keeps at most `budget` bytes (except textures used this
    /// frame).
    pub fn new(budget: usize) -> Cache<H> {
        Cache {
            entries: HashMap::new(),
            bytes: 0,
            budget,
            frame: 0,
            previews: HashMap::new(),
            preview_bytes: 0,
            preview_scales: HashMap::new(),
            pending: VecDeque::new(),
        }
    }

    /// Ends the previous frame and starts a new one.
    ///
    /// First evicts down to the budget, sparing what the previous frame drew (or inserted);
    /// then advances the frame counter. Call once per frame before inserting new textures.
    pub fn begin_frame(&mut self) {
        self.trim();
        self.frame += 1;
    }

    /// The current frame number.
    #[cfg(test)]
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Total bytes held by the sharp layer (previews are not counted).
    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Total bytes held by the preview layer.
    #[cfg(test)]
    pub fn preview_bytes(&self) -> usize {
        self.preview_bytes
    }

    /// Number of textures held by the sharp layer.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Number of preview textures held for `doc`.
    #[cfg(test)]
    pub fn preview_count(&self, doc: DocId) -> usize {
        self.previews.keys().filter(|(d, _)| *d == doc).count()
    }

    /// Sets the scale at which `doc`'s previews are rendered. Renders of that key arriving
    /// later go to the preview layer. Previews at an older scale stay until the new one of the
    /// same page replaces them, so changing the scale never leaves a page without a texture.
    pub fn set_preview_scale(&mut self, doc: DocId, generation: u32, scale_milli: u32) {
        self.preview_scales.insert(doc, (generation, scale_milli));
    }

    /// Whether `key` is the preview key of its page for `doc`.
    fn is_preview_key(&self, doc: DocId, key: RenderKey) -> bool {
        self.preview_scales.get(&doc) == Some(&(key.generation, key.scale_milli))
    }

    /// Whether the preview texture of this exact key is held.
    pub fn has_preview(&self, doc: DocId, key: RenderKey) -> bool {
        self.previews
            .get(&(doc, key.page))
            .is_some_and(|p| p.key == key)
    }

    /// Whether a texture for exactly this key is held, in either layer. Does not count as a use.
    pub fn contains(&self, doc: DocId, key: RenderKey) -> bool {
        self.entries.contains_key(&(doc, key)) || self.has_preview(doc, key)
    }

    /// Stores a texture, replacing one with the same key. A key at the document's preview scale
    /// goes to the preview layer (replacing that page's preview); anything else to the sharp
    /// layer, where it counts as used in this frame.
    pub fn insert_handle(&mut self, doc: DocId, key: RenderKey, handle: H, size: [u32; 2]) {
        let bytes = texture_bytes(size[0], size[1]);
        if self.is_preview_key(doc, key) {
            let preview = Preview { key, handle, bytes };
            if let Some(old) = self.previews.insert((doc, key.page), preview) {
                self.preview_bytes -= old.bytes;
            }
            self.preview_bytes += bytes;
            return;
        }
        let entry = Entry {
            handle,
            bytes,
            last_used: self.frame,
        };
        if let Some(old) = self.entries.insert((doc, key), entry) {
            self.bytes -= old.bytes;
        }
        self.bytes += bytes;
    }

    /// The texture for exactly this key, from either layer. A sharp one is marked as used in
    /// this frame.
    pub fn get(&mut self, doc: DocId, key: RenderKey) -> Option<&H> {
        let frame = self.frame;
        if let Some(e) = self.entries.get_mut(&(doc, key)) {
            e.last_used = frame;
            return Some(&e.handle);
        }
        self.previews
            .get(&(doc, key.page))
            .filter(|p| p.key == key)
            .map(|p| &p.handle)
    }

    /// The key of the texture of the same document, page and generation whose scale is closest
    /// to `scale_milli` (by ratio; on a tie the larger, sharper one), in either layer. Does not
    /// count as a use.
    fn nearest_key(
        &self,
        doc: DocId,
        page: u32,
        generation: u32,
        scale_milli: u32,
    ) -> Option<RenderKey> {
        let target = f64::from(scale_milli.max(1)).ln();
        let distance = |k: &RenderKey| (f64::from(k.scale_milli.max(1)).ln() - target).abs();
        let preview = self
            .previews
            .get(&(doc, page))
            .map(|p| p.key)
            .filter(|k| k.generation == generation);
        self.entries
            .keys()
            .filter(|(d, k)| *d == doc && k.page == page && k.generation == generation)
            .map(|(_, k)| *k)
            .chain(preview)
            .min_by(|a, b| {
                distance(a)
                    .total_cmp(&distance(b))
                    .then(b.scale_milli.cmp(&a.scale_milli))
            })
    }

    /// The texture of the same page and generation whose scale is closest to `scale_milli`,
    /// marked as used in this frame. With no sharp texture of the page, its preview.
    pub fn nearest(
        &mut self,
        doc: DocId,
        page: u32,
        generation: u32,
        scale_milli: u32,
    ) -> Option<&H> {
        let key = self.nearest_key(doc, page, generation, scale_milli)?;
        self.get(doc, key)
    }

    /// What to draw for a page wanted at `key`: the exact texture, else the nearest scale of the
    /// same page and generation, else nothing. The chosen texture is marked as used.
    pub fn pick(&mut self, doc: DocId, key: RenderKey) -> Pick<'_, H> {
        if self.contains(doc, key) {
            return self.get(doc, key).map_or(Pick::Blank, Pick::Exact);
        }
        self.nearest(doc, key.page, key.generation, key.scale_milli)
            .map_or(Pick::Blank, Pick::Stretched)
    }

    /// Evicts least recently used sharp textures until their total is within the budget.
    /// Textures used in the current frame are kept even when that leaves the cache over budget.
    /// Previews are never evicted.
    pub fn trim(&mut self) {
        if self.bytes <= self.budget {
            return;
        }
        let mut candidates: Vec<((DocId, RenderKey), u64)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.last_used < self.frame)
            .map(|(k, e)| (*k, e.last_used))
            .collect();
        candidates.sort_by_key(|(_, used)| *used);
        for (key, _) in candidates {
            if self.bytes <= self.budget {
                break;
            }
            self.remove(key);
        }
    }

    fn remove(&mut self, key: (DocId, RenderKey)) {
        if let Some(e) = self.entries.remove(&key) {
            self.bytes -= e.bytes;
        }
    }

    fn remove_previews(&mut self, gone: impl Fn(DocId, RenderKey) -> bool) {
        let keys: Vec<(DocId, u32)> = self
            .previews
            .iter()
            .filter(|((d, _), p)| gone(*d, p.key))
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            if let Some(p) = self.previews.remove(&k) {
                self.preview_bytes -= p.bytes;
            }
        }
    }

    /// Drops the textures (both layers) and pending uploads of documents for which `open` is
    /// false.
    pub fn retain_docs(&mut self, open: impl Fn(DocId) -> bool) {
        let gone: Vec<_> = self
            .entries
            .keys()
            .filter(|(d, _)| !open(*d))
            .copied()
            .collect();
        for key in gone {
            self.remove(key);
        }
        self.remove_previews(|d, _| !open(d));
        self.preview_scales.retain(|d, _| open(*d));
        self.pending.retain(|(d, _)| open(*d));
    }

    /// Drops the textures (both layers) and pending uploads of `doc` that belong to another
    /// generation (rule R-7).
    pub fn retain_generation(&mut self, doc: DocId, generation: u32) {
        let old: Vec<_> = self
            .entries
            .keys()
            .filter(|(d, k)| *d == doc && k.generation != generation)
            .copied()
            .collect();
        for key in old {
            self.remove(key);
        }
        self.remove_previews(|d, k| d == doc && k.generation != generation);
        if self
            .preview_scales
            .get(&doc)
            .is_some_and(|(g, _)| *g != generation)
        {
            self.preview_scales.remove(&doc);
        }
        self.pending
            .retain(|(d, p)| *d != doc || p.key.generation == generation);
    }

    /// Queues a rendered page for upload.
    pub fn queue(&mut self, doc: DocId, page: RenderedPage) {
        self.pending.push_back((doc, page));
    }

    /// The keys of `doc`'s rendered pages that wait for upload.
    pub fn pending_keys(&self, doc: DocId) -> impl Iterator<Item = RenderKey> + '_ {
        self.pending
            .iter()
            .filter(move |(d, _)| *d == doc)
            .map(|(_, p)| p.key)
    }

    /// Takes the pending pages to upload this frame: sharp ones before previews (what the
    /// views wait for comes first), in arrival order, until `max_bytes` of texture is reached
    /// (always at least one page). The rest stays queued.
    pub fn take_uploads(&mut self, max_bytes: usize) -> Vec<(DocId, RenderedPage)> {
        let (mut sharp, previews): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|(d, p)| !self.is_preview_key(*d, p.key));
        sharp.extend(previews);
        let mut out = Vec::new();
        let mut bytes = 0;
        while let Some((doc, page)) = sharp.pop_front() {
            if !out.is_empty() && bytes >= max_bytes {
                sharp.push_front((doc, page));
                break;
            }
            bytes += texture_bytes(page.width, page.height);
            out.push((doc, page));
        }
        self.pending = sharp;
        out
    }

    /// Whether rendered pages wait for upload.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

impl<H> Default for Cache<H> {
    fn default() -> Self {
        Cache::new(DEFAULT_BUDGET)
    }
}

/// The application's texture cache.
pub type TextureCache = Cache<egui::TextureHandle>;

impl Cache<egui::TextureHandle> {
    /// Uploads a rendered page as a texture.
    pub fn insert(&mut self, ctx: &egui::Context, doc: DocId, page: RenderedPage) {
        let (w, h) = (page.width, page.height);
        if page.rgb.len() != texture_bytes(w, h) / 4 * 3 {
            log::warn!(
                "dropping a rendered page with {} bytes for {w}x{h} pixels",
                page.rgb.len()
            );
            return;
        }
        let image = egui::ColorImage::from_rgb([w as usize, h as usize], &page.rgb);
        let name = format!(
            "doc{doc:?}-p{}-s{}-g{}",
            page.key.page, page.key.scale_milli, page.key.generation
        );
        let handle = ctx.load_texture(name, image, egui::TextureOptions::LINEAR);
        self.insert_handle(doc, page.key, handle, [w, h]);
    }
}

/// Takes the finished renders of every open document, drops the textures of closed documents
/// and of old generations, and uploads up to [`UPLOAD_BYTES_PER_FRAME`] of the queue. When more
/// is queued, asks for another frame. Called from `logic()` once per frame.
pub fn collect_rendered(ctx: &egui::Context, docs: &DocStore, textures: &mut TextureCache) {
    textures.begin_frame();
    for (id, doc) in docs.iter() {
        if let Some(info) = doc.info() {
            textures.retain_generation(id, info.generation);
        }
        for page in doc.poll_rendered() {
            textures.queue(id, page);
        }
    }
    textures.retain_docs(|id| docs.get(id).is_some());
    for (doc, page) in textures.take_uploads(UPLOAD_BYTES_PER_FRAME) {
        textures.insert(ctx, doc, page);
    }
    if textures.has_pending() {
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(page: u32, scale_milli: u32, generation: u32) -> RenderKey {
        RenderKey {
            page,
            scale_milli,
            generation,
        }
    }

    /// A texture of `n` bytes (1 x n/4 pixels).
    fn size_for(bytes: usize) -> [u32; 2] {
        [1, (bytes / 4) as u32]
    }

    fn docs() -> (DocId, DocId) {
        (DocId::for_test(1), DocId::for_test(2))
    }

    #[test]
    fn insert_then_get_exact() {
        let (a, b) = docs();
        let mut c: Cache<&str> = Cache::new(1000);
        c.insert_handle(a, key(0, 1000, 0), "a0", [2, 3]);
        assert_eq!(c.get(a, key(0, 1000, 0)), Some(&"a0"));
        assert_eq!(c.get(a, key(0, 1001, 0)), None);
        assert_eq!(c.get(b, key(0, 1000, 0)), None);
        assert_eq!(c.bytes(), 24);
        // Replacing a key does not count its bytes twice.
        c.insert_handle(a, key(0, 1000, 0), "a0'", [2, 3]);
        assert_eq!(c.bytes(), 24);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn nearest_scale_is_chosen_by_ratio_within_page_and_generation() {
        let (a, b) = docs();
        let mut c: Cache<&str> = Cache::new(usize::MAX);
        c.insert_handle(a, key(3, 500, 0), "half", [1, 1]);
        c.insert_handle(a, key(3, 2000, 0), "double", [1, 1]);
        c.insert_handle(a, key(3, 1300, 1), "other generation", [1, 1]);
        c.insert_handle(a, key(4, 1300, 0), "other page", [1, 1]);
        c.insert_handle(b, key(3, 1300, 0), "other doc", [1, 1]);

        assert_eq!(c.nearest(a, 3, 0, 1300), Some(&"double"));
        assert_eq!(c.nearest(a, 3, 0, 700), Some(&"half"));
        // Equal ratio (x2 and /2): the sharper one wins.
        assert_eq!(c.nearest(a, 3, 0, 1000), Some(&"double"));
        assert_eq!(c.nearest(a, 3, 1, 9999), Some(&"other generation"));
        assert_eq!(c.nearest(a, 3, 2, 1000), None);
        assert_eq!(c.nearest(a, 5, 0, 1000), None);
    }

    #[test]
    fn pick_prefers_exact_then_nearest_then_blank() {
        let (a, _) = docs();
        let mut c: Cache<&str> = Cache::new(usize::MAX);
        assert_eq!(c.pick(a, key(0, 1000, 0)), Pick::Blank);
        c.insert_handle(a, key(0, 1500, 0), "near", [1, 1]);
        assert_eq!(c.pick(a, key(0, 1000, 0)), Pick::Stretched(&"near"));
        c.insert_handle(a, key(0, 1000, 0), "exact", [1, 1]);
        assert_eq!(c.pick(a, key(0, 1000, 0)), Pick::Exact(&"exact"));
        // Another generation is never used.
        assert_eq!(c.pick(a, key(0, 1000, 1)), Pick::Blank);
    }

    #[test]
    fn over_budget_evicts_least_recently_used_first() {
        let (a, _) = docs();
        let mut c: Cache<u32> = Cache::new(300);
        for page in 0..3 {
            c.insert_handle(a, key(page, 1000, 0), page, size_for(100));
            c.begin_frame();
        }
        // Page 0 is used again: page 1 becomes the oldest.
        assert!(c.get(a, key(0, 1000, 0)).is_some());
        c.begin_frame();
        c.insert_handle(a, key(3, 1000, 0), 3, size_for(100));
        c.begin_frame();
        assert_eq!(c.bytes(), 300);
        assert!(!c.contains(a, key(1, 1000, 0)), "the oldest went first");
        for page in [0, 2, 3] {
            assert!(c.contains(a, key(page, 1000, 0)), "page {page} stays");
        }
    }

    #[test]
    fn textures_used_this_frame_are_never_evicted() {
        let (a, _) = docs();
        let mut c: Cache<u32> = Cache::new(100);
        c.insert_handle(a, key(0, 1000, 0), 0, size_for(100));
        c.begin_frame();
        c.insert_handle(a, key(1, 1000, 0), 1, size_for(100));
        c.insert_handle(a, key(2, 1000, 0), 2, size_for(100));
        assert!(c.get(a, key(0, 1000, 0)).is_some());
        // All three were used in this frame: nothing can go, the cache stays over budget.
        c.trim();
        assert_eq!(c.len(), 3);
        // Next frame only page 2 is drawn: the others go.
        c.begin_frame();
        assert!(c.get(a, key(2, 1000, 0)).is_some());
        c.trim();
        assert_eq!(c.len(), 1);
        assert!(c.contains(a, key(2, 1000, 0)));
        assert_eq!(c.bytes(), 100);
    }

    #[test]
    fn closed_documents_and_old_generations_are_dropped() {
        let (a, b) = docs();
        let mut c: Cache<u32> = Cache::new(usize::MAX);
        c.insert_handle(a, key(0, 1000, 0), 0, size_for(40));
        c.insert_handle(a, key(0, 1000, 1), 1, size_for(40));
        c.insert_handle(b, key(0, 1000, 0), 2, size_for(40));

        c.retain_generation(a, 1);
        assert!(!c.contains(a, key(0, 1000, 0)));
        assert!(c.contains(a, key(0, 1000, 1)));
        assert!(
            c.contains(b, key(0, 1000, 0)),
            "other documents keep theirs"
        );

        c.retain_docs(|d| d == b);
        assert_eq!(c.len(), 1);
        assert!(c.contains(b, key(0, 1000, 0)));
        assert_eq!(c.bytes(), 40);
    }

    /// A rendered page of `w` x `h` white pixels.
    fn rendered(key: RenderKey, w: u32, h: u32) -> RenderedPage {
        RenderedPage {
            key,
            width: w,
            height: h,
            rgb: vec![255; (w * h * 3) as usize],
        }
    }

    #[test]
    fn previews_survive_budget_pressure_and_are_not_counted_in_it() {
        let (a, _) = docs();
        let mut c: Cache<u32> = Cache::new(100);
        c.set_preview_scale(a, 0, 300);
        for page in 0..5 {
            c.insert_handle(a, key(page, 300, 0), page, size_for(400));
        }
        assert_eq!(c.preview_count(a), 5);
        assert_eq!(c.preview_bytes(), 2000);
        assert_eq!(c.bytes(), 0, "previews are outside the sharp budget");
        // Sharp textures far over the budget: only sharp ones are evicted.
        for page in 0..5 {
            c.insert_handle(a, key(page, 1000, 0), 10 + page, size_for(100));
        }
        assert_eq!(c.bytes(), 500);
        for _ in 0..3 {
            c.begin_frame();
        }
        assert!(c.bytes() <= 100);
        assert_eq!(c.preview_count(a), 5);
        assert_eq!(c.preview_bytes(), 2000);
        for page in 0..5 {
            assert!(c.has_preview(a, key(page, 300, 0)));
            assert!(c.contains(a, key(page, 300, 0)));
        }
    }

    #[test]
    fn a_new_preview_scale_replaces_previews_page_by_page() {
        let (a, _) = docs();
        let mut c: Cache<&str> = Cache::new(usize::MAX);
        c.set_preview_scale(a, 0, 300);
        c.insert_handle(a, key(0, 300, 0), "old0", size_for(40));
        c.insert_handle(a, key(1, 300, 0), "old1", size_for(40));
        c.set_preview_scale(a, 0, 200);
        // The old previews still draw until replaced.
        assert_eq!(c.pick(a, key(0, 1000, 0)), Pick::Stretched(&"old0"));
        assert!(!c.has_preview(a, key(0, 200, 0)));
        c.insert_handle(a, key(0, 200, 0), "new0", size_for(20));
        assert_eq!(c.preview_count(a), 2);
        assert_eq!(c.preview_bytes(), 60);
        assert_eq!(c.pick(a, key(0, 1000, 0)), Pick::Stretched(&"new0"));
        assert_eq!(c.bytes(), 0);
    }

    #[test]
    fn previews_go_with_their_document_and_generation() {
        let (a, b) = docs();
        let mut c: Cache<u32> = Cache::new(usize::MAX);
        c.set_preview_scale(a, 0, 300);
        c.set_preview_scale(b, 0, 300);
        c.insert_handle(a, key(0, 300, 0), 0, size_for(40));
        c.insert_handle(b, key(0, 300, 0), 1, size_for(40));
        c.queue(a, rendered(key(1, 300, 0), 1, 1));

        // A reload of `a`: its previews, its preview scale and its queued pages go.
        c.retain_generation(a, 1);
        assert_eq!(c.preview_count(a), 0);
        assert_eq!(c.pending_keys(a).count(), 0);
        assert_eq!(c.preview_count(b), 1);
        // Without a preview scale for generation 1 nothing counts as a preview yet.
        c.insert_handle(a, key(0, 300, 1), 2, size_for(40));
        assert_eq!(c.preview_count(a), 0);
        assert_eq!(c.bytes(), 40);

        // Closing `b`.
        c.queue(b, rendered(key(1, 300, 0), 1, 1));
        c.retain_docs(|d| d == a);
        assert_eq!(c.preview_count(b), 0);
        assert_eq!(c.preview_bytes(), 0);
        assert!(!c.has_pending());
    }

    #[test]
    fn nearest_falls_back_to_the_preview() {
        let (a, _) = docs();
        let mut c: Cache<&str> = Cache::new(usize::MAX);
        c.set_preview_scale(a, 0, 300);
        c.insert_handle(a, key(4, 300, 0), "preview", [1, 1]);
        assert_eq!(c.nearest(a, 4, 0, 2000), Some(&"preview"));
        assert_eq!(c.pick(a, key(4, 2000, 0)), Pick::Stretched(&"preview"));
        assert_eq!(c.pick(a, key(4, 300, 0)), Pick::Exact(&"preview"));
        assert_eq!(c.nearest(a, 4, 1, 2000), None, "other generation");
        // A sharp texture closer to the wanted scale wins over the preview.
        c.insert_handle(a, key(4, 1500, 0), "sharp", [1, 1]);
        assert_eq!(c.nearest(a, 4, 0, 2000), Some(&"sharp"));
        // Once it is evicted the preview is used again.
        c.remove((a, key(4, 1500, 0)));
        assert_eq!(c.nearest(a, 4, 0, 2000), Some(&"preview"));
    }

    #[test]
    fn uploads_are_limited_per_frame_and_sharp_pages_go_first() {
        let (a, _) = docs();
        let mut c: Cache<u32> = Cache::new(usize::MAX);
        c.set_preview_scale(a, 0, 300);
        for page in 0..4 {
            c.queue(a, rendered(key(page, 300, 0), 10, 10)); // 400 bytes each
        }
        c.queue(a, rendered(key(9, 1000, 0), 10, 10));
        let first = c.take_uploads(800);
        let keys: Vec<RenderKey> = first.iter().map(|(_, p)| p.key).collect();
        assert_eq!(keys, vec![key(9, 1000, 0), key(0, 300, 0)]);
        assert_eq!(c.pending_keys(a).count(), 3);
        // A page larger than the limit still goes, alone.
        assert_eq!(c.take_uploads(1).len(), 1);
        assert_eq!(c.take_uploads(usize::MAX).len(), 2);
        assert!(!c.has_pending());
    }

    #[test]
    fn begin_frame_advances_the_frame() {
        let mut c: Cache<u32> = Cache::new(0);
        let f = c.frame();
        c.begin_frame();
        assert_eq!(c.frame(), f + 1);
    }

    #[test]
    fn uploading_a_rendered_page_creates_a_texture() {
        let ctx = egui::Context::default();
        let (a, _) = docs();
        let mut c = TextureCache::new(usize::MAX);
        let k = key(0, 1000, 0);
        c.insert(
            &ctx,
            a,
            RenderedPage {
                key: k,
                width: 2,
                height: 2,
                rgb: vec![255; 12],
            },
        );
        assert!(c.contains(a, k));
        assert_eq!(c.get(a, k).map(|t| t.size()), Some([2, 2]));
        // A malformed page is dropped, not a panic.
        c.insert(
            &ctx,
            a,
            RenderedPage {
                key: key(1, 1000, 0),
                width: 2,
                height: 2,
                rgb: vec![255; 5],
            },
        );
        assert!(!c.contains(a, key(1, 1000, 0)));
    }
}
