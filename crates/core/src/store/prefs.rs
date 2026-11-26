//! `prefs.json`: settings changed from inside the app (see `docs/ARCHITECTURE.md` §4.10).
//!
//! `config.toml` belongs to the user and the app never rewrites it, so a value picked in a menu
//! lives here instead. A value present here wins over the same item of `config.toml`.
//!
//! The scroll speed rules (the allowed range, the menu steps, stepping up and down) are here too
//! so that they are tested without a window (R-10).

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::atomic::{Loaded, atomic_write, load_with};
use super::config::{Config, TranslateConfig};
use super::{FORMAT_VERSION, json_version, warn_if_newer};
use crate::diff::{MarginSide, Margins};

/// Smallest scroll speed multiplier that is accepted.
pub const MIN_SCROLL_SPEED: f32 = 0.25;
/// Largest scroll speed multiplier that is accepted.
pub const MAX_SCROLL_SPEED: f32 = 10.0;
/// The multiplier used when neither `prefs.json` nor `config.toml` says anything.
pub const DEFAULT_SCROLL_SPEED: f32 = 2.5;
/// Logical pixels one mouse wheel notch scrolls at multiplier 1 (egui's own default).
pub const BASE_LINE_SCROLL_SPEED: f32 = 40.0;
/// The multipliers of the scroll speed menu, ascending.
pub const SCROLL_SPEED_STEPS: [f32; 9] = [0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 8.0];

/// Two multipliers closer than this are the same one.
const SAME_SPEED: f32 = 0.005;

/// Settings the user changed inside the app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Format version of the file.
    pub version: u32,
    /// The scroll speed multiplier picked in the menu. `None` means "use `config.toml`".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scroll_speed: Option<f32>,
    /// Whether translation mode is on. `None` (never switched) means off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub translate_mode: Option<bool>,
    /// The translation model picked in the menu. `None` means "use `config.toml`".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub translate_model: Option<String>,
    /// The page margins a comparison leaves out, as last used. A new comparison starts with
    /// these. All zero (the default) is not written.
    #[serde(skip_serializing_if = "Margins::is_zero")]
    pub diff_margins: Margins,
}

impl Default for Prefs {
    fn default() -> Prefs {
        Prefs {
            version: FORMAT_VERSION,
            scroll_speed: None,
            translate_mode: None,
            translate_model: None,
            diff_margins: Margins::default(),
        }
    }
}

impl Prefs {
    /// Reads the preferences. Never fails: a missing or empty file gives the defaults; a file
    /// that is not a JSON object gives the defaults and is moved to `.bad` (R-8). Unknown fields
    /// are ignored; a field of the wrong type takes its default and is logged.
    pub fn load(path: &Path) -> Prefs {
        load_with(path, "prefs", |bytes| {
            let value: serde_json::Value =
                serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
            let serde_json::Value::Object(obj) = value else {
                return Err("expected a JSON object".to_string());
            };
            warn_if_newer("prefs", json_version(&obj));
            let scroll_speed = match obj.get("scroll_speed") {
                None | Some(serde_json::Value::Null) => None,
                Some(v) => match v.as_f64().filter(|s| s.is_finite()) {
                    Some(s) => Some(s as f32),
                    None => {
                        log::warn!("prefs: scroll_speed: expected a number, got {v}; ignored");
                        None
                    }
                },
            };
            let translate_mode = match obj.get("translate_mode") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::Bool(b)) => Some(*b),
                Some(v) => {
                    log::warn!("prefs: translate_mode: expected true or false, got {v}; ignored");
                    None
                }
            };
            let translate_model = match obj.get("translate_model") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(m)) if !m.trim().is_empty() => {
                    Some(m.trim().to_string())
                }
                Some(v) => {
                    log::warn!("prefs: translate_model: expected a model name, got {v}; ignored");
                    None
                }
            };
            let diff_margins = load_margins(obj.get("diff_margins"));
            Ok(Loaded {
                value: Prefs {
                    version: FORMAT_VERSION,
                    scroll_speed,
                    translate_mode,
                    translate_model,
                    diff_margins,
                },
                dropped: None,
            })
        })
    }

    /// Writes the preferences with [`atomic_write`].
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut out = self.clone();
        out.version = FORMAT_VERSION;
        let bytes = serde_json::to_vec_pretty(&out).map_err(io::Error::other)?;
        atomic_write(path, &bytes)
    }
}

/// The margins of a comparison from the `diff_margins` entry: a missing entry gives none; an
/// entry that is not an object, or a side that is not a number, is logged and read as 0.
/// Values are limited to the allowed range.
fn load_margins(value: Option<&serde_json::Value>) -> Margins {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Margins::default();
    };
    let Some(obj) = value.as_object() else {
        log::warn!("prefs: diff_margins: expected an object, got {value}; ignored");
        return Margins::default();
    };
    let mut margins = Margins::default();
    for side in MarginSide::ALL {
        let key = match side {
            MarginSide::Top => "top",
            MarginSide::Bottom => "bottom",
            MarginSide::Left => "left",
            MarginSide::Right => "right",
        };
        match obj.get(key) {
            None | Some(serde_json::Value::Null) => {}
            Some(v) => match v.as_f64().filter(|n| n.is_finite()) {
                Some(n) => margins.set(side, n as f32),
                None => {
                    log::warn!("prefs: diff_margins.{key}: expected a number, got {v}; ignored")
                }
            },
        }
    }
    margins.clamped()
}

/// `speed` limited to [`MIN_SCROLL_SPEED`]..=[`MAX_SCROLL_SPEED`]; something that is not a
/// number gives [`DEFAULT_SCROLL_SPEED`].
pub fn clamp_scroll_speed(speed: f32) -> f32 {
    if speed.is_finite() {
        speed.clamp(MIN_SCROLL_SPEED, MAX_SCROLL_SPEED)
    } else {
        DEFAULT_SCROLL_SPEED
    }
}

/// The scroll speed to use: the preference if there is one, else `config.toml`'s, limited to the
/// allowed range.
pub fn effective_scroll_speed(prefs: &Prefs, config: &Config) -> f32 {
    clamp_scroll_speed(prefs.scroll_speed.unwrap_or(config.scroll_speed))
}

/// Whether translation mode is on: the preference, off when it was never set.
pub fn effective_translate_mode(prefs: &Prefs) -> bool {
    prefs.translate_mode.unwrap_or(false)
}

/// The model to translate with: the one picked in the app if there is one, else
/// `config.toml`'s.
pub fn effective_translate_model(prefs: &Prefs, config: &TranslateConfig) -> String {
    prefs
        .translate_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map_or_else(|| config.model.clone(), str::to_string)
}

/// `config` with the model replaced by [`effective_translate_model`]: what the backend is built
/// from.
pub fn effective_translate_config(prefs: &Prefs, config: &TranslateConfig) -> TranslateConfig {
    TranslateConfig {
        model: effective_translate_model(prefs, config),
        ..config.clone()
    }
}

/// The multiplier one step above (`up`) or below `current` in [`SCROLL_SPEED_STEPS`]. A value
/// between steps moves to the nearest step in that direction. At or beyond the end of the list
/// the value stays as it is.
pub fn step_scroll_speed(current: f32, up: bool) -> f32 {
    let next = if up {
        SCROLL_SPEED_STEPS
            .iter()
            .copied()
            .find(|s| *s > current + SAME_SPEED)
    } else {
        SCROLL_SPEED_STEPS
            .iter()
            .rev()
            .copied()
            .find(|s| *s < current - SAME_SPEED)
    };
    next.unwrap_or(current)
}

/// One row of the scroll speed menu.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollSpeedItem {
    /// The multiplier the row sets.
    pub speed: f32,
    /// The row is the current speed (shown with a check mark).
    pub checked: bool,
}

/// The rows of the scroll speed menu for the current speed: every step, plus the current value
/// in its place when it is not a step (it came from the configuration file).
pub fn scroll_speed_menu(current: f32) -> Vec<ScrollSpeedItem> {
    let same = |a: f32, b: f32| (a - b).abs() < SAME_SPEED;
    let mut items: Vec<ScrollSpeedItem> = SCROLL_SPEED_STEPS
        .iter()
        .map(|s| ScrollSpeedItem {
            speed: *s,
            checked: same(*s, current),
        })
        .collect();
    if !items.iter().any(|i| i.checked) {
        let at = items
            .iter()
            .position(|i| i.speed > current)
            .unwrap_or(items.len());
        items.insert(
            at,
            ScrollSpeedItem {
                speed: current,
                checked: true,
            },
        );
    }
    items
}

/// "2.5×": the multiplier with at most two decimals.
pub fn scroll_speed_label(speed: f32) -> String {
    let text = format!("{speed:.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("{text}×")
}

/// The egui `line_scroll_speed` for a multiplier.
pub fn line_scroll_speed(multiplier: f32) -> f32 {
    BASE_LINE_SCROLL_SPEED * multiplier
}

/// The multiplier an egui `line_scroll_speed` stands for.
pub fn multiplier_of_line_scroll_speed(line_scroll_speed: f32) -> f32 {
    line_scroll_speed / BASE_LINE_SCROLL_SPEED
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::atomic::bad_path;
    use crate::store::paths::Paths;
    use std::fs;

    fn prefs_with(speed: Option<f32>) -> Prefs {
        Prefs {
            scroll_speed: speed,
            ..Prefs::default()
        }
    }

    #[test]
    fn prefs_file_is_in_the_state_dir() {
        let paths = Paths::under(Path::new("/r"));
        assert_eq!(
            paths.prefs_file(),
            std::path::PathBuf::from("/r/state/prefs.json")
        );
    }

    #[test]
    fn prefs_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/prefs.json");
        let prefs = prefs_with(Some(3.0));
        prefs.save(&path).unwrap();
        assert_eq!(Prefs::load(&path), prefs);
        assert!(!bad_path(&path).exists());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\""), "{text}");
    }

    #[test]
    fn an_unset_value_is_not_written_and_reads_back_unset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefs.json");
        Prefs::default().save(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("scroll_speed"));
        assert_eq!(Prefs::load(&path), Prefs::default());
    }

    #[test]
    fn a_missing_or_empty_file_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefs.json");
        assert_eq!(Prefs::load(&path), Prefs::default());
        fs::write(&path, "  \n").unwrap();
        assert_eq!(Prefs::load(&path), Prefs::default());
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn a_damaged_file_gives_the_defaults_and_a_bad_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefs.json");
        let junk = b"{ \"scroll_speed\": ";
        fs::write(&path, junk).unwrap();
        assert_eq!(Prefs::load(&path), Prefs::default());
        assert_eq!(fs::read(bad_path(&path)).unwrap(), junk);
        assert!(!path.exists());

        // Valid JSON that is not an object is damaged too.
        fs::write(&path, "[1, 2]").unwrap();
        assert_eq!(Prefs::load(&path), Prefs::default());
        assert!(!path.exists());
    }

    #[test]
    fn unknown_fields_are_ignored_and_a_wrong_type_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefs.json");
        fs::write(
            &path,
            r#"{"version": 1, "future_thing": {"a": 1}, "scroll_speed": 4}"#,
        )
        .unwrap();
        assert_eq!(Prefs::load(&path), prefs_with(Some(4.0)));
        fs::write(&path, r#"{"scroll_speed": "fast"}"#).unwrap();
        assert_eq!(Prefs::load(&path), Prefs::default());
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn a_missing_field_takes_its_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefs.json");
        fs::write(&path, "{}").unwrap();
        assert_eq!(Prefs::load(&path), Prefs::default());
    }

    #[test]
    fn config_scroll_speed_defaults_to_two_and_a_half() {
        assert_eq!(Config::default().scroll_speed, 2.5);
        assert_eq!(DEFAULT_SCROLL_SPEED, 2.5);
    }

    fn config_from(text: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        Config::load(&path)
    }

    #[test]
    fn config_scroll_speed_is_read_and_bad_values_fall_back() {
        assert_eq!(config_from("scroll_speed = 4.0").scroll_speed, 4.0);
        assert_eq!(config_from("scroll_speed = 3").scroll_speed, 3.0);
        assert_eq!(config_from("scroll_speed = 0.25").scroll_speed, 0.25);
        assert_eq!(config_from("scroll_speed = 10.0").scroll_speed, 10.0);
        for bad in [
            "scroll_speed = 0.1",
            "scroll_speed = 11",
            "scroll_speed = -2",
            "scroll_speed = nan",
            "scroll_speed = inf",
            "scroll_speed = \"fast\"",
            "scroll_speed = true",
        ] {
            assert_eq!(config_from(bad).scroll_speed, 2.5, "{bad}");
        }
    }

    #[test]
    fn a_bad_scroll_speed_does_not_affect_other_settings() {
        let c = config_from("scroll_speed = \"fast\"\ntheme = \"dark\"\ntexture_cache_mb = 99\n");
        assert_eq!(c.scroll_speed, 2.5);
        assert_eq!(c.theme, crate::store::config::Theme::Dark);
        assert_eq!(c.texture_cache_mb, 99);
    }

    #[test]
    fn config_scroll_speed_roundtrips_through_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let c = Config {
            scroll_speed: 2.2,
            ..Config::default()
        };
        c.save(&path).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("scroll_speed = 2.2\n")
        );
        assert_eq!(Config::load(&path), c);
    }

    #[test]
    fn the_effective_speed_prefers_the_preference() {
        let config = Config {
            scroll_speed: 4.0,
            ..Config::default()
        };
        assert_eq!(effective_scroll_speed(&prefs_with(Some(1.5)), &config), 1.5);
        assert_eq!(effective_scroll_speed(&prefs_with(None), &config), 4.0);
        assert_eq!(
            effective_scroll_speed(&Prefs::default(), &Config::default()),
            2.5
        );
    }

    #[test]
    fn the_effective_speed_is_limited_to_the_range() {
        let config = Config::default();
        assert_eq!(
            effective_scroll_speed(&prefs_with(Some(0.01)), &config),
            0.25
        );
        assert_eq!(
            effective_scroll_speed(&prefs_with(Some(99.0)), &config),
            10.0
        );
        assert_eq!(
            effective_scroll_speed(&prefs_with(Some(f32::NAN)), &config),
            2.5
        );
        // A config built in code with a bad value is limited too.
        let wild = Config {
            scroll_speed: 50.0,
            ..Config::default()
        };
        assert_eq!(effective_scroll_speed(&Prefs::default(), &wild), 10.0);
    }

    #[test]
    fn stepping_moves_one_step_inside_the_list() {
        assert_eq!(step_scroll_speed(2.5, true), 3.0);
        assert_eq!(step_scroll_speed(2.5, false), 2.0);
        assert_eq!(step_scroll_speed(1.0, false), 0.5);
        assert_eq!(step_scroll_speed(5.0, true), 8.0);
    }

    #[test]
    fn stepping_stays_at_the_ends() {
        assert_eq!(step_scroll_speed(8.0, true), 8.0);
        assert_eq!(step_scroll_speed(0.5, false), 0.5);
        // Beyond the list the value is kept rather than pulled back to the end.
        assert_eq!(step_scroll_speed(9.0, true), 9.0);
        assert_eq!(step_scroll_speed(0.3, false), 0.3);
    }

    #[test]
    fn stepping_from_a_value_between_steps_goes_to_the_nearest_step_in_that_direction() {
        assert_eq!(step_scroll_speed(2.2, true), 2.5);
        assert_eq!(step_scroll_speed(2.2, false), 2.0);
        assert_eq!(step_scroll_speed(6.0, true), 8.0);
        assert_eq!(step_scroll_speed(6.0, false), 5.0);
        assert_eq!(step_scroll_speed(0.3, true), 0.5);
        assert_eq!(step_scroll_speed(9.0, false), 8.0);
    }

    #[test]
    fn the_menu_marks_the_current_step() {
        let items = scroll_speed_menu(2.5);
        let speeds: Vec<f32> = items.iter().map(|i| i.speed).collect();
        assert_eq!(speeds, SCROLL_SPEED_STEPS.to_vec());
        let checked: Vec<f32> = items
            .iter()
            .filter(|i| i.checked)
            .map(|i| i.speed)
            .collect();
        assert_eq!(checked, vec![2.5]);
    }

    #[test]
    fn the_menu_shows_a_value_that_is_not_a_step_as_one_more_row() {
        let items = scroll_speed_menu(2.2);
        assert_eq!(items.len(), SCROLL_SPEED_STEPS.len() + 1);
        let at = items.iter().position(|i| i.checked).unwrap();
        assert_eq!(items[at].speed, 2.2);
        assert_eq!(items[at - 1].speed, 2.0);
        assert_eq!(items[at + 1].speed, 2.5);
        assert_eq!(items.iter().filter(|i| i.checked).count(), 1);

        let low = scroll_speed_menu(0.3);
        assert_eq!(
            low[0],
            ScrollSpeedItem {
                speed: 0.3,
                checked: true
            }
        );
        let high = scroll_speed_menu(9.0);
        assert_eq!(
            high.last().copied(),
            Some(ScrollSpeedItem {
                speed: 9.0,
                checked: true
            })
        );
    }

    #[test]
    fn labels_drop_needless_decimals() {
        assert_eq!(scroll_speed_label(2.5), "2.5×");
        assert_eq!(scroll_speed_label(1.0), "1×");
        assert_eq!(scroll_speed_label(0.25), "0.25×");
        assert_eq!(scroll_speed_label(8.0), "8×");
        assert_eq!(scroll_speed_label(10.0), "10×");
    }

    #[test]
    fn line_scroll_speed_is_forty_times_the_multiplier() {
        assert_eq!(line_scroll_speed(3.0), 120.0);
        assert_eq!(line_scroll_speed(1.0), 40.0);
        assert_eq!(multiplier_of_line_scroll_speed(120.0), 3.0);
    }

    #[test]
    fn translate_mode_and_model_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefs.json");
        let prefs = Prefs {
            translate_mode: Some(true),
            translate_model: Some("qwen2.5:7b".into()),
            ..Prefs::default()
        };
        prefs.save(&path).unwrap();
        assert_eq!(Prefs::load(&path), prefs);
        let off = Prefs {
            translate_mode: Some(false),
            ..Prefs::default()
        };
        off.save(&path).unwrap();
        assert_eq!(Prefs::load(&path), off);
    }

    #[test]
    fn a_prefs_file_without_translate_fields_reads_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefs.json");
        fs::write(&path, r#"{"version": 1, "scroll_speed": 3.0}"#).unwrap();
        let prefs = Prefs::load(&path);
        assert_eq!(prefs.scroll_speed, Some(3.0));
        assert_eq!(prefs.translate_mode, None);
        assert_eq!(prefs.translate_model, None);
        assert!(!effective_translate_mode(&prefs));
        // Unset fields are not written.
        Prefs::default().save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("translate"), "{text}");
        // Wrong types are dropped.
        fs::write(&path, r#"{"translate_mode": "on", "translate_model": 5}"#).unwrap();
        assert_eq!(Prefs::load(&path), Prefs::default());
        fs::write(&path, r#"{"translate_model": "  "}"#).unwrap();
        assert_eq!(Prefs::load(&path).translate_model, None);
    }

    #[test]
    fn the_picked_model_wins_over_the_config() {
        let config = TranslateConfig::default();
        let none = Prefs::default();
        assert_eq!(
            effective_translate_model(&none, &config),
            "translategemma:4b"
        );
        let picked = Prefs {
            translate_model: Some("qwen2.5:7b".into()),
            ..Prefs::default()
        };
        assert_eq!(effective_translate_model(&picked, &config), "qwen2.5:7b");
        let effective = effective_translate_config(&picked, &config);
        assert_eq!(effective.model, "qwen2.5:7b");
        assert_eq!(effective.endpoint(), config.endpoint());
        let blank = Prefs {
            translate_model: Some(" ".into()),
            ..Prefs::default()
        };
        assert_eq!(
            effective_translate_model(&blank, &config),
            "translategemma:4b"
        );
        assert!(effective_translate_mode(&Prefs {
            translate_mode: Some(true),
            ..Prefs::default()
        }));
    }
}
