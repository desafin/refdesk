//! `session.json`: what was open when the app last ran.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::atomic::{Loaded, atomic_write, load_with};
use super::{FORMAT_VERSION, SAVED_HISTORY_ENTRIES, json_version, lenient_items, warn_if_newer};
use crate::model::ViewState;

/// Smallest and largest share of a split the first child may take.
const MIN_FRACTION: f32 = 0.05;
const MAX_FRACTION: f32 = 0.95;

/// Smallest and largest share of the sidebar the explorer may take.
const MIN_SIDEBAR_SPLIT: f32 = 0.1;
const MAX_SIDEBAR_SPLIT: f32 = 0.9;

/// The windows and tabs that were open.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// Format version of the file.
    pub version: u32,
    /// The windows, in the order they were opened.
    pub windows: Vec<SessionWindow>,
    /// Every tab, keyed by the key the layouts refer to.
    pub tabs: BTreeMap<String, SessionTab>,
    /// The folder opened as a workspace, if any.
    pub workspace: Option<SessionWorkspace>,
}

impl Default for Session {
    fn default() -> Session {
        Session {
            version: FORMAT_VERSION,
            windows: Vec::new(),
            tabs: BTreeMap::new(),
            workspace: None,
        }
    }
}

/// The workspace folder (`docs/ARCHITECTURE.md` §4.13) as remembered by the session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionWorkspace {
    /// The folder.
    pub root: PathBuf,
    /// Folders the explorer showed expanded, as paths relative to `root`.
    #[serde(default)]
    pub expanded: Vec<PathBuf>,
}

/// One window of the session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionWindow {
    /// Whether this is the main window (closing it quits the app). Exactly one window has it.
    pub main: bool,
    /// Inner size in logical points. The position on screen is not stored.
    pub size: (f32, f32),
    /// Whether the window was maximized.
    pub maximized: bool,
    /// How the tabs are split and grouped.
    pub layout: LayoutNode,
    /// Whether the sidebar (explorer and outline together) was open. The name is kept from the
    /// time the sidebar held only the table of contents, so that stored files still match.
    pub toc_open: bool,
    /// Width of the sidebar in points.
    pub toc_width: f32,
    /// Whether the explorer section of the sidebar was expanded.
    pub explorer_open: bool,
    /// Whether the outline (table of contents) section of the sidebar was expanded.
    pub outline_open: bool,
    /// Share of the sidebar's height the explorer takes, within 0.1 ..= 0.9.
    pub sidebar_split: f32,
}

impl Default for SessionWindow {
    fn default() -> SessionWindow {
        SessionWindow {
            main: false,
            size: (1280.0, 800.0),
            maximized: false,
            layout: LayoutNode::default(),
            toc_open: false,
            toc_width: 260.0,
            explorer_open: true,
            outline_open: true,
            sidebar_split: default_fraction(),
        }
    }
}

fn default_fraction() -> f32 {
    0.5
}

/// The tab layout of a window: tab groups and the splits between them. This is our own format,
/// not the docking library's, so that a library upgrade cannot invalidate session files.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNode {
    /// A group of tabs shown one at a time.
    Tabs {
        /// Keys into [`Session::tabs`], in tab-bar order.
        #[serde(default)]
        tabs: Vec<String>,
        /// Index into `tabs` of the visible tab.
        #[serde(default)]
        active: usize,
    },
    /// Two areas side by side or one above the other.
    Split {
        /// `true` stacks the areas top to bottom, `false` puts them left to right.
        #[serde(default)]
        vertical: bool,
        /// Share of the space the first area takes, within 0.05 ..= 0.95.
        #[serde(default = "default_fraction")]
        fraction: f32,
        /// Left or top area.
        first: Box<LayoutNode>,
        /// Right or bottom area.
        second: Box<LayoutNode>,
    },
}

impl Default for LayoutNode {
    /// An empty tab group.
    fn default() -> LayoutNode {
        LayoutNode::Tabs {
            tabs: Vec::new(),
            active: 0,
        }
    }
}

impl LayoutNode {
    /// The tab keys of this layout in order (first area before second, tab-bar order).
    pub fn tab_keys(&self) -> TabKeys<'_> {
        TabKeys {
            pending: vec![self],
            current: [].iter(),
        }
    }
}

/// Iterator returned by [`LayoutNode::tab_keys`].
pub struct TabKeys<'a> {
    /// Nodes still to visit; the next one is at the end.
    pending: Vec<&'a LayoutNode>,
    current: std::slice::Iter<'a, String>,
}

impl<'a> Iterator for TabKeys<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        loop {
            if let Some(key) = self.current.next() {
                return Some(key.as_str());
            }
            match self.pending.pop()? {
                LayoutNode::Tabs { tabs, .. } => self.current = tabs.iter(),
                LayoutNode::Split { first, second, .. } => {
                    self.pending.push(second);
                    self.pending.push(first);
                }
            }
        }
    }
}

/// What one tab shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", from = "SessionTabRepr")]
pub enum SessionTab {
    /// A PDF and where the reader was in it.
    Pdf {
        /// The file.
        path: PathBuf,
        /// Position, zoom and history.
        view: ViewState,
        /// Whether this is a preview tab (replaced by the next file opened from the explorer).
        preview: bool,
    },
    /// A comparison of two PDFs.
    Diff {
        /// The left (older) file.
        left: PathBuf,
        /// The right (newer) file.
        right: PathBuf,
    },
}

/// Stored form of [`SessionTab`] in which a missing `view` is allowed.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionTabRepr {
    Pdf {
        path: PathBuf,
        #[serde(default)]
        view: Option<ViewState>,
        #[serde(default)]
        preview: bool,
    },
    Diff {
        left: PathBuf,
        right: PathBuf,
    },
}

impl From<SessionTabRepr> for SessionTab {
    fn from(repr: SessionTabRepr) -> SessionTab {
        match repr {
            SessionTabRepr::Pdf {
                path,
                view,
                preview,
            } => {
                let view = view.unwrap_or_else(|| ViewState::new(path.clone()));
                SessionTab::Pdf {
                    path,
                    view,
                    preview,
                }
            }
            SessionTabRepr::Diff { left, right } => SessionTab::Diff { left, right },
        }
    }
}

impl Session {
    /// Reads the session. Never fails: a missing or empty file gives an empty session, a damaged
    /// one gives an empty session and is moved to `.bad`. Windows or tabs that cannot be parsed
    /// are dropped (the original is then copied to `.bad`). The result is [`normalized`].
    ///
    /// [`normalized`]: Session::normalized
    pub fn load(path: &Path) -> Session {
        load_with(path, "session", parse_session).normalized()
    }

    /// Writes the session with [`atomic_write`]. Histories are cut to the 50 entries around the
    /// current one.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut out = self.clone();
        out.version = FORMAT_VERSION;
        for tab in out.tabs.values_mut() {
            if let SessionTab::Pdf { view, .. } = tab {
                view.history = view.history.trimmed(SAVED_HISTORY_ENTRIES);
            }
        }
        let bytes = serde_json::to_vec_pretty(&out).map_err(io::Error::other)?;
        atomic_write(path, &bytes)
    }

    /// Repairs inconsistencies so the app can trust the session:
    ///
    /// * tab keys in a layout that are not in `tabs` are removed, and so are repeats of a key
    ///   already used earlier;
    /// * a tab group left with no tabs is removed, and a split that lost one side is replaced by
    ///   the other side (a window's root stays as an empty group);
    /// * tabs that no layout shows are removed from `tabs`;
    /// * if there is any window, exactly one is `main` (the first one marked, else the first);
    /// * `active` is moved into range and `fraction` into 0.05 ..= 0.95;
    /// * each window's `sidebar_split` is moved into 0.1 ..= 0.9.
    pub fn normalized(mut self) -> Session {
        let mut used = HashSet::new();
        for window in &mut self.windows {
            let layout = std::mem::take(&mut window.layout);
            window.layout = prune(layout, &self.tabs, &mut used).unwrap_or_default();
            window.sidebar_split = if window.sidebar_split.is_nan() {
                default_fraction()
            } else {
                window
                    .sidebar_split
                    .clamp(MIN_SIDEBAR_SPLIT, MAX_SIDEBAR_SPLIT)
            };
        }
        self.tabs.retain(|key, _| used.contains(key));

        if !self.windows.is_empty() {
            let main = self.windows.iter().position(|w| w.main).unwrap_or(0);
            for (i, window) in self.windows.iter_mut().enumerate() {
                window.main = i == main;
            }
        }
        self
    }
}

/// Removes unknown and repeated keys and empty parts; `None` when nothing is left.
fn prune(
    node: LayoutNode,
    tabs: &BTreeMap<String, SessionTab>,
    used: &mut HashSet<String>,
) -> Option<LayoutNode> {
    match node {
        LayoutNode::Tabs { tabs: keys, active } => {
            let active_key = keys.get(active).cloned();
            let kept: Vec<String> = keys
                .into_iter()
                .filter(|k| tabs.contains_key(k) && used.insert(k.clone()))
                .collect();
            if kept.is_empty() {
                return None;
            }
            // Keep the same tab visible when earlier ones were removed.
            let active = active_key
                .and_then(|k| kept.iter().position(|x| *x == k))
                .unwrap_or_else(|| active.min(kept.len() - 1));
            Some(LayoutNode::Tabs { tabs: kept, active })
        }
        LayoutNode::Split {
            vertical,
            fraction,
            first,
            second,
        } => {
            let first = prune(*first, tabs, used);
            let second = prune(*second, tabs, used);
            match (first, second) {
                (Some(first), Some(second)) => Some(LayoutNode::Split {
                    vertical,
                    fraction: if fraction.is_nan() {
                        default_fraction()
                    } else {
                        fraction.clamp(MIN_FRACTION, MAX_FRACTION)
                    },
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

fn parse_session(bytes: &[u8]) -> Result<Loaded<Session>, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let obj = value
        .as_object()
        .ok_or_else(|| "top level is not an object".to_string())?;
    warn_if_newer("session", json_version(obj));

    let mut dropped = Vec::new();
    let windows = lenient_items(obj.get("windows"), "windows", &mut dropped);
    let mut tabs = BTreeMap::new();
    match obj.get("tabs") {
        None => {}
        Some(serde_json::Value::Object(map)) => {
            for (key, item) in map {
                match serde_json::from_value::<SessionTab>(item.clone()) {
                    Ok(tab) => {
                        tabs.insert(key.clone(), tab);
                    }
                    Err(e) => dropped.push(format!("tabs.{key}: {e}")),
                }
            }
        }
        Some(_) => dropped.push("tabs: expected an object".to_string()),
    }
    let workspace = match obj.get("workspace") {
        None | Some(serde_json::Value::Null) => None,
        Some(item) => match serde_json::from_value::<SessionWorkspace>(item.clone()) {
            Ok(workspace) => Some(workspace),
            Err(e) => {
                dropped.push(format!("workspace: {e}"));
                None
            }
        },
    };
    Ok(Loaded {
        value: Session {
            version: FORMAT_VERSION,
            windows,
            tabs,
            workspace,
        },
        dropped: (!dropped.is_empty()).then(|| dropped.join("; ")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Location, ZoomMode};
    use crate::nav::history::Place;
    use crate::store::atomic::bad_path;
    use std::fs;

    fn pdf(path: &str) -> SessionTab {
        SessionTab::Pdf {
            path: PathBuf::from(path),
            view: ViewState::new(PathBuf::from(path)),
            preview: false,
        }
    }

    fn tabs_node(keys: &[&str], active: usize) -> LayoutNode {
        LayoutNode::Tabs {
            tabs: keys.iter().map(|k| k.to_string()).collect(),
            active,
        }
    }

    fn split(fraction: f32, first: LayoutNode, second: LayoutNode) -> LayoutNode {
        LayoutNode::Split {
            vertical: false,
            fraction,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    fn window(main: bool, layout: LayoutNode) -> SessionWindow {
        SessionWindow {
            main,
            layout,
            ..SessionWindow::default()
        }
    }

    fn session(keys: &[&str], windows: Vec<SessionWindow>) -> Session {
        Session {
            windows,
            tabs: keys
                .iter()
                .map(|k| (k.to_string(), pdf(&format!("/docs/{k}.pdf"))))
                .collect(),
            ..Session::default()
        }
    }

    fn sample() -> Session {
        let mut s = session(
            &["a", "b", "c"],
            vec![
                SessionWindow {
                    main: true,
                    size: (1000.0, 700.0),
                    maximized: true,
                    layout: split(0.4, tabs_node(&["a", "b"], 1), tabs_node(&["c"], 0)),
                    toc_open: true,
                    toc_width: 300.0,
                    ..SessionWindow::default()
                },
                window(false, tabs_node(&[], 0)),
            ],
        );
        s.tabs.insert(
            "c".into(),
            SessionTab::Diff {
                left: "/l.pdf".into(),
                right: "/r.pdf".into(),
            },
        );
        s.windows.pop(); // keep the sample free of empty windows
        s
    }

    #[test]
    fn roundtrip_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let s = sample();
        s.save(&path).unwrap();
        assert_eq!(Session::load(&path), s);
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn saved_json_has_a_version_and_tagged_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        sample().save(&path).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["windows"][0]["layout"]["type"], "split");
        assert_eq!(v["tabs"]["c"]["type"], "diff");
    }

    #[test]
    fn garbage_gives_default_and_a_bad_copy_of_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let junk = b"\x00\x01 not json {{{";
        fs::write(&path, junk).unwrap();
        assert_eq!(Session::load(&path), Session::default());
        assert_eq!(fs::read(bad_path(&path)).unwrap(), junk);
    }

    #[test]
    fn empty_and_missing_files_give_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        assert_eq!(Session::load(&path), Session::default());
        fs::write(&path, "").unwrap();
        assert_eq!(Session::load(&path), Session::default());
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_fields_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::write(
            &path,
            r#"{"version": 1, "future": [1,2], "windows": [
                 {"layout": {"type": "tabs", "tabs": ["a"], "extra": 1}, "new_thing": true}
               ],
               "tabs": {"a": {"type": "pdf", "path": "/docs/a.pdf", "note": "x"}}}"#,
        )
        .unwrap();
        let s = Session::load(&path);
        assert_eq!(s.windows.len(), 1);
        let w = &s.windows[0];
        assert!(w.main, "the only window becomes main");
        assert_eq!(w.size, SessionWindow::default().size);
        assert_eq!(w.layout, tabs_node(&["a"], 0));
        assert_eq!(s.tabs["a"], pdf("/docs/a.pdf"));
    }

    #[test]
    fn higher_version_is_read_as_far_as_possible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::write(
            &path,
            r#"{"version": 99, "windows": [{"main": true, "layout": {"type": "tabs", "tabs": ["a"]}}],
                "tabs": {"a": {"type": "pdf", "path": "/docs/a.pdf"}}}"#,
        )
        .unwrap();
        let s = Session::load(&path);
        assert_eq!(s.windows.len(), 1);
        assert_eq!(s.tabs.len(), 1);
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn unparseable_entries_are_dropped_and_the_original_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let text = r#"{"version": 1,
              "windows": [{"main": true, "layout": {"type": "tabs", "tabs": ["a", "b"]}},
                          {"layout": {"type": "nonsense"}}],
              "tabs": {"a": {"type": "pdf", "path": "/docs/a.pdf"}, "b": {"type": "wat"}}}"#;
        fs::write(&path, text).unwrap();
        let s = Session::load(&path);
        assert_eq!(s.windows.len(), 1);
        assert_eq!(s.windows[0].layout, tabs_node(&["a"], 0));
        assert_eq!(s.tabs.keys().collect::<Vec<_>>(), ["a"]);
        assert_eq!(fs::read_to_string(bad_path(&path)).unwrap(), text);
    }

    #[test]
    fn saving_trims_history_to_fifty_and_keeps_the_current_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let doc = PathBuf::from("/docs/a.pdf");
        let place = |p: u32| Place {
            doc: doc.clone(),
            loc: Location::new(p, 0.0, 0.0),
        };
        let mut view = ViewState::new(doc.clone());
        for i in 0..199 {
            view.history.jump(place(i), place(i + 1));
        }
        assert_eq!(view.history.len(), 200);
        view.zoom = ZoomMode::Custom(1.25);
        let mut s = session(&["a"], vec![window(true, tabs_node(&["a"], 0))]);
        s.tabs.insert(
            "a".into(),
            SessionTab::Pdf {
                path: doc,
                view: view.clone(),
                preview: false,
            },
        );
        s.save(&path).unwrap();
        let loaded = Session::load(&path);
        let SessionTab::Pdf { view: back, .. } = &loaded.tabs["a"] else {
            panic!("not a pdf tab");
        };
        assert_eq!(back.history.len(), 50);
        assert_eq!(back.history.current(), view.history.current());
        assert_eq!(back.zoom, ZoomMode::Custom(1.25));
    }

    #[test]
    fn tab_missing_its_view_gets_a_fresh_one() {
        let tab: SessionTab =
            serde_json::from_str(r#"{"type": "pdf", "path": "/docs/a.pdf"}"#).unwrap();
        assert_eq!(tab, pdf("/docs/a.pdf"));
    }

    // ---- normalized ----

    #[test]
    fn normalized_removes_keys_that_are_not_in_tabs() {
        let s = session(&["a"], vec![window(true, tabs_node(&["ghost", "a"], 0))]).normalized();
        assert_eq!(s.windows[0].layout, tabs_node(&["a"], 0));
    }

    #[test]
    fn normalized_keeps_the_same_tab_active_when_earlier_ones_go() {
        let s = session(
            &["a", "b"],
            vec![window(true, tabs_node(&["ghost", "a", "b"], 2))],
        )
        .normalized();
        assert_eq!(s.windows[0].layout, tabs_node(&["a", "b"], 1));
    }

    #[test]
    fn normalized_folds_empty_tabs_and_splits() {
        let layout = split(
            0.5,
            split(0.5, tabs_node(&["ghost"], 0), tabs_node(&[], 0)),
            tabs_node(&["a"], 0),
        );
        let s = session(&["a"], vec![window(true, layout)]).normalized();
        assert_eq!(s.windows[0].layout, tabs_node(&["a"], 0));
    }

    #[test]
    fn normalized_folds_one_empty_side_of_a_split() {
        let layout = split(0.3, tabs_node(&["a"], 0), tabs_node(&["ghost"], 0));
        let s = session(&["a"], vec![window(true, layout)]).normalized();
        assert_eq!(s.windows[0].layout, tabs_node(&["a"], 0));
    }

    #[test]
    fn normalized_keeps_an_empty_root_group() {
        let s = session(&[], vec![window(true, tabs_node(&["ghost"], 3))]).normalized();
        assert_eq!(s.windows[0].layout, tabs_node(&[], 0));
    }

    #[test]
    fn normalized_removes_orphan_tabs() {
        let s = session(&["a", "orphan"], vec![window(true, tabs_node(&["a"], 0))]).normalized();
        assert_eq!(s.tabs.keys().collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn normalized_drops_repeated_keys() {
        let s = session(
            &["a", "b"],
            vec![
                window(true, tabs_node(&["a", "a", "b"], 0)),
                window(false, tabs_node(&["b"], 0)),
            ],
        )
        .normalized();
        assert_eq!(s.windows[0].layout, tabs_node(&["a", "b"], 0));
        assert_eq!(s.windows[1].layout, tabs_node(&[], 0));
    }

    #[test]
    fn normalized_makes_the_first_window_main_when_none_is() {
        let s = session(
            &["a", "b"],
            vec![
                window(false, tabs_node(&["a"], 0)),
                window(false, tabs_node(&["b"], 0)),
            ],
        )
        .normalized();
        assert_eq!(
            s.windows.iter().map(|w| w.main).collect::<Vec<_>>(),
            [true, false]
        );
    }

    #[test]
    fn normalized_keeps_only_the_first_main_when_there_are_two() {
        let s = session(
            &["a", "b"],
            vec![
                window(false, tabs_node(&["a"], 0)),
                window(true, tabs_node(&["b"], 0)),
                window(true, tabs_node(&[], 0)),
            ],
        )
        .normalized();
        assert_eq!(
            s.windows.iter().map(|w| w.main).collect::<Vec<_>>(),
            [false, true, false]
        );
    }

    #[test]
    fn normalized_without_windows_stays_empty() {
        let s = Session::default().normalized();
        assert!(s.windows.is_empty() && s.tabs.is_empty());
    }

    #[test]
    fn normalized_clamps_active_and_fraction() {
        let layout = split(
            1.7,
            tabs_node(&["a", "b"], 9),
            split(-3.0, tabs_node(&["c"], 0), tabs_node(&["d"], 0)),
        );
        let s = session(&["a", "b", "c", "d"], vec![window(true, layout)]).normalized();
        let LayoutNode::Split {
            fraction,
            first,
            second,
            ..
        } = &s.windows[0].layout
        else {
            panic!("expected a split");
        };
        assert_eq!(*fraction, 0.95);
        assert_eq!(**first, tabs_node(&["a", "b"], 1));
        let LayoutNode::Split { fraction, .. } = &**second else {
            panic!("expected a split");
        };
        assert_eq!(*fraction, 0.05);
    }

    #[test]
    fn normalized_replaces_nan_fraction() {
        let layout = split(f32::NAN, tabs_node(&["a"], 0), tabs_node(&["b"], 0));
        let s = session(&["a", "b"], vec![window(true, layout)]).normalized();
        let LayoutNode::Split { fraction, .. } = &s.windows[0].layout else {
            panic!("expected a split");
        };
        assert_eq!(*fraction, 0.5);
    }

    #[test]
    fn normalized_is_idempotent() {
        let layout = split(2.0, tabs_node(&["x", "a"], 5), tabs_node(&["b"], 0));
        let once = session(&["a", "b", "orphan"], vec![window(false, layout)]).normalized();
        assert_eq!(once.clone().normalized(), once);
    }

    #[test]
    fn tab_keys_iterates_in_layout_order() {
        let layout = split(
            0.5,
            split(0.5, tabs_node(&["a", "b"], 0), tabs_node(&["c"], 0)),
            tabs_node(&["d"], 0),
        );
        assert_eq!(layout.tab_keys().collect::<Vec<_>>(), ["a", "b", "c", "d"]);
        assert_eq!(tabs_node(&[], 0).tab_keys().count(), 0);
    }

    #[test]
    fn sidebar_and_workspace_fields_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let mut s = sample();
        s.windows[0].explorer_open = false;
        s.windows[0].outline_open = false;
        s.windows[0].sidebar_split = 0.3;
        s.workspace = Some(SessionWorkspace {
            root: "/docs".into(),
            expanded: vec!["a".into(), "a/b".into()],
        });
        s.tabs.insert(
            "a".into(),
            SessionTab::Pdf {
                path: "/docs/a/x.pdf".into(),
                view: ViewState::new("/docs/a/x.pdf".into()),
                preview: true,
            },
        );
        s.save(&path).unwrap();
        assert_eq!(Session::load(&path), s);
    }

    #[test]
    fn files_without_the_new_fields_read_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::write(
            &path,
            r#"{"version": 1, "windows": [
                 {"main": true, "toc_open": true, "toc_width": 300.0,
                  "layout": {"type": "tabs", "tabs": ["a"], "active": 0}}
               ],
               "tabs": {"a": {"type": "pdf", "path": "/docs/a.pdf"}}}"#,
        )
        .unwrap();
        let s = Session::load(&path);
        assert_eq!(s.workspace, None);
        let w = &s.windows[0];
        assert!(w.toc_open);
        assert_eq!(w.toc_width, 300.0);
        assert!(w.explorer_open);
        assert!(w.outline_open);
        assert_eq!(w.sidebar_split, 0.5);
        let SessionTab::Pdf { preview, .. } = &s.tabs["a"] else {
            panic!("not a pdf tab");
        };
        assert!(!preview);
        assert!(!bad_path(&path).exists());
    }

    #[test]
    fn a_file_with_the_new_fields_still_reads_by_the_old_shape() {
        // The shape before this change: the same fields minus the new ones, unknown ones ignored.
        #[derive(Deserialize)]
        struct OldWindow {
            toc_open: bool,
            toc_width: f32,
        }
        #[derive(Deserialize)]
        struct OldSession {
            windows: Vec<OldWindow>,
        }
        let mut s = sample();
        s.workspace = Some(SessionWorkspace {
            root: "/docs".into(),
            expanded: Vec::new(),
        });
        let json = serde_json::to_string(&s).unwrap();
        let old: OldSession = serde_json::from_str(&json).unwrap();
        assert!(old.windows[0].toc_open);
        assert_eq!(old.windows[0].toc_width, 300.0);
    }

    #[test]
    fn a_damaged_workspace_is_dropped_and_the_rest_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        fs::write(
            &path,
            r#"{"version": 1, "workspace": {"expanded": 3}, "windows": []}"#,
        )
        .unwrap();
        let s = Session::load(&path);
        assert_eq!(s.workspace, None);
        assert!(bad_path(&path).exists(), "the original is kept");
    }

    #[test]
    fn normalized_clamps_sidebar_split() {
        let mut windows = vec![
            window(true, tabs_node(&[], 0)),
            window(false, tabs_node(&[], 0)),
            window(false, tabs_node(&[], 0)),
            window(false, tabs_node(&[], 0)),
            window(false, tabs_node(&[], 0)),
        ];
        for (w, split) in windows
            .iter_mut()
            .zip([0.0, 1.0, f32::NAN, 0.25, f32::INFINITY])
        {
            w.sidebar_split = split;
        }
        let s = session(&[], windows).normalized();
        let got: Vec<f32> = s.windows.iter().map(|w| w.sidebar_split).collect();
        assert_eq!(got, [0.1, 0.9, 0.5, 0.25, 0.9]);
    }
}
