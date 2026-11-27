//! Background translation: the documents of a project translated ahead of reading, sentence by
//! sentence (`docs/ARCHITECTURE.md` §4.16, TR-15 ~ 19).
//!
//! The work is core's [`PretranslateJob`]; this module decides what it works on and shows how
//! far it got:
//!
//! * It is only for projects (`refdesk_core::project`). Whether it is on, and paused, is kept in
//!   the project's own `pretranslate.json`.
//! * One document at a time ([`pick_document`]): the focused PDF tab's document, else another
//!   open project document, else the rest of the project in explorer order (opened in the
//!   background, closed when done). A focus that moves to another project document takes the
//!   job there once the page in progress is done ([`plan`]).
//! * The page being read in the focused view goes to the job's page order half a second after
//!   the scrolling stopped ([`PageReporter`]).
//! * A translation the user asked for comes first: the shared [`Hub::busy`] flag is raised when a
//!   popup starts a job and while the model loads; the job does not send a segment while it is up.
//! * Popups read the stores of the open project documents ([`Lookup`]); a store is read on a
//!   thread when its document opens, and the job fills the same copy.
//! * It pauses on battery power ([`on_battery`], read every 30 seconds on a thread) unless
//!   `[translate] pretranslate_on_battery` says otherwise.
//!
//! Only `dispatch` changes this state through [`toggle`], [`toggle_pause`] and [`clear`] (R-6);
//! [`tick`] runs in `logic()`. Files, fingerprints and the server are only touched on threads
//! (R-1). The decisions and texts are plain functions, tested without egui (R-10).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use egui::Ui;
use refdesk_core::project;
use refdesk_core::translate::pretranslate::{list, remove_all};
use refdesk_core::translate::{
    PageSource, PretranslateJob, PretranslateOptions, PretranslateOrder, PretranslateSettings,
    PretranslateSetup, PretranslateSnapshot, PretranslateStatus, PretranslateStore, SharedStore,
    store_path,
};
use refdesk_core::{DocumentHandle, TranslateError, fingerprint_file};

use crate::action::{Action, notifier};
use crate::i18n::tr;
use crate::overlays::translate::{FailureHint, Readiness, error_text};
use crate::panels::toc::reading_location;
use crate::state::{AppState, StatusNotice, Tab, normalize_path};

/// Seconds after the app starts (or a project opens) before a job that was on goes on.
pub const RESUME_DELAY: f64 = 10.0;
/// Seconds the page being read must stay the same before the job hears of it.
pub const PAGE_SETTLE: f64 = 0.5;
/// How often the power supply is read.
pub const POWER_POLL: Duration = Duration::from_secs(30);
/// Where the power supplies are described.
pub const POWER_SUPPLY_DIR: &str = "/sys/class/power_supply";
/// Seconds a notice stays in the status bar.
const NOTICE_SECS: f64 = 3.0;
/// How long a clearing waits for the job thread to let go of its store.
const CLEAR_WAIT: Duration = Duration::from_secs(10);

// ------------------------------------------------------------------------------------------------
// Pure decisions
// ------------------------------------------------------------------------------------------------

/// Whether the machine runs on battery: there are mains supplies (`type` is `Mains`) under
/// `dir` and none is `online`. A machine without one (a desktop) is on mains.
pub fn on_battery(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut mains = 0;
    let mut online = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
        if kind.trim() != "Mains" {
            continue;
        }
        mains += 1;
        let state = std::fs::read_to_string(path.join("online")).unwrap_or_default();
        if state.trim() == "1" {
            online += 1;
        }
    }
    mains > 0 && online == 0
}

/// Why the job is paused, if it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseReason {
    /// The user paused it.
    User,
    /// The machine runs on battery.
    Battery,
}

/// The pause in effect: the user's, else the battery's (unless allowed on battery).
pub fn pause_reason(user: bool, battery: bool, allow_on_battery: bool) -> Option<PauseReason> {
    if user {
        Some(PauseReason::User)
    } else if battery && !allow_on_battery {
        Some(PauseReason::Battery)
    } else {
        None
    }
}

/// The document to translate. `eligible` says whether a document is in the project and not
/// finished. In order: the focused document; the current one if it is open; another open
/// document (in the given order); the current one; the rest of the project (explorer order).
/// The current document is kept where the order allows, so that a job does not hop between
/// documents of the same rank.
pub fn pick_document(
    focused: Option<&Path>,
    current: Option<&Path>,
    open: &[PathBuf],
    project_files: &[PathBuf],
    eligible: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(f) = focused.filter(|f| eligible(f)) {
        return Some(f.to_path_buf());
    }
    let current = current.filter(|c| eligible(c));
    if let Some(c) = current.filter(|c| open.iter().any(|o| o == c)) {
        return Some(c.to_path_buf());
    }
    if let Some(o) = open.iter().find(|o| eligible(o)) {
        return Some(o.clone());
    }
    if let Some(c) = current {
        return Some(c.to_path_buf());
    }
    project_files.iter().find(|p| eligible(p)).cloned()
}

/// What to do with the job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Leave it as it is.
    Keep,
    /// Let it finish its page, then stop (another document comes first).
    StopAfterPage,
    /// Start one for this document.
    Start(PathBuf),
}

/// The step from the current job (`current`, `stopping` when it was already asked to stop) to
/// the `desired` document.
pub fn plan(current: Option<&Path>, stopping: bool, desired: Option<&Path>) -> Plan {
    match (current, desired) {
        (None, Some(d)) => Plan::Start(d.to_path_buf()),
        (None, None) => Plan::Keep,
        (Some(c), Some(d)) if c == d => Plan::Keep,
        (Some(_), _) if stopping => Plan::Keep,
        (Some(_), _) => Plan::StopAfterPage,
    }
}

/// Tells the page being read once it stayed the same for [`PAGE_SETTLE`] seconds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageReporter {
    pending: Option<(u32, f64)>,
    reported: Option<u32>,
}

impl PageReporter {
    /// The page to report at `now`, if one is due (each page once).
    pub fn step(&mut self, now: f64, page: Option<u32>) -> Option<u32> {
        let page = page?;
        match self.pending {
            Some((p, _)) if p == page => {}
            _ => self.pending = Some((page, now)),
        }
        let (p, since) = self.pending?;
        if now - since >= PAGE_SETTLE && self.reported != Some(p) {
            self.reported = Some(p);
            return Some(p);
        }
        None
    }

    /// Seconds until the pending page is due, if one is waiting.
    pub fn wait(&self, now: f64) -> Option<f64> {
        let (p, since) = self.pending?;
        (self.reported != Some(p)).then(|| (since + PAGE_SETTLE - now).max(0.0))
    }

    /// Forgets everything (another document got the focus).
    pub fn reset(&mut self) {
        *self = PageReporter::default();
    }
}

// ------------------------------------------------------------------------------------------------
// Texts
// ------------------------------------------------------------------------------------------------

/// What the status bar says about the job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shown {
    /// Getting ready: the delay after start, reading the store, opening the document.
    Preparing,
    /// Translating.
    Running,
    /// Paused by the user.
    Paused,
    /// Paused because the machine runs on battery.
    Battery,
    /// Waiting for a translation the user asked for.
    Foreground,
    /// The backend cannot be used; the text says why.
    Blocked(String),
    /// Every document of the project is done.
    Done,
}

/// The state shown for a job snapshot (or none) and the pause in effect.
pub fn shown(
    snapshot: Option<&PretranslateSnapshot>,
    pause: Option<PauseReason>,
    all_done: bool,
    hint: &FailureHint,
) -> Shown {
    match pause {
        Some(PauseReason::User) => return Shown::Paused,
        Some(PauseReason::Battery) => return Shown::Battery,
        None => {}
    }
    let Some(snap) = snapshot else {
        return if all_done {
            Shown::Done
        } else {
            Shown::Preparing
        };
    };
    match &snap.status {
        PretranslateStatus::Running => Shown::Running,
        PretranslateStatus::Paused => Shown::Paused,
        PretranslateStatus::WaitingForeground => Shown::Foreground,
        PretranslateStatus::Blocked(e) => Shown::Blocked(blocked_reason(e, hint)),
        PretranslateStatus::Finished if all_done => Shown::Done,
        PretranslateStatus::Starting
        | PretranslateStatus::Finished
        | PretranslateStatus::Stopped
        | PretranslateStatus::NoDocument => Shown::Preparing,
    }
}

fn blocked_reason(e: &TranslateError, hint: &FailureHint) -> String {
    error_text(e, hint)
}

/// "37%" of the pages of a document, rounded down (100% only when all are done).
pub fn percent(done: u32, total: u32) -> Option<u32> {
    (total > 0).then(|| ((u64::from(done) * 100) / u64::from(total)) as u32)
}

/// The text of the status bar indicator: "Translating ahead 37%", with the state after it.
pub fn label(shown: &Shown, percent: Option<u32>) -> String {
    let base = match percent {
        Some(p) => tr("pretranslate.label").replace("{}", &p.to_string()),
        None => tr("pretranslate.label_plain").to_owned(),
    };
    match shown {
        Shown::Running | Shown::Preparing => base,
        Shown::Foreground => format!("{base} · {}", tr("pretranslate.short_foreground")),
        Shown::Paused => format!("{base} · {}", tr("pretranslate.short_paused")),
        Shown::Battery => format!("{base} · {}", tr("pretranslate.short_battery")),
        Shown::Blocked(_) => tr("pretranslate.label_blocked").to_owned(),
        Shown::Done => tr("pretranslate.label_done").to_owned(),
    }
}

/// The state in words, for the hover text and the accessible name.
pub fn state_text(shown: &Shown) -> String {
    match shown {
        Shown::Preparing => tr("pretranslate.state_preparing").to_owned(),
        Shown::Running => tr("pretranslate.state_running").to_owned(),
        Shown::Paused => tr("pretranslate.state_paused").to_owned(),
        Shown::Battery => tr("pretranslate.state_battery").to_owned(),
        Shown::Foreground => tr("pretranslate.state_foreground").to_owned(),
        Shown::Blocked(why) => tr("pretranslate.state_blocked").replace("{}", why),
        Shown::Done => tr("pretranslate.state_done").to_owned(),
    }
}

/// `1234567` as "1,234,567".
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A time left in hours and minutes: "about 2 h 5 min", "less than a minute".
pub fn duration_text(d: Duration) -> String {
    let minutes = d.as_secs().div_ceil(60);
    if d.as_secs() < 60 {
        return tr("pretranslate.eta_under_minute").to_owned();
    }
    let (h, m) = (minutes / 60, minutes % 60);
    if h == 0 {
        tr("pretranslate.eta_minutes").replace("{m}", &m.to_string())
    } else {
        tr("pretranslate.eta_hours")
            .replace("{h}", &h.to_string())
            .replace("{m}", &m.to_string())
    }
}

/// What the hover text of the indicator tells.
#[derive(Clone, Debug, PartialEq)]
pub struct Details {
    /// File name of the document being translated.
    pub doc: Option<String>,
    /// Its pages done and pages.
    pub pages: Option<(u32, u32)>,
    /// Segments per second.
    pub rate: Option<f32>,
    /// Time left for the document.
    pub remaining: Option<Duration>,
    /// The state.
    pub shown: Shown,
    /// Documents of the project done, and documents.
    pub project: Option<(usize, usize)>,
    /// Paused (by the user): a click resumes.
    pub paused: bool,
}

/// The hover text: document and pages, speed and time left, state, the project, what a click
/// does.
pub fn tooltip(d: &Details) -> String {
    let mut lines = Vec::new();
    match (&d.doc, d.pages) {
        (Some(doc), Some((done, total))) => lines.push(
            tr("pretranslate.tip_pages")
                .replace("{doc}", doc)
                .replace("{done}", &thousands(u64::from(done)))
                .replace("{total}", &thousands(u64::from(total))),
        ),
        (Some(doc), None) => lines.push(doc.clone()),
        _ => {}
    }
    if let Some(rate) = d.rate {
        let mut line = tr("pretranslate.tip_rate").replace("{}", &format!("{rate:.1}"));
        if let Some(left) = d.remaining {
            line = format!(
                "{line} · {}",
                tr("pretranslate.tip_left").replace("{}", &duration_text(left))
            );
        }
        lines.push(line);
    }
    lines.push(tr("pretranslate.tip_state").replace("{}", &state_text(&d.shown)));
    if let Some((done, total)) = d.project {
        lines.push(
            tr("pretranslate.tip_project")
                .replace("{done}", &done.to_string())
                .replace("{total}", &total.to_string()),
        );
    }
    lines.push(
        if d.paused {
            tr("pretranslate.tip_click_resume")
        } else {
            tr("pretranslate.tip_click_pause")
        }
        .to_owned(),
    );
    lines.join("\n")
}

/// The accessible name of the indicator: "Background translation: Translating ahead 37%,
/// running".
pub fn accessible_name(label: &str, shown: &Shown) -> String {
    format!(
        "{}: {label}, {}",
        tr("cmd.view.pretranslate"),
        state_text(shown)
    )
}

// ------------------------------------------------------------------------------------------------
// What popups share with the job
// ------------------------------------------------------------------------------------------------

/// Shared by the popups and the job.
#[derive(Debug, Default)]
pub struct Hub {
    /// Up while a translation the user asked for runs (or the model loads): the job waits.
    pub busy: Arc<AtomicBool>,
    misses: Mutex<Vec<(PathBuf, u32)>>,
}

impl Hub {
    /// A popup had to translate on the spot on `page` of `doc`: that page goes first.
    pub fn missed(&self, doc: &Path, page: u32) {
        let mut m = self.misses.lock().unwrap_or_else(PoisonError::into_inner);
        if !m.iter().any(|(d, p)| d == doc && *p == page) {
            m.push((doc.to_path_buf(), page));
        }
    }

    /// A popup starts a job of its own.
    pub fn foreground_started(&self) {
        self.busy.store(true, Ordering::Release);
    }

    /// The pages missed and not yet routed (tests).
    #[cfg(test)]
    pub fn misses(&self) -> Vec<(PathBuf, u32)> {
        self.misses
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn take_misses(&self) -> Vec<(PathBuf, u32)> {
        std::mem::take(&mut *self.misses.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The stores the popups may read, by the path the views know their document by. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct Lookup {
    /// Shared with the job.
    pub hub: Arc<Hub>,
    stores: Arc<HashMap<PathBuf, SharedStore>>,
}

/// What one view's popup gets.
#[derive(Clone, Debug)]
pub struct PopupPre {
    /// Shared with the job.
    pub hub: Arc<Hub>,
    /// The document (as the job knows it) and its store, when it has one.
    pub doc: Option<(PathBuf, SharedStore)>,
}

impl Lookup {
    /// A lookup with one store (tests).
    #[cfg(test)]
    pub fn single(hub: Arc<Hub>, path: &Path, store: SharedStore) -> Lookup {
        Lookup {
            hub,
            stores: Arc::new(HashMap::from([(path.to_path_buf(), store)])),
        }
    }

    /// What the view of the document opened from `path` gets.
    pub fn resolve(&self, path: &Path) -> PopupPre {
        PopupPre {
            hub: Arc::clone(&self.hub),
            doc: self
                .stores
                .get(path)
                .map(|s| (path.to_path_buf(), Arc::clone(s))),
        }
    }
}

// ------------------------------------------------------------------------------------------------
// State
// ------------------------------------------------------------------------------------------------

/// The open project.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Project {
    root: PathBuf,
    data: PathBuf,
}

/// A document's store, read on a thread.
#[derive(Clone)]
struct Loaded {
    store: SharedStore,
    path: PathBuf,
    complete: bool,
}

type LoadResult = Arc<Mutex<Option<Option<Loaded>>>>;

enum SlotState {
    Loading(LoadResult),
    Ready(Loaded),
    Failed,
}

/// A project document the app keeps a store for: an open one, or the one the job works on.
struct Slot {
    handle: DocumentHandle,
    generation: u32,
    state: SlotState,
}

/// The job and the document it works on.
struct Running {
    doc: PathBuf,
    job: PretranslateJob,
    order: Arc<PretranslateOrder>,
    store: SharedStore,
    store_path: PathBuf,
    stopping: bool,
    paused: bool,
}

/// Background translation, one for the app (`AppState.pretranslate`).
pub struct PretranslateApp {
    /// On and paused, for the open project.
    pub settings: PretranslateSettings,
    /// The machine runs on battery (set by the power thread; tests set it).
    pub on_battery: Arc<AtomicBool>,
    /// Tuning of the jobs (tests write after every page).
    pub options: PretranslateOptions,
    /// The confirmation of "Clear Translations Made Ahead" is open.
    pub confirm_clear: bool,
    project: Option<Project>,
    checked_root: Option<PathBuf>,
    checked_at: f64,
    identity: String,
    hub: Arc<Hub>,
    lookup: Lookup,
    lookup_dirty: bool,
    slots: HashMap<PathBuf, Slot>,
    /// View paths by their normalized form.
    aliases: HashMap<PathBuf, PathBuf>,
    running: Option<Running>,
    /// Documents found done (or unreadable) since the project or the model changed.
    finished: HashSet<PathBuf>,
    /// Pages popups missed, for documents the job is not on yet.
    urgent: HashMap<PathBuf, Vec<u32>>,
    resume_at: Option<f64>,
    reporter: PageReporter,
    reporter_doc: Option<PathBuf>,
    /// The page last reported for a document.
    reading: Option<(PathBuf, u32)>,
    /// The last snapshot and its document, for the status bar between jobs.
    last: Option<(PathBuf, Arc<PretranslateSnapshot>)>,
    progress: Arc<Mutex<Option<(usize, usize)>>>,
    clearing: Option<Arc<AtomicBool>>,
    power: Option<PowerWatch>,
}

impl Default for PretranslateApp {
    fn default() -> PretranslateApp {
        let hub = Arc::new(Hub::default());
        PretranslateApp {
            settings: PretranslateSettings::default(),
            on_battery: Arc::new(AtomicBool::new(false)),
            options: PretranslateOptions::default(),
            confirm_clear: false,
            project: None,
            checked_root: None,
            checked_at: f64::NEG_INFINITY,
            identity: String::new(),
            lookup: Lookup {
                hub: Arc::clone(&hub),
                stores: Arc::default(),
            },
            hub,
            lookup_dirty: false,
            slots: HashMap::new(),
            aliases: HashMap::new(),
            running: None,
            finished: HashSet::new(),
            urgent: HashMap::new(),
            resume_at: None,
            reporter: PageReporter::default(),
            reporter_doc: None,
            reading: None,
            last: None,
            progress: Arc::default(),
            clearing: None,
            power: None,
        }
    }
}

impl PretranslateApp {
    /// A project is open: the feature can be used.
    pub fn has_project(&self) -> bool {
        self.project.is_some()
    }

    /// The feature is on for the open project, so the model stays loaded.
    pub fn keeps_model(&self) -> bool {
        self.project.is_some() && self.settings.enabled
    }

    /// The stores the popups read.
    pub fn lookup(&self) -> Lookup {
        self.lookup.clone()
    }

    /// The document the job works on.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn current_doc(&self) -> Option<&Path> {
        self.running.as_ref().map(|r| r.doc.as_path())
    }

    /// The job's snapshot.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn snapshot(&self) -> Option<Arc<PretranslateSnapshot>> {
        self.running.as_ref().map(|r| r.job.snapshot())
    }

    /// The page order of the job.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn order(&self) -> Option<Arc<PretranslateOrder>> {
        self.running.as_ref().map(|r| Arc::clone(&r.order))
    }

    /// The store of a document the app keeps, by its normalized path.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn store_of(&self, doc: &Path) -> Option<SharedStore> {
        match &self.slots.get(doc)?.state {
            SlotState::Ready(l) => Some(Arc::clone(&l.store)),
            _ => None,
        }
    }

    /// Drops the job (it writes what it has on its own thread) and the stores.
    fn reset(&mut self) {
        self.running = None;
        self.slots.clear();
        self.finished.clear();
        self.urgent.clear();
        self.last = None;
        self.lookup_dirty = true;
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn pause(&self, allow_on_battery: bool) -> Option<PauseReason> {
        pause_reason(
            self.settings.paused,
            self.on_battery.load(Ordering::Relaxed),
            allow_on_battery,
        )
    }

    fn save_settings(&self) {
        if let Some(p) = &self.project
            && let Err(e) = self.settings.save(&p.data)
        {
            log::warn!(
                "could not save {}: {e}",
                PretranslateSettings::path(&p.data).display()
            );
        }
    }
}

/// Reads the power supply every [`POWER_POLL`] on a thread; stops when dropped.
struct PowerWatch {
    stop: Arc<AtomicBool>,
}

impl Drop for PowerWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl PowerWatch {
    fn start(flag: Arc<AtomicBool>, dir: PathBuf, ctx: &egui::Context) -> Option<PowerWatch> {
        let stop = Arc::new(AtomicBool::new(false));
        let (s, ctx) = (Arc::clone(&stop), ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("refdesk-power".into())
            .spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    let battery = on_battery(&dir);
                    if flag.swap(battery, Ordering::Relaxed) != battery {
                        ctx.request_repaint();
                    }
                    for _ in 0..POWER_POLL.as_secs() {
                        if s.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            });
        match spawned {
            Ok(_) => Some(PowerWatch { stop }),
            Err(e) => {
                log::warn!("cannot watch the power supply: {e}");
                None
            }
        }
    }
}

/// At start: the power thread.
pub fn start(state: &mut AppState, ctx: &egui::Context) {
    let p = &mut state.pretranslate;
    p.power = PowerWatch::start(
        Arc::clone(&p.on_battery),
        PathBuf::from(POWER_SUPPLY_DIR),
        ctx,
    );
}

fn notice(state: &mut AppState, text: String, ctx: &egui::Context) {
    state.status_notice = Some(StatusNotice {
        text,
        until: ctx.input(|i| i.time) + NOTICE_SECS,
    });
    ctx.request_repaint();
}

// ------------------------------------------------------------------------------------------------
// Dispatch
// ------------------------------------------------------------------------------------------------

/// `TogglePretranslate`: switches background translation for the open project, remembers it in
/// the project, loads the model and starts at once when it goes on; drops the job when it goes
/// off (and lets the model go when translation mode is off too).
pub fn toggle(state: &mut AppState, ctx: &egui::Context) {
    if state.pretranslate.project.is_none() {
        return;
    }
    let on = !state.pretranslate.settings.enabled;
    let p = &mut state.pretranslate;
    p.settings.enabled = on;
    p.settings.paused = false;
    p.save_settings();
    if on {
        p.resume_at = None;
        crate::overlays::translate::warm_up_for_background(state, ctx);
        notice(state, tr("status.pretranslate_on").to_owned(), ctx);
    } else {
        p.running = None;
        crate::overlays::translate::release_if_unused(state);
        notice(state, tr("status.pretranslate_off").to_owned(), ctx);
    }
}

/// `TogglePretranslatePause`: pauses or resumes, remembered in the project.
pub fn toggle_pause(state: &mut AppState, ctx: &egui::Context) {
    let p = &mut state.pretranslate;
    if p.project.is_none() || !p.settings.enabled {
        return;
    }
    p.settings.paused = !p.settings.paused;
    p.save_settings();
    ctx.request_repaint();
}

/// `ClearPretranslations`: unconfirmed, asks; confirmed, drops the job and the stores and
/// removes the project's translations on a thread once the job has let go of its store.
pub fn clear(state: &mut AppState, confirmed: bool, ctx: &egui::Context) {
    let p = &mut state.pretranslate;
    let Some(project) = p.project.clone() else {
        return;
    };
    if !confirmed {
        p.confirm_clear = true;
        return;
    }
    p.confirm_clear = false;
    let held: Vec<Weak<_>> = p
        .running
        .iter()
        .map(|r| Arc::downgrade(&r.store))
        .chain(p.slots.values().filter_map(|s| match &s.state {
            SlotState::Ready(l) => Some(Arc::downgrade(&l.store)),
            _ => None,
        }))
        .collect();
    p.reset();
    // The views hold the stores until their next frame, the job thread until it ends.
    p.lookup = Lookup {
        hub: Arc::clone(&p.hub),
        stores: Arc::default(),
    };
    let done = Arc::new(AtomicBool::new(false));
    p.clearing = Some(Arc::clone(&done));
    let wake = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("refdesk-pretranslate-clear".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            while held.iter().any(|w| w.strong_count() > 0) && started.elapsed() < CLEAR_WAIT {
                std::thread::sleep(Duration::from_millis(20));
            }
            if let Err(e) = remove_all(&project.data) {
                log::warn!("could not remove the translations made ahead: {e}");
            }
            done.store(true, Ordering::Release);
            wake.request_repaint();
        });
    if let Err(e) = spawned {
        log::warn!("cannot remove the translations made ahead: {e}");
        p.clearing = None;
    }
    notice(state, tr("status.pretranslate_cleared").to_owned(), ctx);
}

/// At exit: the job's store is written now and the job dropped.
pub fn save_now(state: &mut AppState) {
    let Some(r) = state.pretranslate.running.take() else {
        return;
    };
    drop(r.job);
    let st = r.store.read().unwrap_or_else(PoisonError::into_inner);
    if st.dirty() {
        let revision = st.revision();
        match st.save(&r.store_path) {
            Ok(()) => {
                drop(st);
                r.store
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .mark_saved(revision);
            }
            Err(e) => log::warn!("could not save {}: {e}", r.store_path.display()),
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Per frame
// ------------------------------------------------------------------------------------------------

/// Per frame, in `logic()`: follows the project, the backend and the open documents, keeps the
/// stores, raises the "user's translation first" flag, tells the job the page being read and
/// what popups missed, pauses it, and starts, moves and ends it.
pub fn tick(state: &mut AppState, ctx: &egui::Context) {
    let now = ctx.input(|i| i.time);
    follow_project(state, now);
    let identity = state.translate.backend.identity();
    if identity != state.pretranslate.identity {
        state.pretranslate.reset();
        state.pretranslate.identity = identity;
    }
    update_busy(state);
    if let Some(done) = &state.pretranslate.clearing
        && done.load(Ordering::Acquire)
    {
        state.pretranslate.clearing = None;
        state.pretranslate.reset();
    }
    if state.pretranslate.project.is_none() {
        rebuild_lookup(state);
        return;
    }
    let desired = desired_document(state, now);
    sync_slots(state, desired.as_deref(), ctx);
    rebuild_lookup(state);
    route_misses(state);
    report_page(state, now, ctx);
    apply_pause(state);
    drive(state, desired, ctx);
}

/// Notices a project opened or closed (the workspace changed, or its project file appeared).
fn follow_project(state: &mut AppState, now: f64) {
    let root = state.workspace.as_ref().map(|w| w.root.clone());
    let p = &mut state.pretranslate;
    // A folder that is not a project yet is looked at again every few seconds (it may become
    // one); a stat is cheap.
    let recheck = p.project.is_none() && root.is_some() && now - p.checked_at >= 5.0;
    if root == p.checked_root && !recheck {
        return;
    }
    p.checked_at = now;
    p.checked_root.clone_from(&root);
    let project = root.filter(|r| project::is_project(r)).map(|root| Project {
        data: project::data_dir(&root),
        root,
    });
    if project == p.project {
        return;
    }
    p.reset();
    p.settings = project
        .as_ref()
        .map(|pr| PretranslateSettings::load(&pr.data))
        .unwrap_or_default();
    p.resume_at = (project.is_some() && p.settings.enabled).then_some(now + RESUME_DELAY);
    p.project = project;
}

/// Up while a popup's own job runs or the model loads.
fn update_busy(state: &AppState) {
    let popup = state.tabs.iter().any(|(_, t)| match t {
        Tab::Pdf(p) => p.ui.translate.popup().is_some_and(|p| p.running()),
        _ => false,
    });
    let loading = state.translate.readiness == Readiness::Loading;
    state
        .pretranslate
        .hub
        .busy
        .store(popup || loading, Ordering::Release);
}

/// The normalized path of a document as a view knows it (worked out once per path).
fn normalized(aliases: &mut HashMap<PathBuf, PathBuf>, path: &Path) -> PathBuf {
    aliases
        .entry(path.to_path_buf())
        .or_insert_with(|| normalize_path(path))
        .clone()
}

/// The open project documents (normalized, in tab order) and the focused one.
fn open_documents(state: &mut AppState) -> (Vec<PathBuf>, Option<PathBuf>) {
    let Some(root) = state.pretranslate.project.as_ref().map(|p| p.root.clone()) else {
        return (Vec::new(), None);
    };
    let mut tabs: Vec<_> = state.tabs.iter().map(|(id, _)| id).collect();
    tabs.sort();
    let mut open = Vec::new();
    let mut focused = None;
    for id in tabs {
        let Some(pdf) = state.tabs.pdf(id) else {
            continue;
        };
        let Some(doc) = state.docs.get(pdf.doc) else {
            continue;
        };
        let path = normalized(&mut state.pretranslate.aliases, doc.path());
        if !path.starts_with(&root) {
            continue;
        }
        if state.focus == Some(id) {
            focused = Some(path.clone());
        }
        if !open.contains(&path) {
            open.push(path);
        }
    }
    (open, focused)
}

/// The project's PDFs in explorer order.
fn project_files(state: &AppState) -> Vec<PathBuf> {
    let Some(index) = state.workspace.as_ref().and_then(|w| w.scan.index()) else {
        return Vec::new();
    };
    index.files().map(|i| index.abs_path(i)).collect()
}

/// The document the job should work on now (`None` while it is off or waiting).
fn desired_document(state: &mut AppState, now: f64) -> Option<PathBuf> {
    let p = &state.pretranslate;
    if !p.settings.enabled || p.clearing.is_some() {
        return None;
    }
    if p.resume_at.is_some_and(|t| now < t) {
        return None;
    }
    let (open, focused) = open_documents(state);
    let files = project_files(state);
    let p = &state.pretranslate;
    let root = p.project.as_ref().map(|p| p.root.clone())?;
    let current = p.running.as_ref().map(|r| r.doc.clone());
    pick_document(focused.as_deref(), current.as_deref(), &open, &files, |d| {
        d.starts_with(&root) && !p.finished.contains(d)
    })
}

/// Keeps a store for every open project document and for the document the job needs; reads
/// new ones on threads and takes what they read.
fn sync_slots(state: &mut AppState, desired: Option<&Path>, ctx: &egui::Context) {
    let (open, _) = open_documents(state);
    let AppState {
        docs, pretranslate, ..
    } = state;
    let p = pretranslate;
    let Some(project) = p.project.clone() else {
        return;
    };
    // The open documents' own handles, by normalized path.
    let mut handles: HashMap<PathBuf, DocumentHandle> = HashMap::new();
    for (_, doc) in docs.iter() {
        let path = normalized(&mut p.aliases, doc.path());
        if open.contains(&path) {
            handles.insert(path, doc.clone());
        }
    }
    let mut wanted: Vec<PathBuf> = open.clone();
    wanted.extend(desired.map(Path::to_path_buf));
    wanted.extend(p.running.as_ref().map(|r| r.doc.clone()));
    let before = p.slots.len();
    p.slots.retain(|k, _| wanted.contains(k));
    let mut changed = p.slots.len() != before;
    for doc in wanted {
        let generation = handles
            .get(&doc)
            .and_then(DocumentHandle::info)
            .map_or(0, |i| i.generation);
        let stale = p.slots.get(&doc).is_some_and(|s| {
            handles.contains_key(&doc) && s.generation != generation && generation != 0
        });
        if stale {
            // The file changed (R-7): its fingerprint and text did too.
            if p.running.as_ref().is_some_and(|r| r.doc == doc) {
                p.running = None;
            }
            p.slots.remove(&doc);
            p.finished.remove(&doc);
        }
        if !p.slots.contains_key(&doc) {
            let handle = match handles.get(&doc) {
                Some(h) => h.clone(),
                // Not open in a tab: the job opens it in the background (one at a time).
                None => DocumentHandle::open(&doc, notifier(ctx)),
            };
            let result = load_store(&handle, &doc, &project.data, &p.identity, ctx);
            p.slots.insert(
                doc.clone(),
                Slot {
                    handle,
                    generation,
                    state: SlotState::Loading(result),
                },
            );
            changed = true;
        }
    }
    for slot in p.slots.values_mut() {
        if let SlotState::Loading(result) = &slot.state {
            let taken = result.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(loaded) = taken {
                slot.state = match loaded {
                    Some(l) => SlotState::Ready(l),
                    None => SlotState::Failed,
                };
                if slot.generation == 0 {
                    slot.generation = slot.handle.info().map_or(0, |i| i.generation);
                }
                changed = true;
            }
        }
    }
    if changed {
        p.lookup_dirty = true;
    }
}

/// Reads the store of `doc` on a thread: its fingerprint, its page count, the store file.
fn load_store(
    handle: &DocumentHandle,
    doc: &Path,
    data: &Path,
    identity: &str,
    ctx: &egui::Context,
) -> LoadResult {
    let result: LoadResult = Arc::default();
    let (slot, handle, doc, data, identity, wake) = (
        Arc::clone(&result),
        handle.clone(),
        doc.to_path_buf(),
        data.to_path_buf(),
        identity.to_string(),
        ctx.clone(),
    );
    let spawned = std::thread::Builder::new()
        .name("refdesk-pretranslate-load".into())
        .spawn(move || {
            let loaded = (|| {
                let fingerprint = fingerprint_file(&doc)
                    .map_err(|e| log::warn!("cannot read {}: {e}", doc.display()))
                    .ok()?;
                let pages = PageSource::page_count(&handle)?;
                let path = store_path(&data, &fingerprint, &identity);
                let store = PretranslateStore::open(&path, &fingerprint, &identity, pages);
                let complete = store.is_complete();
                Some(Loaded {
                    store: Arc::new(std::sync::RwLock::new(store)),
                    path,
                    complete,
                })
            })();
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(loaded);
            wake.request_repaint();
        });
    if let Err(e) = spawned {
        log::warn!("cannot read the translations made ahead: {e}");
        *result.lock().unwrap_or_else(PoisonError::into_inner) = Some(None);
    }
    result
}

/// The stores the views see: the open documents', under the paths their handles have.
fn rebuild_lookup(state: &mut AppState) {
    if !std::mem::take(&mut state.pretranslate.lookup_dirty) {
        return;
    }
    let mut stores = HashMap::new();
    let p = &state.pretranslate;
    for (raw, norm) in &p.aliases {
        if let Some(Slot {
            state: SlotState::Ready(l),
            ..
        }) = p.slots.get(norm)
        {
            stores.insert(raw.clone(), Arc::clone(&l.store));
        }
    }
    state.pretranslate.lookup = Lookup {
        hub: Arc::clone(&state.pretranslate.hub),
        stores: Arc::new(stores),
    };
}

/// Pages that popups missed go first: now for the job's document, later for the others.
fn route_misses(state: &mut AppState) {
    let p = &mut state.pretranslate;
    for (doc, page) in p.hub.take_misses() {
        let doc = normalized(&mut p.aliases, &doc);
        match &p.running {
            Some(r) if r.doc == doc => r.order.prioritize(page),
            _ => {
                let pages = p.urgent.entry(doc).or_default();
                pages.retain(|q| *q != page);
                pages.insert(0, page);
            }
        }
    }
}

/// Tells the job the page being read in the focused view, once it settled.
fn report_page(state: &mut AppState, now: f64, ctx: &egui::Context) {
    let focused = state.focused_pdf().and_then(|(_, pdf)| {
        let doc = state.docs.get(pdf.doc)?;
        let page = reading_location(pdf.view.anchor, pdf.ui.layout(), pdf.ui.viewport().h).page;
        Some((doc.path().to_path_buf(), page))
    });
    let p = &mut state.pretranslate;
    let Some((raw, page)) = focused else {
        p.reporter.reset();
        p.reporter_doc = None;
        return;
    };
    let doc = normalized(&mut p.aliases, &raw);
    if p.reporter_doc.as_ref() != Some(&doc) {
        p.reporter.reset();
        p.reporter_doc = Some(doc.clone());
    }
    if let Some(page) = p.reporter.step(now, Some(page)) {
        p.reading = Some((doc.clone(), page));
        if let Some(r) = &p.running
            && r.doc == doc
        {
            r.order.set_current(page);
        }
    }
    if let Some(wait) = p.reporter.wait(now) {
        ctx.request_repaint_after(Duration::from_secs_f64(wait));
    }
}

fn apply_pause(state: &mut AppState) {
    let allow = state.config.translate.pretranslate_on_battery;
    let p = &mut state.pretranslate;
    let pause = p.pause(allow).is_some();
    if let Some(r) = &mut p.running
        && r.paused != pause
    {
        r.job.set_paused(pause);
        r.paused = pause;
    }
}

/// Ends finished jobs, moves the job to the desired document and starts it there.
fn drive(state: &mut AppState, desired: Option<PathBuf>, ctx: &egui::Context) {
    let now = ctx.input(|i| i.time);
    let p = &mut state.pretranslate;
    if !p.settings.enabled {
        p.running = None;
        return;
    }
    if let Some(t) = p.resume_at {
        if now < t {
            ctx.request_repaint_after(Duration::from_secs_f64(t - now));
            return;
        }
        p.resume_at = None;
        crate::overlays::translate::warm_up_for_background(state, ctx);
    }
    let p = &mut state.pretranslate;
    if let Some(r) = &p.running {
        let snap = r.job.snapshot();
        p.last = Some((r.doc.clone(), Arc::clone(&snap)));
        match snap.status {
            PretranslateStatus::Finished | PretranslateStatus::NoDocument => {
                p.finished.insert(r.doc.clone());
                p.running = None;
                refresh_progress(state);
                return;
            }
            PretranslateStatus::Stopped => {
                p.running = None;
                return;
            }
            _ => {}
        }
    }
    let current = p.running.as_ref().map(|r| r.doc.clone());
    let stopping = p.running.as_ref().is_some_and(|r| r.stopping);
    match plan(current.as_deref(), stopping, desired.as_deref()) {
        Plan::Keep => {}
        Plan::StopAfterPage => {
            if let Some(r) = &mut p.running {
                r.job.stop_after_page();
                r.stopping = true;
            }
        }
        Plan::Start(doc) => start_job(state, &doc, ctx),
    }
}

/// Starts the job on `doc` once its store is read; a complete store marks the document done.
fn start_job(state: &mut AppState, doc: &Path, ctx: &egui::Context) {
    let allow = state.config.translate.pretranslate_on_battery;
    let backend = Arc::clone(&state.translate.backend);
    let p = &mut state.pretranslate;
    let Some(slot) = p.slots.get(doc) else {
        return;
    };
    let loaded = match &slot.state {
        SlotState::Loading(_) => return,
        SlotState::Failed => {
            p.finished.insert(doc.to_path_buf());
            return;
        }
        SlotState::Ready(l) => l.clone(),
    };
    if loaded.complete {
        p.finished.insert(doc.to_path_buf());
        refresh_progress(state);
        return;
    }
    let order = PretranslateOrder::new();
    if let Some((d, page)) = &p.reading
        && d == doc
    {
        order.set_current(*page);
    }
    for page in p.urgent.remove(doc).unwrap_or_default().into_iter().rev() {
        order.prioritize(page);
    }
    let job = PretranslateJob::start(PretranslateSetup {
        backend,
        source: Arc::new(slot.handle.clone()),
        store: Arc::clone(&loaded.store),
        store_path: loaded.path.clone(),
        order: Arc::clone(&order),
        foreground_busy: Arc::clone(&p.hub.busy),
        notifier: notifier(ctx),
        options: p.options.clone(),
    });
    let paused = p.pause(allow).is_some();
    if paused {
        job.set_paused(true);
    }
    p.running = Some(Running {
        doc: doc.to_path_buf(),
        job,
        order,
        store: loaded.store,
        store_path: loaded.path,
        stopping: false,
        paused,
    });
    refresh_progress(state);
}

/// Counts, on a thread, the project's documents whose translations are complete.
fn refresh_progress(state: &mut AppState) {
    let total = project_files(state).len();
    let p = &state.pretranslate;
    let Some(project) = p.project.clone() else {
        return;
    };
    let identity = p.identity.clone();
    let slot = Arc::clone(&p.progress);
    let spawned = std::thread::Builder::new()
        .name("refdesk-pretranslate-progress".into())
        .spawn(move || {
            let done = list(&project.data)
                .iter()
                .filter(|s| s.identity == identity && s.pages_done == s.page_count)
                .count();
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some((done.min(total), total));
        });
    if let Err(e) = spawned {
        log::warn!("cannot count the documents translated ahead: {e}");
    }
}

// ------------------------------------------------------------------------------------------------
// egui
// ------------------------------------------------------------------------------------------------

/// The indicator of the status bar, while background translation is on: "Translating ahead
/// 37%". The hover text tells the details; a click pauses or resumes.
pub fn status_indicator(ui: &mut Ui, state: &AppState, queue: &mut Vec<Action>) {
    let p = &state.pretranslate;
    if !p.keeps_model() {
        return;
    }
    let pause = p.pause(state.config.translate.pretranslate_on_battery);
    let running = p
        .running
        .as_ref()
        .map(|r| (r.doc.clone(), r.job.snapshot()));
    let (doc, snap) = match running.or_else(|| p.last.clone()) {
        Some((d, s)) => (Some(d), Some(s)),
        None => (None, None),
    };
    let all_done = p.running.is_none() && p.resume_at.is_none() && p.clearing.is_none() && {
        let progress = *p.progress.lock().unwrap_or_else(PoisonError::into_inner);
        progress.is_some_and(|(done, total)| done >= total)
    };
    let hint = state.translate.hint();
    let shown = shown(snap.as_deref(), pause, all_done, &hint);
    let pages = snap
        .as_ref()
        .filter(|s| s.page_count > 0)
        .map(|s| (s.pages_done, s.page_count));
    let text = label(&shown, pages.and_then(|(d, t)| percent(d, t)));
    let details = Details {
        doc: doc
            .as_ref()
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().into_owned()),
        pages,
        rate: snap.as_ref().and_then(|s| s.rate),
        remaining: snap.as_ref().and_then(|s| s.remaining),
        shown: shown.clone(),
        project: *p.progress.lock().unwrap_or_else(PoisonError::into_inner),
        paused: p.settings.paused,
    };
    ui.separator();
    let rich = match &shown {
        Shown::Blocked(_) => egui::RichText::new(&text).color(ui.visuals().warn_fg_color),
        _ => egui::RichText::new(&text),
    };
    let response = ui
        .add(egui::Button::new(rich).frame(false))
        .on_hover_text(tooltip(&details));
    let name = accessible_name(&text, &shown);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &name));
    if response.clicked() {
        queue.push(Action::TogglePretranslatePause);
    }
}

/// The confirmation of "Clear Translations Made Ahead", while it is open.
pub fn show_confirm(ctx: &egui::Context, state: &AppState, queue: &mut Vec<Action>) {
    if !state.pretranslate.confirm_clear {
        return;
    }
    let id = egui::Id::unique("pretranslate_clear");
    let response = egui::Modal::new(id).show(ctx, |ui| {
        ui.set_max_width(360.0);
        ui.label(egui::RichText::new(tr("pretranslate.clear_title")).strong());
        ui.add(egui::Label::new(tr("pretranslate.clear_body")).wrap());
        ui.add_space(8.0);
        let mut chosen = None;
        ui.horizontal(|ui| {
            if ui.button(tr("pretranslate.clear_yes")).clicked() {
                chosen = Some(true);
            }
            if ui.button(tr("pretranslate.clear_no")).clicked() {
                chosen = Some(false);
            }
        });
        chosen
    });
    let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    match response.inner {
        Some(true) => queue.push(Action::ClearPretranslations { confirmed: true }),
        Some(false) => queue.push(Action::CancelClearPretranslations),
        None if escape || response.backdrop_response.clicked() => {
            queue.push(Action::CancelClearPretranslations);
        }
        None => {}
    }
}

/// `CancelClearPretranslations`: closes the confirmation.
pub fn cancel_clear(state: &mut AppState) {
    state.pretranslate.confirm_clear = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use refdesk_core::testutil::PdfBuilder;
    use refdesk_core::translate::{TranslateKind, segments, text_hash};
    use refdesk_core::{Location, TranslateBackendKind, TranslateRequest};

    use crate::action::scroll_tests::ScratchDir;
    use crate::action::{OpenTarget, dispatch, dispatch_queued};
    use crate::overlays::translate::fake::{self, Log};
    use crate::overlays::translate::{self, TranslateApp};
    use crate::state::TabId;
    use crate::views::test_support::{Repaints, TIMEOUT, watch_repaints};

    fn hint() -> FailureHint {
        FailureHint {
            backend: TranslateBackendKind::Ollama,
            endpoint: "http://127.0.0.1:11434".into(),
            model: "translategemma:4b".into(),
            fallback_model: "translategemma-4b-it.Q4_K_M.gguf".into(),
            models_dir: "/home/u/.local/share/refdesk/models".into(),
        }
    }

    // ---- power ----

    fn supply(dir: &Path, name: &str, kind: &str, online: Option<&str>) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("type"), format!("{kind}\n")).unwrap();
        if let Some(o) = online {
            std::fs::write(d.join("online"), format!("{o}\n")).unwrap();
        }
    }

    #[test]
    fn battery_is_read_from_the_mains_supplies() {
        let dir = ScratchDir::new("power");
        let root = dir.0.join("power_supply");
        // No folder, no supplies: a desktop on mains.
        assert!(!on_battery(&root));
        std::fs::create_dir_all(&root).unwrap();
        assert!(!on_battery(&root));
        // A battery alone says nothing about the mains.
        supply(&root, "BAT1", "Battery", None);
        assert!(!on_battery(&root));
        supply(&root, "ADP1", "Mains", Some("0"));
        assert!(on_battery(&root));
        supply(&root, "ADP1", "Mains", Some("1"));
        assert!(!on_battery(&root));
        // Two mains supplies: one online is enough.
        supply(&root, "USB0", "Mains", Some("0"));
        assert!(!on_battery(&root));
        supply(&root, "ADP1", "Mains", Some("0"));
        assert!(on_battery(&root));
    }

    #[test]
    fn the_pause_in_effect() {
        assert_eq!(pause_reason(false, false, false), None);
        assert_eq!(pause_reason(true, false, false), Some(PauseReason::User));
        assert_eq!(pause_reason(true, true, false), Some(PauseReason::User));
        assert_eq!(pause_reason(false, true, false), Some(PauseReason::Battery));
        assert_eq!(pause_reason(false, true, true), None);
    }

    // ---- which document ----

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from(format!("/p/{n}")))
            .collect()
    }

    #[test]
    fn documents_go_focused_then_open_then_the_rest() {
        let files = paths(&["a.pdf", "b.pdf", "c.pdf", "d.pdf"]);
        let open = paths(&["c.pdf", "b.pdf"]);
        let in_project = |d: &Path| d.starts_with("/p");
        let p = |n: &str| PathBuf::from(format!("/p/{n}"));
        // The focused document first.
        assert_eq!(
            pick_document(Some(&p("d.pdf")), None, &open, &files, in_project),
            Some(p("d.pdf"))
        );
        // A focused document outside the project is not a target: the open ones come next.
        assert_eq!(
            pick_document(Some(Path::new("/x/y.pdf")), None, &open, &files, in_project),
            Some(p("c.pdf"))
        );
        // Then the rest in explorer order.
        assert_eq!(
            pick_document(None, None, &[], &files, in_project),
            Some(p("a.pdf"))
        );
        // Finished documents are passed over.
        let not_done = |d: &Path| in_project(d) && d != p("c.pdf") && d != p("a.pdf");
        assert_eq!(
            pick_document(Some(&p("c.pdf")), None, &open, &files, not_done),
            Some(p("b.pdf"))
        );
        assert_eq!(
            pick_document(None, None, &[], &files, not_done),
            Some(p("b.pdf"))
        );
        assert_eq!(pick_document(None, None, &[], &files, |_| false), None);
    }

    #[test]
    fn the_current_document_is_kept_within_its_rank() {
        let files = paths(&["a.pdf", "b.pdf", "c.pdf"]);
        let open = paths(&["a.pdf", "b.pdf"]);
        let p = |n: &str| PathBuf::from(format!("/p/{n}"));
        let all = |_: &Path| true;
        // An open document stays while no document has the focus.
        assert_eq!(
            pick_document(None, Some(&p("b.pdf")), &open, &files, all),
            Some(p("b.pdf"))
        );
        // A background document gives way to an open one.
        assert_eq!(
            pick_document(None, Some(&p("c.pdf")), &open, &files, all),
            Some(p("a.pdf"))
        );
        // And stays among the rest.
        assert_eq!(
            pick_document(None, Some(&p("c.pdf")), &[], &files, all),
            Some(p("c.pdf"))
        );
        // The focus always wins.
        assert_eq!(
            pick_document(Some(&p("a.pdf")), Some(&p("b.pdf")), &open, &files, all),
            Some(p("a.pdf"))
        );
    }

    #[test]
    fn another_document_waits_for_the_page_in_progress() {
        let (a, b) = (Path::new("/p/a.pdf"), Path::new("/p/b.pdf"));
        assert_eq!(plan(None, false, Some(a)), Plan::Start(a.to_path_buf()));
        assert_eq!(plan(None, false, None), Plan::Keep);
        assert_eq!(plan(Some(a), false, Some(a)), Plan::Keep);
        assert_eq!(plan(Some(a), false, Some(b)), Plan::StopAfterPage);
        assert_eq!(plan(Some(a), false, None), Plan::StopAfterPage);
        // Already asked to stop: wait for it.
        assert_eq!(plan(Some(a), true, Some(b)), Plan::Keep);
    }

    // ---- the page being read ----

    #[test]
    fn the_page_is_told_half_a_second_after_scrolling_stopped() {
        let mut r = PageReporter::default();
        assert_eq!(r.step(10.0, Some(3)), None);
        assert_eq!(r.wait(10.0), Some(0.5));
        assert_eq!(r.step(10.3, Some(3)), None);
        assert_eq!(r.step(10.49, Some(3)), None);
        assert_eq!(r.step(10.5, Some(3)), Some(3));
        // Once.
        assert_eq!(r.step(11.0, Some(3)), None);
        assert_eq!(r.wait(11.0), None);
        // Scrolling on: every new page starts the wait again.
        assert_eq!(r.step(12.0, Some(4)), None);
        assert_eq!(r.step(12.3, Some(5)), None);
        assert_eq!(r.step(12.7, Some(5)), None);
        assert_eq!(r.step(12.8, Some(5)), Some(5));
        // Back to a page told before: told again.
        assert_eq!(r.step(13.0, Some(3)), None);
        assert_eq!(r.step(13.5, Some(3)), Some(3));
        assert_eq!(r.step(14.0, None), None);
    }

    // ---- texts ----

    #[test]
    fn the_indicator_texts() {
        assert_eq!(percent(412, 1100), Some(37));
        assert_eq!(percent(1100, 1100), Some(100));
        assert_eq!(percent(0, 0), None);
        assert_eq!(thousands(1100), "1,100");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(
            duration_text(Duration::from_secs(20)),
            tr("pretranslate.eta_under_minute")
        );
        assert!(duration_text(Duration::from_secs(5 * 60)).contains('5'));
        let long = duration_text(Duration::from_secs(2 * 3600 + 5 * 60));
        assert!(long.contains('2') && long.contains('5'), "{long}");

        let running = label(&Shown::Running, Some(37));
        assert!(running.contains("37"), "{running}");
        assert_eq!(running, tr("pretranslate.label").replace("{}", "37"));
        assert!(label(&Shown::Paused, Some(37)).contains(tr("pretranslate.short_paused")));
        assert!(label(&Shown::Battery, Some(37)).contains(tr("pretranslate.short_battery")));
        assert_eq!(
            label(&Shown::Blocked("x".into()), Some(37)),
            tr("pretranslate.label_blocked")
        );
        assert_eq!(label(&Shown::Done, None), tr("pretranslate.label_done"));
        assert_eq!(
            label(&Shown::Preparing, None),
            tr("pretranslate.label_plain")
        );
        let name = accessible_name(&running, &Shown::Running);
        assert!(name.contains(tr("cmd.view.pretranslate")) && name.contains(&running));
    }

    #[test]
    fn the_state_follows_the_snapshot_and_the_pause() {
        let snap = |status| PretranslateSnapshot {
            status,
            ..PretranslateSnapshot::default()
        };
        let h = hint();
        let running = snap(PretranslateStatus::Running);
        assert_eq!(shown(Some(&running), None, false, &h), Shown::Running);
        assert_eq!(
            shown(Some(&running), Some(PauseReason::User), false, &h),
            Shown::Paused
        );
        assert_eq!(
            shown(Some(&running), Some(PauseReason::Battery), false, &h),
            Shown::Battery
        );
        assert_eq!(
            shown(
                Some(&snap(PretranslateStatus::WaitingForeground)),
                None,
                false,
                &h
            ),
            Shown::Foreground
        );
        let blocked = snap(PretranslateStatus::Blocked(TranslateError::Unreachable(
            "refused".into(),
        )));
        let Shown::Blocked(why) = shown(Some(&blocked), None, false, &h) else {
            panic!("blocked");
        };
        assert_eq!(
            why,
            error_text(&TranslateError::Unreachable("refused".into()), &h)
        );
        assert!(state_text(&Shown::Blocked(why.clone())).contains(&why));
        assert_eq!(shown(None, None, false, &h), Shown::Preparing);
        assert_eq!(shown(None, None, true, &h), Shown::Done);
        assert_eq!(
            shown(Some(&snap(PretranslateStatus::Finished)), None, true, &h),
            Shown::Done
        );
    }

    #[test]
    fn the_hover_text_names_the_document_pages_speed_time_state_and_project() {
        let d = Details {
            doc: Some("spec.pdf".into()),
            pages: Some((412, 1100)),
            rate: Some(1.75),
            remaining: Some(Duration::from_secs(3600 + 600)),
            shown: Shown::Running,
            project: Some((2, 5)),
            paused: false,
        };
        let text = tooltip(&d);
        for part in ["spec.pdf", "412", "1,100", "1.8", "2", "5"] {
            assert!(text.contains(part), "{part} in {text}");
        }
        assert!(text.contains(&state_text(&Shown::Running)));
        assert!(text.ends_with(tr("pretranslate.tip_click_pause")));
        let paused = tooltip(&Details { paused: true, ..d });
        assert!(paused.ends_with(tr("pretranslate.tip_click_resume")));
    }

    // ---- the app ----

    const SENTENCES: [&str; 3] = [
        "The process reports its state to the manager.",
        "It shall do so within one hundred milliseconds.",
        "Otherwise the watchdog restarts the process.",
    ];

    /// A PDF of `pages` pages; every page has the three sentences, the first one naming the page
    /// and the document so that every text differs.
    fn write_pdf(path: &Path, name: &str, pages: usize) {
        let mut b = PdfBuilder::new();
        for i in 0..pages {
            b = b.page(612.0, 792.0, |p| {
                p.text(
                    72.0,
                    100.0,
                    12.0,
                    &format!("Page {i} of {name} starts here."),
                );
                for (k, s) in SENTENCES.iter().enumerate() {
                    p.text(72.0, 140.0 + 40.0 * k as f32, 12.0, s);
                }
            });
        }
        std::fs::write(path, b.build()).unwrap();
    }

    struct H {
        ctx: egui::Context,
        repaints: Repaints,
        state: AppState,
        log: Arc<Log>,
        time: f64,
        dir: ScratchDir,
    }

    impl H {
        /// A project folder with these documents (name, pages); nothing opened yet.
        fn new(name: &str, docs: &[(&str, usize)]) -> H {
            let dir = ScratchDir::new(name);
            let root = dir.0.join("project");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join(project::PROJECT_FILE), "").unwrap();
            for (doc, pages) in docs {
                write_pdf(&root.join(doc), doc, *pages);
            }
            H::with_dir(dir, Log::new())
        }

        fn with_dir(dir: ScratchDir, log: Arc<Log>) -> H {
            let ctx = egui::Context::default();
            let repaints = watch_repaints(&ctx);
            let mut state = AppState::new();
            state.translate = TranslateApp::new(fake::factory(&log));
            translate::start(&mut state, &ctx);
            state.pretranslate.options.save_interval = Duration::ZERO;
            H {
                ctx,
                repaints,
                state,
                log,
                time: 1.0,
                dir,
            }
        }

        fn root(&self) -> PathBuf {
            normalize_path(&self.dir.0.join("project"))
        }

        fn data(&self) -> PathBuf {
            project::data_dir(&self.root())
        }

        fn doc(&self, name: &str) -> PathBuf {
            self.root().join(name)
        }

        fn act(&mut self, action: Action) {
            dispatch(&mut self.state, action, &self.ctx);
        }

        fn frame_at(&mut self, at: f64) {
            self.time = at;
            let input = egui::RawInput {
                time: Some(at),
                ..Default::default()
            };
            let state = &mut self.state;
            let output = self.ctx.run_ui(input, |ui| {
                translate::tick(state, ui.ctx());
                tick(state, ui.ctx());
            });
            output.drop_without_applying_deltas();
            dispatch_queued(&mut self.state, &self.ctx);
        }

        /// Frames at the same time until `check` holds; the clock does not move.
        fn until(&mut self, what: &str, mut check: impl FnMut(&H) -> bool) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.frame_at(self.time);
                if check(self) {
                    return;
                }
                self.repaints.wait_one(what, deadline);
            }
        }

        /// Opens the project as the workspace and waits for its scan.
        fn open_project(&mut self) {
            let root = self.root();
            self.act(Action::OpenFolder(root));
            self.until("the scan", |h| {
                h.state
                    .workspace
                    .as_ref()
                    .is_some_and(|w| w.scan.index().is_some())
                    && h.state.pretranslate.has_project()
            });
        }

        fn open_tab(&mut self, name: &str) -> TabId {
            let target = OpenTarget {
                path: self.doc(name),
                page: None,
            };
            self.act(Action::OpenFiles(vec![target]));
            self.state.focus.expect("a tab")
        }

        fn passages(&self) -> Vec<String> {
            self.log
                .requests()
                .into_iter()
                .filter(|(_, r)| r.kind == TranslateKind::Passage)
                .map(|(_, r)| r.text)
                .collect()
        }

        /// Answers the request held by the fake and stops holding the next ones.
        fn release(&self) {
            self.log.set(|i| {
                i.manual = false;
                i.feed.push_back(fake::Feed::Piece("번역".into()));
                i.feed.push_back(fake::Feed::End);
            });
        }

        fn current(&self) -> Option<PathBuf> {
            self.state.pretranslate.current_doc().map(Path::to_path_buf)
        }

        fn status(&self) -> Option<PretranslateStatus> {
            self.state.pretranslate.snapshot().map(|s| s.status.clone())
        }

        fn pages_done(&self, name: &str) -> u32 {
            self.state
                .pretranslate
                .store_of(&self.doc(name))
                .map_or(0, |s| s.read().unwrap().pages_done())
        }
    }

    fn settings_on_disk(h: &H) -> PretranslateSettings {
        PretranslateSettings::load(&h.data())
    }

    #[test]
    fn background_translation_needs_a_project() {
        let dir = ScratchDir::new("pre-noproject");
        let folder = dir.0.join("plain");
        std::fs::create_dir_all(&folder).unwrap();
        write_pdf(&folder.join("a.pdf"), "a", 1);
        let mut h = H::with_dir(dir, Log::new());
        let command = crate::commands::command("view.pretranslate").expect("command");
        assert!(!(command.enabled)(&h.state));
        h.act(Action::OpenFolder(normalize_path(&folder)));
        h.until("the scan", |h| {
            h.state
                .workspace
                .as_ref()
                .is_some_and(|w| w.scan.index().is_some())
        });
        assert!(!(command.enabled)(&h.state));
        h.act(Action::TogglePretranslate);
        assert!(!h.state.pretranslate.settings.enabled);
        assert!(!project::data_dir(&folder).exists());
        // The folder becomes a project: noticed within five seconds.
        std::fs::write(folder.join(project::PROJECT_FILE), "").unwrap();
        h.frame_at(3.0);
        h.frame_at(6.5);
        assert!((command.enabled)(&h.state));
        assert!(
            crate::commands::command("view.pretranslate_clear")
                .is_some_and(|c| (c.enabled)(&h.state))
        );
    }

    #[test]
    fn switching_on_remembers_it_and_translates_the_focused_document() {
        let mut h = H::new("pre-on", &[("a.pdf", 2)]);
        h.open_project();
        h.open_tab("a.pdf");
        h.act(Action::TogglePretranslate);
        assert!(settings_on_disk(&h).enabled);
        // The model is loaded as for translation mode.
        h.log.wait("warm-up", |i| i.warm_ups.len() == 1);
        h.until("both pages", |h| h.pages_done("a.pdf") == 2);
        let passages = h.passages();
        assert!(passages.iter().any(|t| t == "Page 0 of a.pdf starts here."));
        assert!(passages.iter().any(|t| t == SENTENCES[0]));
        // A repeated sentence is sent once.
        assert_eq!(passages.iter().filter(|t| *t == SENTENCES[1]).count(), 1);
        h.until("finished", |h| h.current().is_none());
        // Off: no job, remembered off.
        h.act(Action::TogglePretranslate);
        assert!(!settings_on_disk(&h).enabled);
        assert!(h.current().is_none());
    }

    #[test]
    fn a_new_start_goes_on_ten_seconds_later() {
        let mut h = H::new("pre-resume", &[("a.pdf", 1)]);
        PretranslateSettings {
            enabled: true,
            paused: false,
        }
        .save(&h.data())
        .unwrap();
        h.open_project();
        assert!(h.state.pretranslate.settings.enabled);
        let opened_at = h.time;
        h.frame_at(opened_at + 9.9);
        assert!(h.current().is_none());
        assert!(h.log.requests().is_empty());
        h.frame_at(opened_at + RESUME_DELAY + 0.1);
        h.until("translated", |h| {
            h.passages().iter().any(|t| t == SENTENCES[0])
        });
        // A document that was not open: opened in the background.
        assert!(h.state.tabs.iter().all(|(_, t)| !matches!(t, Tab::Pdf(_))));
    }

    #[test]
    fn a_focus_on_another_document_takes_the_job_there_after_the_page() {
        let mut h = H::new("pre-focus", &[("a.pdf", 3), ("b.pdf", 3)]);
        h.open_project();
        let tab_a = h.open_tab("a.pdf");
        let tab_b = h.open_tab("b.pdf");
        h.act(Action::FocusTab(tab_a));
        h.log.set(|i| i.manual = true);
        h.act(Action::TogglePretranslate);
        h.until("the first segment", |h| !h.passages().is_empty());
        assert_eq!(h.current(), Some(h.doc("a.pdf")));
        h.act(Action::FocusTab(tab_b));
        h.frame_at(h.time);
        // Still on a.pdf: its page goes on.
        assert_eq!(h.current(), Some(h.doc("a.pdf")));
        h.release();
        h.until("on b.pdf", |h| {
            h.current() == Some(h.doc("b.pdf")) && h.passages().iter().any(|t| t.contains("b.pdf"))
        });
        assert_eq!(h.pages_done("a.pdf"), 1);
        let passages = h.passages();
        let first_b = passages.iter().position(|t| t.contains("b.pdf"));
        let last_a = passages.iter().rposition(|t| t.contains("a.pdf"));
        assert!(
            first_b.is_some_and(|b| last_a.is_none_or(|a| a < b)),
            "{passages:?}"
        );
    }

    #[test]
    fn the_page_being_read_reaches_the_job_after_half_a_second() {
        let mut h = H::new("pre-page", &[("a.pdf", 6)]);
        h.open_project();
        let tab = h.open_tab("a.pdf");
        h.log.set(|i| i.manual = true);
        h.act(Action::TogglePretranslate);
        h.until("the job", |h| !h.passages().is_empty());
        let order = h.state.pretranslate.order().expect("order");
        let t = h.time;
        if let Some(Tab::Pdf(pdf)) = h.state.tabs.get_mut(tab) {
            pdf.view.anchor = Location {
                page: 4,
                x: 0.0,
                y: 0.0,
            };
        }
        h.frame_at(t + 1.0);
        h.frame_at(t + 1.3);
        assert_eq!(order.current(), 0);
        h.frame_at(t + 1.5);
        assert_eq!(order.current(), 4);
        h.release();
        h.until("page 4 done", |h| {
            h.state
                .pretranslate
                .store_of(&h.doc("a.pdf"))
                .is_some_and(|s| s.read().unwrap().page_done(4))
        });
        // Page 0 (in progress), then the page being read.
        let pages: Vec<u32> = h
            .passages()
            .iter()
            .filter(|t| t.starts_with("Page "))
            .map(|t| t[5..6].parse().unwrap())
            .collect();
        assert_eq!(&pages[..2], [0, 4]);
    }

    #[test]
    fn the_users_own_translation_comes_first() {
        let mut h = H::new("pre-busy", &[("a.pdf", 2)]);
        h.open_project();
        let tab = h.open_tab("a.pdf");
        h.until("text", |h| {
            let pdf = h.state.tabs.pdf(tab).unwrap();
            h.state.docs.get(pdf.doc).unwrap().page_text(0).is_some()
        });
        // A selection translated once; its answer is held back.
        let n = h
            .state
            .docs
            .get(h.state.tabs.pdf(tab).unwrap().doc)
            .unwrap()
            .page_text(0)
            .unwrap()
            .chars
            .len() as u32;
        if let Some(Tab::Pdf(pdf)) = h.state.tabs.get_mut(tab) {
            pdf.ui.selection.set_for_test(refdesk_core::Selection {
                anchor: refdesk_core::TextPos { page: 0, ch: 0 },
                focus: refdesk_core::TextPos { page: 0, ch: n },
            });
        }
        h.log.set(|i| i.manual = true);
        h.act(Action::TranslateSelection(tab));
        h.log.wait("the popup's request", |i| i.requests.len() == 1);
        h.act(Action::TogglePretranslate);
        h.until("waiting", |h| {
            h.status() == Some(PretranslateStatus::WaitingForeground)
        });
        assert_eq!(h.log.requests().len(), 1, "only the popup asked");
        // The popup goes: the job goes on.
        if let Some(Tab::Pdf(pdf)) = h.state.tabs.get_mut(tab) {
            pdf.ui.translate.close();
        }
        h.log.set(|i| i.manual = false);
        h.until("translated ahead", |h| h.pages_done("a.pdf") == 2);
        assert!(h.log.requests().len() > 1);

        // While the model loads, the job waits too.
        let mut h = H::new("pre-loading", &[("a.pdf", 1)]);
        h.open_project();
        h.open_tab("a.pdf");
        h.log.set(|i| i.warm_hold = true);
        h.act(Action::TogglePretranslate);
        h.until("waiting", |h| {
            h.status() == Some(PretranslateStatus::WaitingForeground)
        });
        assert!(h.log.requests().is_empty());
        h.log.set(|i| i.warm_hold = false);
        h.until("translated ahead", |h| h.pages_done("a.pdf") == 1);
    }

    #[test]
    fn a_click_pauses_and_resumes() {
        let mut h = H::new("pre-pause", &[("a.pdf", 3)]);
        h.open_project();
        h.open_tab("a.pdf");
        h.log.set(|i| i.manual = true);
        h.act(Action::TogglePretranslate);
        h.until("the job", |h| !h.passages().is_empty());
        h.act(Action::TogglePretranslatePause);
        assert!(settings_on_disk(&h).paused);
        h.release();
        h.until("paused", |h| h.status() == Some(PretranslateStatus::Paused));
        let asked = h.log.requests().len();
        h.act(Action::TogglePretranslatePause);
        assert!(!settings_on_disk(&h).paused);
        h.until("finished", |h| h.pages_done("a.pdf") == 3);
        assert!(h.log.requests().len() > asked);
    }

    #[test]
    fn battery_power_pauses_unless_allowed() {
        let mut h = H::new("pre-battery", &[("a.pdf", 3)]);
        h.open_project();
        h.open_tab("a.pdf");
        h.log.set(|i| i.manual = true);
        h.act(Action::TogglePretranslate);
        h.until("the job", |h| !h.passages().is_empty());
        h.state
            .pretranslate
            .on_battery
            .store(true, Ordering::Relaxed);
        h.release();
        h.until("paused", |h| h.status() == Some(PretranslateStatus::Paused));
        let snap = h.state.pretranslate.snapshot();
        let pause = h.state.pretranslate.pause(false);
        assert_eq!(
            shown(snap.as_deref(), pause, false, &hint()),
            Shown::Battery
        );
        // Plugged in again.
        h.state
            .pretranslate
            .on_battery
            .store(false, Ordering::Relaxed);
        h.until("finished", |h| h.pages_done("a.pdf") == 3);

        // Allowed on battery: no pause.
        let mut h = H::new("pre-battery-ok", &[("a.pdf", 2)]);
        h.state.config.translate.pretranslate_on_battery = true;
        h.state
            .pretranslate
            .on_battery
            .store(true, Ordering::Relaxed);
        h.open_project();
        h.open_tab("a.pdf");
        h.act(Action::TogglePretranslate);
        h.until("finished", |h| h.pages_done("a.pdf") == 2);
    }

    #[test]
    fn another_model_starts_over_in_its_own_file_and_clearing_removes_them() {
        let mut h = H::new("pre-model", &[("a.pdf", 1)]);
        h.open_project();
        h.open_tab("a.pdf");
        h.act(Action::TogglePretranslate);
        h.until("written", |h| list(&h.data()).len() == 1);
        let first = list(&h.data());
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].identity, "translategemma:4b|ko");
        h.act(Action::SetTranslateModel(Some("qwen2.5:7b".into())));
        // Translation mode is off, but the new model is loaded for background translation.
        h.log.wait("the new model's warm-up", |i| {
            i.warm_ups.last().map(String::as_str) == Some("qwen2.5:7b")
        });
        h.until("translated again", |h| {
            list(&h.data())
                .iter()
                .any(|s| s.identity == "qwen2.5:7b|ko" && s.pages_done == 1)
        });
        // The first model's file is still there.
        assert_eq!(list(&h.data()).len(), 2);
        assert!(h.passages().len() >= 2 * 4);
        assert!(
            h.log
                .requests()
                .iter()
                .any(|(m, r)| m == "qwen2.5:7b" && r.text == SENTENCES[2])
        );

        // Clearing asks first.
        h.act(Action::TogglePretranslate);
        h.act(Action::ClearPretranslations { confirmed: false });
        assert!(h.state.pretranslate.confirm_clear);
        assert_eq!(list(&h.data()).len(), 2);
        h.act(Action::CancelClearPretranslations);
        assert!(!h.state.pretranslate.confirm_clear);
        h.act(Action::ClearPretranslations { confirmed: true });
        h.until("cleared", |h| {
            h.state.pretranslate.clearing.is_none() && list(&h.data()).is_empty()
        });
        // The settings stay.
        assert!(PretranslateSettings::path(&h.data()).exists());
    }

    #[test]
    fn translation_mode_off_keeps_the_model_while_background_translation_is_on() {
        let mut h = H::new("pre-release", &[("a.pdf", 1)]);
        h.open_project();
        h.open_tab("a.pdf");
        h.act(Action::ToggleTranslateMode);
        h.act(Action::TogglePretranslate);
        h.act(Action::ToggleTranslateMode);
        assert!(h.log.get(|i| i.releases.is_empty()));
        h.until("translated", |h| h.pages_done("a.pdf") == 1);
        // Both off: the model goes.
        h.act(Action::TogglePretranslate);
        assert_eq!(h.log.get(|i| i.releases.len()), 1);
        assert_eq!(h.state.translate.readiness, Readiness::Off);
    }

    #[test]
    fn the_status_bar_shows_the_progress_and_a_click_pauses() {
        let mut h = H::new("pre-status", &[("a.pdf", 2)]);
        h.open_project();
        h.open_tab("a.pdf");
        h.act(Action::TogglePretranslate);
        h.until("translated", |h| h.pages_done("a.pdf") == 2);
        h.ctx.enable_accesskit();
        let input = egui::RawInput {
            time: Some(h.time),
            ..Default::default()
        };
        let mut queue = Vec::new();
        let state = &mut h.state;
        let output = h.ctx.run_ui(input, |ui| {
            crate::panels::status_bar::show(ui, state, state.focus, &mut queue);
        });
        let names: Vec<String> = output
            .platform_output
            .accesskit_update
            .clone()
            .map(|u| u.nodes)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, n)| n.label().map(str::to_owned))
            .collect();
        output.drop_without_applying_deltas();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with(tr("cmd.view.pretranslate"))),
            "{names:?}"
        );
    }

    #[test]
    fn the_new_commands_follow_translation_mode_in_the_view_menu() {
        let view: Vec<&str> = crate::commands::in_menu(crate::commands::MenuGroup::View)
            .map(|c| c.id)
            .collect();
        let at = view
            .iter()
            .position(|id| *id == "view.translate_mode")
            .unwrap();
        assert_eq!(view[at + 1], "view.pretranslate");
        assert_eq!(view[at + 2], "view.pretranslate_clear");
        for id in ["view.pretranslate", "view.pretranslate_clear"] {
            let c = crate::commands::command(id).unwrap();
            assert!(c.shortcut.is_none());
            assert_ne!(tr(c.name_key), "???");
        }
    }

    #[test]
    fn the_stores_a_popup_reads_are_those_of_the_open_documents() {
        let mut h = H::new("pre-lookup", &[("a.pdf", 1)]);
        h.open_project();
        let tab = h.open_tab("a.pdf");
        h.act(Action::TogglePretranslate);
        h.until("translated", |h| h.pages_done("a.pdf") == 1);
        let doc = h
            .state
            .docs
            .get(h.state.tabs.pdf(tab).unwrap().doc)
            .unwrap()
            .clone();
        let pre = h.state.pretranslate.lookup().resolve(doc.path());
        let (_, store) = pre.doc.expect("a store for the open document");
        let text = doc.page_text(0).unwrap();
        let seg = segments(&text)
            .into_iter()
            .find(|s| s.text == SENTENCES[0])
            .unwrap();
        let st = store.read().unwrap();
        assert_eq!(
            st.get(0, seg.chars.clone(), text_hash(&seg.text)),
            Some(fake::answer(&TranslateRequest::passage(SENTENCES[0])).as_str())
        );
        assert_eq!(st.identity(), "translategemma:4b|ko");
    }
}
