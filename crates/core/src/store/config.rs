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
}
