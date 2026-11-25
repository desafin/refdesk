//! The background search: one thread per search that walks the pages of a document.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::matcher::Matcher;
use super::{MAX_MATCHES, SearchError, SearchMatch, SearchQuery, SearchSnapshot};
use crate::engine::{DocStatus, DocumentHandle, Notifier};

/// How often the thread looks at a document that is still opening.
const OPEN_POLL: Duration = Duration::from_millis(10);

/// Tuning of a search. [`SearchOptions::default`] is what the app uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    /// The snapshot is rebuilt (a copy of every match) at most this often while the search runs;
    /// the final snapshot is always published. `Duration::ZERO` publishes after every page,
    /// which tests use to see each step. Default 50 ms.
    pub publish_interval: Duration,
    /// The search stops, with `truncated` set, when there would be more matches than this.
    /// Default [`MAX_MATCHES`].
    pub max_matches: usize,
}

impl Default for SearchOptions {
    fn default() -> SearchOptions {
        SearchOptions {
            publish_interval: Duration::from_millis(50),
            max_matches: MAX_MATCHES,
        }
    }
}

struct Shared {
    cancel: AtomicBool,
    snapshot: Mutex<Arc<SearchSnapshot>>,
}

impl Shared {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn set(&self, snapshot: SearchSnapshot) {
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(snapshot);
    }
}

/// A running (or finished) search. Dropping it cancels the search; the drop does not wait for
/// the thread, so it is safe on the UI thread.
pub struct SearchJob {
    shared: Arc<Shared>,
}

impl Drop for SearchJob {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for SearchJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchJob").finish_non_exhaustive()
    }
}

impl SearchJob {
    /// Starts searching `doc` for `query` and returns at once.
    ///
    /// The thread reads the pages from `start_page` to the end, then from the first page up to
    /// `start_page`. A `start_page` past the end counts as 0. `notifier` is called whenever a
    /// page brought new matches and when the search has finished (never after the job was
    /// dropped).
    ///
    /// Fails, without starting anything, when the query is empty or is a regular expression
    /// that does not compile.
    pub fn start(
        doc: DocumentHandle,
        query: SearchQuery,
        start_page: u32,
        notifier: Notifier,
    ) -> Result<SearchJob, SearchError> {
        SearchJob::start_with(doc, query, start_page, notifier, SearchOptions::default())
    }

    /// [`SearchJob::start`] with non-default options (used by tests).
    pub fn start_with(
        doc: DocumentHandle,
        query: SearchQuery,
        start_page: u32,
        notifier: Notifier,
        options: SearchOptions,
    ) -> Result<SearchJob, SearchError> {
        let matcher = Matcher::new(&query)?;
        let shared = Arc::new(Shared {
            cancel: AtomicBool::new(false),
            snapshot: Mutex::new(Arc::new(SearchSnapshot::default())),
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("refdesk-search".to_string())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run(&worker, &doc, &matcher, start_page, &notifier, options);
                }));
                if outcome.is_err() {
                    // A bug must not leave the UI waiting for a search that never ends.
                    let mut last = SearchSnapshot::clone(&current(&worker));
                    last.finished = true;
                    worker.set(last);
                    if !worker.cancelled() {
                        notifier();
                    }
                }
            })
            .map_err(|e| SearchError::Thread(e.to_string()))?;
        Ok(SearchJob { shared })
    }

    /// The state of the search now. Cheap: it hands out a shared copy that the search thread
    /// replaces as it goes, so it may be called every frame. `matches` is always in document
    /// order, whatever order the pages were processed in.
    pub fn snapshot(&self) -> Arc<SearchSnapshot> {
        current(&self.shared)
    }
}

fn current(shared: &Shared) -> Arc<SearchSnapshot> {
    Arc::clone(
        &shared
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    )
}

/// What the thread has found so far.
struct Progress {
    /// Matches by the page they start on. Iterating gives document order.
    by_page: BTreeMap<u32, Vec<SearchMatch>>,
    count: usize,
    done: u32,
    total: u32,
    finished: bool,
    truncated: bool,
    last_publish: Option<Instant>,
}

impl Progress {
    fn publish(&mut self, shared: &Shared) {
        let matches = self.by_page.values().flatten().cloned().collect();
        shared.set(SearchSnapshot {
            matches,
            pages_done: self.done,
            pages_total: self.total,
            finished: self.finished,
            truncated: self.truncated,
        });
        self.last_publish = Some(Instant::now());
    }

    fn publish_due(&self, interval: Duration) -> bool {
        self.last_publish.is_none_or(|t| t.elapsed() >= interval)
    }
}

/// Waits until the document is open. `false` when it never will be (or the job was dropped).
fn wait_until_open(shared: &Shared, doc: &DocumentHandle) -> bool {
    loop {
        if shared.cancelled() {
            return false;
        }
        match doc.status() {
            DocStatus::Loading => std::thread::sleep(OPEN_POLL),
            DocStatus::Ready => return true,
            DocStatus::NeedsPassword | DocStatus::Failed(_) => return false,
        }
    }
}

fn run(
    shared: &Shared,
    doc: &DocumentHandle,
    matcher: &Matcher,
    start_page: u32,
    notifier: &Notifier,
    options: SearchOptions,
) {
    let mut progress = Progress {
        by_page: BTreeMap::new(),
        count: 0,
        done: 0,
        total: 0,
        finished: false,
        truncated: false,
        last_publish: None,
    };

    if !wait_until_open(shared, doc) {
        if !shared.cancelled() {
            // Nothing to search: a document that cannot be read has no pages.
            progress.finished = true;
            progress.publish(shared);
            notifier();
        }
        return;
    }
    progress.total = doc
        .info()
        .map_or(0, |i| u32::try_from(i.page_sizes.len()).unwrap_or(u32::MAX));
    progress.publish(shared);

    let total = progress.total;
    let start = if start_page < total { start_page } else { 0 };
    for page in (start..total).chain(0..start) {
        if shared.cancelled() {
            return;
        }
        let found = match doc.page_text_wait(page) {
            Ok(text) if !text.chars.is_empty() => {
                // The next page of the document, for a phrase that runs over the page break.
                let next = if page + 1 < total {
                    doc.page_text_wait(page + 1).ok()
                } else {
                    None
                };
                matcher.find_in_page(page, &text, next.as_deref())
            }
            // No text, or the page could not be read: skip it and go on.
            _ => Vec::new(),
        };
        progress.done += 1;

        let mut found = found;
        if progress.count + found.len() > options.max_matches {
            found.truncate(options.max_matches.saturating_sub(progress.count));
            progress.truncated = true;
        }
        let new = !found.is_empty();
        progress.count += found.len();
        if new {
            progress.by_page.insert(page, found);
        }

        if progress.truncated || progress.publish_due(options.publish_interval) {
            progress.publish(shared);
        }
        if shared.cancelled() {
            return;
        }
        if new {
            notifier();
        }
        if progress.truncated {
            break;
        }
    }

    progress.finished = true;
    progress.publish(shared);
    if !shared.cancelled() {
        notifier();
    }
}
