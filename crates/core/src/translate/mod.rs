//! Translation through a model on a local server (`docs/ARCHITECTURE.md` §4.14, TR-5 ~ 8, 11 ~ 14).
//!
//! The app only knows [`TranslateBackend`]. Two implementations talk HTTP to a server on this
//! machine: [`OllamaBackend`] (Ollama's own API) and [`OpenAiBackend`] (an OpenAI compatible
//! API). A third, [`EmbeddedBackend`], runs llama.cpp in the process on a GGUF file when the
//! `embedded` build feature is on. [`backend_from_config`] picks one from `[translate]`.
//!
//! A backend blocks while it waits for the server, so it is only ever called from a
//! [`TranslateJob`] thread (R-1). Prompts and output clean-up are pure functions in [`prompt`];
//! [`TranslationCache`] remembers finished translations across runs (TR-6). [`segment`] splits a
//! page into sentences and [`pretranslate`] translates a document ahead of reading (TR-15 ~ 19).

pub mod cache;
pub mod embedded;
mod fallback;
mod http;
pub mod ollama;
pub mod openai;
pub mod pretranslate;
pub mod prompt;
pub mod segment;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::engine::Notifier;
use crate::store::{Paths, TranslateBackendKind, TranslateConfig};

pub use cache::{MAX_CACHE_ENTRIES, TranslationCache};
pub use embedded::EmbeddedBackend;
pub use fallback::FallbackBackend;
pub use ollama::OllamaBackend;
pub use openai::OpenAiBackend;
pub use pretranslate::{
    Assembled, Outcome, PageSource, PretranslateJob, PretranslateOptions, PretranslateOrder,
    PretranslateSettings, PretranslateSetup, PretranslateSnapshot, PretranslateStatus,
    PretranslateStore, SharedStore, StoredSegment, StoredTranslation, assemble, pick_page,
    store_path,
};
pub use prompt::{
    MAX_PASSAGE_CHARS, MAX_WORD_OUTPUT_CHARS, Prompts, clean_output, fill_passage_template,
    fill_word_template, gemma_chat, language_name, max_tokens, passage_prompt, prepare_passage,
    sentence_around, word_prompt,
};
pub use segment::{Segment, SkipReason, covering, segments, skip_reason, text_hash};

/// What is asked.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TranslateKind {
    /// A sentence or a passage: translate it as a whole.
    Passage,
    /// One word: give its meaning in the sentence `context` (may be empty).
    Word {
        /// The sentence the word is in.
        context: String,
    },
}

/// One translation request.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TranslateRequest {
    /// The text to translate (a passage, or the word alone).
    pub text: String,
    /// Passage or word.
    pub kind: TranslateKind,
}

impl TranslateRequest {
    /// A passage request.
    pub fn passage(text: impl Into<String>) -> TranslateRequest {
        TranslateRequest {
            text: text.into(),
            kind: TranslateKind::Passage,
        }
    }

    /// A word request with the sentence it is in.
    pub fn word(word: impl Into<String>, context: impl Into<String>) -> TranslateRequest {
        TranslateRequest {
            text: word.into(),
            kind: TranslateKind::Word {
                context: context.into(),
            },
        }
    }

    /// Whether this is a word request.
    pub fn is_word(&self) -> bool {
        matches!(self.kind, TranslateKind::Word { .. })
    }
}

/// Why a translation did not come. `Display` gives an English sentence for logs and the command
/// line; the app builds its own (translated) text from the variant.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TranslateError {
    /// The server could not be reached (not running, wrong address). The text says where and why.
    #[error("cannot reach the translation server: {0}")]
    Unreachable(String),
    /// The server does not have this model.
    #[error("the model {0} is not available on the translation server")]
    ModelMissing(String),
    /// The server answered with an error status.
    #[error("the translation server answered {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// The error text of the server (or the body).
        message: String,
    },
    /// The answer was not what the API promises (not JSON, cut off, ...).
    #[error("unexpected answer from the translation server: {0}")]
    Protocol(String),
    /// The request was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// `backend = "embedded"` in an executable built without the `embedded` feature.
    #[error(
        "this executable has no embedded translation engine; build it with `cargo build --features embedded-vulkan`"
    )]
    NotBuilt,
    /// The embedded engine failed: the model file could not be read (format, architecture,
    /// memory) or the engine reported an error.
    #[error("the embedded translation engine failed: {0}")]
    Engine(String),
    /// The configured server was unreachable and the local fallback also failed.
    #[error("server unavailable ({server}); local fallback failed ({local})")]
    Fallback {
        /// The configured server's error.
        server: Box<TranslateError>,
        /// The embedded model's error.
        local: Box<TranslateError>,
    },
    /// The configured environment variable for an OpenAI-compatible API key was not set.
    #[error("the environment variable {0} is not set")]
    MissingApiKey(String),
}

/// A translation server. Implementations block, so they are called from worker threads only.
pub trait TranslateBackend: Send + Sync {
    /// Streams the translation of `req` piece by piece into `on_text`. Returns
    /// `Err(Cancelled)` as soon as possible once `cancel` is set.
    fn translate(
        &self,
        req: &TranslateRequest,
        cancel: &AtomicBool,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<(), TranslateError>;

    /// Loads the model into memory (`ModelMissing` when the server does not have it,
    /// `Unreachable` when there is no server).
    fn warm_up(&self, cancel: &AtomicBool) -> Result<(), TranslateError>;

    /// The names of the models the server has (TR-12).
    fn models(&self, cancel: &AtomicBool) -> Result<Vec<String>, TranslateError>;

    /// The name that goes into cache keys: model and target language.
    fn identity(&self) -> String;

    /// Whether this backend is currently translating with a local fallback.
    fn local_fallback_active(&self) -> bool {
        false
    }

    /// Gives back the memory the model holds, now (translation mode off, another model picked).
    /// The next request loads it again. Does not block. Server backends leave this to the
    /// server's own keep-alive.
    fn release(&self) {}
}

/// The backend `[translate]` asks for (`backend`, `endpoint`, `model`, ...). The caller applies
/// the model picked in the app first (see `store::effective_translate_config`). The embedded
/// backend without `models_dir` uses the models folder of [`Paths::from_env`].
pub fn backend_from_config(config: &TranslateConfig) -> Arc<dyn TranslateBackend> {
    let dir = config
        .models_dir
        .clone()
        .unwrap_or_else(|| Paths::from_env().models_dir());
    if config.backend == TranslateBackendKind::Embedded {
        return Arc::new(EmbeddedBackend::new(config, dir));
    }
    let primary: Arc<dyn TranslateBackend> = match config.backend {
        TranslateBackendKind::Ollama => Arc::new(OllamaBackend::new(config)),
        TranslateBackendKind::OpenAi => Arc::new(OpenAiBackend::new(config)),
        TranslateBackendKind::Embedded => unreachable!("handled above"),
    };
    if !config.offline_fallback {
        return primary;
    }
    let mut local_config = config.clone();
    local_config.backend = TranslateBackendKind::Embedded;
    local_config.model = config.fallback_model.clone();
    let local: Arc<dyn TranslateBackend> = Arc::new(EmbeddedBackend::new(&local_config, dir));
    Arc::new(FallbackBackend::new(primary, local))
}

/// The cache identity shared by both backends: `"{model}|{target}"`.
pub(crate) fn identity_of(model: &str, target: &str) -> String {
    format!("{model}|{target}")
}

/// Where a job is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranslateState {
    /// Still waiting for (more of) the answer.
    Running,
    /// Finished; the text is complete.
    Done,
    /// Failed; the text holds what came before the failure.
    Failed(TranslateError),
}

/// What a job has so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslateSnapshot {
    /// The translation so far, already cleaned with [`clean_output`] (empty for a warm-up).
    pub text: String,
    /// Running, done or failed.
    pub state: TranslateState,
}

impl TranslateSnapshot {
    fn running() -> TranslateSnapshot {
        TranslateSnapshot {
            text: String::new(),
            state: TranslateState::Running,
        }
    }

    /// Whether the job has ended (done or failed).
    pub fn finished(&self) -> bool {
        !matches!(self.state, TranslateState::Running)
    }
}

/// The notifier is called at most this often while text streams in (the end always notifies).
const NOTIFY_INTERVAL: Duration = Duration::from_millis(25);

struct Shared {
    cancel: AtomicBool,
    snapshot: Mutex<Arc<TranslateSnapshot>>,
}

impl Shared {
    fn set(&self, snapshot: TranslateSnapshot) {
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(snapshot);
    }

    fn get(&self) -> Arc<TranslateSnapshot> {
        Arc::clone(&self.snapshot.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// A translation (or a model warm-up) running on its own thread. Dropping it cancels it; the
/// drop does not wait, so it is safe on the UI thread.
pub struct TranslateJob {
    shared: Arc<Shared>,
}

impl Drop for TranslateJob {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for TranslateJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranslateJob")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl TranslateJob {
    /// Starts translating `req` and returns at once. The snapshot text grows as pieces come;
    /// `notifier` is called as it grows (at most every 25 ms) and always when the job ends
    /// (never after the job was dropped).
    pub fn start(
        backend: Arc<dyn TranslateBackend>,
        req: TranslateRequest,
        notifier: Notifier,
    ) -> TranslateJob {
        TranslateJob::spawn(notifier, move |shared, notifier| {
            let mut raw = String::new();
            let mut last_notify: Option<Instant> = None;
            let result = backend.translate(&req, &shared.cancel, &mut |piece: &str| {
                raw.push_str(piece);
                shared.set(TranslateSnapshot {
                    text: clean_output(&req.kind, &raw),
                    state: TranslateState::Running,
                });
                if last_notify.is_none_or(|t| t.elapsed() >= NOTIFY_INTERVAL) && !shared.cancelled()
                {
                    last_notify = Some(Instant::now());
                    notifier();
                }
            });
            TranslateSnapshot {
                text: clean_output(&req.kind, &raw),
                state: match result {
                    Ok(()) => TranslateState::Done,
                    Err(e) => TranslateState::Failed(e),
                },
            }
        })
    }

    /// Starts loading the model of `backend`. The snapshot text stays empty; only the state
    /// changes.
    pub fn warm_up(backend: Arc<dyn TranslateBackend>, notifier: Notifier) -> TranslateJob {
        TranslateJob::spawn(notifier, move |shared, _| TranslateSnapshot {
            text: String::new(),
            state: match backend.warm_up(&shared.cancel) {
                Ok(()) => TranslateState::Done,
                Err(e) => TranslateState::Failed(e),
            },
        })
    }

    /// The state now. Cheap: a shared copy that the thread replaces as it goes.
    pub fn snapshot(&self) -> Arc<TranslateSnapshot> {
        self.shared.get()
    }

    /// Runs `work` on a thread and publishes the snapshot it returns as the final one.
    fn spawn(
        notifier: Notifier,
        work: impl FnOnce(&Shared, &Notifier) -> TranslateSnapshot + Send + 'static,
    ) -> TranslateJob {
        let shared = Arc::new(Shared {
            cancel: AtomicBool::new(false),
            snapshot: Mutex::new(Arc::new(TranslateSnapshot::running())),
        });
        let worker = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("refdesk-translate".to_string())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| work(&worker, &notifier)));
                let last = match outcome {
                    Ok(last) => last,
                    // A bug must not leave the UI waiting for a job that never ends.
                    Err(_) => TranslateSnapshot {
                        text: worker.get().text.clone(),
                        state: TranslateState::Failed(TranslateError::Protocol(
                            "internal error".to_string(),
                        )),
                    },
                };
                worker.set(last);
                if !worker.cancelled() {
                    notifier();
                }
            });
        if let Err(e) = spawned {
            shared.set(TranslateSnapshot {
                text: String::new(),
                state: TranslateState::Failed(TranslateError::Protocol(format!(
                    "cannot start a thread: {e}"
                ))),
            });
        }
        TranslateJob { shared }
    }
}
