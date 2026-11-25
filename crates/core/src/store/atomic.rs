//! Crash-safe file writing and the "read or fall back to default" helpers (R-8).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::DeserializeOwned;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `path` so that a crash at any point leaves either the old file or the new
/// one, never a mix.
///
/// The data goes to a temporary file in the same directory, is fsynced, and is renamed over
/// `path`; then the directory is fsynced (best effort: the rename has happened by then, so a
/// failure there is only logged). Missing parent directories are created. If anything fails the
/// temporary file is removed and `path` is untouched.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    fs::create_dir_all(parent)?;

    let tmp = parent.join(format!(
        ".{}.{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = write_then_rename(&tmp, path, bytes);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return result;
    }
    if let Err(e) = File::open(parent).and_then(|d| d.sync_all()) {
        log::warn!("could not fsync directory {}: {e}", parent.display());
    }
    Ok(())
}

fn write_then_rename(tmp: &Path, dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, dest)
}

/// `path` with `.bad` appended to its file name.
pub fn bad_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".bad");
    PathBuf::from(name)
}

/// Moves a damaged file to `<name>.bad`, replacing an older `.bad` file. Returns the new path.
pub fn quarantine(path: &Path) -> io::Result<PathBuf> {
    let bad = bad_path(path);
    fs::rename(path, &bad)?;
    Ok(bad)
}

/// A parsed file: the value, plus a note when some part of the file had to be dropped.
pub(crate) struct Loaded<T> {
    pub value: T,
    pub dropped: Option<String>,
}

/// Reads `path` and parses it with `parse`, never failing.
///
/// * Missing, unreadable, empty or whitespace-only file: `T::default()`. Nothing is moved.
/// * `parse` returns `Err`: warn, move the file to `.bad`, `T::default()`.
/// * `parse` returns a value with `dropped` set: warn and keep a copy of the original in `.bad`
///   (the next save replaces the file), then use the value.
pub(crate) fn load_with<T: Default>(
    path: &Path,
    what: &str,
    parse: impl FnOnce(&[u8]) -> Result<Loaded<T>, String>,
) -> T {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return T::default(),
        Err(e) => {
            log::warn!(
                "{what}: cannot read {}: {e}; using defaults",
                path.display()
            );
            return T::default();
        }
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return T::default();
    }
    match parse(&bytes) {
        Ok(Loaded {
            value,
            dropped: None,
        }) => value,
        Ok(Loaded {
            value,
            dropped: Some(note),
        }) => {
            let bad = bad_path(path);
            log::warn!(
                "{what}: {}: {note}; the original is kept as {}",
                path.display(),
                bad.display()
            );
            if let Err(e) = fs::write(&bad, &bytes) {
                log::warn!("{what}: cannot write {}: {e}", bad.display());
            }
            value
        }
        Err(note) => {
            log::warn!(
                "{what}: {} is damaged ({note}); using defaults",
                path.display()
            );
            if let Err(e) = quarantine(path) {
                log::warn!("{what}: cannot move {} to .bad: {e}", path.display());
            }
            T::default()
        }
    }
}

/// Reads a JSON file into `T`, never failing. A missing or empty file gives `T::default()`; a
/// file that does not parse gives `T::default()` and is moved to `.bad`.
pub fn read_json_or_default<T: DeserializeOwned + Default>(path: &Path) -> T {
    load_with(path, "json", |bytes| {
        serde_json::from_slice(bytes)
            .map(|value| Loaded {
                value,
                dropped: None,
            })
            .map_err(|e| e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn writes_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        assert_eq!(names(dir.path()), ["a.json"]);
    }

    #[test]
    fn overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        atomic_write(&path, b"old old old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(names(dir.path()), ["a.json"]);
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x/y/z/a.json");
        atomic_write(&path, b"deep").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"deep");
    }

    #[test]
    fn failure_when_target_is_a_directory_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("inner.txt"), b"keep").unwrap();

        assert!(atomic_write(&target, b"data").is_err());

        assert_eq!(fs::read(target.join("inner.txt")).unwrap(), b"keep");
        assert_eq!(names(dir.path()), ["target"]);
    }

    #[test]
    fn failure_to_create_the_temp_file_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let path = sub.join("a.json");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o555)).unwrap();

        let result = atomic_write(&path, b"new");

        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        if result.is_ok() {
            // Running as root: permissions do not apply, so there is nothing to check.
            return;
        }
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(names(&sub), ["a.json"]);
    }

    #[test]
    fn path_without_file_name_is_rejected() {
        assert!(atomic_write(Path::new("/"), b"x").is_err());
    }

    #[test]
    fn bad_path_appends_suffix() {
        assert_eq!(
            bad_path(Path::new("/a/session.json")),
            PathBuf::from("/a/session.json.bad")
        );
    }

    #[test]
    fn quarantine_replaces_an_older_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.json");
        fs::write(&path, b"second").unwrap();
        fs::write(bad_path(&path), b"first").unwrap();
        let bad = quarantine(&path).unwrap();
        assert_eq!(fs::read(bad).unwrap(), b"second");
        assert!(!path.exists());
    }

    #[test]
    fn read_json_or_default_handles_good_missing_empty_and_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.json");

        assert_eq!(read_json_or_default::<Vec<u32>>(&path), Vec::<u32>::new());

        fs::write(&path, "[1,2,3]").unwrap();
        assert_eq!(read_json_or_default::<Vec<u32>>(&path), vec![1, 2, 3]);

        fs::write(&path, "").unwrap();
        assert!(read_json_or_default::<Vec<u32>>(&path).is_empty());
        assert!(!bad_path(&path).exists());

        fs::write(&path, b"\xff\xfenot json").unwrap();
        assert!(read_json_or_default::<Vec<u32>>(&path).is_empty());
        assert_eq!(fs::read(bad_path(&path)).unwrap(), b"\xff\xfenot json");
        assert!(!path.exists());
    }
}
