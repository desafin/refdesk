//! Translation mode, the backend, the cache and the model list: the app-wide part of the
//! translation feature (`docs/ARCHITECTURE.md` §5.12, TR-1, TR-7, TR-12). The popup of each view
//! is in [`super::translate_popup`].
//!
//! Only `dispatch` changes this state (R-6): [`toggle_mode`], [`translate_selection`],
//! [`set_model`], [`refresh_models`], [`retry_warm_up`]. [`tick`] runs in `logic()`: it takes the
//! results of the warm-up and of the model list, and saves the cache five seconds after it
//! changed. Every call to the server runs on a job thread (R-1).
//!
//! The texts of the status bar, the menu rows and the failure explanations are plain functions
//! ([`readiness_text`], [`error_text`], [`model_menu_rows`]), tested without egui (R-10).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use egui::Ui;
use refdesk_core::{
    Paths, Prefs, TranslateBackend, TranslateBackendKind, TranslateConfig, TranslateError,
    TranslateJob, TranslateState, TranslationCache, atomic_write, backend_from_config,
    effective_translate_config, effective_translate_mode,
};

use crate::action::{Action, notifier};
use crate::i18n::tr;
use crate::state::{AppState, StatusNotice, Tab, TabId};

pub use super::translate_popup::ViewEnv;

/// The cache is written this long (seconds) after it first changed.
pub const CACHE_SAVE_DELAY: f64 = 5.0;
/// Seconds a translation notice stays in the status bar.
const NOTICE_SECS: f64 = 3.0;

/// Builds a backend for a configuration. The app uses [`backend_from_config`]; tests put in
/// fakes.
pub type BackendFactory = Arc<dyn Fn(&TranslateConfig) -> Arc<dyn TranslateBackend> + Send + Sync>;

/// Whether the model is ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// Translation mode is off; the model was not loaded on purpose.
    Off,
    /// The model is being loaded.
    Loading,
    /// The model is loaded.
    Ready,
    /// Loading failed, for this reason.
    Failed(TranslateError),
}

/// The names a failure explanation needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureHint {
    /// Which API.
    pub backend: TranslateBackendKind,
    /// The server address.
    pub endpoint: String,
    /// The model in use.
    pub model: String,
}

impl FailureHint {
    fn of(config: &TranslateConfig) -> FailureHint {
        FailureHint {
            backend: config.backend,
            endpoint: config.endpoint(),
            model: config.model.clone(),
        }
    }
}

/// What the user can do about a failure, with the address and the model filled in.
pub fn error_text(e: &TranslateError, hint: &FailureHint) -> String {
    let fill = |key: &str| {
        tr(key)
            .replace("{endpoint}", &hint.endpoint)
            .replace("{model}", &hint.model)
    };
    match (e, hint.backend) {
        (TranslateError::Unreachable(_), TranslateBackendKind::Ollama) => {
            fill("translate.err.unreachable_ollama")
        }
        (TranslateError::Unreachable(_), TranslateBackendKind::OpenAi) => {
            fill("translate.err.unreachable_openai")
        }
        (TranslateError::ModelMissing(_), TranslateBackendKind::Ollama) => {
            fill("translate.err.model_missing_ollama")
        }
        (TranslateError::ModelMissing(_), TranslateBackendKind::OpenAi) => {
            fill("translate.err.model_missing_openai")
        }
        (TranslateError::Http { status, message }, _) => tr("translate.err.http")
            .replace("{status}", &status.to_string())
            .replace("{message}", message),
        (TranslateError::Protocol(message), _) => {
            tr("translate.err.protocol").replace("{message}", message)
        }
        (TranslateError::Cancelled, _) => tr("translate.err.cancelled").to_owned(),
    }
}

/// The short text next to the status bar button, and the explanation for its hover text.
/// Nothing while ready or off.
pub fn readiness_text(r: &Readiness, hint: &FailureHint) -> Option<(String, Option<String>)> {
    match r {
        Readiness::Off | Readiness::Ready => None,
        Readiness::Loading => Some((tr("translate.loading_model").to_owned(), None)),
        Readiness::Failed(e) => Some((
            tr("translate.unavailable").to_owned(),
            Some(error_text(e, hint)),
        )),
    }
}

/// The model list of the menu.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ModelList {
    /// Never asked for.
    #[default]
    NotLoaded,
    /// Being fetched.
    Loading,
    /// The models of the server.
    Loaded(Vec<String>),
    /// The server could not be asked.
    Failed(TranslateError),
}

/// One row of "View ▸ Translation Model".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelRow {
    /// A model; `checked` for the one in use.
    Model {
        /// Its name.
        name: String,
        /// It is in use.
        checked: bool,
    },
    /// "Loading…".
    Loading,
    /// The list could not be fetched, with the reason on one line.
    Failed(String),
    /// The server has no models.
    Empty,
    /// A separator.
    Separator,
    /// "Use the Model of the Settings File"; enabled when a model was picked in the app.
    UseConfig {
        /// A model was picked in the app.
        enabled: bool,
    },
    /// "Refresh the List".
    Refresh,
}

/// The rows of the model menu: the models (the one in use checked, and added as one more row
/// when the server does not list it), then the two fixed rows.
pub fn model_menu_rows(list: &ModelList, current: &str, picked: bool) -> Vec<ModelRow> {
    let mut rows = Vec::new();
    let current_row = ModelRow::Model {
        name: current.to_string(),
        checked: true,
    };
    match list {
        ModelList::NotLoaded | ModelList::Loading => {
            rows.push(ModelRow::Loading);
            rows.push(current_row);
        }
        ModelList::Failed(e) => {
            let reason = e.to_string();
            rows.push(ModelRow::Failed(
                tr("menu.translate_model_failed").replace("{}", &reason),
            ));
            rows.push(current_row);
        }
        ModelList::Loaded(names) => {
            if names.is_empty() {
                rows.push(ModelRow::Empty);
            }
            for name in names {
                rows.push(ModelRow::Model {
                    name: name.clone(),
                    checked: name == current,
                });
            }
            if !names.iter().any(|n| n == current) {
                rows.push(current_row);
            }
        }
    }
    rows.push(ModelRow::Separator);
    rows.push(ModelRow::UseConfig { enabled: picked });
    rows.push(ModelRow::Refresh);
    rows
}

/// The text of a model row: the name with a check mark for the one in use.
pub fn model_row_text(name: &str, checked: bool) -> String {
    if checked {
        format!("✓ {name}")
    } else {
        format!("\u{2003} {name}")
    }
}

/// What a model list fetch gives.
type ModelsResult = Result<Vec<String>, TranslateError>;

/// A model list fetched on its own thread.
struct ModelsJob {
    cancel: Arc<AtomicBool>,
    result: Arc<Mutex<Option<ModelsResult>>>,
}

impl Drop for ModelsJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl ModelsJob {
    fn start(backend: Arc<dyn TranslateBackend>, ctx: &egui::Context) -> ModelsJob {
        let cancel = Arc::new(AtomicBool::new(false));
        let result: Arc<Mutex<Option<ModelsResult>>> = Arc::default();
        let (c, r, wake) = (Arc::clone(&cancel), Arc::clone(&result), notifier(ctx));
        let spawned = std::thread::Builder::new()
            .name("refdesk-translate-models".into())
            .spawn(move || {
                let got = backend.models(&c);
                *r.lock().unwrap_or_else(PoisonError::into_inner) = Some(got);
                if !c.load(Ordering::Relaxed) {
                    wake();
                }
            });
        if let Err(e) = spawned {
            *result.lock().unwrap_or_else(PoisonError::into_inner) = Some(Err(
                TranslateError::Protocol(format!("cannot start a thread: {e}")),
            ));
        }
        ModelsJob { cancel, result }
    }

    fn take(&self) -> Option<ModelsResult> {
        self.result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// The translation feature's state, one for the app (`AppState.translate`).
pub struct TranslateApp {
    /// Translation mode is on (TR-1).
    pub mode: bool,
    /// The backend for the configuration and the picked model.
    pub backend: Arc<dyn TranslateBackend>,
    /// Builds backends (when the model changes).
    pub factory: BackendFactory,
    /// Finished translations, shared with the views.
    pub cache: Arc<Mutex<TranslationCache>>,
    /// Whether the model is ready.
    pub readiness: Readiness,
    /// The backend's configuration (with the picked model), for texts.
    pub settings: TranslateConfig,
    /// Bumped whenever the backend is replaced.
    pub generation: u64,
    warm_up: Option<TranslateJob>,
    models: ModelList,
    models_job: Option<ModelsJob>,
    cache_dirty_since: Option<f64>,
    /// Set once the cache file has been read (it is read on a thread at start).
    cache_loaded: Arc<AtomicBool>,
}

impl Default for TranslateApp {
    fn default() -> TranslateApp {
        TranslateApp::new(Arc::new(|c: &TranslateConfig| backend_from_config(c)))
    }
}

impl TranslateApp {
    /// Translation off, the default configuration, an empty cache. Contacts nothing.
    pub fn new(factory: BackendFactory) -> TranslateApp {
        let settings = TranslateConfig::default();
        TranslateApp {
            mode: false,
            backend: factory(&settings),
            factory,
            cache: Arc::default(),
            readiness: Readiness::Off,
            settings,
            generation: 0,
            warm_up: None,
            models: ModelList::NotLoaded,
            models_job: None,
            cache_dirty_since: None,
            cache_loaded: Arc::new(AtomicBool::new(true)),
        }
    }

    /// The names for failure texts.
    pub fn hint(&self) -> FailureHint {
        FailureHint::of(&self.settings)
    }

    /// What the views get for a frame.
    pub fn view_env(&self, overlay_open: bool) -> ViewEnv {
        ViewEnv {
            mode: self.mode,
            backend: Arc::clone(&self.backend),
            cache: Arc::clone(&self.cache),
            readiness: self.readiness.clone(),
            word_with_sentence: self.settings.word_with_sentence,
            generation: self.generation,
            overlay_open,
            hint: self.hint(),
        }
    }

    /// The model list as the menu sees it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn models(&self) -> &ModelList {
        &self.models
    }

    /// Whether the cache file has been read.
    pub fn cache_loaded(&self) -> bool {
        self.cache_loaded.load(Ordering::Acquire)
    }

    fn start_warm_up(&mut self, ctx: &egui::Context) {
        self.readiness = Readiness::Loading;
        self.warm_up = Some(TranslateJob::warm_up(
            Arc::clone(&self.backend),
            notifier(ctx),
        ));
    }

    fn start_models(&mut self, ctx: &egui::Context) {
        self.models = ModelList::Loading;
        self.models_job = Some(ModelsJob::start(Arc::clone(&self.backend), ctx));
    }

    /// Builds the backend for `config` and `prefs`.
    fn rebuild(&mut self, config: &TranslateConfig, prefs: &Prefs) {
        self.settings = effective_translate_config(prefs, config);
        self.backend = (self.factory)(&self.settings);
        self.generation += 1;
        self.warm_up = None;
        self.models_job = None;
        self.models = ModelList::NotLoaded;
    }
}

/// Sets the feature up at start, after `prefs.json` was read: the backend for the settings, the
/// cache read on a thread, and the warm-up when translation mode was left on.
pub fn start(state: &mut AppState, ctx: &egui::Context) {
    let (config, prefs) = (state.config.translate.clone(), state.prefs.clone());
    let t = &mut state.translate;
    t.rebuild(&config, &prefs);
    t.mode = effective_translate_mode(&prefs);
    if let Some(paths) = &state.paths {
        load_cache(t, paths.translations_file());
    }
    if t.mode {
        t.start_warm_up(ctx);
        t.start_models(ctx);
    } else {
        t.readiness = Readiness::Off;
    }
}

/// Reads the cache file on a thread and puts it in place when done.
fn load_cache(t: &mut TranslateApp, path: std::path::PathBuf) {
    let loaded = Arc::new(AtomicBool::new(false));
    t.cache_loaded = Arc::clone(&loaded);
    let cache = Arc::clone(&t.cache);
    let spawned = std::thread::Builder::new()
        .name("refdesk-translations-load".into())
        .spawn(move || {
            let read = TranslationCache::load(&path);
            let mut slot = cache.lock().unwrap_or_else(PoisonError::into_inner);
            // Translations made in the first moments are dropped rather than the whole file.
            *slot = read;
            loaded.store(true, Ordering::Release);
        });
    if let Err(e) = spawned {
        log::warn!("cannot read the translation cache: {e}");
        t.cache_loaded.store(true, Ordering::Release);
    }
}

fn notice(state: &mut AppState, text: String, ctx: &egui::Context) {
    state.status_notice = Some(StatusNotice {
        text,
        until: ctx.input(|i| i.time) + NOTICE_SECS,
    });
    ctx.request_repaint();
}

fn save_prefs(state: &AppState) {
    if let Some(paths) = &state.paths
        && let Err(err) = state.prefs.save(&paths.prefs_file())
    {
        log::warn!("could not save {}: {err}", paths.prefs_file().display());
    }
}

/// Closes the translation popup of every PDF view and drops its jobs.
fn close_all_popups(state: &mut AppState) {
    let ids: Vec<TabId> = state.tabs.iter().map(|(id, _)| id).collect();
    for id in ids {
        if let Some(Tab::Pdf(pdf)) = state.tabs.get_mut(id) {
            pdf.ui.translate.close();
        }
    }
}

/// `ToggleTranslateMode`: switches translation mode, remembers it in `prefs.json`, loads the
/// model when it goes on and drops popups and jobs when it goes off.
pub fn toggle_mode(state: &mut AppState, ctx: &egui::Context) {
    let on = !state.translate.mode;
    state.translate.mode = on;
    state.prefs.translate_mode = Some(on);
    save_prefs(state);
    if on {
        state.translate.start_warm_up(ctx);
        if !matches!(state.translate.models, ModelList::Loaded(_)) {
            state.translate.start_models(ctx);
        }
        notice(state, tr("status.translate_on").to_owned(), ctx);
    } else {
        state.translate.readiness = Readiness::Off;
        state.translate.warm_up = None;
        close_all_popups(state);
        notice(state, tr("status.translate_off").to_owned(), ctx);
    }
}

/// `TranslateSelection`: translates the selection of a PDF tab once, in translation mode or not.
pub fn translate_selection(state: &mut AppState, tab: TabId, ctx: &egui::Context) {
    let env = state.translate.view_env(state.palette.open.is_some());
    let AppState { docs, tabs, .. } = state;
    let Some(Tab::Pdf(pdf)) = tabs.get_mut(tab) else {
        return;
    };
    let (Some(doc), Some(sel)) = (docs.get(pdf.doc), pdf.ui.selection.active()) else {
        return;
    };
    let generation = doc.info().map_or(0, |i| i.generation);
    let texts = |p: u32| doc.page_text(p);
    let retry = super::translate_popup::open_selection(
        &mut pdf.ui.translate,
        &env,
        sel,
        &texts,
        generation,
        ctx,
    );
    if retry {
        retry_warm_up(state, ctx);
    }
    ctx.request_repaint();
}

/// `RetryTranslateWarmUp`: a request was made while loading the model had failed; in
/// translation mode, try loading it again (the server may have been started since).
pub fn retry_warm_up(state: &mut AppState, ctx: &egui::Context) {
    let t = &mut state.translate;
    if t.mode && matches!(t.readiness, Readiness::Failed(_)) {
        t.start_warm_up(ctx);
    }
}

/// `SetTranslateModel`: remembers the model picked in the app (`None`: the one of
/// `config.toml`), builds the backend for it, drops popups and jobs, loads the new model in
/// translation mode and fetches the list again.
pub fn set_model(state: &mut AppState, model: Option<String>, ctx: &egui::Context) {
    state.prefs.translate_model = model.filter(|m| !m.trim().is_empty());
    save_prefs(state);
    let (config, prefs) = (state.config.translate.clone(), state.prefs.clone());
    state.translate.rebuild(&config, &prefs);
    close_all_popups(state);
    if state.translate.mode {
        state.translate.start_warm_up(ctx);
    }
    state.translate.start_models(ctx);
    let name = state.translate.settings.model.clone();
    notice(
        state,
        tr("status.translate_model").replace("{}", &name),
        ctx,
    );
}

/// `RefreshTranslateModels`: fetches the model list again.
pub fn refresh_models(state: &mut AppState, ctx: &egui::Context) {
    state.translate.start_models(ctx);
}

/// Per frame, in `logic()`: takes the end of the warm-up and of the model list, and saves the
/// cache [`CACHE_SAVE_DELAY`] seconds after it changed (on a thread).
pub fn tick(state: &mut AppState, ctx: &egui::Context) {
    let now = ctx.input(|i| i.time);
    let t = &mut state.translate;
    if let Some(job) = &t.warm_up {
        let snap = job.snapshot();
        match &snap.state {
            TranslateState::Running => {}
            TranslateState::Done => {
                t.readiness = Readiness::Ready;
                t.warm_up = None;
            }
            TranslateState::Failed(e) => {
                t.readiness = Readiness::Failed(e.clone());
                t.warm_up = None;
            }
        }
    }
    if let Some(result) = t.models_job.as_ref().and_then(ModelsJob::take) {
        t.models = match result {
            Ok(names) => ModelList::Loaded(names),
            Err(e) => ModelList::Failed(e),
        };
        t.models_job = None;
    }
    let dirty = t
        .cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .dirty();
    if !dirty || !t.cache_loaded() {
        t.cache_dirty_since = None;
        return;
    }
    let since = *t.cache_dirty_since.get_or_insert(now);
    let left = CACHE_SAVE_DELAY - (now - since);
    if left > 0.0 {
        ctx.request_repaint_after(Duration::from_secs_f64(left));
        return;
    }
    t.cache_dirty_since = None;
    if let Some(paths) = &state.paths {
        save_cache_in_background(&state.translate.cache, paths);
    }
}

/// Writes the cache on a thread; the bytes are made here so that later changes wait for the
/// next save.
fn save_cache_in_background(cache: &Arc<Mutex<TranslationCache>>, paths: &Paths) {
    let bytes = {
        let mut c = cache.lock().unwrap_or_else(PoisonError::into_inner);
        let bytes = c.to_bytes();
        c.mark_saved();
        bytes
    };
    let path = paths.translations_file();
    match bytes {
        Ok(bytes) => {
            let spawned = std::thread::Builder::new()
                .name("refdesk-translations-save".into())
                .spawn(move || {
                    if let Err(e) = atomic_write(&path, &bytes) {
                        log::warn!("could not save {}: {e}", path.display());
                    }
                });
            if let Err(e) = spawned {
                log::warn!("cannot save the translation cache: {e}");
            }
        }
        Err(e) => log::warn!("cannot encode the translation cache: {e}"),
    }
}

/// Saves the cache now, when it changed (at exit).
pub fn save_now(state: &mut AppState) {
    let Some(paths) = &state.paths else {
        return;
    };
    if !state.translate.cache_loaded() {
        return;
    }
    let mut cache = state
        .translate
        .cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if !cache.dirty() {
        return;
    }
    match cache.save(&paths.translations_file()) {
        Ok(()) => cache.mark_saved(),
        Err(e) => log::warn!(
            "could not save {}: {e}",
            paths.translations_file().display()
        ),
    }
}

// ------------------------------------------------------------------------------------------------
// egui
// ------------------------------------------------------------------------------------------------

/// The hover text of the status bar button: the model, the backend and its address.
pub fn button_tooltip(t: &TranslateApp) -> String {
    tr("translate.tooltip")
        .replace("{model}", &t.settings.model)
        .replace("{backend}", t.settings.backend.name())
        .replace("{endpoint}", &t.settings.endpoint())
}

/// The accessible name of the status bar button, which says whether the mode is on.
pub fn button_label(mode: bool) -> &'static str {
    if mode {
        tr("translate.button_on")
    } else {
        tr("translate.button_off")
    }
}

/// The "Translate" toggle of the status bar, with the readiness next to it while the mode is on.
pub fn status_button(ui: &mut Ui, state: &AppState, queue: &mut Vec<Action>) {
    let t = &state.translate;
    let shortcut = crate::commands::shortcut_text(ui.ctx(), "view.translate_mode");
    let mut tip = button_tooltip(t);
    if let Some(keys) = shortcut {
        tip = format!("{tip}\n{} ({keys})", tr("cmd.view.translate_mode"));
    }
    let response = ui
        .add(egui::Button::selectable(t.mode, tr("translate.button")))
        .on_hover_text(tip);
    let label = button_label(t.mode);
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, t.mode, label));
    if response.clicked() {
        queue.push(Action::ToggleTranslateMode);
    }
    if !t.mode {
        return;
    }
    if let Some((text, why)) = readiness_text(&t.readiness, &t.hint()) {
        match why {
            Some(why) => {
                ui.colored_label(ui.visuals().warn_fg_color, text)
                    .on_hover_text(why);
            }
            None => {
                ui.weak(text);
            }
        }
    }
}

/// "Translation Model ▸": the server's models, then "Use the Model of the Settings File" and
/// "Refresh the List" (TR-12).
pub fn model_menu(ui: &mut Ui, state: &AppState, queue: &mut Vec<Action>) {
    let t = &state.translate;
    ui.menu_button(tr("menu.translate_model"), |ui| {
        if matches!(t.models, ModelList::NotLoaded) {
            queue.push(Action::RefreshTranslateModels);
        }
        let picked = state.prefs.translate_model.is_some();
        for row in model_menu_rows(&t.models, &t.settings.model, picked) {
            match row {
                ModelRow::Model { name, checked } => {
                    let button =
                        egui::Button::new(model_row_text(&name, checked)).selected(checked);
                    if ui.add(button).clicked() {
                        if !checked {
                            queue.push(Action::SetTranslateModel(Some(name)));
                        }
                        ui.close();
                    }
                }
                ModelRow::Loading => {
                    ui.weak(tr("menu.translate_model_loading"));
                }
                ModelRow::Failed(text) => {
                    ui.colored_label(ui.visuals().warn_fg_color, text);
                }
                ModelRow::Empty => {
                    ui.weak(tr("menu.translate_model_empty"));
                }
                ModelRow::Separator => {
                    ui.separator();
                }
                ModelRow::UseConfig { enabled } => {
                    let button = egui::Button::new(tr("menu.translate_model_config"));
                    if ui.add_enabled(enabled, button).clicked() {
                        queue.push(Action::SetTranslateModel(None));
                        ui.close();
                    }
                }
                ModelRow::Refresh => {
                    if ui.button(tr("menu.translate_model_refresh")).clicked() {
                        queue.push(Action::RefreshTranslateModels);
                    }
                }
            }
        }
    });
}

/// A scripted backend for tests: records what it is asked, answers from a script or piece by
/// piece as the test feeds it, and notices cancellation.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use refdesk_core::{
        TranslateBackend, TranslateConfig, TranslateError, TranslateKind, TranslateRequest,
    };

    use super::BackendFactory;

    /// One step of a fed answer.
    pub enum Feed {
        Piece(String),
        End,
    }

    /// What the backends of one test share.
    pub struct Inner {
        /// The model of the backend and the request, in the order asked.
        pub requests: Vec<(String, TranslateRequest)>,
        /// The models warmed up, in order.
        pub warm_ups: Vec<String>,
        /// Requests that saw their cancel flag.
        pub cancelled: usize,
        /// `translate` waits for pieces from `feed` instead of answering at once.
        pub manual: bool,
        pub feed: VecDeque<Feed>,
        /// `translate` fails with this.
        pub fail: Option<TranslateError>,
        /// `warm_up` fails with this.
        pub warm_error: Option<TranslateError>,
        /// `warm_up` waits until this is false.
        pub warm_hold: bool,
        /// What `models` gives; `models_hold` makes it wait.
        pub models: Result<Vec<String>, TranslateError>,
        pub models_hold: bool,
        pub list_calls: usize,
    }

    pub struct Log {
        inner: Mutex<Inner>,
        cv: Condvar,
    }

    impl Log {
        pub fn new() -> Arc<Log> {
            Arc::new(Log {
                inner: Mutex::new(Inner {
                    requests: Vec::new(),
                    warm_ups: Vec::new(),
                    cancelled: 0,
                    manual: false,
                    feed: VecDeque::new(),
                    fail: None,
                    warm_error: None,
                    warm_hold: false,
                    models: Ok(vec!["translategemma:4b".into(), "qwen2.5:7b".into()]),
                    models_hold: false,
                    list_calls: 0,
                }),
                cv: Condvar::new(),
            })
        }

        fn lock(&self) -> MutexGuard<'_, Inner> {
            self.inner.lock().unwrap()
        }

        /// Changes the script and wakes waiting backends.
        pub fn set(&self, f: impl FnOnce(&mut Inner)) {
            f(&mut self.lock());
            self.cv.notify_all();
        }

        /// Reads the record.
        pub fn get<R>(&self, f: impl FnOnce(&Inner) -> R) -> R {
            f(&self.lock())
        }

        /// Waits (on the condition variable, never by sleeping) until `done` holds.
        pub fn wait(&self, what: &str, done: impl Fn(&Inner) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut guard = self.lock();
            while !done(&guard) {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "timed out waiting for {what}");
                guard = self.cv.wait_timeout(guard, left).unwrap().0;
            }
        }

        pub fn requests(&self) -> Vec<(String, TranslateRequest)> {
            self.get(|i| i.requests.clone())
        }

        pub fn feed(&self, piece: &str) {
            self.set(|i| i.feed.push_back(Feed::Piece(piece.to_string())));
        }

        pub fn end(&self) {
            self.set(|i| i.feed.push_back(Feed::End));
        }
    }

    /// The scripted answer: "뜻[word]" for a word, "번역[text]" for a passage.
    pub fn answer(req: &TranslateRequest) -> String {
        match req.kind {
            TranslateKind::Word { .. } => format!("뜻[{}]", req.text),
            TranslateKind::Passage => format!("번역[{}]", req.text),
        }
    }

    pub struct Fake {
        model: String,
        log: Arc<Log>,
    }

    impl Fake {
        /// Waits for `stop` or the cancel flag, re-checking every few milliseconds (a wait on
        /// the condition variable, which every change of the script also ends).
        fn hold(&self, cancel: &AtomicBool, stop: impl Fn(&Inner) -> bool) -> bool {
            let mut guard = self.log.lock();
            loop {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return false;
                }
                if stop(&guard) {
                    return true;
                }
                guard = self
                    .log
                    .cv
                    .wait_timeout(guard, Duration::from_millis(5))
                    .unwrap()
                    .0;
            }
        }
    }

    impl TranslateBackend for Fake {
        fn translate(
            &self,
            req: &TranslateRequest,
            cancel: &AtomicBool,
            on_text: &mut dyn FnMut(&str),
        ) -> Result<(), TranslateError> {
            let manual = {
                let mut i = self.log.lock();
                i.requests.push((self.model.clone(), req.clone()));
                if let Some(e) = i.fail.clone() {
                    drop(i);
                    self.log.cv.notify_all();
                    return Err(e);
                }
                i.manual
            };
            self.log.cv.notify_all();
            if !manual {
                on_text(&answer(req));
                return Ok(());
            }
            loop {
                if !self.hold(cancel, |i| !i.feed.is_empty()) {
                    self.log.set(|i| i.cancelled += 1);
                    return Err(TranslateError::Cancelled);
                }
                let step = self.log.lock().feed.pop_front();
                match step {
                    Some(Feed::Piece(p)) => on_text(&p),
                    Some(Feed::End) => return Ok(()),
                    None => {}
                }
            }
        }

        fn warm_up(&self, cancel: &AtomicBool) -> Result<(), TranslateError> {
            self.log.set(|i| i.warm_ups.push(self.model.clone()));
            if !self.hold(cancel, |i| !i.warm_hold) {
                return Err(TranslateError::Cancelled);
            }
            match self.log.get(|i| i.warm_error.clone()) {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }

        fn models(&self, cancel: &AtomicBool) -> Result<Vec<String>, TranslateError> {
            self.log.set(|i| i.list_calls += 1);
            if !self.hold(cancel, |i| !i.models_hold) {
                return Err(TranslateError::Cancelled);
            }
            self.log.get(|i| i.models.clone())
        }

        fn identity(&self) -> String {
            format!("{}|ko", self.model)
        }
    }

    /// Backends that share `log`, one per configuration (named after its model).
    pub fn factory(log: &Arc<Log>) -> BackendFactory {
        let log = Arc::clone(log);
        Arc::new(move |c: &TranslateConfig| {
            Arc::new(Fake {
                model: c.model.clone(),
                log: Arc::clone(&log),
            }) as Arc<dyn TranslateBackend>
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::action::dispatch;
    use crate::action::scroll_tests::ScratchDir;
    use fake::Log;
    use refdesk_core::{TranslateRequest, TranslationCache};

    fn hint() -> FailureHint {
        FailureHint {
            backend: TranslateBackendKind::Ollama,
            endpoint: "http://127.0.0.1:11434".into(),
            model: "translategemma:4b".into(),
        }
    }

    /// A state whose backends are fakes sharing `log`, set up like the app does at start.
    fn state_with(log: &Arc<Log>, paths: Option<Paths>, ctx: &egui::Context) -> AppState {
        let mut state = AppState::new();
        state.translate = TranslateApp::new(fake::factory(log));
        state.paths = paths;
        if let Some(p) = &state.paths {
            state.prefs = Prefs::load(&p.prefs_file());
        }
        start(&mut state, ctx);
        state
    }

    /// Runs `tick` in a frame at time `t`.
    fn tick_at(state: &mut AppState, ctx: &egui::Context, t: f64) {
        let input = egui::RawInput {
            time: Some(t),
            ..Default::default()
        };
        ctx.run_ui(input, |ui| tick(state, ui.ctx()))
            .drop_without_applying_deltas();
    }

    /// Ticks until `done` holds; the clock stays at `t` so no delay passes by accident.
    fn tick_until(
        state: &mut AppState,
        ctx: &egui::Context,
        t: f64,
        what: &str,
        done: impl Fn(&AppState) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            tick_at(state, ctx, t);
            if done(state) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::yield_now();
        }
    }

    // ---- texts ----

    #[test]
    fn readiness_texts_name_the_cause_and_the_fix() {
        let h = hint();
        assert_eq!(readiness_text(&Readiness::Off, &h), None);
        assert_eq!(readiness_text(&Readiness::Ready, &h), None);
        let (text, why) = readiness_text(&Readiness::Loading, &h).unwrap();
        assert_eq!(text, tr("translate.loading_model"));
        assert_eq!(why, None);
        let failed = Readiness::Failed(TranslateError::Unreachable("x".into()));
        let (text, why) = readiness_text(&failed, &h).unwrap();
        assert_eq!(text, tr("translate.unavailable"));
        let why = why.unwrap();
        assert!(why.contains("http://127.0.0.1:11434"), "{why}");
        assert!(why.contains("Ollama"), "{why}");
    }

    #[test]
    fn error_texts_carry_the_model_and_the_address() {
        let h = hint();
        let missing = error_text(&TranslateError::ModelMissing("m".into()), &h);
        assert!(
            missing.contains("ollama pull translategemma:4b"),
            "{missing}"
        );
        let unreachable = error_text(&TranslateError::Unreachable("x".into()), &h);
        assert!(
            unreachable.contains("http://127.0.0.1:11434"),
            "{unreachable}"
        );
        let openai = FailureHint {
            backend: TranslateBackendKind::OpenAi,
            endpoint: "http://127.0.0.1:8080".into(),
            ..h.clone()
        };
        let text = error_text(&TranslateError::Unreachable("x".into()), &openai);
        assert!(
            text.contains("http://127.0.0.1:8080") && !text.contains("Ollama"),
            "{text}"
        );
        let text = error_text(&TranslateError::ModelMissing("m".into()), &openai);
        assert!(
            text.contains("translategemma:4b") && !text.contains("ollama pull"),
            "{text}"
        );
        let http = error_text(
            &TranslateError::Http {
                status: 500,
                message: "boom".into(),
            },
            &h,
        );
        assert!(http.contains("500") && http.contains("boom"), "{http}");
        let protocol = error_text(&TranslateError::Protocol("cut".into()), &h);
        assert!(protocol.contains("cut"), "{protocol}");
        assert!(!error_text(&TranslateError::Cancelled, &h).is_empty());
    }

    #[test]
    fn the_button_tooltip_names_model_backend_and_address() {
        let t = TranslateApp::new(fake::factory(&Log::new()));
        let tip = button_tooltip(&t);
        assert!(tip.contains("translategemma:4b"), "{tip}");
        assert!(tip.contains("ollama"), "{tip}");
        assert!(tip.contains("http://127.0.0.1:11434"), "{tip}");
        assert_ne!(button_label(true), button_label(false));
    }

    // ---- the model menu ----

    fn names(rows: &[ModelRow]) -> Vec<(String, bool)> {
        rows.iter()
            .filter_map(|r| match r {
                ModelRow::Model { name, checked } => Some((name.clone(), *checked)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_menu_checks_the_model_in_use_and_ends_with_the_fixed_rows() {
        let list = ModelList::Loaded(vec!["a".into(), "b".into()]);
        let rows = model_menu_rows(&list, "b", true);
        assert_eq!(names(&rows), [("a".into(), false), ("b".into(), true)]);
        let n = rows.len();
        assert_eq!(rows[n - 3], ModelRow::Separator);
        assert_eq!(rows[n - 2], ModelRow::UseConfig { enabled: true });
        assert_eq!(rows[n - 1], ModelRow::Refresh);
        // Without a picked model there is nothing to go back to.
        let rows = model_menu_rows(&list, "b", false);
        assert!(rows.contains(&ModelRow::UseConfig { enabled: false }));
        assert_eq!(model_row_text("b", true), "✓ b");
        assert!(model_row_text("a", false).ends_with(" a"));
    }

    #[test]
    fn a_model_the_server_does_not_list_is_one_more_row() {
        let list = ModelList::Loaded(vec!["a".into()]);
        let rows = model_menu_rows(&list, "from-config", false);
        assert_eq!(
            names(&rows),
            [("a".into(), false), ("from-config".into(), true)]
        );
        let empty = model_menu_rows(&ModelList::Loaded(vec![]), "m", false);
        assert_eq!(empty[0], ModelRow::Empty);
        assert_eq!(names(&empty), [("m".into(), true)]);
    }

    #[test]
    fn while_loading_or_after_a_failure_the_menu_says_so() {
        for list in [ModelList::NotLoaded, ModelList::Loading] {
            let rows = model_menu_rows(&list, "m", false);
            assert_eq!(rows[0], ModelRow::Loading);
            assert_eq!(names(&rows), [("m".into(), true)]);
        }
        let rows = model_menu_rows(
            &ModelList::Failed(TranslateError::Unreachable("http://x: refused".into())),
            "m",
            false,
        );
        match &rows[0] {
            ModelRow::Failed(text) => assert!(text.contains("refused"), "{text}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(rows.last(), Some(&ModelRow::Refresh));
    }

    // ---- the mode ----

    #[test]
    fn switching_the_mode_on_loads_the_model_and_tells_when_it_is_ready() {
        let log = Log::new();
        log.set(|i| i.warm_hold = true);
        let ctx = egui::Context::default();
        let mut state = state_with(&log, None, &ctx);
        assert!(!state.translate.mode);
        assert_eq!(state.translate.readiness, Readiness::Off);
        dispatch(&mut state, Action::ToggleTranslateMode, &ctx);
        assert!(state.translate.mode);
        assert_eq!(state.translate.readiness, Readiness::Loading);
        log.wait("the warm-up", |i| i.warm_ups == ["translategemma:4b"]);
        tick_at(&mut state, &ctx, 1.0);
        assert_eq!(state.translate.readiness, Readiness::Loading);
        log.set(|i| i.warm_hold = false);
        tick_until(&mut state, &ctx, 1.0, "ready", |s| {
            s.translate.readiness == Readiness::Ready
        });
        let notice = state.status_notice.as_ref().unwrap();
        assert_eq!(notice.text, tr("status.translate_on"));
    }

    #[test]
    fn a_failed_warm_up_is_kept_with_its_reason_and_retried_on_request() {
        let log = Log::new();
        log.set(|i| i.warm_error = Some(TranslateError::ModelMissing("m".into())));
        let ctx = egui::Context::default();
        let mut state = state_with(&log, None, &ctx);
        dispatch(&mut state, Action::ToggleTranslateMode, &ctx);
        tick_until(&mut state, &ctx, 1.0, "failed", |s| {
            matches!(s.translate.readiness, Readiness::Failed(_))
        });
        assert_eq!(
            state.translate.readiness,
            Readiness::Failed(TranslateError::ModelMissing("m".into()))
        );
        // The server came back: a request tries again.
        log.set(|i| i.warm_error = None);
        dispatch(&mut state, Action::RetryTranslateWarmUp, &ctx);
        assert_eq!(state.translate.readiness, Readiness::Loading);
        tick_until(&mut state, &ctx, 1.0, "ready", |s| {
            s.translate.readiness == Readiness::Ready
        });
        assert_eq!(log.get(|i| i.warm_ups.len()), 2);
    }

    #[test]
    fn the_mode_is_saved_and_comes_back_after_a_restart_with_a_warm_up() {
        let dir = ScratchDir::new("translate-mode");
        let ctx = egui::Context::default();
        let log = Log::new();
        let mut state = state_with(&log, Some(dir.paths()), &ctx);
        dispatch(&mut state, Action::ToggleTranslateMode, &ctx);
        let saved = Prefs::load(&dir.paths().prefs_file());
        assert_eq!(saved.translate_mode, Some(true));
        drop(state);

        let log2 = Log::new();
        let state = state_with(&log2, Some(dir.paths()), &ctx);
        assert!(state.translate.mode);
        assert_eq!(state.translate.readiness, Readiness::Loading);
        log2.wait("the warm-up after the restart", |i| i.warm_ups.len() == 1);
    }

    #[test]
    fn switching_off_drops_the_warm_up_and_works_without_a_place_to_save() {
        let log = Log::new();
        log.set(|i| i.warm_hold = true);
        let ctx = egui::Context::default();
        let mut state = state_with(&log, None, &ctx);
        dispatch(&mut state, Action::ToggleTranslateMode, &ctx);
        dispatch(&mut state, Action::ToggleTranslateMode, &ctx);
        assert!(!state.translate.mode);
        assert_eq!(state.translate.readiness, Readiness::Off);
        assert!(state.translate.warm_up.is_none());
        assert_eq!(
            state.status_notice.as_ref().map(|n| n.text.as_str()),
            Some(tr("status.translate_off"))
        );
    }

    // ---- the model list ----

    #[test]
    fn the_model_list_is_fetched_on_a_thread() {
        let log = Log::new();
        log.set(|i| i.models_hold = true);
        let ctx = egui::Context::default();
        let mut state = state_with(&log, None, &ctx);
        assert_eq!(state.translate.models(), &ModelList::NotLoaded);
        dispatch(&mut state, Action::RefreshTranslateModels, &ctx);
        assert_eq!(state.translate.models(), &ModelList::Loading);
        tick_at(&mut state, &ctx, 1.0);
        assert_eq!(state.translate.models(), &ModelList::Loading);
        log.set(|i| i.models_hold = false);
        tick_until(&mut state, &ctx, 1.0, "the list", |s| {
            matches!(s.translate.models(), ModelList::Loaded(_))
        });
        assert_eq!(
            state.translate.models(),
            &ModelList::Loaded(vec!["translategemma:4b".into(), "qwen2.5:7b".into()])
        );

        log.set(|i| i.models = Err(TranslateError::Unreachable("x".into())));
        dispatch(&mut state, Action::RefreshTranslateModels, &ctx);
        tick_until(&mut state, &ctx, 1.0, "the failure", |s| {
            matches!(s.translate.models(), ModelList::Failed(_))
        });
    }

    #[test]
    fn switching_the_mode_on_fetches_the_model_list() {
        let log = Log::new();
        let ctx = egui::Context::default();
        let mut state = state_with(&log, None, &ctx);
        dispatch(&mut state, Action::ToggleTranslateMode, &ctx);
        tick_until(&mut state, &ctx, 1.0, "the list", |s| {
            matches!(s.translate.models(), ModelList::Loaded(_))
        });
    }

    // ---- choosing a model ----

    #[test]
    fn choosing_a_model_saves_it_rebuilds_the_backend_and_loads_it_again() {
        let dir = ScratchDir::new("translate-model");
        let ctx = egui::Context::default();
        let log = Log::new();
        let mut state = state_with(&log, Some(dir.paths()), &ctx);
        dispatch(&mut state, Action::ToggleTranslateMode, &ctx);
        log.wait("first warm-up", |i| i.warm_ups.len() == 1);
        let generation = state.translate.generation;

        dispatch(
            &mut state,
            Action::SetTranslateModel(Some("qwen2.5:7b".into())),
            &ctx,
        );
        assert_eq!(state.translate.settings.model, "qwen2.5:7b");
        assert_eq!(state.translate.backend.identity(), "qwen2.5:7b|ko");
        assert!(state.translate.generation > generation);
        assert_eq!(state.translate.readiness, Readiness::Loading);
        log.wait("the new model's warm-up", |i| {
            i.warm_ups == ["translategemma:4b", "qwen2.5:7b"]
        });
        assert_eq!(
            Prefs::load(&dir.paths().prefs_file())
                .translate_model
                .as_deref(),
            Some("qwen2.5:7b")
        );
        assert_eq!(
            state.status_notice.as_ref().map(|n| n.text.clone()),
            Some(tr("status.translate_model").replace("{}", "qwen2.5:7b"))
        );
        drop(state);

        // A restart keeps it.
        let log2 = Log::new();
        let mut state = state_with(&log2, Some(dir.paths()), &ctx);
        assert_eq!(state.translate.backend.identity(), "qwen2.5:7b|ko");

        // Going back to the settings file's model.
        dispatch(&mut state, Action::SetTranslateModel(None), &ctx);
        assert_eq!(state.translate.settings.model, "translategemma:4b");
        assert_eq!(Prefs::load(&dir.paths().prefs_file()).translate_model, None);
    }

    #[test]
    fn the_cache_of_another_model_is_not_used() {
        let ctx = egui::Context::default();
        let log = Log::new();
        let mut state = state_with(&log, None, &ctx);
        let req = TranslateRequest::passage("The watchdog.");
        let old = state.translate.backend.identity();
        state
            .translate
            .cache
            .lock()
            .unwrap()
            .insert(&old, &req, "감시자.".into());
        dispatch(
            &mut state,
            Action::SetTranslateModel(Some("qwen2.5:7b".into())),
            &ctx,
        );
        let new = state.translate.backend.identity();
        assert_ne!(old, new);
        let mut cache = state.translate.cache.lock().unwrap();
        assert_eq!(cache.get(&new, &req), None);
        assert_eq!(cache.get(&old, &req), Some("감시자."));
    }

    // ---- the command table ----

    #[test]
    fn the_two_commands_have_their_keys_and_menus_and_need_a_selection() {
        use crate::commands::{self, MenuGroup};
        use egui::{Key, KeyboardShortcut, Modifiers};
        let mode = commands::command("view.translate_mode").expect("mode command");
        assert_eq!(
            mode.shortcut,
            Some(KeyboardShortcut::new(Modifiers::NONE, Key::F2))
        );
        assert_eq!(mode.menu, MenuGroup::View);
        assert!(commands::in_menu(MenuGroup::View).any(|c| c.id == "view.translate_mode"));
        let sel = commands::command("edit.translate_selection").expect("selection command");
        assert_eq!(
            sel.shortcut,
            Some(KeyboardShortcut::new(Modifiers::SHIFT, Key::F2))
        );
        assert!(commands::in_menu(MenuGroup::Edit).any(|c| c.id == "edit.translate_selection"));
        // No other command uses the keys (the table test checks all pairs; these by name).
        for c in commands::COMMANDS {
            if c.id != mode.id && c.id != sel.id {
                assert!(
                    c.all_shortcuts()
                        .all(|s| s != mode.shortcut.unwrap() && s != sel.shortcut.unwrap()),
                    "{}",
                    c.id
                );
            }
        }
        let state = AppState::new();
        assert!((mode.enabled)(&state));
        assert!(!(sel.enabled)(&state), "nothing selected");
        assert!(
            commands::SELECTION_MENU
                .iter()
                .any(|e| e.name_key == "cmd.edit.translate"
                    && e.command_id == Some("edit.translate_selection"))
        );
    }

    // ---- saving the cache ----

    #[test]
    fn the_cache_is_saved_five_seconds_after_it_changed() {
        let dir = ScratchDir::new("translate-cache");
        let ctx = egui::Context::default();
        let log = Log::new();
        let mut state = state_with(&log, Some(dir.paths()), &ctx);
        tick_until(&mut state, &ctx, 0.0, "the cache read", |s| {
            s.translate.cache_loaded()
        });
        let file = dir.paths().translations_file();
        let identity = state.translate.backend.identity();
        state.translate.cache.lock().unwrap().insert(
            &identity,
            &TranslateRequest::passage("a"),
            "A".into(),
        );
        tick_at(&mut state, &ctx, 10.0);
        tick_at(&mut state, &ctx, 14.9);
        assert!(!file.exists(), "not before five seconds");
        tick_at(&mut state, &ctx, 15.0);
        assert!(!state.translate.cache.lock().unwrap().dirty());
        // The file is written on a thread.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while TranslationCache::load(&file).is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the file was not written"
            );
            std::thread::yield_now();
        }
        let mut back = TranslationCache::load(&file);
        assert_eq!(
            back.get(&identity, &TranslateRequest::passage("a")),
            Some("A")
        );
    }

    #[test]
    fn a_change_not_saved_yet_is_saved_at_exit_and_read_back_at_start() {
        let dir = ScratchDir::new("translate-exit");
        let ctx = egui::Context::default();
        let log = Log::new();
        let mut state = state_with(&log, Some(dir.paths()), &ctx);
        tick_until(&mut state, &ctx, 0.0, "the cache read", |s| {
            s.translate.cache_loaded()
        });
        let identity = state.translate.backend.identity();
        let req = TranslateRequest::word("bank", "The river bank.");
        state
            .translate
            .cache
            .lock()
            .unwrap()
            .insert(&identity, &req, "강둑".into());
        tick_at(&mut state, &ctx, 1.0);
        assert!(!dir.paths().translations_file().exists());
        save_now(&mut state);
        assert!(dir.paths().translations_file().exists());
        drop(state);

        let mut state = state_with(&Log::new(), Some(dir.paths()), &ctx);
        tick_until(&mut state, &ctx, 0.0, "the cache read", |s| {
            s.translate.cache_loaded()
        });
        assert_eq!(
            state.translate.cache.lock().unwrap().get(&identity, &req),
            Some("강둑")
        );
    }
}
