//! Document engine: the contract between core and app (`docs/ARCHITECTURE.md` §4.2).
//!
//! This file currently holds the public API skeleton only: a handle can be created, but it stays
//! in [`DocStatus::Loading`] and produces nothing. WP-12 replaces the bodies with the
//! worker-backed implementation. The public signatures are the contract and stay as they are.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::geom::{Rect, Size};
use crate::link::Link;
use crate::outline::Outline;
use crate::text::PageText;

/// Called whenever a new result is available. The app passes a repaint request.
pub type Notifier = Arc<dyn Fn() + Send + Sync>;

/// Where a document is in its life cycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocStatus {
    /// The worker is still opening the file.
    Loading,
    /// The document is open and [`DocumentHandle::info`] returns `Some`.
    Ready,
    /// The file is encrypted; call [`DocumentHandle::authenticate`].
    NeedsPassword,
    /// The file could not be opened. The message is shown to the user.
    Failed(String),
}

/// What is known about a document once it is open.
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentInfo {
    /// The file this document was opened from.
    pub path: PathBuf,
    /// Incremented by every reload. Results of an older generation are discarded (rule R-7).
    pub generation: u32,
    /// Page sizes in points. The length is the page count.
    pub page_sizes: Vec<Size>,
    /// Page labels such as "iv" or "A-1". Empty when the document defines none.
    pub page_labels: Vec<String>,
    /// The document title from its metadata.
    pub title: Option<String>,
    /// The table of contents. Empty when the document has none.
    pub outline: Outline,
}

/// Identifies one rendering of a whole page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RenderKey {
    /// Zero-based page index.
    pub page: u32,
    /// Rendering scale (device pixels per point) times 1000.
    pub scale_milli: u32,
    /// The document generation this key belongs to.
    pub generation: u32,
}

/// A rendered page: opaque RGB on a white background, three bytes per pixel, rows top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedPage {
    /// The request this answers.
    pub key: RenderKey,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 3` bytes.
    pub rgb: Vec<u8>,
}

/// A rendered region of a page, in the same pixel format as [`RenderedPage`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 3` bytes.
    pub rgb: Vec<u8>,
}

/// How urgent a request is. Earlier variants are served first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// A page on screen right now.
    Visible,
    /// A link preview or similar transient view.
    Preview,
    /// Pages next to the visible ones.
    Prefetch,
    /// Whole-document work such as indexing.
    Background,
}

/// Errors reported by the blocking engine calls.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// The document is still loading, needs a password, or failed to open.
    #[error("the document is not ready")]
    NotReady,
    /// The page index is past the end of the document.
    #[error("page {page} is out of range (the document has {count} pages)")]
    PageOutOfRange {
        /// The requested zero-based page index.
        page: u32,
        /// The number of pages in the document.
        count: u32,
    },
    /// The document worker has stopped.
    #[error("the document worker has stopped")]
    WorkerGone,
    /// The engine failed on this request.
    #[error("{0}")]
    Failed(String),
}

/// A cheap, clonable handle to one open document and its worker thread.
#[derive(Clone)]
pub struct DocumentHandle {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    // Read by the worker-backed implementation (WP-12).
    #[allow(dead_code)]
    notifier: Notifier,
}

impl fmt::Debug for DocumentHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentHandle")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

impl DocumentHandle {
    /// Starts opening `path` and returns at once. Opening happens on the document's worker.
    pub fn open(path: &Path, notifier: Notifier) -> DocumentHandle {
        DocumentHandle {
            inner: Arc::new(Inner {
                path: path.to_path_buf(),
                notifier,
            }),
        }
    }

    /// The path this handle was opened with.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The current life-cycle state.
    pub fn status(&self) -> DocStatus {
        DocStatus::Loading
    }

    /// Document information. `None` until the status is [`DocStatus::Ready`].
    pub fn info(&self) -> Option<Arc<DocumentInfo>> {
        None
    }

    /// Supplies the password of an encrypted document.
    pub fn authenticate(&self, _password: &str) {}

    /// Reads the file again and increments the generation.
    pub fn reload(&self) {}

    /// Declares that the page is needed now. Call it every frame while that holds; never blocks.
    pub fn request_render(&self, _key: RenderKey, _prio: Priority) {}

    /// Takes the pages rendered since the last call. Never blocks.
    pub fn poll_rendered(&self) -> Vec<RenderedPage> {
        Vec::new()
    }

    /// The text of a page, or `None` after queuing its extraction. Never blocks.
    pub fn page_text(&self, _page: u32) -> Option<Arc<PageText>> {
        None
    }

    /// The links of a page, or `None` after queuing their extraction. Never blocks.
    pub fn page_links(&self, _page: u32) -> Option<Arc<[Link]>> {
        None
    }

    /// The text of a page. Blocks; must not be called on the UI thread (rule R-1).
    pub fn page_text_wait(&self, _page: u32) -> Result<Arc<PageText>, EngineError> {
        Err(EngineError::NotReady)
    }

    /// Renders a page. Blocks; must not be called on the UI thread (rule R-1).
    pub fn render_wait(&self, _key: RenderKey) -> Result<RenderedPage, EngineError> {
        Err(EngineError::NotReady)
    }

    /// Renders `region` (page space) of a page at the given scale. Blocks; must not be called on
    /// the UI thread (rule R-1).
    pub fn render_region_wait(
        &self,
        _page: u32,
        _region: Rect,
        _scale_milli: u32,
    ) -> Result<RenderedImage, EngineError> {
        Err(EngineError::NotReady)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_handle_keeps_its_path_and_stays_loading() {
        let handle = DocumentHandle::open(Path::new("/tmp/a.pdf"), Arc::new(|| {}));
        assert_eq!(handle.path(), Path::new("/tmp/a.pdf"));
        assert_eq!(handle.status(), DocStatus::Loading);
        assert!(handle.info().is_none());
        assert!(handle.poll_rendered().is_empty());
    }
}
