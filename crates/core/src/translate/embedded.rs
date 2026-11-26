//! The in-process engine: llama.cpp reads a GGUF model file and translates without a server
//! (TR-14, WP-83). `docs/ARCHITECTURE.md` §4.14; the measurements behind the choices are in
//! `spikes/llm-embedded/README.md`.
//!
//! The engine is an optional build feature (`embedded`, `embedded-vulkan`, `embedded-cuda`)
//! because llama.cpp needs cmake, libclang and a C++ compiler to build. Without it,
//! [`EmbeddedBackend`] still exists: it lists the model folder and answers every request with
//! [`TranslateError::NotBuilt`].
//!
//! With the feature, one engine thread per backend owns the model and its context (the context
//! borrows the model, so both live in that thread's stack frames and no `unsafe` is needed).
//! Calls send it a command over a channel and wait for its events in short steps, checking their
//! `cancel` flag between them, so a cancelled call returns at once. The thread serves commands
//! in order, skips cancelled ones, and drops the model when `keep_alive_minutes` pass without a
//! request, on [`TranslateBackend::release`], and when the backend is dropped. A dropped backend
//! waits (briefly, bounded) until the model is freed: freeing GPU buffers while the process
//! exits crashes in the driver. The app drops a replaced backend on a helper thread; at exit
//! the wait is what lets the process end cleanly.
//!
//! The pure parts — which file to load, the model list, the thread count, assembling UTF-8 from
//! token bytes — are plain functions tested without the engine.

use std::path::{Path, PathBuf};

use super::{TranslateBackend, TranslateError, TranslateRequest};
use crate::store::TranslateConfig;

/// The automatic thread count never goes above this: in the spike 32 threads on a 32-thread CPU
/// were a hundred times slower than 12 ~ 16.
pub const MAX_AUTO_THREADS: u32 = 16;

/// The automatic CPU thread count for a machine with `logical_cores`: half of them, at least 1,
/// at most [`MAX_AUTO_THREADS`].
pub fn auto_threads(logical_cores: usize) -> u32 {
    let half = u32::try_from(logical_cores / 2).unwrap_or(u32::MAX);
    half.clamp(1, MAX_AUTO_THREADS)
}

/// The thread count for the `threads` setting: the value itself, or [`auto_threads`] of this
/// machine when it is 0.
pub fn threads_for(setting: u32) -> u32 {
    if setting > 0 {
        return setting;
    }
    let cores = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
    auto_threads(cores)
}

/// Whether `name` ends in `.gguf` (any case).
pub fn is_gguf_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
}

/// The names of the `.gguf` files in `dir`, in name order. A folder that does not exist has
/// none; other read failures are errors.
pub fn list_models(dir: &Path) -> Result<Vec<String>, TranslateError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(TranslateError::Engine(format!(
                "cannot read {}: {e}",
                dir.display()
            )));
        }
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| is_gguf_name(n))
        .collect();
    names.sort();
    Ok(names)
}

/// The model file for the `model` setting:
///
/// * an absolute path is used as it is;
/// * a file `model` or `model.gguf` in `dir` is used;
/// * otherwise (such as the default `translategemma:4b`, an Ollama name) the first `.gguf` file
///   of `dir` in name order, which is logged;
/// * no file at all is `ModelMissing` with the model name (or the absolute path).
pub fn resolve_model(dir: &Path, model: &str) -> Result<PathBuf, TranslateError> {
    resolve(dir, model, true)
}

/// [`resolve_model`], logging the fallback to another file only when `log` is set.
fn resolve(dir: &Path, model: &str, log: bool) -> Result<PathBuf, TranslateError> {
    let model = model.trim();
    let as_path = Path::new(model);
    if as_path.is_absolute() {
        return if as_path.is_file() {
            Ok(as_path.to_path_buf())
        } else {
            Err(TranslateError::ModelMissing(model.to_string()))
        };
    }
    if !model.is_empty() {
        let exact = dir.join(model);
        if exact.is_file() {
            return Ok(exact);
        }
        let with_extension = dir.join(format!("{model}.gguf"));
        if with_extension.is_file() {
            return Ok(with_extension);
        }
    }
    let files = list_models(dir)?;
    match files.first() {
        Some(first) => {
            if log {
                let of = match files.len() {
                    1 => String::new(),
                    n => format!(" (the first of {n} files)"),
                };
                log::info!(
                    "translate: no model file {model:?} in {}; using {first}{of}",
                    dir.display()
                );
            }
            Ok(dir.join(first))
        }
        None => Err(TranslateError::ModelMissing(model.to_string())),
    }
}

/// The cache identity of the embedded backend: `"embedded|{file name}|{target}"`. The file name
/// is in it because the same model in another file (another quantisation) gives other text.
pub fn identity_for(file_name: &str, target: &str) -> String {
    format!("embedded|{file_name}|{target}")
}

/// The model file name inside an identity made by [`identity_for`]; `None` for the identity of
/// another backend.
pub fn file_of_identity(identity: &str) -> Option<&str> {
    let rest = identity.strip_prefix("embedded|")?;
    rest.rsplit_once('|').map(|(file, _)| file)
}

/// The file name part of `path` (lossy for names that are not UTF-8).
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// The identity for a configuration before anything was loaded: the file it resolves to now, or
/// the configured name when there is none yet.
fn first_identity(dir: &Path, model: &str, target: &str) -> String {
    let file = resolve(dir, model, false).map_or_else(|_| model.to_string(), |p| file_name_of(&p));
    identity_for(&file, target)
}

/// Turns the bytes of generated tokens into text. A token may end in the middle of a UTF-8
/// sequence (a Korean syllable is three bytes and may span tokens), so the incomplete tail waits
/// for the next token. Invalid bytes become U+FFFD.
#[derive(Debug, Default)]
pub struct Utf8Pieces {
    pending: Vec<u8>,
}

impl Utf8Pieces {
    /// Adds the bytes of one token and gives the text that is complete now (`None`: nothing yet).
    pub fn push(&mut self, bytes: &[u8]) -> Option<String> {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    // The prefix up to `valid_up_to` is valid UTF-8 by definition.
                    out.push_str(std::str::from_utf8(&self.pending[..valid]).unwrap_or_default());
                    match e.error_len() {
                        Some(bad) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..valid + bad);
                        }
                        None => {
                            self.pending.drain(..valid);
                            break;
                        }
                    }
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Bytes still waiting for the rest of their character.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Ends the stream: an incomplete character at the end is dropped.
    pub fn finish(&mut self) {
        self.pending.clear();
    }
}

/// The in-process translation backend (`backend = "embedded"`).
///
/// Creating it reads no model: it only looks which file the setting names (a directory listing
/// at most). The model is loaded by the first `warm_up` or `translate`, on the engine thread.
pub struct EmbeddedBackend {
    dir: PathBuf,
    #[cfg(feature = "embedded")]
    engine: engine::Engine,
    #[cfg(not(feature = "embedded"))]
    identity: String,
}

impl EmbeddedBackend {
    /// A backend for `config` (`model`, `target`, prompts, `gpu_layers`, `threads`,
    /// `context_tokens`, `keep_alive_minutes`) reading model files from `models_dir`.
    pub fn new(config: &TranslateConfig, models_dir: PathBuf) -> EmbeddedBackend {
        let identity = first_identity(&models_dir, &config.model, &config.target);
        #[cfg(feature = "embedded")]
        {
            EmbeddedBackend {
                engine: engine::Engine::new(config, models_dir.clone(), identity),
                dir: models_dir,
            }
        }
        #[cfg(not(feature = "embedded"))]
        {
            EmbeddedBackend {
                dir: models_dir,
                identity,
            }
        }
    }

    /// Whether this executable has the engine (the `embedded` build feature).
    pub const fn built() -> bool {
        cfg!(feature = "embedded")
    }

    /// The folder the model files are read from.
    pub fn models_dir(&self) -> &Path {
        &self.dir
    }

    /// Whether a model is loaded now (always false without the engine).
    pub fn loaded(&self) -> bool {
        #[cfg(feature = "embedded")]
        {
            self.engine.loaded()
        }
        #[cfg(not(feature = "embedded"))]
        {
            false
        }
    }

    /// Replaces the keep-alive time (how long the model stays loaded after the last request).
    /// Takes effect when the engine thread starts, so call it before the first request; for
    /// tests.
    #[must_use]
    pub fn with_keep_alive(self, keep_alive: std::time::Duration) -> EmbeddedBackend {
        #[cfg(feature = "embedded")]
        {
            EmbeddedBackend {
                engine: self.engine.with_keep_alive(keep_alive),
                dir: self.dir,
            }
        }
        #[cfg(not(feature = "embedded"))]
        {
            let _ = keep_alive;
            self
        }
    }
}

#[cfg(not(feature = "embedded"))]
impl TranslateBackend for EmbeddedBackend {
    fn translate(
        &self,
        _req: &TranslateRequest,
        _cancel: &std::sync::atomic::AtomicBool,
        _on_text: &mut dyn FnMut(&str),
    ) -> Result<(), TranslateError> {
        Err(TranslateError::NotBuilt)
    }

    fn warm_up(&self, _cancel: &std::sync::atomic::AtomicBool) -> Result<(), TranslateError> {
        Err(TranslateError::NotBuilt)
    }

    fn models(
        &self,
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<String>, TranslateError> {
        list_models(&self.dir)
    }

    fn identity(&self) -> String {
        self.identity.clone()
    }
}

#[cfg(feature = "embedded")]
impl TranslateBackend for EmbeddedBackend {
    fn translate(
        &self,
        req: &TranslateRequest,
        cancel: &std::sync::atomic::AtomicBool,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(), TranslateError> {
        self.engine
            .call(engine::Work::Translate(req.clone()), cancel, on_text)
    }

    fn warm_up(&self, cancel: &std::sync::atomic::AtomicBool) -> Result<(), TranslateError> {
        self.engine.call(engine::Work::WarmUp, cancel, &mut |_| {})
    }

    fn models(
        &self,
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<String>, TranslateError> {
        list_models(&self.dir)
    }

    fn identity(&self) -> String {
        self.engine.identity()
    }

    fn release(&self) {
        self.engine.release();
    }
}

/// The engine thread and the calls into it. Only compiled with the `embedded` feature.
#[cfg(feature = "embedded")]
mod engine {
    use std::num::NonZeroU32;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use llama_cpp_2::context::LlamaContext;
    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::llama_backend::LlamaBackend;
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::model::{LlamaChatMessage, LlamaModel};
    use llama_cpp_2::sampling::LlamaSampler;
    use llama_cpp_2::token::LlamaToken;
    use llama_cpp_2::{LlamaBackendDeviceType, LogOptions};

    use super::{Utf8Pieces, file_name_of, identity_for, resolve_model, threads_for};
    use crate::store::TranslateConfig;
    use crate::store::config::GpuLayers;
    use crate::translate::prompt::{Prompts, gemma_chat, max_tokens};
    use crate::translate::{TranslateError, TranslateRequest};

    /// How often a waiting call looks at its `cancel` flag.
    const CANCEL_POLL: Duration = Duration::from_millis(20);
    /// The prompt is decoded in pieces of this many tokens so that a cancel is seen between
    /// them (one decode cannot be interrupted: on the CPU 64 tokens take ~0.3 s).
    const PROMPT_CHUNK: usize = 64;
    /// `n_gpu_layers` for "all" (more than any model has; the spike used the same).
    const ALL_LAYERS: u32 = 999;
    /// The answer of a warm-up request is cut after this many tokens.
    const WARM_UP_TOKENS: usize = 8;
    /// The longest a dropped backend waits for its engine thread to free the model.
    const DROP_WAIT: Duration = Duration::from_secs(3);

    /// What a call asks the engine thread to do.
    pub(super) enum Work {
        /// Load the model and run two short dummy requests (Vulkan compiles its shaders on the
        /// first requests; better here than in the first real one).
        WarmUp,
        /// Translate.
        Translate(TranslateRequest),
        /// Drop the model now.
        Release,
    }

    struct Command {
        work: Work,
        /// Set by the caller when it gives up (its own cancel flag cannot cross threads).
        cancel: Arc<AtomicBool>,
        events: Sender<Event>,
    }

    enum Event {
        Text(String),
        Done(Result<(), TranslateError>),
    }

    /// The fixed settings of one backend.
    #[derive(Clone)]
    struct Settings {
        dir: PathBuf,
        model: String,
        prompts: Prompts,
        gpu_layers: GpuLayers,
        threads: u32,
        context_tokens: u32,
        keep_alive: Duration,
    }

    /// What the backend and its engine thread share.
    struct Shared {
        /// The backend was dropped: stop as soon as possible.
        stop: AtomicBool,
        /// A model is loaded.
        loaded: AtomicBool,
        /// The cache identity (the file actually loaded).
        identity: Mutex<String>,
    }

    pub(super) struct Engine {
        settings: Settings,
        shared: Arc<Shared>,
        sender: Mutex<Option<Sender<Command>>>,
        thread: Mutex<Option<JoinHandle<()>>>,
    }

    impl Drop for Engine {
        /// Stops the engine thread: generation and loading end at the next token or progress
        /// report, an idle thread sees its channel close. Then waits — at most [`DROP_WAIT`] —
        /// until the thread has freed the model. Not waiting lets a process that exits right
        /// after (`refdesk --translate`, closing the app) tear the GPU driver down while the
        /// thread is still freeing its buffers, which crashes the process (seen with Vulkan on
        /// the NVIDIA driver). Freeing takes a fraction of a second; the app drops replaced
        /// backends off the UI thread.
        fn drop(&mut self) {
            self.shared.stop.store(true, Ordering::Relaxed);
            drop(
                self.sender
                    .get_mut()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take(),
            );
            let thread = self
                .thread
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let Some(thread) = thread else {
                return;
            };
            let deadline = Instant::now() + DROP_WAIT;
            while !thread.is_finished() {
                if Instant::now() >= deadline {
                    log::warn!("translate: the engine thread did not stop in time; leaving it");
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = thread.join();
        }
    }

    impl Engine {
        pub(super) fn new(config: &TranslateConfig, dir: PathBuf, identity: String) -> Engine {
            let minutes = u64::from(config.keep_alive_minutes.max(1));
            Engine {
                settings: Settings {
                    dir,
                    model: config.model.clone(),
                    prompts: Prompts::from_config(config),
                    gpu_layers: config.gpu_layers,
                    threads: threads_for(config.threads),
                    context_tokens: config.context_tokens,
                    keep_alive: Duration::from_secs(minutes * 60),
                },
                shared: Arc::new(Shared {
                    stop: AtomicBool::new(false),
                    loaded: AtomicBool::new(false),
                    identity: Mutex::new(identity),
                }),
                sender: Mutex::new(None),
                thread: Mutex::new(None),
            }
        }

        pub(super) fn with_keep_alive(mut self, keep_alive: Duration) -> Engine {
            self.settings.keep_alive = keep_alive;
            self
        }

        pub(super) fn loaded(&self) -> bool {
            self.shared.loaded.load(Ordering::Acquire)
        }

        pub(super) fn identity(&self) -> String {
            self.shared
                .identity
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// Queues a release behind the work already sent; returns at once.
        pub(super) fn release(&self) {
            let sender = self.sender.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(tx) = sender.as_ref() {
                let (events, _) = mpsc::channel();
                let _ = tx.send(Command {
                    work: Work::Release,
                    cancel: Arc::new(AtomicBool::new(false)),
                    events,
                });
            }
        }

        /// Sends `work` to the engine thread (starting it when needed) and passes its text to
        /// `on_text` until it is done. Returns `Err(Cancelled)` within ~20 ms of `cancel`.
        pub(super) fn call(
            &self,
            work: Work,
            cancel: &AtomicBool,
            on_text: &mut dyn FnMut(&str),
        ) -> Result<(), TranslateError> {
            if cancel.load(Ordering::Relaxed) {
                return Err(TranslateError::Cancelled);
            }
            let flag = Arc::new(AtomicBool::new(false));
            let (events, rx) = mpsc::channel();
            self.submit(Command {
                work,
                cancel: Arc::clone(&flag),
                events,
            })?;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    flag.store(true, Ordering::Relaxed);
                    return Err(TranslateError::Cancelled);
                }
                match rx.recv_timeout(CANCEL_POLL) {
                    Ok(Event::Text(text)) => on_text(&text),
                    Ok(Event::Done(result)) => return result,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(TranslateError::Engine(
                            "the engine thread stopped".to_string(),
                        ));
                    }
                }
            }
        }

        /// Hands `command` to the engine thread; starts the thread when there is none (or it
        /// ended after an internal error).
        fn submit(&self, command: Command) -> Result<(), TranslateError> {
            let mut sender = self.sender.lock().unwrap_or_else(PoisonError::into_inner);
            let command = match sender.as_ref() {
                Some(tx) => match tx.send(command) {
                    Ok(()) => return Ok(()),
                    Err(mpsc::SendError(back)) => back,
                },
                None => command,
            };
            let (tx, rx) = mpsc::channel();
            let settings = self.settings.clone();
            let shared = Arc::clone(&self.shared);
            let thread = std::thread::Builder::new()
                .name("refdesk-llama".to_string())
                .spawn(move || run(&settings, &shared, &rx))
                .map_err(|e| TranslateError::Engine(format!("cannot start a thread: {e}")))?;
            tx.send(command)
                .map_err(|_| TranslateError::Engine("the engine thread stopped".to_string()))?;
            *sender = Some(tx);
            // A thread that ended after an internal error is replaced; it has nothing to free.
            *self.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(thread);
            Ok(())
        }
    }

    /// The process-wide llama.cpp backend: it can be initialised once per process, so every
    /// engine thread shares this one and it is never freed. Its log (thousands of lines) is
    /// switched off first.
    fn llama() -> Result<&'static LlamaBackend, TranslateError> {
        static LLAMA: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();
        LLAMA
            .get_or_init(|| {
                llama_cpp_2::send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
                LlamaBackend::init().map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| TranslateError::Engine(format!("cannot start llama.cpp: {e}")))
    }

    fn reply(command: &Command, result: Result<(), TranslateError>) {
        let _ = command.events.send(Event::Done(result));
    }

    /// The engine thread: waits for a command, loads the model, serves until the model is to be
    /// dropped, and again; ends when the backend is gone.
    fn run(settings: &Settings, shared: &Arc<Shared>, rx: &Receiver<Command>) {
        let backend = match llama() {
            Ok(b) => Some(b),
            Err(e) => {
                log::warn!("translate: {e}");
                None
            }
        };
        while let Ok(command) = rx.recv() {
            if shared.stop.load(Ordering::Relaxed) {
                return;
            }
            if matches!(command.work, Work::Release) {
                continue;
            }
            if command.cancel.load(Ordering::Relaxed) {
                reply(&command, Err(TranslateError::Cancelled));
                continue;
            }
            let Some(backend) = backend else {
                reply(
                    &command,
                    Err(TranslateError::Engine(
                        "llama.cpp did not start".to_string(),
                    )),
                );
                continue;
            };
            let model = match load(backend, settings, shared, &command) {
                Ok(m) => m,
                Err(e) => {
                    reply(&command, Err(e));
                    continue;
                }
            };
            shared.loaded.store(true, Ordering::Release);
            let after = serve(backend, settings, shared, &model, command, rx);
            drop(model);
            shared.loaded.store(false, Ordering::Release);
            match after {
                After::Unload => log::info!("translate: model unloaded"),
                After::Exit => return,
            }
        }
    }

    /// A loaded model and what was learnt while loading it.
    struct Loaded {
        model: LlamaModel,
        prompts: Prompts,
        /// The model's own chat template applies (else Gemma's format is used).
        own_template: bool,
    }

    impl std::ops::Deref for Loaded {
        type Target = LlamaModel;
        fn deref(&self) -> &LlamaModel {
            &self.model
        }
    }

    /// Where the layers go: a count for `n_gpu_layers`, the ggml device index (when a GPU is
    /// used) and a description for the log.
    struct Placement {
        layers: u32,
        device: Option<usize>,
        name: String,
    }

    /// Picks the device. "all" uses the discrete GPU with the most memory; with only an
    /// integrated GPU it runs on the CPU, which the spike measured as faster (AMD 610M 4 ~ 8
    /// tokens/s against 17 ~ 19 on the CPU). A number of layers uses the best GPU of any kind.
    fn placement(want: GpuLayers) -> Placement {
        let cpu = |name: &str| Placement {
            layers: 0,
            device: None,
            name: name.to_string(),
        };
        let layers = match want {
            GpuLayers::Count(0) => return cpu("CPU (gpu_layers = 0)"),
            GpuLayers::Count(n) => n,
            GpuLayers::All => ALL_LAYERS,
        };
        let devices = llama_cpp_2::list_llama_ggml_backend_devices();
        let best = |kind: LlamaBackendDeviceType| {
            devices
                .iter()
                .filter(|d| d.device_type == kind)
                .max_by_key(|d| d.memory_total)
        };
        let gpu = match (best(LlamaBackendDeviceType::Gpu), want) {
            (Some(d), _) => Some(d),
            (None, GpuLayers::Count(_)) => best(LlamaBackendDeviceType::IntegratedGpu),
            (None, GpuLayers::All) => {
                if let Some(igpu) = best(LlamaBackendDeviceType::IntegratedGpu) {
                    log::info!(
                        "translate: only an integrated GPU ({}); running on the CPU, which is faster there",
                        igpu.description
                    );
                    return cpu("CPU (integrated GPU only)");
                }
                None
            }
        };
        match gpu {
            Some(d) => Placement {
                layers,
                device: Some(d.index),
                name: format!("{} ({})", d.name, d.description),
            },
            None => cpu("CPU (no GPU)"),
        }
    }

    /// Reads the first bytes: a file that does not start with `GGUF` is not a model file. This
    /// gives a reason where llama.cpp would only fail (its log is off).
    fn check_gguf(path: &Path) -> Result<(), TranslateError> {
        use std::io::Read;
        let mut magic = [0u8; 4];
        let read = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic));
        match read {
            Ok(()) if &magic == b"GGUF" => Ok(()),
            Ok(()) => Err(TranslateError::Engine(format!(
                "{} is not a GGUF model file",
                file_name_of(path)
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(TranslateError::ModelMissing(path.display().to_string()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(TranslateError::Engine(
                format!("{} is not a GGUF model file", file_name_of(path)),
            )),
            Err(e) => Err(TranslateError::Engine(format!(
                "cannot read {}: {e}",
                path.display()
            ))),
        }
    }

    /// Whether the file or its metadata names a TranslateGemma model, which wants its own
    /// prompt (see `prompt::passage_prompt`).
    fn is_translategemma(file: &str, model: &LlamaModel) -> bool {
        let squash = |s: &str| {
            s.chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase()
        };
        squash(file).starts_with("translategemma")
            || ["general.basename", "general.name"].iter().any(|key| {
                model
                    .meta_val_str(key)
                    .is_ok_and(|v| squash(&v).starts_with("translategemma"))
            })
    }

    fn load(
        backend: &LlamaBackend,
        settings: &Settings,
        shared: &Arc<Shared>,
        command: &Command,
    ) -> Result<Loaded, TranslateError> {
        let path = resolve_model(&settings.dir, &settings.model)?;
        check_gguf(&path)?;
        let file = file_name_of(&path);
        *shared
            .identity
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            identity_for(&file, &settings.prompts.target);

        let place = placement(settings.gpu_layers);
        let mut params = LlamaModelParams::default().with_n_gpu_layers(place.layers);
        if let Some(device) = place.device {
            params = params
                .with_devices(&[device])
                .map_err(|e| TranslateError::Engine(e.to_string()))?;
        }
        // Returning false aborts the load: a cancelled request or a dropped backend does not
        // wait for it.
        let (cancel, stop) = (Arc::clone(&command.cancel), Arc::clone(shared));
        params = params.with_progress_callback(move |_| {
            !cancel.load(Ordering::Relaxed) && !stop.stop.load(Ordering::Relaxed)
        });
        let started = Instant::now();
        let model = LlamaModel::load_from_file(backend, &path, &params).map_err(|e| {
            if command.cancel.load(Ordering::Relaxed) || shared.stop.load(Ordering::Relaxed) {
                TranslateError::Cancelled
            } else {
                TranslateError::Engine(format!(
                    "llama.cpp could not load {file} ({e}: unsupported model, damaged file or not enough memory)"
                ))
            }
        })?;
        let mut prompts = settings.prompts.clone();
        prompts.model = if is_translategemma(&file, &model) {
            "translategemma".to_string()
        } else {
            file.clone()
        };
        let own_template = render(&model, "x", true).is_some();
        let all = model.n_layer().saturating_add(1);
        log::info!(
            "translate: loaded {file} in {} ms on {}, {}/{all} layers on the GPU, {} threads, chat template: {}",
            started.elapsed().as_millis(),
            place.name,
            place.layers.min(all),
            settings.threads,
            if own_template { "the model's" } else { "Gemma" },
        );
        Ok(Loaded {
            model,
            prompts,
            own_template,
        })
    }

    /// What to do after [`serve`].
    enum After {
        Unload,
        Exit,
    }

    /// Serves commands with the loaded model until it is to be dropped.
    fn serve(
        backend: &LlamaBackend,
        settings: &Settings,
        shared: &Shared,
        model: &Loaded,
        first: Command,
        rx: &Receiver<Command>,
    ) -> After {
        let n_ctx = match model.n_ctx_train() {
            0 => settings.context_tokens,
            train => settings.context_tokens.min(train),
        };
        let threads = i32::try_from(settings.threads).unwrap_or(16);
        let params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(n_ctx))
            .with_n_threads(threads)
            .with_n_threads_batch(threads);
        let mut ctx = match model.new_context(backend, params) {
            Ok(ctx) => ctx,
            Err(e) => {
                reply(
                    &first,
                    Err(TranslateError::Engine(format!(
                        "cannot create the context of {n_ctx} tokens ({e}; not enough memory?)"
                    ))),
                );
                return After::Unload;
            }
        };
        let mut warmed = false;
        let mut next = Some(first);
        loop {
            let command = match next.take() {
                Some(c) => c,
                None => match rx.recv_timeout(settings.keep_alive) {
                    Ok(c) => c,
                    Err(RecvTimeoutError::Timeout) => {
                        log::info!("translate: unloading the model after the keep-alive time");
                        return After::Unload;
                    }
                    Err(RecvTimeoutError::Disconnected) => return After::Exit,
                },
            };
            if shared.stop.load(Ordering::Relaxed) {
                return After::Exit;
            }
            let stopped =
                || command.cancel.load(Ordering::Relaxed) || shared.stop.load(Ordering::Relaxed);
            match &command.work {
                Work::Release => return After::Unload,
                _ if stopped() => reply(&command, Err(TranslateError::Cancelled)),
                Work::WarmUp => {
                    let mut result = Ok(());
                    if !warmed {
                        for req in [
                            TranslateRequest::word("bus", "Each ECU sends its frames on the bus."),
                            TranslateRequest::passage("The process is started."),
                        ] {
                            let message = model.prompts.message(&req);
                            result = generate(
                                model,
                                &mut ctx,
                                &message,
                                WARM_UP_TOKENS,
                                &stopped,
                                &mut |_| {},
                            );
                            if result.is_err() {
                                break;
                            }
                        }
                        warmed = result.is_ok();
                    }
                    reply(&command, result);
                }
                Work::Translate(req) => {
                    let message = model.prompts.message(req);
                    let budget = usize::try_from(max_tokens(req)).unwrap_or(usize::MAX);
                    let result = generate(model, &mut ctx, &message, budget, &stopped, &mut |t| {
                        let _ = command.events.send(Event::Text(t));
                    });
                    warmed |= result.is_ok();
                    reply(&command, result);
                }
            }
        }
    }

    /// `message` as one user turn in the model's chat template (`own`) or in Gemma's format.
    /// `None` when `own` is asked and the model has no template llama.cpp can apply.
    fn render(model: &LlamaModel, message: &str, own: bool) -> Option<String> {
        let message = message.replace('\0', "");
        if !own {
            return Some(gemma_chat(&message));
        }
        let template = model.chat_template(None).ok()?;
        let turn = LlamaChatMessage::new("user".to_string(), message).ok()?;
        model.apply_chat_template(&template, &[turn], true).ok()
    }

    fn tokens_of(model: &Loaded, message: &str) -> Vec<LlamaToken> {
        let text = render(model, message, model.own_template)
            .unwrap_or_else(|| gemma_chat(&message.replace('\0', "")));
        let vocab = model.vocab();
        vocab.tokenize(text.as_bytes(), vocab.should_add_bos(), true)
    }

    /// The prompt tokens for `message`, at most `room` of them. A message that does not fit is
    /// cut at its end (where a passage's text is) and wrapped again, so the turn still ends the
    /// way the model expects; llama.cpp never gets more than the context holds.
    fn fit_prompt(model: &Loaded, message: &str, room: usize) -> Vec<LlamaToken> {
        let mut text = message.to_string();
        let first = tokens_of(model, &text);
        let mut tokens = first.clone();
        for _ in 0..8 {
            if tokens.len() <= room {
                break;
            }
            let chars = text.chars().count();
            let keep = chars.saturating_mul(room) / tokens.len().max(1) * 9 / 10;
            text = text.chars().take(keep).collect();
            tokens = tokens_of(model, &text);
        }
        // The cut above shrinks the text by at least 10 % each round; this is only a fence.
        tokens.truncate(room);
        if tokens.len() < first.len() {
            log::info!(
                "translate: the prompt of {} tokens does not fit the context; cut to {}",
                first.len(),
                tokens.len()
            );
        }
        tokens
    }

    fn engine_error(e: impl std::fmt::Display) -> TranslateError {
        TranslateError::Engine(e.to_string())
    }

    /// One answer: greedy sampling until an end-of-generation token, `budget` tokens or the end
    /// of the context. The text goes to `emit` in whole characters. The KV cache is cleared
    /// first, so a cancelled request leaves the context usable.
    fn generate(
        model: &Loaded,
        ctx: &mut LlamaContext<'_>,
        message: &str,
        budget: usize,
        stopped: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(String),
    ) -> Result<(), TranslateError> {
        ctx.clear_kv_cache();
        let n_ctx = usize::try_from(ctx.n_ctx()).unwrap_or(usize::MAX);
        // Keep room for the answer: its budget, but at most half the context.
        let reserve = budget.min(n_ctx / 2);
        let room = n_ctx.saturating_sub(reserve + 1).max(1);
        let tokens = fit_prompt(model, message, room);
        if tokens.is_empty() {
            return Err(TranslateError::Engine("empty prompt".to_string()));
        }

        let mut batch = LlamaBatch::new(PROMPT_CHUNK, 1);
        let last = tokens.len() - 1;
        for (chunk_index, chunk) in tokens.chunks(PROMPT_CHUNK).enumerate() {
            if stopped() {
                return Err(TranslateError::Cancelled);
            }
            batch.clear();
            for (j, token) in chunk.iter().enumerate() {
                let i = chunk_index * PROMPT_CHUNK + j;
                let pos = i32::try_from(i).map_err(engine_error)?;
                batch
                    .add(*token, pos, &[0], i == last)
                    .map_err(engine_error)?;
            }
            ctx.decode(&mut batch).map_err(engine_error)?;
        }

        let vocab = model.vocab();
        let mut sampler = LlamaSampler::greedy();
        let mut pieces = Utf8Pieces::default();
        let mut one = LlamaBatch::new(1, 1);
        let mut pos = tokens.len();
        let mut sample_at = batch.n_tokens() - 1;
        let mut produced = 0;
        loop {
            if stopped() {
                return Err(TranslateError::Cancelled);
            }
            let token = sampler.sample(ctx, sample_at);
            if vocab.is_eog(token) || produced >= budget || pos + 1 >= n_ctx {
                break;
            }
            produced += 1;
            if let Some(text) = pieces.push(&vocab.token_to_piece(token, false, None)) {
                emit(text);
            }
            one.clear();
            one.add(token, i32::try_from(pos).map_err(engine_error)?, &[0], true)
                .map_err(engine_error)?;
            pos += 1;
            sample_at = 0;
            ctx.decode(&mut one).map_err(engine_error)?;
        }
        pieces.finish();
        Ok(())
    }
}
