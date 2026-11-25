//! The texture cache (`docs/ARCHITECTURE.md` §5.6).
//!
//! Rendered pages arrive from the document workers as RGB bytes (`poll_rendered`); `logic()`
//! uploads them here as textures and the PDF view draws them. The cache owns the textures: core
//! keeps no rendered pixels.
//!
//! The bookkeeping (lookup, nearest scale, eviction) lives in [`Cache`], which is generic over
//! the texture handle so it is tested without egui. [`TextureCache`] is the egui instance.

use std::collections::HashMap;

use refdesk_core::{RenderKey, RenderedPage};

use crate::state::{DocId, DocStore};

/// Default byte budget: 2 GB (SPEC principle 2: memory is used generously).
pub const DEFAULT_BUDGET: usize = 2 * 1024 * 1024 * 1024;

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
    entries: HashMap<(DocId, RenderKey), Entry<H>>,
    bytes: usize,
    budget: usize,
    frame: u64,
}

impl<H> Cache<H> {
    /// An empty cache that keeps at most `budget` bytes (except textures used this frame).
    pub fn new(budget: usize) -> Cache<H> {
        Cache {
            entries: HashMap::new(),
            bytes: 0,
            budget,
            frame: 0,
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

    /// Total bytes held.
    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Number of textures held.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether a texture for exactly this key is held. Does not count as a use.
    pub fn contains(&self, doc: DocId, key: RenderKey) -> bool {
        self.entries.contains_key(&(doc, key))
    }

    /// Stores a texture, replacing one with the same key. It counts as used in this frame.
    pub fn insert_handle(&mut self, doc: DocId, key: RenderKey, handle: H, size: [u32; 2]) {
        let bytes = texture_bytes(size[0], size[1]);
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

    /// The texture for exactly this key, marked as used in this frame.
    pub fn get(&mut self, doc: DocId, key: RenderKey) -> Option<&H> {
        let frame = self.frame;
        self.entries.get_mut(&(doc, key)).map(|e| {
            e.last_used = frame;
            &e.handle
        })
    }

    /// The key of the texture of the same document, page and generation whose scale is closest
    /// to `scale_milli` (by ratio; on a tie the larger, sharper one). Does not count as a use.
    fn nearest_key(
        &self,
        doc: DocId,
        page: u32,
        generation: u32,
        scale_milli: u32,
    ) -> Option<RenderKey> {
        let target = f64::from(scale_milli.max(1)).ln();
        let distance = |k: &RenderKey| (f64::from(k.scale_milli.max(1)).ln() - target).abs();
        self.entries
            .keys()
            .filter(|(d, k)| *d == doc && k.page == page && k.generation == generation)
            .map(|(_, k)| *k)
            .min_by(|a, b| {
                distance(a)
                    .total_cmp(&distance(b))
                    .then(b.scale_milli.cmp(&a.scale_milli))
            })
    }

    /// The texture of the same page and generation whose scale is closest to `scale_milli`,
    /// marked as used in this frame.
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

    /// Evicts least recently used textures until the total is within the budget. Textures used
    /// in the current frame are kept even when that leaves the cache over budget.
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

    /// Drops the textures of documents for which `open` is false.
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
    }

    /// Drops the textures of `doc` that belong to another generation (rule R-7).
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

/// Takes the finished renders of every open document and uploads them; drops the textures of
/// closed documents and of old generations. Called from `logic()` once per frame.
pub fn collect_rendered(ctx: &egui::Context, docs: &DocStore, textures: &mut TextureCache) {
    textures.begin_frame();
    for (id, doc) in docs.iter() {
        for page in doc.poll_rendered() {
            textures.insert(ctx, id, page);
        }
        if let Some(info) = doc.info() {
            textures.retain_generation(id, info.generation);
        }
    }
    textures.retain_docs(|id| docs.get(id).is_some());
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
