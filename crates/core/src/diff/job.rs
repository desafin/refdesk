//! The background comparison: one thread per comparison that reads both documents, extracts
//! them and runs [`diff_tokens`].

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::extract::PageSource;
use super::result::{DiffInput, DiffLimits, DiffOptions, DiffResult, diff_tokens};
use crate::engine::{DocStatus, DocumentHandle, DocumentInfo, EngineError, Notifier};
use crate::text::PageText;

/// How often the thread looks at a document that is still opening.
const OPEN_POLL: Duration = Duration::from_millis(10);

/// Share of the progress bar for reading page texts; extraction and comparison take the rest.
const READ_SHARE: f32 = 0.9;
const EXTRACT_SHARE: f32 = 0.05;

/// The notifier is called when the progress has grown by at least this much since the last call.
const NOTIFY_STEP: f32 = 0.01;

/// What a finished comparison hands out.
type Outcome = Result<Arc<DiffResult>, String>;

struct Shared {
    cancel: AtomicBool,
    /// The progress as `f32` bits. Non-negative floats order like their bits, so `fetch_max`
    /// keeps it from going down.
    progress: AtomicU32,
    result: Mutex<Option<Outcome>>,
}

impl Shared {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(Ordering::Relaxed))
    }

    fn finish(&self, outcome: Outcome) {
        self.progress.fetch_max(1.0f32.to_bits(), Ordering::Relaxed);
        *self.result.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
    }
}

/// A running (or finished) comparison of two documents. Dropping it cancels the comparison; the
/// drop does not wait for the thread, so it is safe on the UI thread.
pub struct DiffJob {
    shared: Arc<Shared>,
}

impl Drop for DiffJob {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for DiffJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffJob")
            .field("progress", &self.progress())
            .finish_non_exhaustive()
    }
}

impl DiffJob {
    /// Starts comparing `left` (the older document) with `right` and returns at once.
    ///
    /// The thread waits for documents that are still opening, reads the text of every page of
    /// both, extracts them with `opts` and compares them. `notifier` is called when the progress
    /// has visibly grown and when the result is ready (never after the job was dropped). A
    /// document that cannot be opened ends the job with an error.
    pub fn start(
        left: DocumentHandle,
        right: DocumentHandle,
        opts: DiffOptions,
        notifier: Notifier,
    ) -> DiffJob {
        DiffJob::start_with(left, right, opts, DiffLimits::default(), notifier)
    }

    /// [`DiffJob::start`] with non-default time limits (used by tests).
    pub fn start_with(
        left: DocumentHandle,
        right: DocumentHandle,
        opts: DiffOptions,
        limits: DiffLimits,
        notifier: Notifier,
    ) -> DiffJob {
        let shared = Arc::new(Shared {
            cancel: AtomicBool::new(false),
            progress: AtomicU32::new(0.0f32.to_bits()),
            result: Mutex::new(None),
        });
        let worker = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("refdesk-diff".to_string())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run(&worker, &left, &right, opts, &limits, &notifier)
                }));
                let outcome = match outcome {
                    Ok(Some(o)) => o,
                    Ok(None) => return, // cancelled
                    Err(_) => Err("the comparison failed with an internal error".to_string()),
                };
                worker.finish(outcome);
                if !worker.cancelled() {
                    notifier();
                }
            });
        if let Err(e) = spawned {
            shared.finish(Err(format!("cannot start the comparison thread: {e}")));
        }
        DiffJob { shared }
    }

    /// How far the comparison is, from 0.0 to 1.0. Never goes down; 1.0 once the result is
    /// there.
    pub fn progress(&self) -> f32 {
        self.shared.progress()
    }

    /// The result once the comparison has finished: the differences, or a message saying why
    /// the documents could not be compared.
    pub fn result(&self) -> Option<Result<Arc<DiffResult>, String>> {
        self.shared
            .result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Raises the progress and calls the notifier for visible steps.
struct Reporter<'a> {
    shared: &'a Shared,
    notifier: &'a Notifier,
    notified: f32,
}

impl Reporter<'_> {
    fn set(&mut self, p: f32) {
        let p = p.clamp(0.0, 1.0);
        self.shared
            .progress
            .fetch_max(p.to_bits(), Ordering::Relaxed);
        if p - self.notified >= NOTIFY_STEP && !self.shared.cancelled() {
            self.notified = p;
            (self.notifier)();
        }
    }
}

/// Waits until `doc` is open. `None` when the job was dropped meanwhile.
fn wait_until_open(
    shared: &Shared,
    doc: &DocumentHandle,
) -> Option<Result<Arc<DocumentInfo>, String>> {
    loop {
        if shared.cancelled() {
            return None;
        }
        match doc.status() {
            DocStatus::Loading => std::thread::sleep(OPEN_POLL),
            DocStatus::Ready => {
                return Some(doc.info().ok_or_else(|| {
                    format!(
                        "{}: the document information is missing",
                        doc.path().display()
                    )
                }));
            }
            DocStatus::NeedsPassword => {
                return Some(Err(format!(
                    "{}: the document needs a password",
                    doc.path().display()
                )));
            }
            DocStatus::Failed(msg) => {
                return Some(Err(format!("{}: {msg}", doc.path().display())));
            }
        }
    }
}

/// The whole comparison. `None` when the job was dropped.
fn run(
    shared: &Shared,
    left: &DocumentHandle,
    right: &DocumentHandle,
    opts: DiffOptions,
    limits: &DiffLimits,
    notifier: &Notifier,
) -> Option<Outcome> {
    let linfo = match wait_until_open(shared, left)? {
        Ok(i) => i,
        Err(e) => return Some(Err(e)),
    };
    let rinfo = match wait_until_open(shared, right)? {
        Ok(i) => i,
        Err(e) => return Some(Err(e)),
    };
    let total = (linfo.page_sizes.len() + rinfo.page_sizes.len()).max(1) as f32;
    let mut report = Reporter {
        shared,
        notifier,
        notified: 0.0,
    };

    let mut done = 0usize;
    let mut read =
        |doc: &DocumentHandle, info: &DocumentInfo| -> Option<Result<Vec<PageSource>, String>> {
            let mut pages = Vec::with_capacity(info.page_sizes.len());
            for (p, &size) in info.page_sizes.iter().enumerate() {
                if shared.cancelled() {
                    return None;
                }
                let page = u32::try_from(p).unwrap_or(u32::MAX);
                let text = match doc.page_text_wait(page) {
                    Ok(t) => t,
                    // A page whose text cannot be read counts as a page without text.
                    Err(EngineError::Failed(_) | EngineError::PageOutOfRange { .. }) => {
                        Arc::new(PageText::default())
                    }
                    Err(e) => return Some(Err(format!("{}: {e}", doc.path().display()))),
                };
                pages.push(PageSource { text, size });
                done += 1;
                report.set(READ_SHARE * done as f32 / total);
            }
            Some(Ok(pages))
        };
    let lpages = match read(left, &linfo)? {
        Ok(p) => p,
        Err(e) => return Some(Err(e)),
    };
    let rpages = match read(right, &rinfo)? {
        Ok(p) => p,
        Err(e) => return Some(Err(e)),
    };

    let eopts = opts.extract_options();
    let linput = DiffInput::from_pages(&lpages, &eopts);
    drop(lpages);
    if shared.cancelled() {
        return None;
    }
    report.set(READ_SHARE + EXTRACT_SHARE / 2.0);
    let rinput = DiffInput::from_pages(&rpages, &eopts);
    drop(rpages);
    if shared.cancelled() {
        return None;
    }
    report.set(READ_SHARE + EXTRACT_SHARE);
    let mut result = diff_tokens(&linput, &rinput, limits);
    result.generations = (linfo.generation, rinfo.generation);
    if shared.cancelled() {
        return None;
    }
    Some(Ok(Arc::new(result)))
}
