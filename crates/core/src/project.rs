//! Projects: a folder that refdesk manages (`docs/SPEC.md` §7.15).
//!
//! A project is a folder with a project file in it. PDFs are copied into the folder when they
//! are imported; what refdesk keeps about the project (translations made ahead, the project's
//! trash) lives in a hidden data folder inside it, which the workspace scan skips.
//!
//! This file holds only what several parts need to agree on. Creating projects and importing
//! files is WP-90's; the translations made ahead are WP-92's.

use std::path::{Path, PathBuf};

/// Name of the project file in the project's folder.
pub const PROJECT_FILE: &str = "refdesk-project.toml";

/// Name of the hidden folder inside a project where refdesk keeps its own data.
pub const PROJECT_DATA_DIR: &str = ".refdesk";

/// Whether `root` is the folder of a project (it has a project file).
pub fn is_project(root: &Path) -> bool {
    root.join(PROJECT_FILE).is_file()
}

/// The hidden data folder of the project at `root`. It may not exist yet.
pub fn data_dir(root: &Path) -> PathBuf {
    root.join(PROJECT_DATA_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_a_project_when_it_has_the_project_file() {
        let dir = std::env::temp_dir().join(format!("refdesk-project-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        assert!(!is_project(&dir));
        std::fs::write(dir.join(PROJECT_FILE), "").expect("project file");
        assert!(is_project(&dir));
        assert_eq!(data_dir(&dir), dir.join(".refdesk"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
