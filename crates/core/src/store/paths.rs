//! Where the stored files live.

use std::path::{Path, PathBuf};

/// The directories and files refdesk uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// Holds `session.json`, `documents.json`, `prefs.json` and `search_history.json`.
    pub state_dir: PathBuf,
    /// Holds `config.toml`.
    pub config_dir: PathBuf,
    /// The Unix socket of the running instance.
    pub socket_path: PathBuf,
    /// Default folder for saved screen captures.
    pub capture_dir: PathBuf,
    /// Holds `models/` (`<data_home>/refdesk`, else `<home>/.local/share/refdesk`).
    pub data_dir: PathBuf,
    /// Where new projects go by default (`<documents>/refdesk`, see
    /// [`Paths::default_projects_dir`]).
    pub projects_dir: PathBuf,
}

impl Paths {
    /// Reads `XDG_STATE_HOME`, `XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR`, `XDG_DATA_HOME` and `HOME`
    /// and resolves the paths with [`Paths::from_values`] and [`Paths::with_data_home`]. This is
    /// the only function in the store that reads the environment.
    pub fn from_env() -> Paths {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        Paths::from_values(
            var("XDG_STATE_HOME"),
            var("XDG_CONFIG_HOME"),
            var("XDG_RUNTIME_DIR"),
            var("HOME"),
        )
        .with_data_home(var("XDG_DATA_HOME"))
        .with_documents_dir(
            directories::UserDirs::new().and_then(|u| u.document_dir().map(Path::to_path_buf)),
        )
    }

    /// Puts `projects_dir` at `<documents>/refdesk` when `documents` (the XDG documents
    /// folder) is an absolute path; otherwise it stays at `<home>/Documents/refdesk`.
    pub fn with_documents_dir(mut self, documents: Option<PathBuf>) -> Paths {
        if let Some(dir) = documents.filter(|p| p.is_absolute()) {
            self.projects_dir = dir.join("refdesk");
        }
        self
    }

    /// The folder new projects are made in by default (PROJ-1).
    pub fn default_projects_dir(&self) -> PathBuf {
        self.projects_dir.clone()
    }

    /// Puts `data_dir` at `<data_home>/refdesk` when `data_home` is an absolute path; otherwise
    /// it stays where [`Paths::from_values`] put it (`<home>/.local/share/refdesk`).
    pub fn with_data_home(mut self, data_home: Option<PathBuf>) -> Paths {
        if let Some(dir) = data_home.filter(|p| p.is_absolute()) {
            self.data_dir = dir.join("refdesk");
        }
        self
    }

    /// Resolves the paths from already-read values. A value that is empty or not an absolute
    /// path counts as unset (as the XDG specification says). Without `home` the paths fall back
    /// to `/tmp`.
    ///
    /// * `state_dir`: `<state_home>/refdesk`, else `<home>/.local/state/refdesk`
    /// * `config_dir`: `<config_home>/refdesk`, else `<home>/.config/refdesk`
    /// * `socket_path`: `<runtime_dir>/refdesk.sock`, else `<state_dir>/refdesk.sock`
    /// * `capture_dir`: `<home>/Pictures/refdesk`
    /// * `data_dir`: `<home>/.local/share/refdesk` (see [`Paths::with_data_home`])
    /// * `projects_dir`: `<home>/Documents/refdesk` (see [`Paths::with_documents_dir`])
    pub fn from_values(
        state_home: Option<PathBuf>,
        config_home: Option<PathBuf>,
        runtime_dir: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Paths {
        let absolute = |p: Option<PathBuf>| p.filter(|p| p.is_absolute());
        let home = absolute(home).unwrap_or_else(|| PathBuf::from("/tmp"));
        let state_dir = absolute(state_home)
            .unwrap_or_else(|| home.join(".local/state"))
            .join("refdesk");
        let config_dir = absolute(config_home)
            .unwrap_or_else(|| home.join(".config"))
            .join("refdesk");
        let socket_path = match absolute(runtime_dir) {
            Some(dir) => dir.join("refdesk.sock"),
            None => state_dir.join("refdesk.sock"),
        };
        Paths {
            state_dir,
            config_dir,
            socket_path,
            capture_dir: home.join("Pictures/refdesk"),
            data_dir: home.join(".local/share/refdesk"),
            projects_dir: home.join("Documents/refdesk"),
        }
    }

    /// Puts every path under `root`. For tests.
    pub fn under(root: &Path) -> Paths {
        Paths {
            state_dir: root.join("state"),
            config_dir: root.join("config"),
            socket_path: root.join("refdesk.sock"),
            capture_dir: root.join("captures"),
            data_dir: root.join("data"),
            projects_dir: root.join("projects"),
        }
    }

    /// `<state_dir>/session.json`.
    pub fn session_file(&self) -> PathBuf {
        self.state_dir.join("session.json")
    }

    /// `<state_dir>/documents.json`.
    pub fn documents_file(&self) -> PathBuf {
        self.state_dir.join("documents.json")
    }

    /// `<state_dir>/prefs.json`.
    pub fn prefs_file(&self) -> PathBuf {
        self.state_dir.join("prefs.json")
    }

    /// `<state_dir>/search_history.json`.
    pub fn search_history_file(&self) -> PathBuf {
        self.state_dir.join("search_history.json")
    }

    /// `<state_dir>/translations.json`: remembered translations.
    pub fn translations_file(&self) -> PathBuf {
        self.state_dir.join("translations.json")
    }

    /// `<state_dir>/annotations.json`: bookmarks, marks and highlights (ETC-2, ETC-3, ETC-5).
    pub fn annotations_file(&self) -> PathBuf {
        self.state_dir.join("annotations.json")
    }

    /// `<data_dir>/models`: the GGUF model files of the embedded translation engine.
    pub fn models_dir(&self) -> PathBuf {
        self.data_dir.join("models")
    }

    /// `<config_dir>/config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<PathBuf> {
        Some(PathBuf::from(s))
    }

    #[test]
    fn xdg_values_are_used_when_set() {
        let paths = Paths::from_values(
            p("/x/state"),
            p("/x/config"),
            p("/run/user/1000"),
            p("/home/u"),
        );
        assert_eq!(paths.state_dir, PathBuf::from("/x/state/refdesk"));
        assert_eq!(paths.config_dir, PathBuf::from("/x/config/refdesk"));
        assert_eq!(
            paths.socket_path,
            PathBuf::from("/run/user/1000/refdesk.sock")
        );
        assert_eq!(paths.capture_dir, PathBuf::from("/home/u/Pictures/refdesk"));
    }

    #[test]
    fn defaults_come_from_home_when_xdg_is_unset() {
        let paths = Paths::from_values(None, None, None, p("/home/u"));
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/home/u/.local/state/refdesk")
        );
        assert_eq!(paths.config_dir, PathBuf::from("/home/u/.config/refdesk"));
        assert_eq!(
            paths.socket_path,
            PathBuf::from("/home/u/.local/state/refdesk/refdesk.sock")
        );
        assert_eq!(paths.capture_dir, PathBuf::from("/home/u/Pictures/refdesk"));
    }

    #[test]
    fn relative_or_empty_xdg_values_count_as_unset() {
        let paths = Paths::from_values(p("relative"), p(""), p("also/relative"), p("/home/u"));
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/home/u/.local/state/refdesk")
        );
        assert_eq!(paths.config_dir, PathBuf::from("/home/u/.config/refdesk"));
        assert_eq!(
            paths.socket_path,
            PathBuf::from("/home/u/.local/state/refdesk/refdesk.sock")
        );
    }

    #[test]
    fn under_puts_everything_below_root() {
        let root = Path::new("/tmp/root");
        let paths = Paths::under(root);
        for dir in [
            &paths.state_dir,
            &paths.config_dir,
            &paths.socket_path,
            &paths.capture_dir,
            &paths.data_dir,
            &paths.projects_dir,
        ] {
            assert!(dir.starts_with(root), "{dir:?}");
        }
    }

    #[test]
    fn the_models_folder_is_under_the_data_home() {
        let paths = Paths::from_values(None, None, None, p("/home/u"));
        assert_eq!(
            paths.models_dir(),
            PathBuf::from("/home/u/.local/share/refdesk/models")
        );
        let paths = paths.with_data_home(p("/x/data"));
        assert_eq!(paths.models_dir(), PathBuf::from("/x/data/refdesk/models"));
        let unchanged = Paths::from_values(None, None, None, p("/home/u"))
            .with_data_home(p("relative"))
            .with_data_home(None);
        assert_eq!(
            unchanged.models_dir(),
            PathBuf::from("/home/u/.local/share/refdesk/models")
        );
        assert_eq!(
            Paths::under(Path::new("/r")).models_dir(),
            PathBuf::from("/r/data/models")
        );
    }

    #[test]
    fn new_projects_go_to_the_documents_folder() {
        let paths = Paths::from_values(None, None, None, p("/home/u"));
        assert_eq!(
            paths.default_projects_dir(),
            PathBuf::from("/home/u/Documents/refdesk")
        );
        let moved = paths.clone().with_documents_dir(p("/home/u/Dokumente"));
        assert_eq!(
            moved.default_projects_dir(),
            PathBuf::from("/home/u/Dokumente/refdesk")
        );
        assert_eq!(
            paths.clone().with_documents_dir(p("relative")),
            paths.clone().with_documents_dir(None)
        );
        assert_eq!(
            Paths::under(Path::new("/r")).default_projects_dir(),
            PathBuf::from("/r/projects")
        );
    }

    #[test]
    fn file_helpers_name_the_files() {
        let paths = Paths::under(Path::new("/r"));
        assert_eq!(paths.session_file(), PathBuf::from("/r/state/session.json"));
        assert_eq!(
            paths.documents_file(),
            PathBuf::from("/r/state/documents.json")
        );
        assert_eq!(paths.config_file(), PathBuf::from("/r/config/config.toml"));
        assert_eq!(
            paths.annotations_file(),
            PathBuf::from("/r/state/annotations.json")
        );
    }
}
