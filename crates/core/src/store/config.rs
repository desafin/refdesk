//! `config.toml`: settings the user edits by hand.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::atomic::{Loaded, atomic_write, load_with};
use super::prefs::{DEFAULT_SCROLL_SPEED, MAX_SCROLL_SPEED, MIN_SCROLL_SPEED};
use super::{FORMAT_VERSION, warn_if_newer};
use crate::model::ZoomMode;
use crate::store::paths::Paths;

/// Smallest and largest accepted `default_zoom` percentage.
const MIN_PERCENT: f64 = 10.0;
const MAX_PERCENT: f64 = 1000.0;

/// Color scheme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow the desktop setting.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

/// Interface language.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// Pick from the locale of the desktop.
    #[default]
    Auto,
    /// Korean.
    Ko,
    /// English.
    En,
}

/// The zoom a newly opened document starts with: `"fit-width"`, `"fit-page"` or a percentage
/// such as `125` (valid range 10 to 1000).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum DefaultZoom {
    /// Fit the widest page to the viewport width.
    #[default]
    FitWidth,
    /// Fit a whole page into the viewport.
    FitPage,
    /// A fixed zoom in percent; 100 is actual size.
    Percent(f32),
}

impl DefaultZoom {
    /// The view-model zoom this setting stands for.
    pub fn to_zoom_mode(self) -> ZoomMode {
        match self {
            DefaultZoom::FitWidth => ZoomMode::FitWidth,
            DefaultZoom::FitPage => ZoomMode::FitPage,
            DefaultZoom::Percent(p) => ZoomMode::Custom(p / 100.0),
        }
    }

    fn from_percent(p: f64) -> Result<DefaultZoom, String> {
        if p.is_finite() && (MIN_PERCENT..=MAX_PERCENT).contains(&p) {
            Ok(DefaultZoom::Percent(p as f32))
        } else {
            Err(format!(
                "zoom percentage must be between {MIN_PERCENT} and {MAX_PERCENT}, got {p}"
            ))
        }
    }
}

impl Serialize for DefaultZoom {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            DefaultZoom::FitWidth => s.serialize_str("fit-width"),
            DefaultZoom::FitPage => s.serialize_str("fit-page"),
            DefaultZoom::Percent(p) => {
                // Round away the f32 noise so that 133.3 is written as 133.3.
                let p = (f64::from(*p) * 100.0).round() / 100.0;
                if p.fract() == 0.0 {
                    s.serialize_i64(p as i64)
                } else {
                    s.serialize_f64(p)
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for DefaultZoom {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<DefaultZoom, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(f64),
            Text(String),
        }
        let result = match Raw::deserialize(d)? {
            Raw::Number(p) => DefaultZoom::from_percent(p),
            Raw::Text(t) => match t.trim().to_ascii_lowercase().as_str() {
                "fit-width" => Ok(DefaultZoom::FitWidth),
                "fit-page" => Ok(DefaultZoom::FitPage),
                other => other
                    .trim_end_matches('%')
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| format!("unknown zoom {t:?}"))
                    .and_then(DefaultZoom::from_percent),
            },
        };
        result.map_err(serde::de::Error::custom)
    }
}

/// Settings of the screen capture feature (`[capture]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    /// Render scale relative to the screen scale (must be positive).
    pub scale: f32,
    /// Save every capture as a PNG file, not only copy it.
    pub always_save: bool,
    /// Folder for saved captures; `None` means [`Paths::capture_dir`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
}

impl Default for CaptureConfig {
    fn default() -> CaptureConfig {
        CaptureConfig {
            scale: 2.0,
            always_save: false,
            dir: None,
        }
    }
}

/// Which kind of server `[translate]` talks to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranslateBackendKind {
    /// Ollama's own API (`/api/chat`, `/api/generate`, `/api/tags`).
    #[default]
    Ollama,
    /// An OpenAI compatible API (`/v1/chat/completions`, `/v1/models`): llama.cpp's
    /// `llama-server`, LM Studio, vLLM, ...
    #[serde(rename = "openai")]
    OpenAi,
    /// The in-process engine (llama.cpp) reading a GGUF model file; an optional build feature
    /// (`embedded`, `embedded-vulkan`).
    Embedded,
}

impl TranslateBackendKind {
    /// The address used when `endpoint` is not set.
    pub fn default_endpoint(self) -> &'static str {
        match self {
            TranslateBackendKind::Ollama => DEFAULT_OLLAMA_ENDPOINT,
            TranslateBackendKind::OpenAi => DEFAULT_OPENAI_ENDPOINT,
            TranslateBackendKind::Embedded => EMBEDDED_ENDPOINT,
        }
    }

    /// `"ollama"`, `"openai"` or `"embedded"`, as written in the file.
    pub fn name(self) -> &'static str {
        match self {
            TranslateBackendKind::Ollama => "ollama",
            TranslateBackendKind::OpenAi => "openai",
            TranslateBackendKind::Embedded => "embedded",
        }
    }
}

/// Default address of the Ollama backend (this machine).
pub const DEFAULT_OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";
/// Default address of the OpenAI compatible backend (`llama-server`'s default port).
pub const DEFAULT_OPENAI_ENDPOINT: &str = "http://127.0.0.1:8080";
/// What [`TranslateConfig::endpoint`] gives for the embedded backend, which has no server.
pub const EMBEDDED_ENDPOINT: &str = "in-process";
/// Default translation model.
pub const DEFAULT_TRANSLATE_MODEL: &str = "translategemma:4b";
/// Default context size of the embedded engine, in tokens.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 4096;
/// Smallest and largest accepted `context_tokens`.
const MIN_CONTEXT_TOKENS: u32 = 256;
const MAX_CONTEXT_TOKENS: u32 = 131_072;

/// How many layers of the model the embedded engine puts on the GPU (`gpu_layers`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GpuLayers {
    /// Every layer (`"all"`, the default). Falls back to the CPU when there is no usable GPU.
    #[default]
    All,
    /// This many layers; 0 runs on the CPU only.
    Count(u32),
}

impl Serialize for GpuLayers {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            GpuLayers::All => s.serialize_str("all"),
            GpuLayers::Count(n) => s.serialize_u32(*n),
        }
    }
}

impl<'de> Deserialize<'de> for GpuLayers {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<GpuLayers, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Count(i64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Count(n) => u32::try_from(n).map(GpuLayers::Count).map_err(|_| {
                serde::de::Error::custom(format!("{n} is not a layer count (0 or more)"))
            }),
            Raw::Text(t) if t.trim().eq_ignore_ascii_case("all") => Ok(GpuLayers::All),
            Raw::Text(t) => Err(serde::de::Error::custom(format!(
                "{t:?} is not \"all\" or a number"
            ))),
        }
    }
}

/// Settings of the translation feature (`[translate]`, `docs/ARCHITECTURE.md` §4.14).
///
/// The text to translate is sent to `endpoint`. The default is a server on this machine; an
/// address of another machine sends the document text there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslateConfig {
    /// `"ollama"` (default), `"openai"` or `"embedded"`.
    pub backend: TranslateBackendKind,
    /// Server address, `http://` only (the feature is meant for a local server, so no TLS).
    /// `None` means the default of the backend (see [`TranslateConfig::endpoint`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Model name as the server knows it. A model picked in the app (`prefs.json`) wins.
    /// For the embedded backend: a file name in `models_dir` (`.gguf` may be left out) or an
    /// absolute path.
    pub model: String,
    /// Target language code (`ko`, `en`, `ja`, ...).
    pub target: String,
    /// How long the model stays loaded after the last request (Ollama and embedded).
    pub keep_alive_minutes: u32,
    /// The word popup also translates the sentence the word is in.
    pub word_with_sentence: bool,
    /// Own instruction for passages; must contain `{text}`. `{target}` is the language name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passage_prompt: Option<String>,
    /// Own instruction for words; must contain `{word}`. `{sentence}` is the sentence the word
    /// is in, `{target}` the language name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub word_prompt: Option<String>,
    /// Embedded backend: the folder of the GGUF files. `None` means [`Paths::models_dir`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models_dir: Option<PathBuf>,
    /// Embedded backend: layers on the GPU (`"all"` or a number, 0 = CPU only).
    pub gpu_layers: GpuLayers,
    /// Embedded backend: CPU threads; 0 picks half the logical cores, at most 16.
    pub threads: u32,
    /// Embedded backend: context size in tokens (prompt and answer together).
    pub context_tokens: u32,
}

impl Default for TranslateConfig {
    fn default() -> TranslateConfig {
        TranslateConfig {
            backend: TranslateBackendKind::default(),
            endpoint: None,
            model: DEFAULT_TRANSLATE_MODEL.to_string(),
            target: "ko".to_string(),
            keep_alive_minutes: 30,
            word_with_sentence: true,
            passage_prompt: None,
            word_prompt: None,
            models_dir: None,
            gpu_layers: GpuLayers::All,
            threads: 0,
            context_tokens: DEFAULT_CONTEXT_TOKENS,
        }
    }
}

impl TranslateConfig {
    /// The server address without a trailing `/`: `endpoint`, or the default of the backend.
    pub fn endpoint(&self) -> String {
        let raw = self
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .unwrap_or(self.backend.default_endpoint());
        raw.trim_end_matches('/').to_string()
    }

    fn from_table(t: &toml::Table, notes: &mut Vec<String>) -> TranslateConfig {
        let d = TranslateConfig::default();
        let backend = match t.get("backend") {
            None => d.backend,
            Some(v) => match v.as_str().map(|s| s.trim().to_ascii_lowercase()).as_deref() {
                Some("ollama") => TranslateBackendKind::Ollama,
                Some("openai") => TranslateBackendKind::OpenAi,
                Some("embedded") => TranslateBackendKind::Embedded,
                _ => {
                    notes.push(format!(
                        "translate.backend: {v} is not \"ollama\", \"openai\" or \"embedded\""
                    ));
                    d.backend
                }
            },
        };
        let endpoint = take::<Option<String>>(t, "endpoint", None, notes).and_then(|e| {
            let e = e.trim().to_string();
            if is_http_endpoint(&e) {
                Some(e)
            } else {
                if e.to_ascii_lowercase().starts_with("https://") {
                    notes.push(format!(
                        "translate.endpoint: {e:?}: https is not supported (local http:// servers only)"
                    ));
                } else {
                    notes.push(format!("translate.endpoint: {e:?} is not an http:// address"));
                }
                None
            }
        });
        let model = take(t, "model", d.model.clone(), notes);
        let model = if model.trim().is_empty() {
            notes.push("translate.model: empty".to_string());
            d.model.clone()
        } else {
            model.trim().to_string()
        };
        let target = take(t, "target", d.target.clone(), notes);
        let target = if target.trim().is_empty() {
            notes.push("translate.target: empty".to_string());
            d.target.clone()
        } else {
            target.trim().to_ascii_lowercase()
        };
        let minutes = take::<i64>(
            t,
            "keep_alive_minutes",
            i64::from(d.keep_alive_minutes),
            notes,
        );
        let keep_alive_minutes = match u32::try_from(minutes) {
            Ok(m) if m > 0 => m,
            _ => {
                notes.push(format!(
                    "translate.keep_alive_minutes: {minutes} is not positive"
                ));
                d.keep_alive_minutes
            }
        };
        let passage_prompt = prompt_from(t, "passage_prompt", "{text}", notes);
        let word_prompt = prompt_from(t, "word_prompt", "{word}", notes);
        let models_dir = take::<Option<PathBuf>>(t, "models_dir", None, notes)
            .filter(|p| !p.as_os_str().is_empty());
        let threads = take::<i64>(t, "threads", i64::from(d.threads), notes);
        let threads = u32::try_from(threads).unwrap_or_else(|_| {
            notes.push(format!("translate.threads: {threads} is negative"));
            d.threads
        });
        let context = take::<i64>(t, "context_tokens", i64::from(d.context_tokens), notes);
        let context_tokens = match u32::try_from(context) {
            Ok(c) if (MIN_CONTEXT_TOKENS..=MAX_CONTEXT_TOKENS).contains(&c) => c,
            _ => {
                notes.push(format!(
                    "translate.context_tokens: {context} is not between {MIN_CONTEXT_TOKENS} and {MAX_CONTEXT_TOKENS}"
                ));
                d.context_tokens
            }
        };
        TranslateConfig {
            backend,
            endpoint,
            model,
            target,
            keep_alive_minutes,
            word_with_sentence: take(t, "word_with_sentence", d.word_with_sentence, notes),
            passage_prompt,
            word_prompt,
            models_dir,
            gpu_layers: take(t, "gpu_layers", d.gpu_layers, notes),
            threads,
            context_tokens,
        }
    }

    /// The folder of the embedded backend's model files: `models_dir`, or
    /// [`Paths::models_dir`] of `paths`.
    pub fn models_dir_or(&self, paths: &Paths) -> PathBuf {
        self.models_dir
            .clone()
            .unwrap_or_else(|| paths.models_dir())
    }
}

/// `http://` followed by something.
fn is_http_endpoint(e: &str) -> bool {
    e.len() > "http://".len()
        && e.get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
}

/// An own instruction template: `None` (and a note) when it lacks `placeholder`.
fn prompt_from(
    t: &toml::Table,
    key: &str,
    placeholder: &str,
    notes: &mut Vec<String>,
) -> Option<String> {
    let text = take::<Option<String>>(t, key, None, notes)?;
    if text.trim().is_empty() {
        return None;
    }
    if text.contains(placeholder) {
        Some(text)
    } else {
        notes.push(format!(
            "translate.{key}: has no {placeholder}; the built-in instruction is used"
        ));
        None
    }
}

/// The user's settings.
///
/// This file is edited by people, so the app never rewrites it by itself: [`Config::save`] exists
/// for an explicit "save settings" action only, and saving drops any comments in the file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Format version of the file.
    pub version: u32,
    /// `system`, `light` or `dark`.
    pub theme: Theme,
    /// `auto`, `ko` or `en`.
    pub language: Language,
    /// Whether the last session is restored at start.
    pub restore_session: bool,
    /// Zoom of newly opened documents.
    pub default_zoom: DefaultZoom,
    /// Budget of the page texture cache in MiB.
    pub texture_cache_mb: u32,
    /// Scroll speed multiplier (0.25 to 10; 1 is egui's default of 40 logical pixels per wheel
    /// notch). A value picked in the app's menu (`prefs.json`) wins over this one.
    #[serde(serialize_with = "serialize_scroll_speed")]
    pub scroll_speed: f32,
    /// `[capture]` table.
    pub capture: CaptureConfig,
    /// `[translate]` table.
    pub translate: TranslateConfig,
    /// `[keys]` table: command identifier to shortcut text. The app interprets the text.
    pub keys: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            version: FORMAT_VERSION,
            theme: Theme::default(),
            language: Language::default(),
            restore_session: true,
            default_zoom: DefaultZoom::default(),
            texture_cache_mb: 2048,
            scroll_speed: DEFAULT_SCROLL_SPEED,
            capture: CaptureConfig::default(),
            translate: TranslateConfig::default(),
            keys: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Reads the configuration. Never fails. A missing or empty file gives the defaults. A file
    /// that is not valid TOML gives the defaults and is moved to `.bad`. If only some values are
    /// wrong (an unknown `theme`, a negative cache size, ...) just those take their default and
    /// the rest is kept; each is logged.
    pub fn load(path: &Path) -> Config {
        load_with(path, "config", |bytes| {
            let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
            let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
            let mut notes = Vec::new();
            let config = Config::from_table(&table, &mut notes);
            for note in notes {
                log::warn!("config: {}: {note}; using the default", path.display());
            }
            Ok(Loaded {
                value: config,
                dropped: None,
            })
        })
    }

    /// Writes the configuration with [`atomic_write`].
    ///
    /// The app must not call this on its own (the file belongs to the user, and writing loses
    /// their comments and ordering); it is for tests and explicit settings actions.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        atomic_write(path, text.as_bytes())
    }

    /// The folder for saved captures: `capture.dir` if set, else the default in `paths`.
    pub fn capture_dir(&self, paths: &Paths) -> PathBuf {
        self.capture
            .dir
            .clone()
            .unwrap_or_else(|| paths.capture_dir.clone())
    }

    fn from_table(table: &toml::Table, notes: &mut Vec<String>) -> Config {
        let d = Config::default();
        if let Some(v) = table.get("version").and_then(toml::Value::as_integer) {
            warn_if_newer("config", u64::try_from(v).unwrap_or(0));
        }
        let mut config = Config {
            version: FORMAT_VERSION,
            theme: take(table, "theme", d.theme, notes),
            language: take(table, "language", d.language, notes),
            restore_session: take(table, "restore_session", d.restore_session, notes),
            default_zoom: take(table, "default_zoom", d.default_zoom, notes),
            texture_cache_mb: take(table, "texture_cache_mb", d.texture_cache_mb, notes),
            scroll_speed: scroll_speed_from(table, notes),
            capture: d.capture,
            translate: d.translate,
            keys: BTreeMap::new(),
        };
        match table.get("capture") {
            None => {}
            Some(toml::Value::Table(t)) => {
                let c = CaptureConfig::default();
                config.capture = CaptureConfig {
                    scale: take(t, "scale", c.scale, notes),
                    always_save: take(t, "always_save", c.always_save, notes),
                    dir: take(t, "dir", c.dir, notes),
                };
                if !(config.capture.scale.is_finite() && config.capture.scale > 0.0) {
                    notes.push(format!(
                        "capture.scale: {} is not positive",
                        config.capture.scale
                    ));
                    config.capture.scale = CaptureConfig::default().scale;
                }
            }
            Some(_) => notes.push("capture: expected a table".to_string()),
        }
        match table.get("translate") {
            None => {}
            Some(toml::Value::Table(t)) => config.translate = TranslateConfig::from_table(t, notes),
            Some(_) => notes.push("translate: expected a table".to_string()),
        }
        match table.get("keys") {
            None => {}
            Some(toml::Value::Table(t)) => {
                for (command, value) in t {
                    match value.as_str() {
                        Some(shortcut) => {
                            config.keys.insert(command.clone(), shortcut.to_string());
                        }
                        None => notes.push(format!("keys.{command}: expected a string")),
                    }
                }
            }
            Some(_) => notes.push("keys: expected a table".to_string()),
        }
        config
    }
}

/// `scroll_speed`: a number from 0.25 to 10, else the default.
fn scroll_speed_from(table: &toml::Table, notes: &mut Vec<String>) -> f32 {
    let speed = take(table, "scroll_speed", DEFAULT_SCROLL_SPEED, notes);
    if (MIN_SCROLL_SPEED..=MAX_SCROLL_SPEED).contains(&speed) {
        speed
    } else {
        // A value of the wrong type already gave the default (in range), so this is a number.
        notes.push(format!(
            "scroll_speed: {speed} is not between {MIN_SCROLL_SPEED} and {MAX_SCROLL_SPEED}"
        ));
        DEFAULT_SCROLL_SPEED
    }
}

/// Writes the multiplier without the noise of the f32 to f64 conversion (2.2, not 2.2000000477).
fn serialize_scroll_speed<S: Serializer>(speed: &f32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64((f64::from(*speed) * 100.0).round() / 100.0)
}

/// The value at `key` converted to `T`, or `default` (and a note) if it does not convert.
fn take<T: DeserializeOwned>(
    table: &toml::Table,
    key: &str,
    default: T,
    notes: &mut Vec<String>,
) -> T {
    match table.get(key) {
        None => default,
        Some(value) => match value.clone().try_into::<T>() {
            Ok(v) => v,
            Err(e) => {
                notes.push(format!("{key}: {}", e.message()));
                default
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::atomic::bad_path;
    use std::fs;

    fn load_str(text: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        Config::load(&path)
    }

    #[test]
    fn defaults_are_as_specified() {
        let c = Config::default();
        assert_eq!(c.theme, Theme::System);
        assert_eq!(c.language, Language::Auto);
        assert!(c.restore_session);
        assert_eq!(c.default_zoom, DefaultZoom::FitWidth);
        assert_eq!(c.texture_cache_mb, 2048);
        assert_eq!(c.capture.scale, 2.0);
        assert!(!c.capture.always_save);
        assert_eq!(c.capture.dir, None);
        assert!(c.keys.is_empty());
    }

    #[test]
    fn roundtrip_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/config.toml");
        let mut c = Config {
            theme: Theme::Dark,
            language: Language::Ko,
            restore_session: false,
            default_zoom: DefaultZoom::Percent(133.3),
            texture_cache_mb: 512,
            ..Config::default()
        };
        c.capture = CaptureConfig {
            scale: 3.0,
            always_save: true,
            dir: Some(PathBuf::from("/tmp/shots")),
        };
        c.keys.insert("find".into(), "Ctrl+F".into());
        c.keys.insert("quick_open".into(), "Ctrl+P".into());
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path), c);
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn default_config_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save(&path).unwrap();
        assert_eq!(Config::load(&path), Config::default());
    }

    #[test]
    fn empty_and_missing_files_give_all_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(Config::load(&path), Config::default());
        fs::write(&path, "").unwrap();
        assert_eq!(Config::load(&path), Config::default());
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn invalid_toml_gives_defaults_and_a_bad_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let junk = b"theme = = =\n[[[";
        fs::write(&path, junk).unwrap();
        assert_eq!(Config::load(&path), Config::default());
        assert_eq!(fs::read(bad_path(&path)).unwrap(), junk);
        assert!(!path.exists());
    }

    #[test]
    fn only_the_wrong_values_fall_back() {
        let c = load_str(
            r#"
            theme = "neon"
            language = "ko"
            restore_session = "yes"
            default_zoom = "huge"
            texture_cache_mb = -5

            [capture]
            scale = -1.0
            always_save = true
            dir = 12

            [keys]
            find = "Ctrl+F"
            bad = 3
            "#,
        );
        let d = Config::default();
        assert_eq!(c.theme, d.theme);
        assert_eq!(c.language, Language::Ko);
        assert_eq!(c.restore_session, d.restore_session);
        assert_eq!(c.default_zoom, d.default_zoom);
        assert_eq!(c.texture_cache_mb, d.texture_cache_mb);
        assert_eq!(c.capture.scale, d.capture.scale);
        assert!(c.capture.always_save);
        assert_eq!(c.capture.dir, None);
        assert_eq!(c.keys.len(), 1);
        assert_eq!(c.keys["find"], "Ctrl+F");
    }

    #[test]
    fn wrongly_typed_tables_fall_back() {
        let c = load_str("theme = \"dark\"\ncapture = 5\nkeys = \"x\"\n");
        assert_eq!(c.theme, Theme::Dark);
        assert_eq!(c.capture, CaptureConfig::default());
        assert!(c.keys.is_empty());
    }

    #[test]
    fn unknown_keys_are_ignored_and_higher_version_is_read() {
        let c =
            load_str("version = 42\nfuture_option = true\ntheme = \"light\"\n[new_table]\nx = 1\n");
        assert_eq!(c.theme, Theme::Light);
        assert!(c.restore_session);
    }

    #[test]
    fn default_zoom_accepts_three_forms() {
        assert_eq!(
            load_str("default_zoom = \"fit-width\"").default_zoom,
            DefaultZoom::FitWidth
        );
        assert_eq!(
            load_str("default_zoom = \"fit-page\"").default_zoom,
            DefaultZoom::FitPage
        );
        assert_eq!(
            load_str("default_zoom = 125").default_zoom,
            DefaultZoom::Percent(125.0)
        );
        assert_eq!(
            load_str("default_zoom = 87.5").default_zoom,
            DefaultZoom::Percent(87.5)
        );
        assert_eq!(
            load_str("default_zoom = \"150%\"").default_zoom,
            DefaultZoom::Percent(150.0)
        );
    }

    #[test]
    fn default_zoom_out_of_range_falls_back() {
        for text in [
            "default_zoom = 0",
            "default_zoom = -50",
            "default_zoom = 100000",
        ] {
            assert_eq!(load_str(text).default_zoom, DefaultZoom::FitWidth, "{text}");
        }
    }

    #[test]
    fn default_zoom_converts_to_zoom_mode() {
        assert_eq!(DefaultZoom::FitWidth.to_zoom_mode(), ZoomMode::FitWidth);
        assert_eq!(DefaultZoom::FitPage.to_zoom_mode(), ZoomMode::FitPage);
        assert_eq!(
            DefaultZoom::Percent(150.0).to_zoom_mode(),
            ZoomMode::Custom(1.5)
        );
        assert_eq!(
            DefaultZoom::Percent(100.0).to_zoom_mode(),
            ZoomMode::Custom(1.0)
        );
    }

    #[test]
    fn keys_table_is_read() {
        let c = load_str("[keys]\nfind = \"Ctrl+F\"\n\"toc.toggle\" = \"F9\"\n");
        assert_eq!(c.keys.len(), 2);
        assert_eq!(c.keys["toc.toggle"], "F9");
    }

    #[test]
    fn capture_scale_accepts_an_integer() {
        assert_eq!(load_str("[capture]\nscale = 3\n").capture.scale, 3.0);
    }

    #[test]
    fn capture_dir_falls_back_to_the_paths_default() {
        let paths = Paths::under(Path::new("/r"));
        let mut c = Config::default();
        assert_eq!(c.capture_dir(&paths), PathBuf::from("/r/captures"));
        c.capture.dir = Some(PathBuf::from("/custom"));
        assert_eq!(c.capture_dir(&paths), PathBuf::from("/custom"));
    }

    #[test]
    fn translate_defaults() {
        let t = Config::default().translate;
        assert_eq!(t.backend, TranslateBackendKind::Ollama);
        assert_eq!(t.endpoint, None);
        assert_eq!(t.endpoint(), "http://127.0.0.1:11434");
        assert_eq!(t.model, "translategemma:4b");
        assert_eq!(t.target, "ko");
        assert_eq!(t.keep_alive_minutes, 30);
        assert!(t.word_with_sentence);
        assert_eq!(t.passage_prompt, None);
        assert_eq!(t.word_prompt, None);
        assert_eq!(load_str("[translate]\n").translate, t);
    }

    #[test]
    fn translate_items_are_read() {
        let c = load_str(
            r#"
            [translate]
            backend = "openai"
            endpoint = "http://192.168.0.5:8080/"
            model = "qwen2.5:7b"
            target = "JA"
            keep_alive_minutes = 5
            word_with_sentence = false
            passage_prompt = "To {target}: {text}"
            word_prompt = "{word} in {sentence}"
            "#,
        );
        let t = c.translate;
        assert_eq!(t.backend, TranslateBackendKind::OpenAi);
        assert_eq!(t.endpoint.as_deref(), Some("http://192.168.0.5:8080/"));
        assert_eq!(t.endpoint(), "http://192.168.0.5:8080");
        assert_eq!(t.model, "qwen2.5:7b");
        assert_eq!(t.target, "ja");
        assert_eq!(t.keep_alive_minutes, 5);
        assert!(!t.word_with_sentence);
        assert_eq!(t.passage_prompt.as_deref(), Some("To {target}: {text}"));
        assert_eq!(t.word_prompt.as_deref(), Some("{word} in {sentence}"));
    }

    #[test]
    fn the_openai_backend_has_its_own_default_address() {
        let t = load_str("[translate]\nbackend = \"openai\"\n").translate;
        assert_eq!(t.endpoint(), "http://127.0.0.1:8080");
        let t = load_str("[translate]\nbackend = \"OpenAI\"\n").translate;
        assert_eq!(t.backend, TranslateBackendKind::OpenAi);
    }

    #[test]
    fn wrong_translate_values_fall_back_one_by_one() {
        let c = load_str(
            r#"
            theme = "dark"
            [translate]
            backend = "llamafile"
            endpoint = "ftp://x"
            model = "  "
            target = ""
            keep_alive_minutes = 0
            word_with_sentence = "yes"
            passage_prompt = "no placeholder"
            word_prompt = "uses {text} not word"
            "#,
        );
        assert_eq!(c.translate, TranslateConfig::default());
        assert_eq!(c.theme, Theme::Dark);
        let negative = load_str("[translate]\nkeep_alive_minutes = -3\nmodel = \"m\"\n");
        assert_eq!(negative.translate.keep_alive_minutes, 30);
        assert_eq!(negative.translate.model, "m");
        let not_table = load_str("translate = 3\ntexture_cache_mb = 7\n");
        assert_eq!(not_table.translate, TranslateConfig::default());
        assert_eq!(not_table.texture_cache_mb, 7);
    }

    #[test]
    fn an_https_endpoint_falls_back_to_the_default() {
        let t =
            load_str("[translate]\nendpoint = \"https://example.com\"\nmodel = \"m\"\n").translate;
        assert_eq!(t.endpoint, None);
        assert_eq!(t.endpoint(), "http://127.0.0.1:11434");
        assert_eq!(t.model, "m");
        let t = load_str("[translate]\nendpoint = \"HTTP://localhost:1\"\n").translate;
        assert_eq!(t.endpoint(), "HTTP://localhost:1");
        let t = load_str("[translate]\nendpoint = \"http://\"\n").translate;
        assert_eq!(t.endpoint, None);
    }

    #[test]
    fn translate_settings_roundtrip_through_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let c = Config {
            translate: TranslateConfig {
                backend: TranslateBackendKind::OpenAi,
                endpoint: Some("http://127.0.0.1:9000".into()),
                model: "x".into(),
                target: "de".into(),
                keep_alive_minutes: 3,
                word_with_sentence: false,
                passage_prompt: Some("{text}".into()),
                word_prompt: Some("{word}".into()),
                models_dir: Some("/m".into()),
                gpu_layers: GpuLayers::Count(20),
                threads: 6,
                context_tokens: 2048,
            },
            ..Config::default()
        };
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path), c);
        let all = Config {
            translate: TranslateConfig {
                backend: TranslateBackendKind::Embedded,
                ..TranslateConfig::default()
            },
            ..Config::default()
        };
        all.save(&path).unwrap();
        assert_eq!(Config::load(&path), all);
    }

    #[test]
    fn embedded_translate_defaults() {
        let t = Config::default().translate;
        assert_eq!(t.models_dir, None);
        assert_eq!(t.gpu_layers, GpuLayers::All);
        assert_eq!(t.threads, 0);
        assert_eq!(t.context_tokens, 4096);
        let paths = Paths::under(Path::new("/r"));
        assert_eq!(t.models_dir_or(&paths), PathBuf::from("/r/data/models"));
    }

    #[test]
    fn embedded_translate_items_are_read() {
        let t = load_str(
            r#"
            [translate]
            backend = "embedded"
            models_dir = "/home/u/models"
            model = "translategemma-4b-it.Q4_K_M"
            gpu_layers = 0
            threads = 12
            context_tokens = 8192
            "#,
        )
        .translate;
        assert_eq!(t.backend, TranslateBackendKind::Embedded);
        assert_eq!(t.backend.name(), "embedded");
        assert_eq!(t.endpoint(), "in-process");
        assert_eq!(t.models_dir, Some(PathBuf::from("/home/u/models")));
        let paths = Paths::under(Path::new("/r"));
        assert_eq!(t.models_dir_or(&paths), PathBuf::from("/home/u/models"));
        assert_eq!(t.model, "translategemma-4b-it.Q4_K_M");
        assert_eq!(t.gpu_layers, GpuLayers::Count(0));
        assert_eq!(t.threads, 12);
        assert_eq!(t.context_tokens, 8192);

        let t = load_str("[translate]\nbackend = \"Embedded\"\ngpu_layers = \"ALL\"\n").translate;
        assert_eq!(t.backend, TranslateBackendKind::Embedded);
        assert_eq!(t.gpu_layers, GpuLayers::All);
        let t = load_str("[translate]\ngpu_layers = 24\n").translate;
        assert_eq!(t.gpu_layers, GpuLayers::Count(24));
    }

    #[test]
    fn wrong_embedded_values_fall_back_one_by_one() {
        let t = load_str(
            r#"
            [translate]
            backend = "embedded"
            models_dir = ""
            gpu_layers = "most"
            threads = -2
            context_tokens = 10
            target = "ja"
            "#,
        )
        .translate;
        assert_eq!(t.backend, TranslateBackendKind::Embedded);
        assert_eq!(t.target, "ja");
        assert_eq!(t.models_dir, None);
        assert_eq!(t.gpu_layers, GpuLayers::All);
        assert_eq!(t.threads, 0);
        assert_eq!(t.context_tokens, 4096);
        for bad in [
            "gpu_layers = -1",
            "gpu_layers = 1.5",
            "gpu_layers = true",
            "threads = \"many\"",
            "context_tokens = 999999999",
            "context_tokens = \"big\"",
            "models_dir = 3",
        ] {
            let t = load_str(&format!("[translate]\nmodel = \"m\"\n{bad}\n")).translate;
            assert_eq!(
                t,
                TranslateConfig {
                    model: "m".into(),
                    ..TranslateConfig::default()
                },
                "{bad}"
            );
        }
    }
}
