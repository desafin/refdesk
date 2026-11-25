//! Document engine: the contract between core and app (`docs/ARCHITECTURE.md` §4.2).
//!
//! Each document has one worker thread that owns the MuPDF document (`worker.rs`); all MuPDF
//! calls live in `backend.rs` (rule R-2). A [`DocumentHandle`] is a cheap clonable handle to that
//! worker. The non-blocking calls only touch shared state under a short lock; the `*_wait` calls
//! block until the worker answers and must not be used on the UI thread (rule R-1).

mod backend;
mod uri;
mod worker;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use worker::{Control, Shared, WaitJob};

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

/// Tuning of a document engine. [`EngineOptions::default`] is what the app uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineOptions {
    /// A pending render request that is not requested again within this time is dropped.
    /// Default 250 ms (`docs/ARCHITECTURE.md` §4.2).
    pub render_expiry: Duration,
}

impl Default for EngineOptions {
    fn default() -> EngineOptions {
        EngineOptions {
            render_expiry: Duration::from_millis(250),
        }
    }
}

/// A cheap, clonable handle to one open document and its worker thread.
///
/// The worker stops when the last clone is dropped.
#[derive(Clone)]
pub struct DocumentHandle {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    shared: Arc<Shared>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.wake();
    }
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
        DocumentHandle::open_with(path, notifier, EngineOptions::default())
    }

    /// [`DocumentHandle::open`] with non-default options (used by tests).
    pub fn open_with(path: &Path, notifier: Notifier, options: EngineOptions) -> DocumentHandle {
        let shared = Arc::new(Shared::new(notifier, options));
        worker::spawn(Arc::clone(&shared), path.to_path_buf());
        DocumentHandle {
            inner: Arc::new(Inner {
                path: path.to_path_buf(),
                shared,
            }),
        }
    }

    fn shared(&self) -> &Shared {
        &self.inner.shared
    }

    /// The path this handle was opened with.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The current life-cycle state.
    pub fn status(&self) -> DocStatus {
        self.shared().lock().status.clone()
    }

    /// Document information. `None` until the status is [`DocStatus::Ready`].
    pub fn info(&self) -> Option<Arc<DocumentInfo>> {
        self.shared().lock().info.clone()
    }

    /// Supplies the password of an encrypted document. When it is accepted the status becomes
    /// [`DocStatus::Ready`]; otherwise it stays [`DocStatus::NeedsPassword`]. The notifier is
    /// called either way.
    pub fn authenticate(&self, password: &str) {
        self.control(Control::Authenticate(password.to_string()));
    }

    /// Reads the file again and increments the generation. Text and links are extracted anew;
    /// pending requests and undelivered results of the old generation are dropped (rule R-7).
    pub fn reload(&self) {
        self.control(Control::Reload);
    }

    fn control(&self, c: Control) {
        self.shared().lock().control.push_back(c);
        self.shared().wake();
    }

    /// Declares that the page is needed now. Call it every frame while that holds; never blocks.
    ///
    /// Requests for a key that is pending are merged (priority and time are refreshed). Keys of
    /// another generation, pages past the end and keys that already failed are ignored.
    pub fn request_render(&self, key: RenderKey, prio: Priority) {
        self.shared()
            .lock()
            .request_render(key, prio, Instant::now());
        self.shared().wake();
    }

    /// Takes the pages rendered since the last call. Never blocks.
    pub fn poll_rendered(&self) -> Vec<RenderedPage> {
        let mut st = self.shared().lock();
        let generation = st.generation;
        let mut out = std::mem::take(&mut st.rendered);
        out.retain(|r| r.key.generation == generation);
        out
    }

    /// The text of a page, or `None` after queuing its extraction. Never blocks.
    pub fn page_text(&self, page: u32) -> Option<Arc<PageText>> {
        let mut st = self.shared().lock();
        if let Some(t) = st.texts.get(&page) {
            return Some(Arc::clone(t));
        }
        st.want_text(page);
        drop(st);
        self.shared().wake();
        None
    }

    /// The links of a page, or `None` after queuing their extraction. Never blocks.
    pub fn page_links(&self, page: u32) -> Option<Arc<[Link]>> {
        let mut st = self.shared().lock();
        if let Some(l) = st.links.get(&page) {
            return Some(Arc::clone(l));
        }
        st.want_links(page);
        drop(st);
        self.shared().wake();
        None
    }

    /// Queues a blocking job and waits for its answer.
    fn wait<T>(
        &self,
        make: impl FnOnce(mpsc::Sender<Result<T, EngineError>>) -> WaitJob,
    ) -> Result<T, EngineError> {
        let (tx, rx) = mpsc::channel();
        {
            let mut st = self.shared().lock();
            if st.worker_gone {
                return Err(EngineError::WorkerGone);
            }
            st.waits.push_back(make(tx));
        }
        self.shared().wake();
        // The worker answers every job; a dropped sender means it stopped.
        rx.recv().unwrap_or(Err(EngineError::WorkerGone))
    }

    /// Checks `page` against the open document, if it is open.
    fn check_page(&self, page: u32) -> Result<(), EngineError> {
        match self.shared().lock().page_count() {
            Some(count) if page >= count => Err(EngineError::PageOutOfRange { page, count }),
            _ => Ok(()),
        }
    }

    /// The text of a page. Blocks; must not be called on the UI thread (rule R-1).
    pub fn page_text_wait(&self, page: u32) -> Result<Arc<PageText>, EngineError> {
        if let Some(t) = self.shared().lock().texts.get(&page) {
            return Ok(Arc::clone(t));
        }
        self.check_page(page)?;
        self.wait(|reply| WaitJob::Text { page, reply })
    }

    /// Renders a page. Blocks; must not be called on the UI thread (rule R-1).
    ///
    /// The result is returned here only, not through [`DocumentHandle::poll_rendered`].
    pub fn render_wait(&self, key: RenderKey) -> Result<RenderedPage, EngineError> {
        self.check_page(key.page)?;
        self.wait(|reply| WaitJob::Render { key, reply })
    }

    /// Renders `region` (page space) of a page at the given scale. Blocks; must not be called on
    /// the UI thread (rule R-1).
    ///
    /// A region that reaches past the page is clipped to it; when nothing is left the call
    /// fails.
    pub fn render_region_wait(
        &self,
        page: u32,
        region: Rect,
        scale_milli: u32,
    ) -> Result<RenderedImage, EngineError> {
        self.check_page(page)?;
        self.wait(|reply| WaitJob::Region {
            page,
            region,
            scale_milli,
            reply,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_fails_and_keeps_its_path() {
        let (tx, rx) = mpsc::channel();
        let path = std::env::temp_dir().join("refdesk-engine-unit-no-such-file.pdf");
        let handle = DocumentHandle::open(
            &path,
            Arc::new(move || {
                let _ = tx.send(());
            }),
        );
        assert_eq!(handle.path(), path.as_path());
        let deadline = Instant::now() + Duration::from_secs(30);
        while handle.status() == DocStatus::Loading {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(rx.recv_timeout(left).is_ok(), "no status change");
        }
        assert!(matches!(handle.status(), DocStatus::Failed(_)));
        assert!(handle.info().is_none());
        assert!(handle.poll_rendered().is_empty());
        assert_eq!(handle.page_text_wait(0), Err(EngineError::NotReady));
    }
}
