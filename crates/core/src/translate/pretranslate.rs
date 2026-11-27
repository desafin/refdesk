//! Translating a document ahead of reading, sentence by sentence (§4.16, TR-15 ~ 19).
//!
//! * [`PretranslateStore`]: the translations of one document by one backend identity, page by
//!   page: for every segment of [`super::segment::segments`] its character range, the hash of
//!   its text and the translation (or "skipped", or "failed"). One file per document and
//!   identity under the project's data folder: `translations/<fingerprint>.<identity hash>.json`.
//! * [`PretranslateSettings`]: whether the feature is on for a project, and paused
//!   (`pretranslate.json`).
//! * [`PretranslateJob`]: one thread that picks the next page ([`pick_page`]: the urgent pages,
//!   then the nearest page to the one being read, ahead a little before behind), splits it and
//!   sends the segments one by one to the backend. Between segments it looks at cancellation,
//!   pause and "a translation the user asked for is running" ([`PretranslateSetup::foreground_busy`]).
//! * [`assemble`]: what a popup shows for a selection when every segment it touches is stored.
//!
//! Every file write is atomic and a damaged file is moved to `.bad` (R-8). Nothing here panics
//! (R-9); the job thread catches a panic and reports it as a blocked state.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::io;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use serde::de::{self, IgnoredAny, Visitor};
use serde::ser::SerializeTuple;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::segment::{SEGMENT_RULES_VERSION, covering, segments, text_hash};
use super::{TranslateBackend, TranslateError, TranslateKind, TranslateRequest, clean_output};
use crate::engine::{DocStatus, DocumentHandle, Notifier};
use crate::store::atomic::{atomic_write, quarantine};
use crate::text::PageText;

/// The format version of the store files and of `pretranslate.json`.
pub const PRETRANSLATE_FORMAT_VERSION: u32 = 1;

/// Name of the folder (in a project's data folder) that holds the store files.
pub const TRANSLATIONS_DIR: &str = "translations";

/// Name of the settings file in a project's data folder.
pub const SETTINGS_FILE: &str = "pretranslate.json";

/// Shortest wait after the server could not be reached.
pub const BLOCKED_BACKOFF_MIN: Duration = Duration::from_secs(30);
/// Longest wait after the server could not be reached.
pub const BLOCKED_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// Tries for one segment that fails with an error other than "no server".
pub const SEGMENT_TRIES: u32 = 3;
/// How many urgent pages [`PretranslateOrder`] keeps.
const MAX_URGENT: usize = 8;
/// How often the job looks at `foreground_busy` while it waits for it to go down.
const FOREGROUND_POLL: Duration = Duration::from_millis(50);
/// How long a wait for the clock lasts at most before it looks again (a test clock is nudged).
const CLOCK_POLL: Duration = Duration::from_secs(1);
/// How often a document that is still opening is looked at.
const OPEN_POLL: Duration = Duration::from_millis(10);
/// Completed segments the throughput is measured over.
const RATE_WINDOW: usize = 20;

// ---------------------------------------------------------------------------------------------
// Settings: pretranslate.json
// ---------------------------------------------------------------------------------------------

/// Whether translating ahead is on for a project, and paused. Missing file: off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PretranslateSettings {
    /// The feature is on for this project (TR-15).
    pub enabled: bool,
    /// The user paused it (TR-18).
    pub paused: bool,
}

#[derive(Serialize)]
struct SettingsOut {
    version: u32,
    enabled: bool,
    paused: bool,
}

impl PretranslateSettings {
    /// The settings file of the project whose data folder is `data_dir`.
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(SETTINGS_FILE)
    }

    /// Reads the settings. A missing file is "off"; a damaged one is moved to `.bad` (R-8).
    pub fn load(data_dir: &Path) -> PretranslateSettings {
        crate::store::atomic::read_json_or_default(&PretranslateSettings::path(data_dir))
    }

    /// Writes the settings atomically, making the data folder when needed.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&SettingsOut {
            version: PRETRANSLATE_FORMAT_VERSION,
            enabled: self.enabled,
            paused: self.paused,
        })
        .map_err(io::Error::other)?;
        atomic_write(&PretranslateSettings::path(data_dir), &bytes)
    }
}

// ---------------------------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------------------------

/// What became of one segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Translated: the cleaned answer of the model.
    Translated(String),
    /// Not worth translating (`Segment::skip`): shown as it is.
    Skipped,
    /// The backend failed on it [`SEGMENT_TRIES`] times. A popup translates it on the spot.
    Failed,
}

/// One stored segment. In the file: `[start, end, hash, translation]`, the last being a string,
/// `null` (skipped) or `false` (failed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSegment {
    /// The segment's characters on its page.
    pub chars: Range<u32>,
    /// [`text_hash`] of the segment's text.
    pub hash: u64,
    /// What became of it.
    pub outcome: Outcome,
}

impl Serialize for StoredSegment {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut t = s.serialize_tuple(4)?;
        t.serialize_element(&self.chars.start)?;
        t.serialize_element(&self.chars.end)?;
        t.serialize_element(&self.hash)?;
        match &self.outcome {
            Outcome::Translated(text) => t.serialize_element(text)?,
            Outcome::Skipped => t.serialize_element(&())?,
            Outcome::Failed => t.serialize_element(&false)?,
        }
        t.end()
    }
}

impl<'de> Deserialize<'de> for Outcome {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Outcome, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Outcome;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a translation, null or false")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Outcome, E> {
                Ok(Outcome::Translated(v.to_owned()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Outcome, E> {
                Ok(Outcome::Translated(v))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Outcome, E> {
                Ok(Outcome::Failed)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Outcome, E> {
                Ok(Outcome::Skipped)
            }
            fn visit_none<E: de::Error>(self) -> Result<Outcome, E> {
                Ok(Outcome::Skipped)
            }
        }
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for StoredSegment {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<StoredSegment, D::Error> {
        let (start, end, hash, outcome) = <(u32, u32, u64, Outcome)>::deserialize(d)?;
        Ok(StoredSegment {
            chars: start..end,
            hash,
            outcome,
        })
    }
}

#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    rules: u32,
    fingerprint: &'a str,
    identity: &'a str,
    page_count: u32,
    pages: &'a [Option<Vec<StoredSegment>>],
}

#[derive(Deserialize)]
struct FileIn {
    #[serde(default)]
    rules: u32,
    fingerprint: String,
    identity: String,
    page_count: u32,
    pages: Vec<Option<Vec<StoredSegment>>>,
}

/// The parts of a file [`list`] reads (the translations are skipped over).
#[derive(Deserialize)]
struct HeaderIn {
    #[serde(default)]
    rules: u32,
    fingerprint: String,
    identity: String,
    page_count: u32,
    pages: Vec<Option<IgnoredAny>>,
}

/// The translations of one document by one backend identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PretranslateStore {
    fingerprint: String,
    identity: String,
    pages: Vec<Option<Vec<StoredSegment>>>,
    done: u32,
    revision: u64,
    saved_revision: u64,
}

/// A store shared by the job (which fills it) and the app (whose popups read it).
pub type SharedStore = Arc<RwLock<PretranslateStore>>;

/// The 16 hexadecimal digits of the identity's XXH3-64 that name a store file.
pub fn identity_hash(identity: &str) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(identity.as_bytes()))
}

/// The store file of a document (`fingerprint`, see `fingerprint_file`) and a backend
/// `identity` in the project data folder `data_dir`.
pub fn store_path(data_dir: &Path, fingerprint: &str, identity: &str) -> PathBuf {
    data_dir
        .join(TRANSLATIONS_DIR)
        .join(format!("{fingerprint}.{}.json", identity_hash(identity)))
}

impl PretranslateStore {
    /// An empty store: no page done.
    pub fn new(fingerprint: &str, identity: &str, page_count: u32) -> PretranslateStore {
        PretranslateStore {
            fingerprint: fingerprint.to_string(),
            identity: identity.to_string(),
            pages: vec![None; page_count as usize],
            done: 0,
            revision: 0,
            saved_revision: 0,
        }
    }

    /// The document's fingerprint.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The backend identity the translations come from.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Pages of the document.
    pub fn page_count(&self) -> u32 {
        u32::try_from(self.pages.len()).unwrap_or(u32::MAX)
    }

    /// Pages whose segments are all stored.
    pub fn pages_done(&self) -> u32 {
        self.done
    }

    /// Whether `page` is done.
    pub fn page_done(&self, page: u32) -> bool {
        self.pages.get(page as usize).is_some_and(Option::is_some)
    }

    /// Whether every page is done.
    pub fn is_complete(&self) -> bool {
        self.done as usize == self.pages.len()
    }

    /// Whether something changed since the last [`PretranslateStore::mark_saved`].
    pub fn dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    /// A number that changes whenever the content does.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Records that the content of `revision` is on disk.
    pub fn mark_saved(&mut self, revision: u64) {
        self.saved_revision = revision;
    }

    /// The stored segments of `page`, `None` when it is not done.
    pub fn page(&self, page: u32) -> Option<&[StoredSegment]> {
        self.pages.get(page as usize)?.as_deref()
    }

    /// What became of the segment with these characters and text hash on `page`. `None` when
    /// the page is not done or no stored segment has both the same range and the same hash
    /// (the text was extracted differently).
    pub fn outcome(
        &self,
        page: u32,
        segment_chars: Range<u32>,
        text_hash: u64,
    ) -> Option<&Outcome> {
        let segs = self.page(page)?;
        let i = segs
            .binary_search_by_key(&segment_chars.start, |s| s.chars.start)
            .ok()?;
        let s = &segs[i];
        (s.chars == segment_chars && s.hash == text_hash).then_some(&s.outcome)
    }

    /// The translation of a segment, when it was translated (see [`PretranslateStore::outcome`]).
    pub fn get(&self, page: u32, segment_chars: Range<u32>, text_hash: u64) -> Option<&str> {
        match self.outcome(page, segment_chars, text_hash)? {
            Outcome::Translated(t) => Some(t),
            Outcome::Skipped | Outcome::Failed => None,
        }
    }

    /// Stores the segments of `page` (any order; kept sorted by range) and marks it done. A page
    /// past the end is ignored.
    pub fn set_page(&mut self, page: u32, mut segments: Vec<StoredSegment>) {
        let Some(slot) = self.pages.get_mut(page as usize) else {
            return;
        };
        segments.sort_by_key(|s| s.chars.start);
        if slot.is_none() {
            self.done += 1;
        }
        *slot = Some(segments);
        self.revision += 1;
    }

    /// Counts of translated, skipped and failed segments over the done pages.
    pub fn counts(&self) -> (u64, u64, u64) {
        let mut c = (0, 0, 0);
        for s in self.pages.iter().flatten().flatten() {
            match s.outcome {
                Outcome::Translated(_) => c.0 += 1,
                Outcome::Skipped => c.1 += 1,
                Outcome::Failed => c.2 += 1,
            }
        }
        c
    }

    /// The file content.
    pub fn to_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(&FileOut {
            version: PRETRANSLATE_FORMAT_VERSION,
            rules: SEGMENT_RULES_VERSION,
            fingerprint: &self.fingerprint,
            identity: &self.identity,
            page_count: self.page_count(),
            pages: &self.pages,
        })
        .map_err(io::Error::other)
    }

    /// Writes the store atomically (R-8). Does not clear [`PretranslateStore::dirty`]; the caller
    /// marks the revision it wrote with [`PretranslateStore::mark_saved`].
    pub fn save(&self, path: &Path) -> io::Result<()> {
        atomic_write(path, &self.to_bytes()?)
    }

    /// Reads a store file. `None` when there is none, when it was made under other splitting
    /// rules (it is then replaced at the next save), or when it is damaged (moved to `.bad`).
    pub fn load(path: &Path) -> Option<PretranslateStore> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => {
                log::warn!("cannot read {}: {e}", path.display());
                return None;
            }
        };
        let file: FileIn = match serde_json::from_slice(&bytes) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("{} is damaged ({e}); starting over", path.display());
                if let Err(e) = quarantine(path) {
                    log::warn!("cannot move {} to .bad: {e}", path.display());
                }
                return None;
            }
        };
        if file.rules != SEGMENT_RULES_VERSION {
            log::info!(
                "{} was made under splitting rules {}, not {SEGMENT_RULES_VERSION}; starting over",
                path.display(),
                file.rules
            );
            return None;
        }
        let mut pages = file.pages;
        pages.resize(file.page_count as usize, None);
        for p in pages.iter_mut().flatten() {
            p.sort_by_key(|s| s.chars.start);
        }
        let done = u32::try_from(pages.iter().filter(|p| p.is_some()).count()).unwrap_or(u32::MAX);
        Some(PretranslateStore {
            fingerprint: file.fingerprint,
            identity: file.identity,
            pages,
            done,
            revision: 0,
            saved_revision: 0,
        })
    }

    /// The store of `path` when it belongs to this document, identity and page count; else an
    /// empty one.
    pub fn open(
        path: &Path,
        fingerprint: &str,
        identity: &str,
        page_count: u32,
    ) -> PretranslateStore {
        match PretranslateStore::load(path) {
            Some(s)
                if s.fingerprint == fingerprint
                    && s.identity == identity
                    && s.page_count() == page_count =>
            {
                s
            }
            _ => PretranslateStore::new(fingerprint, identity, page_count),
        }
    }
}

/// One store file found by [`list`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredTranslation {
    /// The file.
    pub path: PathBuf,
    /// The document's fingerprint.
    pub fingerprint: String,
    /// The backend identity.
    pub identity: String,
    /// Done pages.
    pub pages_done: u32,
    /// Pages of the document.
    pub page_count: u32,
    /// Size of the file in bytes.
    pub bytes: u64,
}

/// The store files of the project whose data folder is `data_dir`, sorted by path. Files that
/// do not parse or were made under other splitting rules are left out (and left alone).
pub fn list(data_dir: &Path) -> Vec<StoredTranslation> {
    let dir = data_dir.join(TRANSLATIONS_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(h) = serde_json::from_slice::<HeaderIn>(&bytes) else {
            continue;
        };
        if h.rules != SEGMENT_RULES_VERSION {
            continue;
        }
        let pages_done = u32::try_from(
            h.pages
                .iter()
                .take(h.page_count as usize)
                .filter(|p| p.is_some())
                .count(),
        )
        .unwrap_or(u32::MAX);
        out.push(StoredTranslation {
            path,
            fingerprint: h.fingerprint,
            identity: h.identity,
            pages_done,
            page_count: h.page_count,
            bytes: bytes.len() as u64,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Removes one store file.
pub fn remove(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

/// Removes every store file of the project whose data folder is `data_dir` (TR-19). Returns how
/// many went. The settings file stays.
pub fn remove_all(data_dir: &Path) -> io::Result<usize> {
    let dir = data_dir.join(TRANSLATIONS_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut n = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            fs::remove_file(&path)?;
            n += 1;
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------------------------
// What a popup shows
// ---------------------------------------------------------------------------------------------

/// A selection put together from stored translations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assembled {
    /// The text of the segments the selection touches (whole sentences), in order.
    pub source: String,
    /// Their translations in order; a skipped segment stands as it is.
    pub translation: String,
}

/// The stored translation of a selection given per page as `(page, its text, selected
/// characters)`: every segment that shares a character with the selection, in order.
///
/// `Err` lists the pages with a segment that is not stored (not done, failed, or extracted
/// differently) — the popup then translates on the spot and the job is told to do those pages
/// first. `Err` with no page: the selection touches no segment.
pub fn assemble(
    store: &PretranslateStore,
    parts: &[(u32, &PageText, Range<u32>)],
) -> Result<Assembled, Vec<u32>> {
    let mut sources: Vec<String> = Vec::new();
    let mut translations: Vec<String> = Vec::new();
    let mut missing: Vec<u32> = Vec::new();
    for (page, text, chars) in parts {
        let segs = segments(text);
        for seg in &segs[covering(&segs, chars.clone())] {
            match store.outcome(*page, seg.chars.clone(), text_hash(&seg.text)) {
                Some(Outcome::Translated(t)) => {
                    sources.push(seg.text.clone());
                    translations.push(t.clone());
                }
                Some(Outcome::Skipped) => {
                    sources.push(seg.text.clone());
                    translations.push(seg.text.clone());
                }
                Some(Outcome::Failed) | None => {
                    if !missing.contains(page) {
                        missing.push(*page);
                    }
                }
            }
        }
    }
    if !missing.is_empty() || sources.is_empty() {
        return Err(missing);
    }
    Ok(Assembled {
        source: sources.join(" "),
        translation: translations.join(" "),
    })
}

// ---------------------------------------------------------------------------------------------
// The order of pages
// ---------------------------------------------------------------------------------------------

/// The page to do next: the first urgent page that is not done, else the page nearest to
/// `current` that is not done. Pages ahead count a little nearer than pages behind (reading
/// goes forward): a page `d` ahead costs `2d`, a page `d` behind `3d`; on a tie the page ahead
/// wins. `None` when every page is done.
pub fn pick_page(
    current: u32,
    urgent: &[u32],
    page_count: u32,
    done: impl Fn(u32) -> bool,
) -> Option<u32> {
    if let Some(p) = urgent.iter().find(|p| **p < page_count && !done(**p)) {
        return Some(*p);
    }
    let current = current.min(page_count.saturating_sub(1));
    (0..page_count).filter(|p| !done(*p)).min_by_key(|&p| {
        if p >= current {
            (u64::from(p - current) * 2, false)
        } else {
            (u64::from(current - p) * 3, true)
        }
    })
}

#[derive(Debug, Default)]
struct OrderState {
    current: u32,
    urgent: VecDeque<u32>,
}

/// The page priorities the app gives the job: the page being read, and pages a popup missed.
/// The job reads them when it picks its next page.
#[derive(Debug, Default)]
pub struct PretranslateOrder {
    state: Mutex<OrderState>,
}

impl PretranslateOrder {
    /// Starting at page 0, nothing urgent.
    pub fn new() -> Arc<PretranslateOrder> {
        Arc::default()
    }

    fn lock(&self) -> MutexGuard<'_, OrderState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The page being read.
    pub fn set_current(&self, page: u32) {
        self.lock().current = page;
    }

    /// The page being read, as last set.
    pub fn current(&self) -> u32 {
        self.lock().current
    }

    /// Puts `page` first: it is done before any other page not yet started (TR-17: a popup
    /// had to translate on the spot there).
    pub fn prioritize(&self, page: u32) {
        let mut st = self.lock();
        st.urgent.retain(|p| *p != page);
        st.urgent.push_front(page);
        st.urgent.truncate(MAX_URGENT);
    }

    /// The urgent pages, most recent first.
    pub fn urgent(&self) -> Vec<u32> {
        self.lock().urgent.iter().copied().collect()
    }

    fn next(&self, page_count: u32, done: impl Fn(u32) -> bool) -> Option<u32> {
        let mut st = self.lock();
        st.urgent.retain(|p| !done(*p));
        let urgent: Vec<u32> = st.urgent.iter().copied().collect();
        pick_page(st.current, &urgent, page_count, done)
    }
}

// ---------------------------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------------------------

/// The pages of a document, for the job. Both calls may block (they run on the job thread).
pub trait PageSource: Send + Sync {
    /// The number of pages, once the document is open. `None` when it cannot be opened.
    fn page_count(&self) -> Option<u32>;
    /// The text of `page`, `None` when it cannot be read.
    fn page_text(&self, page: u32) -> Option<Arc<PageText>>;
}

impl PageSource for DocumentHandle {
    fn page_count(&self) -> Option<u32> {
        loop {
            match self.status() {
                DocStatus::Loading => std::thread::sleep(OPEN_POLL),
                DocStatus::Ready => {
                    return self
                        .info()
                        .map(|i| u32::try_from(i.page_sizes.len()).unwrap_or(u32::MAX));
                }
                DocStatus::NeedsPassword | DocStatus::Failed(_) => return None,
            }
        }
    }

    fn page_text(&self, page: u32) -> Option<Arc<PageText>> {
        self.page_text_wait(page).ok()
    }
}

/// Monotonic time for the job's waits and measurements. Tests put in a clock they move.
pub trait Clock: Send + Sync {
    /// Time since an arbitrary fixed origin.
    fn now(&self) -> Duration;
}

/// The real clock.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> SystemClock {
        SystemClock {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// How long the job waits after the `attempt`-th (0-based) "no server" failure in a row: 30 s,
/// doubling, at most 5 minutes.
pub fn blocked_backoff(attempt: u32) -> Duration {
    let factor = 1u32 << attempt.min(8);
    (BLOCKED_BACKOFF_MIN * factor).min(BLOCKED_BACKOFF_MAX)
}

/// Where the job is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PretranslateStatus {
    /// Waiting for the document to open.
    Starting,
    /// Translating.
    Running,
    /// Paused by [`PretranslateJob::set_paused`].
    Paused,
    /// A translation the user asked for is running; the next segment waits for it.
    WaitingForeground,
    /// The backend cannot be used (no server, no model, not built); waiting before trying again.
    Blocked(TranslateError),
    /// Every page is done.
    Finished,
    /// Stopped after a page by [`PretranslateJob::stop_after_page`].
    Stopped,
    /// The document cannot be opened.
    NoDocument,
}

/// What the job has done so far.
#[derive(Clone, Debug, PartialEq)]
pub struct PretranslateSnapshot {
    /// Pages of the document (0 until it is open).
    pub page_count: u32,
    /// Pages done.
    pub pages_done: u32,
    /// The page being translated.
    pub current_page: Option<u32>,
    /// Segments translated (stored pages and the page in progress).
    pub translated: u64,
    /// Segments skipped.
    pub skipped: u64,
    /// Segments the backend failed on.
    pub failed: u64,
    /// Segments per second over the last ones the backend translated.
    pub rate: Option<f32>,
    /// An estimate of the time left.
    pub remaining: Option<Duration>,
    /// "No server" failures in a row (0 unless blocked).
    pub blocked_attempts: u32,
    /// Where the job is.
    pub status: PretranslateStatus,
}

impl Default for PretranslateSnapshot {
    fn default() -> PretranslateSnapshot {
        PretranslateSnapshot {
            page_count: 0,
            pages_done: 0,
            current_page: None,
            translated: 0,
            skipped: 0,
            failed: 0,
            rate: None,
            remaining: None,
            blocked_attempts: 0,
            status: PretranslateStatus::Starting,
        }
    }
}

impl PretranslateSnapshot {
    /// Whether the job has ended (finished, stopped, or no document).
    pub fn ended(&self) -> bool {
        matches!(
            self.status,
            PretranslateStatus::Finished
                | PretranslateStatus::Stopped
                | PretranslateStatus::NoDocument
        )
    }
}

/// Tuning of a job. [`PretranslateOptions::default`] is what the app uses.
#[derive(Clone)]
pub struct PretranslateOptions {
    /// The store is written when a page ends and at least this long has passed since the last
    /// write; always when the job pauses, stops, finishes or is dropped. `Duration::ZERO`
    /// writes after every page.
    pub save_interval: Duration,
    /// The clock for waits and measurements.
    pub clock: Arc<dyn Clock>,
}

impl Default for PretranslateOptions {
    fn default() -> PretranslateOptions {
        PretranslateOptions {
            save_interval: Duration::from_secs(15),
            clock: Arc::new(SystemClock::default()),
        }
    }
}

impl fmt::Debug for PretranslateOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PretranslateOptions")
            .field("save_interval", &self.save_interval)
            .finish_non_exhaustive()
    }
}

/// Everything a job needs.
pub struct PretranslateSetup {
    /// The backend that translates.
    pub backend: Arc<dyn TranslateBackend>,
    /// The pages of the document.
    pub source: Arc<dyn PageSource>,
    /// The document's translations so far (see [`PretranslateStore::open`]). The job fills it;
    /// the app reads it. A store whose page count differs from the document's starts over.
    pub store: SharedStore,
    /// Where the store is written.
    pub store_path: PathBuf,
    /// The page priorities.
    pub order: Arc<PretranslateOrder>,
    /// Set while a translation the user asked for runs: the next segment waits.
    pub foreground_busy: Arc<AtomicBool>,
    /// Called when the snapshot changed (at most every 250 ms, and always when a page ends or
    /// the status changes). Never after the job was dropped.
    pub notifier: Notifier,
    /// Tuning.
    pub options: PretranslateOptions,
}

struct Control {
    paused: bool,
    stop_after_page: bool,
}

struct Shared {
    cancel: AtomicBool,
    control: Mutex<Control>,
    wake: Condvar,
    snapshot: Mutex<Arc<PretranslateSnapshot>>,
}

impl Shared {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn control(&self) -> MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, snapshot: PretranslateSnapshot) {
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(snapshot);
    }

    fn get(&self) -> Arc<PretranslateSnapshot> {
        Arc::clone(&self.snapshot.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// A document being translated ahead on its own thread. Dropping it cancels it without waiting
/// (the segment in flight is thrown away; the thread writes what is not written yet).
pub struct PretranslateJob {
    shared: Arc<Shared>,
    store: SharedStore,
}

impl Drop for PretranslateJob {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}

impl fmt::Debug for PretranslateJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PretranslateJob")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl PretranslateJob {
    /// Starts the job thread and returns at once.
    pub fn start(setup: PretranslateSetup) -> PretranslateJob {
        let shared = Arc::new(Shared {
            cancel: AtomicBool::new(false),
            control: Mutex::new(Control {
                paused: false,
                stop_after_page: false,
            }),
            wake: Condvar::new(),
            snapshot: Mutex::new(Arc::new(PretranslateSnapshot::default())),
        });
        let store = Arc::clone(&setup.store);
        let worker = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("refdesk-pretranslate".to_string())
            .spawn(move || {
                let notifier = Arc::clone(&setup.notifier);
                let mut runner = Runner::new(worker, setup);
                let outcome = catch_unwind(AssertUnwindSafe(|| runner.run()));
                if outcome.is_err() {
                    // A bug must not look like a job that runs forever.
                    let shared = Arc::clone(&runner.shared);
                    let mut last = PretranslateSnapshot::clone(&shared.get());
                    last.status = PretranslateStatus::Blocked(TranslateError::Protocol(
                        "internal error".to_string(),
                    ));
                    shared.set(last);
                    if !shared.cancelled() {
                        notifier();
                    }
                }
            });
        if let Err(e) = spawned {
            shared.set(PretranslateSnapshot {
                status: PretranslateStatus::Blocked(TranslateError::Protocol(format!(
                    "cannot start a thread: {e}"
                ))),
                ..PretranslateSnapshot::default()
            });
        }
        PretranslateJob { shared, store }
    }

    /// The state now. Cheap.
    pub fn snapshot(&self) -> Arc<PretranslateSnapshot> {
        self.shared.get()
    }

    /// The store the job fills (the same one it was given).
    pub fn store(&self) -> SharedStore {
        Arc::clone(&self.store)
    }

    /// Pauses (between segments) or resumes the job.
    pub fn set_paused(&self, paused: bool) {
        self.shared.control().paused = paused;
        self.shared.wake.notify_all();
    }

    /// Asks the job to stop once the page in progress is done (it is written; the status becomes
    /// [`PretranslateStatus::Stopped`]). The app uses it to move to another document without
    /// losing the page being translated.
    pub fn stop_after_page(&self) {
        self.shared.control().stop_after_page = true;
        self.shared.wake.notify_all();
    }

    /// Wakes the thread so that it looks at its clock again (tests that move their clock).
    pub fn nudge(&self) {
        self.shared.wake.notify_all();
    }
}

/// Why a wait or a segment ended early.
enum Halt {
    /// The job was dropped.
    Cancelled,
    /// `stop_after_page` came while the page could not go on (the backend is blocked).
    Stop,
}

struct Runner {
    shared: Arc<Shared>,
    backend: Arc<dyn TranslateBackend>,
    source: Arc<dyn PageSource>,
    store: SharedStore,
    store_path: PathBuf,
    order: Arc<PretranslateOrder>,
    busy: Arc<AtomicBool>,
    notifier: Notifier,
    options: PretranslateOptions,
    snap: PretranslateSnapshot,
    last_notify: Option<Instant>,
    last_save: Option<Duration>,
    /// Translations by text hash, so that a repeated text is not sent twice.
    known: HashMap<u64, String>,
    /// Clock times at which the backend finished a segment.
    finished_at: VecDeque<Duration>,
    /// Segments the backend translated on the pages finished in this run.
    sent_in_run: u64,
    /// Segments the backend translated on the page in progress.
    sent_in_page: u64,
    pages_in_run: u64,
}

impl Runner {
    fn new(shared: Arc<Shared>, s: PretranslateSetup) -> Runner {
        Runner {
            shared,
            backend: s.backend,
            source: s.source,
            store: s.store,
            store_path: s.store_path,
            order: s.order,
            busy: s.foreground_busy,
            notifier: s.notifier,
            options: s.options,
            snap: PretranslateSnapshot::default(),
            last_notify: None,
            last_save: None,
            known: HashMap::new(),
            finished_at: VecDeque::new(),
            sent_in_run: 0,
            sent_in_page: 0,
            pages_in_run: 0,
        }
    }

    fn read_store(&self) -> std::sync::RwLockReadGuard<'_, PretranslateStore> {
        self.store.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_store(&self) -> std::sync::RwLockWriteGuard<'_, PretranslateStore> {
        self.store.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Publishes the snapshot; notifies when `force`d or 250 ms after the last notice.
    fn publish(&mut self, force: bool) {
        self.snap.rate = self.rate();
        self.snap.remaining = self.remaining();
        self.shared.set(self.snap.clone());
        if self.shared.cancelled() {
            return;
        }
        let due = self
            .last_notify
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(250));
        if force || due {
            self.last_notify = Some(Instant::now());
            (self.notifier)();
        }
    }

    fn set_status(&mut self, status: PretranslateStatus) {
        if self.snap.status != status {
            self.snap.status = status;
            self.publish(true);
        }
    }

    fn rate(&self) -> Option<f32> {
        let (first, last) = (self.finished_at.front()?, self.finished_at.back()?);
        let span = last.checked_sub(*first)?.as_secs_f32();
        let n = self.finished_at.len();
        (n >= 2 && span > 0.0).then(|| (n - 1) as f32 / span)
    }

    fn remaining(&self) -> Option<Duration> {
        let rate = f64::from(self.rate()?);
        let left = u64::from(self.snap.page_count.saturating_sub(self.snap.pages_done));
        if left == 0 {
            return Some(Duration::ZERO);
        }
        let per_page = if self.pages_in_run > 0 {
            self.sent_in_run as f64 / self.pages_in_run as f64
        } else if self.snap.pages_done > 0 {
            self.snap.translated as f64 / f64::from(self.snap.pages_done)
        } else {
            return None;
        };
        let secs = left as f64 * per_page / rate;
        secs.is_finite()
            .then(|| Duration::from_secs_f64(secs.max(0.0)))
    }

    /// Writes the store when it changed.
    fn save(&mut self) {
        let (bytes, revision) = {
            let st = self.read_store();
            if !st.dirty() {
                return;
            }
            (st.to_bytes(), st.revision())
        };
        let started = Instant::now();
        match bytes.and_then(|b| atomic_write(&self.store_path, &b).map(|()| b.len())) {
            Ok(len) => {
                self.write_store().mark_saved(revision);
                log::debug!(
                    "pretranslate: wrote {} ({len} bytes) in {:?}",
                    self.store_path.display(),
                    started.elapsed()
                );
            }
            Err(e) => log::warn!("could not save {}: {e}", self.store_path.display()),
        }
        self.last_save = Some(self.options.clock.now());
    }

    fn save_if_due(&mut self) {
        let now = self.options.clock.now();
        let due = self
            .last_save
            .is_none_or(|t| now.saturating_sub(t) >= self.options.save_interval);
        if due {
            self.save();
        }
    }

    fn run(&mut self) {
        self.publish(true);
        let Some(page_count) = self.source.page_count() else {
            if !self.shared.cancelled() {
                self.set_status(PretranslateStatus::NoDocument);
            }
            return;
        };
        {
            let mut st = self.write_store();
            if st.page_count() != page_count {
                let (f, i) = (st.fingerprint().to_string(), st.identity().to_string());
                *st = PretranslateStore::new(&f, &i, page_count);
            }
        }
        let (counts, done) = {
            let store = Arc::clone(&self.store);
            let st = store.read().unwrap_or_else(PoisonError::into_inner);
            let mut known = HashMap::new();
            for s in st.pages.iter().flatten().flatten() {
                if let Outcome::Translated(t) = &s.outcome {
                    known.insert(s.hash, t.clone());
                }
            }
            self.known = known;
            (st.counts(), st.pages_done())
        };
        self.snap.page_count = page_count;
        self.snap.pages_done = done;
        (self.snap.translated, self.snap.skipped, self.snap.failed) = counts;
        self.snap.status = PretranslateStatus::Running;
        self.publish(true);

        let end = loop {
            if self.shared.cancelled() {
                break Halt::Cancelled;
            }
            if let Err(h) = self.wait_ready(false) {
                break h;
            }
            if self.shared.control().stop_after_page {
                break Halt::Stop;
            }
            let next = {
                let st = self.read_store();
                self.order.next(page_count, |p| st.page_done(p))
            };
            let Some(page) = next else {
                self.save();
                self.snap.current_page = None;
                self.set_status(PretranslateStatus::Finished);
                return;
            };
            self.snap.current_page = Some(page);
            self.publish(false);
            match self.do_page(page) {
                Ok(()) => {}
                Err(h) => break h,
            }
        };
        self.save();
        if let Halt::Stop = end {
            self.snap.current_page = None;
            self.set_status(PretranslateStatus::Stopped);
        }
    }

    /// Waits while paused (and, before a segment, while the foreground is busy). `Err` when the
    /// job was dropped.
    fn wait_ready(&mut self, before_segment: bool) -> Result<(), Halt> {
        loop {
            if self.shared.cancelled() {
                return Err(Halt::Cancelled);
            }
            let ctl = self.shared.control();
            if ctl.paused {
                if self.snap.status != PretranslateStatus::Paused {
                    drop(ctl);
                    self.save();
                    self.set_status(PretranslateStatus::Paused);
                    continue;
                }
                let _ctl = self
                    .shared
                    .wake
                    .wait(ctl)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            if before_segment && self.busy.load(Ordering::Acquire) {
                if self.snap.status != PretranslateStatus::WaitingForeground {
                    drop(ctl);
                    self.set_status(PretranslateStatus::WaitingForeground);
                    continue;
                }
                let _ctl = self
                    .shared
                    .wake
                    .wait_timeout(ctl, FOREGROUND_POLL)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            drop(ctl);
            if matches!(
                self.snap.status,
                PretranslateStatus::Paused | PretranslateStatus::WaitingForeground
            ) {
                self.set_status(PretranslateStatus::Running);
            }
            return Ok(());
        }
    }

    /// Waits until the clock reaches `until`. `Err` when the job was dropped or asked to stop.
    fn wait_until(&self, until: Duration) -> Result<(), Halt> {
        loop {
            if self.shared.cancelled() {
                return Err(Halt::Cancelled);
            }
            let now = self.options.clock.now();
            if now >= until {
                return Ok(());
            }
            let ctl = self.shared.control();
            if ctl.stop_after_page {
                return Err(Halt::Stop);
            }
            let _ctl = self
                .shared
                .wake
                .wait_timeout(ctl, (until - now).min(CLOCK_POLL))
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn do_page(&mut self, page: u32) -> Result<(), Halt> {
        let segs = match self.source.page_text(page) {
            Some(text) => segments(&text),
            // A page that cannot be read has nothing to translate.
            None => Vec::new(),
        };
        let mut stored = Vec::with_capacity(segs.len());
        self.sent_in_page = 0;
        for seg in segs {
            if self.shared.cancelled() {
                return Err(Halt::Cancelled);
            }
            let hash = text_hash(&seg.text);
            let outcome = if seg.skip.is_some() {
                Outcome::Skipped
            } else if let Some(t) = self.known.get(&hash) {
                Outcome::Translated(t.clone())
            } else {
                let outcome = self.translate(&seg.text)?;
                if let Outcome::Translated(t) = &outcome {
                    self.known.insert(hash, t.clone());
                }
                outcome
            };
            match outcome {
                Outcome::Translated(_) => self.snap.translated += 1,
                Outcome::Skipped => self.snap.skipped += 1,
                Outcome::Failed => self.snap.failed += 1,
            }
            stored.push(StoredSegment {
                chars: seg.chars,
                hash,
                outcome,
            });
            self.publish(false);
        }
        self.write_store().set_page(page, stored);
        self.pages_in_run += 1;
        self.sent_in_run += self.sent_in_page;
        self.sent_in_page = 0;
        let done = self.read_store().pages_done();
        self.snap.pages_done = done;
        self.save_if_due();
        self.publish(true);
        Ok(())
    }

    /// Sends one segment, waiting out "no server" failures and trying other failures again.
    fn translate(&mut self, text: &str) -> Result<Outcome, Halt> {
        let req = TranslateRequest {
            text: text.to_string(),
            kind: TranslateKind::Passage,
        };
        let mut tries = 0;
        let mut blocked = 0;
        loop {
            self.wait_ready(true)?;
            let mut raw = String::new();
            let result = self
                .backend
                .translate(&req, &self.shared.cancel, &mut |piece| raw.push_str(piece));
            if self.shared.cancelled() {
                return Err(Halt::Cancelled);
            }
            let result = result.and_then(|()| {
                let t = clean_output(&req.kind, &raw);
                if t.is_empty() {
                    Err(TranslateError::Protocol("empty answer".to_string()))
                } else {
                    Ok(t)
                }
            });
            match result {
                Ok(t) => {
                    self.sent_in_page += 1;
                    self.finished_at.push_back(self.options.clock.now());
                    while self.finished_at.len() > RATE_WINDOW {
                        self.finished_at.pop_front();
                    }
                    if matches!(self.snap.status, PretranslateStatus::Blocked(_)) {
                        self.snap.blocked_attempts = 0;
                        self.set_status(PretranslateStatus::Running);
                    }
                    return Ok(Outcome::Translated(t));
                }
                Err(
                    e @ (TranslateError::Unreachable(_)
                    | TranslateError::ModelMissing(_)
                    | TranslateError::NotBuilt),
                ) => {
                    log::info!("pretranslate: blocked: {e}");
                    self.save();
                    let until = self.options.clock.now() + blocked_backoff(blocked);
                    blocked += 1;
                    self.snap.blocked_attempts = blocked;
                    self.snap.status = PretranslateStatus::Blocked(e);
                    self.publish(true);
                    self.wait_until(until)?;
                }
                Err(e) => {
                    tries += 1;
                    log::warn!("pretranslate: segment failed (try {tries}): {e}");
                    if tries >= SEGMENT_TRIES {
                        return Ok(Outcome::Failed);
                    }
                }
            }
        }
    }
}
