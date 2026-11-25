//! The document worker thread, its request queues and the state shared with the handles.
//!
//! One worker per document owns the MuPDF document ([`Backend`]); handles talk to it through
//! [`Shared`]: a mutex-protected [`State`] and a condition variable that wakes the worker.
//! The worker takes one task (one page) at a time and looks at the queues again after each.
//!
//! Order of work, highest first:
//! 1. control commands (reload, password),
//! 2. renders requested as [`Priority::Visible`] or [`Priority::Preview`],
//! 3. text and link extraction asked for by the non-blocking `page_text` / `page_links`,
//! 4. blocking `*_wait` jobs, first come first served (they never expire),
//! 5. renders requested as [`Priority::Prefetch`] or [`Priority::Background`].
//!
//! Within one priority the most recently requested render goes first. A pending render that has
//! not been requested again within [`EngineOptions::render_expiry`] is dropped.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::backend::{Backend, guarded};
use super::{
    DocStatus, DocumentInfo, EngineError, EngineOptions, Notifier, Priority, RenderKey,
    RenderedImage, RenderedPage,
};
use crate::geom::Rect;
use crate::link::Link;
use crate::text::PageText;

/// State shared between the handles of one document and its worker.
pub(super) struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    pub(super) notifier: Notifier,
    pub(super) options: EngineOptions,
}

impl Shared {
    pub(super) fn new(notifier: Notifier, options: EngineOptions) -> Shared {
        Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            notifier,
            options,
        }
    }

    /// Locks the state. A panic while holding the lock leaves consistent data behind (every
    /// update is a single assignment or collection operation), so poisoning is ignored.
    pub(super) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wakes the worker after a change to the queues.
    pub(super) fn wake(&self) {
        self.wake.notify_all();
    }

    fn notify(&self) {
        (self.notifier)();
    }
}

/// A command that changes the document itself.
pub(super) enum Control {
    Reload,
    Authenticate(String),
}

/// A blocking request. The caller waits on the receiving end of `reply`.
pub(super) enum WaitJob {
    Text {
        page: u32,
        reply: Sender<Result<Arc<PageText>, EngineError>>,
    },
    Render {
        key: RenderKey,
        reply: Sender<Result<RenderedPage, EngineError>>,
    },
    Region {
        page: u32,
        region: Rect,
        scale_milli: u32,
        reply: Sender<Result<RenderedImage, EngineError>>,
    },
}

impl WaitJob {
    fn fail(self, e: EngineError) {
        // A caller that gave up waiting has dropped its receiver; nothing to do then.
        match self {
            WaitJob::Text { reply, .. } => drop(reply.send(Err(e))),
            WaitJob::Render { reply, .. } => drop(reply.send(Err(e))),
            WaitJob::Region { reply, .. } => drop(reply.send(Err(e))),
        }
    }
}

/// Everything the handles and the worker share.
pub(super) struct State {
    pub(super) status: DocStatus,
    pub(super) info: Option<Arc<DocumentInfo>>,
    /// Current generation; keys of any other generation are ignored (rule R-7).
    pub(super) generation: u32,
    pub(super) renders: RenderQueue,
    /// The render the worker is doing now (not in `renders` any more).
    pub(super) rendering: Option<RenderKey>,
    /// Finished renders not yet taken by `poll_rendered`.
    pub(super) rendered: Vec<RenderedPage>,
    /// Renders that failed in this generation; requests for them are ignored so a broken page is
    /// not retried every frame.
    pub(super) failed_renders: HashSet<RenderKey>,
    pub(super) texts: HashMap<u32, Arc<PageText>>,
    pub(super) links: HashMap<u32, Arc<[Link]>>,
    pub(super) text_wanted: VecDeque<u32>,
    pub(super) links_wanted: VecDeque<u32>,
    pub(super) failed_text: HashSet<u32>,
    pub(super) failed_links: HashSet<u32>,
    pub(super) control: VecDeque<Control>,
    pub(super) waits: VecDeque<WaitJob>,
    /// Set when the last handle is dropped.
    pub(super) shutdown: bool,
    /// Set when the worker has stopped; blocking calls then fail at once.
    pub(super) worker_gone: bool,
}

impl Default for State {
    fn default() -> State {
        State {
            status: DocStatus::Loading,
            info: None,
            generation: 0,
            renders: RenderQueue::default(),
            rendering: None,
            rendered: Vec::new(),
            failed_renders: HashSet::new(),
            texts: HashMap::new(),
            links: HashMap::new(),
            text_wanted: VecDeque::new(),
            links_wanted: VecDeque::new(),
            failed_text: HashSet::new(),
            failed_links: HashSet::new(),
            control: VecDeque::new(),
            waits: VecDeque::new(),
            shutdown: false,
            worker_gone: false,
        }
    }
}

impl State {
    /// The page count of the open document, if it is open.
    pub(super) fn page_count(&self) -> Option<u32> {
        self.info.as_ref().map(|i| i.page_sizes.len() as u32)
    }

    /// Records a render request from `request_render`.
    pub(super) fn request_render(&mut self, key: RenderKey, prio: Priority, now: Instant) {
        if self.status != DocStatus::Ready
            || key.generation != self.generation
            || self.page_count().is_none_or(|n| key.page >= n)
            || self.rendering == Some(key)
            || self.failed_renders.contains(&key)
            || self.rendered.iter().any(|r| r.key == key)
        {
            return;
        }
        self.renders.request(key, prio, now);
    }

    /// Records a wish for the text of `page` from the non-blocking `page_text`.
    pub(super) fn want_text(&mut self, page: u32) {
        if self.status == DocStatus::Ready
            && self.page_count().is_some_and(|n| page < n)
            && !self.failed_text.contains(&page)
            && !self.text_wanted.contains(&page)
        {
            self.text_wanted.push_back(page);
        }
    }

    /// Records a wish for the links of `page` from the non-blocking `page_links`.
    pub(super) fn want_links(&mut self, page: u32) {
        if self.status == DocStatus::Ready
            && self.page_count().is_some_and(|n| page < n)
            && !self.failed_links.contains(&page)
            && !self.links_wanted.contains(&page)
        {
            self.links_wanted.push_back(page);
        }
    }

    /// Picks the next task. `None` when there is nothing to do now.
    fn next_task(&mut self, now: Instant, expiry: Duration) -> Option<Task> {
        if let Some(c) = self.control.pop_front() {
            return Some(Task::Control(c));
        }
        if self.status != DocStatus::Ready {
            // Nothing can be served until a reload or a password makes the document ready.
            for job in self.waits.drain(..) {
                job.fail(EngineError::NotReady);
            }
            self.text_wanted.clear();
            self.links_wanted.clear();
            self.renders.clear();
            return None;
        }
        self.renders.expire(now, expiry);
        if let Some(key) = self.renders.pop(Priority::Preview) {
            return Some(Task::Render(key));
        }
        while let Some(page) = self.text_wanted.pop_front() {
            if !self.texts.contains_key(&page) {
                return Some(Task::Text(page));
            }
        }
        while let Some(page) = self.links_wanted.pop_front() {
            if !self.links.contains_key(&page) {
                return Some(Task::Links(page));
            }
        }
        if let Some(job) = self.waits.pop_front() {
            return Some(Task::Wait(job));
        }
        self.renders.pop(Priority::Background).map(Task::Render)
    }

    /// Drops everything that belongs to the previous generation and moves to `generation`.
    fn start_generation(&mut self, generation: u32) {
        self.generation = generation;
        self.renders.retain_generation(generation);
        self.rendered.retain(|r| r.key.generation == generation);
        self.failed_renders.clear();
        self.texts.clear();
        self.links.clear();
        self.failed_text.clear();
        self.failed_links.clear();
        self.text_wanted.clear();
        self.links_wanted.clear();
    }
}

enum Task {
    Control(Control),
    Render(RenderKey),
    Text(u32),
    Links(u32),
    Wait(WaitJob),
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    prio: Priority,
    /// Order of the latest request; larger is more recent.
    seq: u64,
    /// When the latest request came.
    last: Instant,
}

/// Pending render requests, one entry per key.
#[derive(Debug, Default)]
pub(super) struct RenderQueue {
    pending: HashMap<RenderKey, Pending>,
    seq: u64,
}

impl RenderQueue {
    /// Adds a request, or refreshes the priority and time of the pending request for `key`.
    pub(super) fn request(&mut self, key: RenderKey, prio: Priority, now: Instant) {
        self.seq += 1;
        self.pending.insert(
            key,
            Pending {
                prio,
                seq: self.seq,
                last: now,
            },
        );
    }

    /// Drops requests that were not renewed within `expiry` before `now`.
    pub(super) fn expire(&mut self, now: Instant, expiry: Duration) {
        self.pending
            .retain(|_, p| now.saturating_duration_since(p.last) < expiry);
    }

    /// Removes and returns the most urgent request whose priority is `limit` or more urgent:
    /// the most urgent priority first, then the most recently requested.
    pub(super) fn pop(&mut self, limit: Priority) -> Option<RenderKey> {
        let key = self
            .pending
            .iter()
            .filter(|(_, p)| p.prio <= limit)
            .min_by(|(_, a), (_, b)| a.prio.cmp(&b.prio).then(b.seq.cmp(&a.seq)))
            .map(|(k, _)| *k)?;
        self.pending.remove(&key);
        Some(key)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.pending.len()
    }

    fn clear(&mut self) {
        self.pending.clear();
    }

    fn retain_generation(&mut self, generation: u32) {
        self.pending.retain(|k, _| k.generation == generation);
    }
}

/// Marks the worker as gone when it ends, normally or by panic, and fails waiting callers.
struct ExitGuard(Arc<Shared>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        let mut st = self.0.lock();
        st.worker_gone = true;
        if std::thread::panicking() {
            st.status = DocStatus::Failed("internal error in the document engine".to_string());
            st.info = None;
        }
        for job in st.waits.drain(..) {
            job.fail(EngineError::WorkerGone);
        }
        drop(st);
        self.0.notify();
    }
}

/// Starts the worker thread for `path`.
pub(super) fn spawn(shared: Arc<Shared>, path: PathBuf) {
    let for_worker = Arc::clone(&shared);
    let started = std::thread::Builder::new()
        .name("refdesk-document".to_string())
        .spawn(move || run(for_worker, path));
    if let Err(e) = started {
        let mut st = shared.lock();
        st.status = DocStatus::Failed(format!("cannot start the document worker: {e}"));
        st.worker_gone = true;
        drop(st);
        shared.notify();
    }
}

/// What opening a file produced.
struct Opened {
    status: DocStatus,
    backend: Option<Backend>,
    info: Option<Arc<DocumentInfo>>,
}

fn open(path: &Path, generation: u32) -> Opened {
    let started = Instant::now();
    let mut backend = match Backend::open(path) {
        Ok(b) => b,
        Err(msg) => {
            log::warn!("open {}: {msg}", path.display());
            return Opened {
                status: DocStatus::Failed(msg),
                backend: None,
                info: None,
            };
        }
    };
    if backend.needs_password() {
        return Opened {
            status: DocStatus::NeedsPassword,
            backend: Some(backend),
            info: None,
        };
    }
    let opened = finish_open(&mut backend, generation);
    log::debug!("open {}: {:?}", path.display(), started.elapsed());
    match opened {
        Ok(info) => Opened {
            status: DocStatus::Ready,
            backend: Some(backend),
            info: Some(info),
        },
        Err(msg) => Opened {
            status: DocStatus::Failed(msg),
            backend: None,
            info: None,
        },
    }
}

fn finish_open(backend: &mut Backend, generation: u32) -> Result<Arc<DocumentInfo>, String> {
    backend.load_info(generation).map(Arc::new).map_err(|msg| {
        log::warn!("{msg}");
        msg
    })
}

fn run(shared: Arc<Shared>, path: PathBuf) {
    let _guard = ExitGuard(Arc::clone(&shared));
    let opened = open(&path, 0);
    let mut backend = opened.backend;
    {
        let mut st = shared.lock();
        st.status = opened.status;
        st.info = opened.info;
    }
    shared.notify();

    loop {
        let task = {
            let mut st = shared.lock();
            loop {
                if st.shutdown {
                    return;
                }
                if let Some(t) = st.next_task(Instant::now(), shared.options.render_expiry) {
                    if let Task::Render(key) = &t {
                        st.rendering = Some(*key);
                    }
                    break t;
                }
                st = shared.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        };
        match task {
            Task::Control(Control::Reload) => {
                // Close the old document before opening the file again.
                drop(backend.take());
                let generation = shared.lock().generation.wrapping_add(1);
                let opened = open(&path, generation);
                backend = opened.backend;
                let mut st = shared.lock();
                st.start_generation(generation);
                st.status = opened.status;
                st.info = opened.info;
                drop(st);
                shared.notify();
            }
            Task::Control(Control::Authenticate(password)) => {
                let Some(b) = backend.as_mut() else { continue };
                if shared.lock().status != DocStatus::NeedsPassword || !b.authenticate(&password) {
                    shared.notify();
                    continue;
                }
                let generation = shared.lock().generation;
                let result = finish_open(b, generation);
                let mut st = shared.lock();
                match result {
                    Ok(info) => {
                        st.status = DocStatus::Ready;
                        st.info = Some(info);
                    }
                    Err(msg) => {
                        st.status = DocStatus::Failed(msg);
                        backend = None;
                    }
                }
                drop(st);
                shared.notify();
            }
            Task::Render(key) => {
                let result = with_backend(&backend, |b| render_page(b, key));
                let mut st = shared.lock();
                st.rendering = None;
                let delivered = match result {
                    Ok(page) if page.key.generation == st.generation => {
                        st.rendered.push(page);
                        true
                    }
                    Ok(_) => false,
                    Err(e) => {
                        log::warn!("{}: render page {}: {e}", path.display(), key.page + 1);
                        st.failed_renders.insert(key);
                        false
                    }
                };
                drop(st);
                if delivered {
                    shared.notify();
                }
            }
            Task::Text(page) => {
                let result = with_backend(&backend, |b| b.page_text(page));
                let _ = store_text(&shared, &path, page, result);
                shared.notify();
            }
            Task::Links(page) => {
                let result = with_backend(&backend, |b| b.page_links(page));
                let mut st = shared.lock();
                match result {
                    Ok(links) => {
                        st.links.insert(page, links.into());
                    }
                    Err(e) => {
                        log::warn!("{}: links of page {}: {e}", path.display(), page + 1);
                        st.failed_links.insert(page);
                    }
                }
                drop(st);
                shared.notify();
            }
            Task::Wait(job) => serve_wait(&shared, &path, &backend, job),
        }
    }
}

fn with_backend<T>(
    backend: &Option<Backend>,
    f: impl FnOnce(&Backend) -> Result<T, EngineError>,
) -> Result<T, EngineError> {
    match backend {
        Some(b) => guarded(|| f(b)),
        None => Err(EngineError::NotReady),
    }
}

fn render_page(b: &Backend, key: RenderKey) -> Result<RenderedPage, EngineError> {
    let started = Instant::now();
    let (width, height, rgb) = b.render(key.page, key.scale_milli)?;
    log::debug!(
        "render page {} at {}: {width}x{height} in {:?}",
        key.page + 1,
        key.scale_milli,
        started.elapsed()
    );
    Ok(RenderedPage {
        key,
        width,
        height,
        rgb,
    })
}

/// Caches the result of a text extraction and returns it.
fn store_text(
    shared: &Shared,
    path: &Path,
    page: u32,
    result: Result<PageText, EngineError>,
) -> Result<Arc<PageText>, EngineError> {
    let mut st = shared.lock();
    match result {
        Ok(text) => {
            let text = Arc::new(text);
            st.texts.insert(page, Arc::clone(&text));
            Ok(text)
        }
        Err(e) => {
            log::warn!("{}: text of page {}: {e}", path.display(), page + 1);
            if !matches!(e, EngineError::NotReady) {
                st.failed_text.insert(page);
            }
            Err(e)
        }
    }
}

fn serve_wait(shared: &Shared, path: &Path, backend: &Option<Backend>, job: WaitJob) {
    match job {
        WaitJob::Text { page, reply } => {
            let cached = shared.lock().texts.get(&page).cloned();
            let result = match cached {
                Some(t) => Ok(t),
                None => {
                    let r = with_backend(backend, |b| b.page_text(page));
                    let r = store_text(shared, path, page, r);
                    if r.is_ok() {
                        // `page_text` callers may be waiting for this page too.
                        shared.notify();
                    }
                    r
                }
            };
            drop(reply.send(result));
        }
        WaitJob::Render { key, reply } => {
            let generation = shared.lock().generation;
            let result = if key.generation == generation {
                with_backend(backend, |b| render_page(b, key))
            } else {
                Err(stale(key.generation, generation))
            };
            drop(reply.send(result));
        }
        WaitJob::Region {
            page,
            region,
            scale_milli,
            reply,
        } => {
            let result = with_backend(backend, |b| b.render_region(page, region, scale_milli));
            drop(reply.send(result));
        }
    }
}

/// The error for a render key of another generation.
pub(super) fn stale(key_generation: u32, current: u32) -> EngineError {
    EngineError::Failed(format!(
        "the render key belongs to generation {key_generation}, the document is at {current}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(page: u32) -> RenderKey {
        RenderKey {
            page,
            scale_milli: 1000,
            generation: 0,
        }
    }

    const LONG: Duration = Duration::from_secs(3600);

    #[test]
    fn higher_priority_first_then_most_recent() {
        let t = Instant::now();
        let mut q = RenderQueue::default();
        q.request(key(1), Priority::Background, t);
        q.request(key(2), Priority::Background, t);
        q.request(key(3), Priority::Visible, t);
        q.request(key(4), Priority::Prefetch, t);
        q.request(key(5), Priority::Visible, t);
        let order: Vec<u32> = std::iter::from_fn(|| q.pop(Priority::Background))
            .map(|k| k.page)
            .collect();
        assert_eq!(order, vec![5, 3, 4, 2, 1]);
    }

    #[test]
    fn duplicate_requests_merge_and_refresh() {
        let t = Instant::now();
        let mut q = RenderQueue::default();
        q.request(key(1), Priority::Background, t);
        q.request(key(2), Priority::Background, t);
        q.request(key(1), Priority::Background, t);
        assert_eq!(q.len(), 2);
        // Key 1 was requested again last, so it is now the most recent.
        assert_eq!(q.pop(Priority::Background), Some(key(1)));
        q.request(key(2), Priority::Visible, t);
        assert_eq!(q.pop(Priority::Background), Some(key(2)));
        assert_eq!(q.pop(Priority::Background), None);
    }

    #[test]
    fn pop_respects_the_priority_limit() {
        let t = Instant::now();
        let mut q = RenderQueue::default();
        q.request(key(1), Priority::Prefetch, t);
        assert_eq!(q.pop(Priority::Preview), None);
        q.request(key(2), Priority::Preview, t);
        assert_eq!(q.pop(Priority::Preview), Some(key(2)));
        assert_eq!(q.pop(Priority::Background), Some(key(1)));
    }

    #[test]
    fn requests_not_renewed_expire() {
        let t0 = Instant::now();
        let expiry = Duration::from_millis(250);
        let mut q = RenderQueue::default();
        q.request(key(1), Priority::Visible, t0);
        q.request(key(2), Priority::Visible, t0);
        // Key 2 is requested again 200 ms later; key 1 is not.
        q.request(key(2), Priority::Visible, t0 + Duration::from_millis(200));
        q.expire(t0 + Duration::from_millis(300), expiry);
        assert_eq!(q.pop(Priority::Background), Some(key(2)));
        assert_eq!(q.pop(Priority::Background), None);
        q.request(key(3), Priority::Visible, t0);
        q.expire(t0 + Duration::from_millis(100), expiry);
        assert_eq!(q.len(), 1);
        q.expire(t0 + LONG, expiry);
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn new_generation_drops_old_requests_and_results() {
        let t = Instant::now();
        let mut st = State::default();
        st.renders.request(key(1), Priority::Visible, t);
        st.rendered.push(RenderedPage {
            key: key(2),
            width: 1,
            height: 1,
            rgb: vec![255; 3],
        });
        st.texts.insert(0, Arc::new(PageText::default()));
        let mut k = key(3);
        k.generation = 1;
        st.renders.request(k, Priority::Visible, t);
        st.start_generation(1);
        assert_eq!(st.renders.pop(Priority::Background), Some(k));
        assert!(st.renders.pop(Priority::Background).is_none());
        assert!(st.rendered.is_empty());
        assert!(st.texts.is_empty());
    }

    #[test]
    fn requests_are_ignored_until_ready_and_for_other_generations() {
        let t = Instant::now();
        let mut st = State::default();
        st.request_render(key(0), Priority::Visible, t);
        assert_eq!(st.renders.len(), 0);
        st.status = DocStatus::Ready;
        st.info = Some(Arc::new(DocumentInfo {
            path: PathBuf::from("/x.pdf"),
            generation: 0,
            page_sizes: vec![crate::geom::Size::new(10.0, 10.0); 2],
            page_labels: Vec::new(),
            title: None,
            outline: crate::outline::Outline::new(),
        }));
        st.request_render(key(0), Priority::Visible, t);
        st.request_render(key(2), Priority::Visible, t); // past the end
        let mut other = key(1);
        other.generation = 7;
        st.request_render(other, Priority::Visible, t);
        assert_eq!(st.renders.len(), 1);
    }
}
