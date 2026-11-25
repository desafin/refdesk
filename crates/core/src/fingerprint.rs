//! File identity for "the same document, moved or renamed" (SES-4).
//!
//! Hashing reads the whole file, so call these from a background thread (R-1). Whether a file
//! needs hashing again is the caller's decision: if its [`FileStamp`] equals the one stored in
//! the record, the stored fingerprint is still good.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use xxhash_rust::xxh3::Xxh3;

/// Size of the read buffer used while hashing.
const BUFFER_SIZE: usize = 1 << 20;

/// Size and modification time of a file, used to tell whether it changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    /// Length in bytes.
    pub size: u64,
    /// Modification time in Unix seconds.
    pub mtime: i64,
}

/// XXH3-128 of the whole file as 32 lowercase hexadecimal digits. The file is streamed through a
/// 1 MiB buffer, not loaded into memory.
pub fn fingerprint_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Xxh3::new();
    let mut buf = vec![0u8; BUFFER_SIZE];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(format!("{:032x}", hasher.digest128()))
}

/// The current [`FileStamp`] of `path`.
pub fn stamp(path: &Path) -> io::Result<FileStamp> {
    let meta = fs::metadata(path)?;
    Ok(FileStamp {
        size: meta.len(),
        mtime: meta.mtime(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn same_content_gives_same_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "a.pdf", b"some bytes");
        let b = write(dir.path(), "renamed.pdf", b"some bytes");
        let fa = fingerprint_file(&a).unwrap();
        assert_eq!(fa, fingerprint_file(&b).unwrap());
        assert_eq!(fa.len(), 32);
        assert!(fa.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn one_byte_difference_changes_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "a", b"some bytes");
        let b = write(dir.path(), "b", b"some bytez");
        assert_ne!(fingerprint_file(&a).unwrap(), fingerprint_file(&b).unwrap());
    }

    #[test]
    fn streaming_matches_one_shot_hash_for_a_file_larger_than_the_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..3 * 1024 * 1024u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        assert!(data.len() > BUFFER_SIZE * 2);
        let path = write(dir.path(), "big", &data);
        let expected = format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&data));
        assert_eq!(fingerprint_file(&path).unwrap(), expected);
    }

    #[test]
    fn empty_file_has_a_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "empty", b"");
        let expected = format!("{:032x}", xxhash_rust::xxh3::xxh3_128(b""));
        assert_eq!(fingerprint_file(&path).unwrap(), expected);
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(fingerprint_file(&dir.path().join("nope")).is_err());
        assert!(stamp(&dir.path().join("nope")).is_err());
    }

    #[test]
    fn stamp_reports_size_and_changes_with_content_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "s", b"12345");
        let s1 = stamp(&path).unwrap();
        assert_eq!(s1.size, 5);
        assert!(s1.mtime > 1_000_000_000);
        fs::write(&path, b"1234567").unwrap();
        assert_eq!(stamp(&path).unwrap().size, 7);
    }
}
